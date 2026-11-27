//! Deterministic resource-budget and accounting-equivalence gates for Suds.
//! Every test runs serially in this binary so process-wide operation counters
//! enforce exact invariants independent of machine load.

use std::fs;
use std::sync::Mutex;

use suds_core::bench::{BenchWorkspace, Counts, ScaleProfile, with_reference_scan};
use suds_core::budget::Budget;
use suds_core::development::{Environment, inspect};
use suds_core::maintain::{Context, maintain_observe_with_budget, maintain_with_budget};
use suds_core::reclaim::git::Git;
use suds_core::reclaim::liveness::Liveness;
use suds_core::reclaim::plan::{CheckoutScope, Observed, Policy, plan_reclaim};
use suds_core::registry;
use suds_core::state::{Directories, Settings};
use suds_core::stores::Programs;

static SERIAL: Mutex<()> = Mutex::new(());
const GATE_NOW: i64 = 2_000_000_000;

fn lock_serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

#[test]
fn filesystem_optimization_matches_reference_implementation_across_clones_and_hardlinks() {
    let _guard = lock_serial();
    let ws = BenchWorkspace::generate(&ScaleProfile::small()).unwrap();
    let roots = vec![ws.workspace_root.clone(), ws.store_root.clone()];
    let env = Environment::default();

    let before_opt = Counts::now();
    let optimized = inspect(&roots, &env);
    let opt_counts = Counts::now().since(before_opt);

    let before_ref = Counts::now();
    let reference = with_reference_scan(|| inspect(&roots, &env));
    let ref_counts = Counts::now().since(before_ref);

    println!(
        "measured ceilings: scan_equivalence entries={} opt_bulk={} opt_probes={} ref_probes={}",
        optimized.entries_visited,
        opt_counts.bulk_dir_calls,
        opt_counts.entry_probe_calls,
        ref_counts.entry_probe_calls
    );

    assert_eq!(optimized.outcome, reference.outcome);
    assert_eq!(optimized.entries_visited, reference.entries_visited);
    assert_eq!(optimized.workspaces, reference.workspaces);
    assert_eq!(optimized.checkouts, reference.checkouts);
    assert_eq!(optimized.stores, reference.stores);

    #[cfg(target_os = "macos")]
    {
        assert!(
            opt_counts.bulk_dir_calls > 0,
            "macOS scan must use batched getattrlistbulk calls"
        );
        // Directories still probe on entry (264), while all regular files skip per-entry probes (953 -> 264).
        assert!(
            opt_counts.entry_probe_calls <= 270
                && opt_counts.entry_probe_calls * 3 < ref_counts.entry_probe_calls,
            "batched scan exceeded per-entry probe ceiling: {} vs reference {}",
            opt_counts.entry_probe_calls,
            ref_counts.entry_probe_calls
        );
    }
}

#[test]
fn multi_slice_observation_advances_without_restarting_and_matches_single_pass() {
    let _guard = lock_serial();
    let ws = BenchWorkspace::generate(&ScaleProfile::small()).unwrap();
    let dirs = Directories {
        state: ws.state_dir.clone(),
        config: ws.config_dir.clone(),
    };
    let settings = Settings {
        roots: vec![ws.workspace_root.clone()],
        ceiling_bytes: Some(10 * 1024 * 1024 * 1024),
        idle_minutes: 1,
        standing_approval: false,
        tolerate_unreadable_processes: true,
        budgets: Default::default(),
    };
    let env = Environment::default();
    let programs = Programs::default();
    let git = Git::default();

    let full_budget = Budget {
        max_entries: None,
        pace_every: 0,
        background_priority: false,
        ..Budget::for_maintenance()
    };
    let mut ts = GATE_NOW;
    let single = maintain_observe_with_budget(
        &settings,
        &Context {
            directories: &dirs,
            environment: &env,
            programs: &programs,
            git: &git,
            now: ts,
        },
        &full_budget,
        &mut || ts,
    )
    .unwrap();
    assert!(single.pressure.complete);
    let _ = fs::remove_file(dirs.pressure());
    let _ = fs::remove_file(dirs.maintain_cursor());

    // Slice cap covers roughly two fifths of the workspace per slice, forcing multi-slice progress.
    let slice_cap = (single.entries_visited * 2 / 5).max(120);
    let slice_budget = Budget {
        max_entries: Some(slice_cap),
        pace_every: 0,
        background_priority: false,
        ..Budget::for_maintenance()
    };
    let mut slices = 0_u64;
    let mut total_visited = 0_u64;
    let mut peak_cursor_bytes = 0_u64;
    let mut final_usage = None;
    for _ in 0..12 {
        ts += 10;
        let report = maintain_observe_with_budget(
            &settings,
            &Context {
                directories: &dirs,
                environment: &env,
                programs: &programs,
                git: &git,
                now: ts,
            },
            &slice_budget,
            &mut || ts,
        )
        .unwrap();
        slices += 1;
        assert!(
            report.entries_visited <= slice_cap,
            "slice exceeded entry cap: {} > {slice_cap}",
            report.entries_visited
        );
        assert_eq!(report.draining_workers, 0);
        total_visited += report.entries_visited;
        if let Ok(meta) = fs::metadata(dirs.maintain_cursor()) {
            peak_cursor_bytes = peak_cursor_bytes.max(meta.len());
        }
        if report.pressure.complete && !dirs.maintain_cursor().exists() {
            final_usage = Some(report.pressure.usage_bytes);
            break;
        }
    }

    let resolved_usage = final_usage.expect("multi-slice observation must reach completion");
    println!(
        "measured ceilings: multi_slice_observe slices={slices} single_entries={} total_entries={total_visited} peak_cursor_bytes={peak_cursor_bytes}",
        single.entries_visited
    );
    assert!(slices >= 2, "expected multiple slices, ran {slices}");
    assert!(
        peak_cursor_bytes > 0 && peak_cursor_bytes <= 32_768,
        "unexpected cursor size: {peak_cursor_bytes}"
    );
    assert!(
        total_visited <= (single.entries_visited * 14) / 10,
        "multi-slice observation revisited completed work: {total_visited} vs single-pass {}",
        single.entries_visited
    );
    assert_eq!(
        resolved_usage, single.pressure.usage_bytes,
        "multi-slice usage diverged from single-pass usage"
    );
}

#[test]
fn reclaim_planning_bounds_git_subprocesses_by_scope() {
    let _guard = lock_serial();
    let ws = BenchWorkspace::generate(&ScaleProfile::small()).unwrap();
    let scan = inspect(
        std::slice::from_ref(&ws.workspace_root),
        &Environment::default(),
    );
    let reg = registry::reading(&ws.state_dir.join("registry.jsonl"));
    let observed = Observed {
        liveness: &Ok(Liveness::default()),
        registry: &reg,
    };

    let derived_git = Git::default();
    let derived_plan = plan_reclaim(
        &scan,
        &observed,
        &derived_git,
        Policy {
            idle_seconds: 60,
            checkouts: CheckoutScope::DerivedOnly,
            tolerate_unreadable_processes: true,
        },
        GATE_NOW,
    );
    let derived_launches = derived_git.runner().launches();

    let released_git = Git::default();
    let released_plan = plan_reclaim(
        &scan,
        &observed,
        &released_git,
        Policy {
            idle_seconds: 60,
            checkouts: CheckoutScope::Released,
            tolerate_unreadable_processes: true,
        },
        GATE_NOW,
    );
    let released_launches = released_git.runner().launches();
    let checkout_count = scan.checkouts.len() as u64;

    println!(
        "measured ceilings: reclaim_plan checkouts={checkout_count} derived_git={derived_launches} released_git={released_launches} candidates={}",
        released_plan.candidates.len()
    );
    assert!(
        !derived_plan.candidates.is_empty(),
        "expected build-state candidates"
    );
    // Exact measured ceilings on small profile (12 checkouts, 27 candidates): 15 derived, 78 released.
    assert!(
        derived_launches <= 15,
        "derived-only planning spawned {derived_launches} Git processes (ceiling 15)"
    );
    assert!(
        released_launches <= 78,
        "released-checkout planning spawned {released_launches} Git processes (ceiling 78)"
    );
}

#[test]
fn maintenance_budget_enforces_single_reader_and_entry_cap() {
    let _guard = lock_serial();
    let ws = BenchWorkspace::generate(&ScaleProfile::small()).unwrap();
    let dirs = Directories {
        state: ws.state_dir.clone(),
        config: ws.config_dir.clone(),
    };
    let settings = Settings {
        roots: vec![ws.workspace_root.clone()],
        ceiling_bytes: Some(1024),
        idle_minutes: 1,
        standing_approval: true,
        tolerate_unreadable_processes: true,
        budgets: Default::default(),
    };
    let default_maint = Budget::for_maintenance();
    assert_eq!(default_maint.max_total_readers, 1);
    assert_eq!(default_maint.helper_readers(), 0);

    let cap = 350_u64;
    let bounded = Budget {
        max_entries: Some(cap),
        pace_every: 0,
        background_priority: false,
        ..default_maint
    };
    let mut ts = GATE_NOW;
    let before = Counts::now();
    let report = maintain_with_budget(
        &settings,
        &Context {
            directories: &dirs,
            environment: &Environment::default(),
            programs: &Programs::default(),
            git: &Git::default(),
            now: ts,
        },
        &bounded,
        &mut || {
            ts += 1;
            ts
        },
    )
    .unwrap();
    let delta = Counts::now().since(before);
    println!(
        "measured ceilings: maintain_cap/{cap} entries_visited={} draining={} helper_launches={}",
        report.entries_visited, report.draining_workers, delta.helper_launches
    );
    assert!(report.entries_visited <= cap);
    assert_eq!(report.draining_workers, 0);
}

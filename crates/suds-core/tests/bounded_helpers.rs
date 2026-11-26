//! Regression tests for bounded helper execution, timeout-safe pseudoreference
//! preservation, cancellable coordination locks, and bounded removal effort.

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use suds_core::budget::{Budget, BudgetTracker};
use suds_core::development::{Environment, inspect};
use suds_core::helper::{HelperError, HelperPolicy, HelperRunner, HelperSpec};
use suds_core::organize::journal::Journal;
use suds_core::reclaim::execute::{
    ApplyHooks, Authority, Conditions, ExecutionBounds, apply_with_bounds,
};
use suds_core::reclaim::git::Git;
use suds_core::reclaim::liveness::Liveness;
use suds_core::reclaim::plan::{
    CheckoutScope, Decision, Kept, Observed, Policy, Subject, plan_reclaim,
};
use suds_core::registry;
use tempfile::TempDir;

const FUTURE_NOW: i64 = 2_000_000_000;

fn run_git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn init_cargo_repo(repo: &Path, gitignore: &str) {
    fs::create_dir_all(repo).unwrap();
    run_git(repo, &["init", "-b", "main"]);
    run_git(repo, &["config", "user.email", "test@example.com"]);
    run_git(repo, &["config", "user.name", "Test"]);
    fs::write(repo.join("Cargo.toml"), "[package]\n").unwrap();
    fs::write(repo.join(".gitignore"), gitignore).unwrap();
    run_git(repo, &["add", "."]);
    run_git(repo, &["commit", "-m", "initial"]);
}

fn populate_cargo_target(repo: &Path, file_count: usize) -> PathBuf {
    let target = repo.join("target");
    fs::create_dir_all(target.join("debug")).unwrap();
    fs::write(target.join("CACHEDIR.TAG"), "Signature: cargo").unwrap();
    for i in 0..file_count {
        fs::write(target.join("debug").join(format!("obj-{i}")), "bin").unwrap();
    }
    target
}

fn write_unique_orig_head(linked: &Path) -> PathBuf {
    let pushed_head = run_git(linked, &["rev-parse", "HEAD"]);
    fs::write(linked.join("unique.txt"), "precious\n").unwrap();
    run_git(linked, &["add", "unique.txt"]);
    run_git(linked, &["commit", "-m", "unique unpushed"]);
    let unique_oid = run_git(linked, &["rev-parse", "HEAD"]);
    run_git(linked, &["reset", "--hard", &pushed_head]);
    let orig_head = worktree_git_dir(linked).join("ORIG_HEAD");
    fs::write(&orig_head, format!("{unique_oid}\n")).unwrap();
    orig_head
}

fn setup_repo_with_linked_worktree(base: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let workspace = base.join("workspace");
    let repo = workspace.join("repo");
    let remote = base.join("remote.git");
    let linked = repo.join(".worktrees/feature");
    run_git(base, &["init", "--bare", "remote.git"]);
    init_cargo_repo(&repo, "target/\n.worktrees/\n");
    run_git(
        &repo,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    run_git(&repo, &["push", "origin", "main"]);
    run_git(
        &repo,
        &["worktree", "add", "-b", "feature", linked.to_str().unwrap()],
    );
    run_git(&linked, &["push", "origin", "feature"]);
    (workspace, repo, linked)
}

fn worktree_git_dir(linked: &Path) -> PathBuf {
    let git_dir = PathBuf::from(run_git(linked, &["rev-parse", "--git-dir"]));
    if git_dir.is_absolute() {
        git_dir
    } else {
        linked.join(git_dir)
    }
}

fn plan_linked_candidate(
    base: &Path,
    workspace: PathBuf,
    linked: &Path,
    git: &Git,
) -> suds_core::reclaim::plan::Candidate {
    let registry_path = base.join("registry.jsonl");
    registry::append_ended(&registry_path, linked, "bosun", Some("wf"), None, 0).unwrap();
    let scan = inspect(&[workspace], &Environment::default());
    let observed = Observed {
        liveness: &Ok(Liveness::default()),
        registry: &registry::reading(&registry_path),
    };
    let policy = Policy {
        idle_seconds: 60,
        checkouts: CheckoutScope::AllLinked,
        tolerate_unreadable_processes: false,
    };
    let plan = plan_reclaim(&scan, &observed, git, policy, FUTURE_NOW);
    plan.candidates
        .into_iter()
        .find(|c| c.subject == Subject::LinkedCheckout && c.path.as_path() == linked)
        .expect("linked checkout candidate")
}

#[test]
fn pseudoref_probe_timeout_keeps_worktree_with_unique_orig_head_commit() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let (workspace, _repo, linked) = setup_repo_with_linked_worktree(&base);

    // Create a unique unpushed commit and record it only in ORIG_HEAD before resetting HEAD back.
    let _orig_head = write_unique_orig_head(&linked);

    // A stub git that sleeps during `cat-file` simulates a stalled pseudoreference probe.
    let stub = base.join("slow-git");
    fs::write(
        &stub,
        "#!/bin/sh\nfor arg in \"$@\"\ndo\n  if [ \"$arg\" = \"cat-file\" ]\n  then\n    sleep 5\n  fi\ndone\nexec git \"$@\"\n",
    )
    .unwrap();
    fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();

    let runner = HelperRunner::new(HelperPolicy {
        per_call_timeout: Duration::from_millis(250),
        ..HelperPolicy::default()
    });
    let git = Git::with_program(stub).with_runner(runner);
    let candidate = plan_linked_candidate(&base, workspace, &linked, &git);
    assert_eq!(candidate.decision, Decision::Keep);
    assert_eq!(candidate.kept, Some(Kept::Unverified));
}

#[test]
fn tree_valued_auto_merge_pseudoref_keeps_worktree() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let (workspace, _repo, linked) = setup_repo_with_linked_worktree(&base);

    let tree_oid = run_git(&linked, &["rev-parse", "HEAD^{tree}"]);
    let git_dir = worktree_git_dir(&linked);
    fs::write(git_dir.join("AUTO_MERGE"), format!("{tree_oid}\n")).unwrap();

    let candidate = plan_linked_candidate(&base, workspace, &linked, &Git::default());
    assert_eq!(candidate.decision, Decision::Keep);
    assert_eq!(candidate.kept, Some(Kept::Unverified));
    assert!(
        candidate
            .evidence
            .iter()
            .any(|line| line.contains("AUTO_MERGE") && line.contains("tree")),
        "{:?}",
        candidate.evidence
    );
}

#[test]
fn helper_runner_enforces_launch_limit_output_cap_and_cancellation() {
    let cancel = Arc::new(AtomicBool::new(false));
    let runner = HelperRunner::new(HelperPolicy {
        max_concurrent: 1,
        per_call_timeout: Duration::from_secs(2),
        max_output_bytes: 32,
        min_launch_interval: Duration::from_millis(20),
        max_launches: Some(2),
        cancel: Arc::clone(&cancel),
        deadline: None,
        background_priority: false,
    });

    let ok_spec = HelperSpec {
        operation: "echo-small".to_owned(),
        program: PathBuf::from("/bin/echo"),
        args: vec![OsString::from("ok")],
        current_dir: None,
        env_set: Vec::new(),
        env_remove: Vec::new(),
        stdin: None,
    };
    let start = Instant::now();
    let first = runner.run(ok_spec.clone()).unwrap();
    assert!(first.status.success());

    // Exceeding output cap returns OutputExceeded.
    let loud = runner.run(HelperSpec {
        operation: "echo-loud".to_owned(),
        program: PathBuf::from("/bin/sh"),
        args: vec![
            OsString::from("-c"),
            OsString::from("printf '0123456789012345678901234567890123456789'"),
        ],
        current_dir: None,
        env_set: Vec::new(),
        env_remove: Vec::new(),
        stdin: None,
    });
    assert!(matches!(loud, Err(HelperError::OutputExceeded { .. })));
    assert!(start.elapsed() >= Duration::from_millis(15));

    // Third launch hits max_launches = 2.
    let third = runner.run(ok_spec.clone());
    assert!(matches!(third, Err(HelperError::BudgetExhausted { .. })));
    assert_eq!(runner.launches(), 2);

    // Pre-cancelled runner refuses new launches immediately.
    cancel.store(true, Ordering::Release);
    let after_cancel = runner.run(ok_spec);
    assert!(matches!(after_cancel, Err(HelperError::Cancelled { .. })));
}

#[test]
fn bounded_coordination_lock_honors_timeout_and_cancellation() {
    let tmp = TempDir::new().unwrap();
    let registry_path = tmp.path().join("registry.jsonl");
    let _held = registry::lock(&registry_path).unwrap();

    let cancel = AtomicBool::new(false);
    let timed_out = registry::lock_bounded(
        &registry_path,
        &cancel,
        Duration::from_millis(60),
        "test-timeout",
    );
    let err = timed_out.err().expect("lock should time out");
    assert!(err.contains("timed out") && err.contains("test-timeout"));

    cancel.store(true, Ordering::Release);
    let cancelled = registry::lock_bounded(
        &registry_path,
        &cancel,
        Duration::from_secs(5),
        "test-cancel",
    );
    let err = cancelled.err().expect("lock should observe cancellation");
    assert!(err.contains("cancelled") && err.contains("test-cancel"));
}

#[test]
fn bounded_removal_defers_oversized_candidate_and_stops_on_cancellation() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let (workspace, repo, _linked) = setup_repo_with_linked_worktree(&base);

    let target = populate_cargo_target(&repo, 25);

    let registry_path = base.join("registry.jsonl");
    let scan = inspect(&[workspace], &Environment::default());
    let observed = Observed {
        liveness: &Ok(Liveness::default()),
        registry: &registry::reading(&registry_path),
    };
    let policy = Policy {
        idle_seconds: 60,
        checkouts: CheckoutScope::DerivedOnly,
        tolerate_unreadable_processes: false,
    };
    let plan = plan_reclaim(&scan, &observed, &Git::default(), policy, FUTURE_NOW);
    assert!(
        plan.candidates
            .iter()
            .any(|c| c.path.as_path() == target.as_path() && c.decision == Decision::Remove)
    );

    let mut journal = Journal::open(&base.join("journal")).unwrap();
    let tracker = BudgetTracker::new(Budget::default());
    let bounds = ExecutionBounds {
        tracker: Arc::clone(&tracker),
        lock_timeout: Duration::from_secs(2),
        max_entries_per_candidate: Some(5),
    };
    let run_bounded = |journal: &mut Journal| {
        apply_with_bounds(
            journal,
            &plan,
            Conditions {
                authority: Authority::StandingApproval,
                observe: &mut || Ok(Liveness::default()),
                registry: &mut || registry::reading(&registry_path),
                registry_path: Some(&registry_path),
                git: &Git::default(),
                now: FUTURE_NOW,
            },
            ApplyHooks::default(),
            Some(&bounds),
        )
        .unwrap()
    };
    let report = run_bounded(&mut journal);
    assert!(target.exists(), "oversized candidate must stay intact");
    assert!(
        report.removals.iter().any(|r| {
            !r.removed
                && r.refusal
                    .as_deref()
                    .is_some_and(|msg| msg.contains("entry budget"))
        }),
        "{:?}",
        report.removals
    );

    // Cancellation stops admission before touching any candidate.
    tracker.cancel();
    let report2 = run_bounded(&mut journal);
    assert!(target.exists());
    assert!(report2.removals.iter().all(|r| !r.removed));
}

#[test]
fn timeout_kills_process_group_and_holds_guard_until_reaped() {
    let tmp = TempDir::new().unwrap();
    let marker = tmp.path().join("descendant-ran");
    let registry_path = tmp.path().join("registry.jsonl");
    let coord = registry::lock(&registry_path).unwrap();

    let runner = HelperRunner::new(HelperPolicy {
        max_concurrent: 1,
        per_call_timeout: Duration::from_millis(120),
        ..HelperPolicy::default()
    });
    let result = runner.run_holding(
        HelperSpec {
            operation: "descendant-timeout".to_owned(),
            program: PathBuf::from("/bin/sh"),
            args: vec![
                OsString::from("-c"),
                OsString::from("(sleep 0.4 && touch \"$1\") &\nwait"),
                OsString::from("sh"),
                marker.as_os_str().to_os_string(),
            ],
            current_dir: None,
            env_set: Vec::new(),
            env_remove: Vec::new(),
            stdin: None,
        },
        coord,
    );
    assert!(matches!(result, Err(HelperError::TimedOut { .. })));

    // Wait for the draining worker to kill the process group, reap the child,
    // and release the held coordination lock.
    let deadline = Instant::now() + Duration::from_secs(2);
    while runner.in_flight() > 0 || runner.draining() > 0 {
        assert!(Instant::now() < deadline, "helper slot stayed busy");
        std::thread::sleep(Duration::from_millis(20));
    }
    // Coordination lock becomes available again only after reaping finishes.
    let _reacquired = registry::lock_bounded(
        &registry_path,
        &AtomicBool::new(false),
        Duration::from_millis(200),
        "reacquire-after-reap",
    )
    .expect("helper must release the coordination lock after terminating");

    // Confirm the descendant process group died instead of writing the marker.
    std::thread::sleep(Duration::from_millis(450));
    assert!(
        !marker.exists(),
        "descendant process survived helper timeout"
    );
}

#[test]
fn unreadable_orig_head_with_unique_commit_keeps_worktree() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let (workspace, _repo, linked) = setup_repo_with_linked_worktree(&base);

    // Create a unique unpushed commit retained only in ORIG_HEAD.
    let orig_head = write_unique_orig_head(&linked);

    // Make ORIG_HEAD unreadable: Git's `cat-file --batch-check` exits 0 with
    // `ORIG_HEAD missing` and empty stderr, yet `git status` stays clean.
    fs::set_permissions(&orig_head, fs::Permissions::from_mode(0o000)).unwrap();

    let git = Git::default();
    let is_clean = git.is_clean(&linked);
    let states = git.probe_pseudorefs(&linked);
    let unpushed = git.unpushed(&linked);
    let candidate = plan_linked_candidate(&base, workspace, &linked, &git);

    // Restore permissions before assertions so TempDir cleanup always succeeds.
    let _ = fs::set_permissions(&orig_head, fs::Permissions::from_mode(0o644));

    assert!(is_clean.unwrap());
    let states = states.unwrap();
    assert!(
        matches!(
            states.first(),
            Some(suds_core::reclaim::git::PseudoRefState::Unknown(_))
        ),
        "unreadable ORIG_HEAD must classify as Unknown, found: {states:?}"
    );
    assert!(
        unpushed.is_err(),
        "unreadable ORIG_HEAD must prevent unpushed count from returning 0"
    );
    assert_eq!(candidate.decision, Decision::Keep);
    assert_eq!(candidate.kept, Some(Kept::Unverified));
}

#[test]
fn bounded_seeding_stops_cleanly_and_cleans_staged_directory() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let (_workspace, repo, linked) = setup_repo_with_linked_worktree(&base);

    let _target = populate_cargo_target(&repo, 40);

    let git = Git::default();
    let plan = suds_core::seed::plan_seed(&linked, Some(&repo), &git).unwrap();
    assert_eq!(plan.clones.len(), 1);

    let mut journal = Journal::open(&base.join("journal")).unwrap();
    let tight = Budget {
        max_entries: Some(10),
        ..Budget::for_seeding()
    };
    let report =
        suds_core::seed::apply_with_budget(&mut journal, &plan, Some("test"), &tight).unwrap();
    assert!(
        report.clones.iter().all(|c| !c.cloned),
        "tight seed budget must refuse oversized clone: {report:?}"
    );
    assert!(!linked.join("target").exists());
    let leftovers: Vec<_> = fs::read_dir(&linked)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(".suds-seed-"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "aborted seed must remove staged directory: {leftovers:?}"
    );
}

fn maintain_fixture(
    base: &Path,
    workspace: PathBuf,
    ceiling_bytes: u64,
) -> (suds_core::state::Directories, suds_core::state::Settings) {
    let dirs = suds_core::state::Directories {
        state: base.join("state"),
        config: base.join("config"),
    };
    let settings = suds_core::state::Settings {
        roots: vec![workspace],
        ceiling_bytes: Some(ceiling_bytes),
        idle_minutes: 1,
        standing_approval: true,
        tolerate_unreadable_processes: true,
        budgets: Default::default(),
    };
    (dirs, settings)
}

fn run_maintain_step(
    settings: &suds_core::state::Settings,
    context: &suds_core::maintain::Context<'_>,
    max_entries: u64,
    clock: &mut dyn FnMut() -> i64,
) -> suds_core::maintain::MaintenanceReport {
    let budget = Budget {
        max_entries: Some(max_entries),
        pace_every: 0,
        background_priority: false,
        ..Budget::for_maintenance()
    };
    suds_core::maintain::maintain_with_budget(settings, context, &budget, clock).unwrap()
}

#[test]
fn bounded_maintenance_makes_forward_progress_across_runs_within_unified_budget() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let workspace = base.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    let mut targets = Vec::new();
    for name in ["repo-a", "repo-b", "repo-c"] {
        let repo = workspace.join(name);
        init_cargo_repo(&repo, "target/\n");
        targets.push(populate_cargo_target(&repo, 20));
    }

    let (dirs, settings) = maintain_fixture(&base, workspace, 1024 * 1024 * 1024);
    let env = Environment::default();
    let programs = suds_core::stores::Programs::default();
    let git = Git::default();
    let context = suds_core::maintain::Context {
        directories: &dirs,
        environment: &env,
        programs: &programs,
        git: &git,
        now: FUTURE_NOW,
    };

    // Cap each run below the full 3-repo workspace while leaving room for one
    // repo scan plus its shared-budget verification and reclamation.
    // Successive runs advance via the cursor without exceeding max_entries.
    let per_run_cap = 165;
    let mut clock_now = FUTURE_NOW;
    let mut saw_partial_cursor = false;
    let mut total_reclaimed = 0usize;
    for _ in 0..8 {
        let mut clock = || {
            clock_now += 1;
            clock_now
        };
        let report = run_maintain_step(&settings, &context, per_run_cap, &mut clock);
        assert!(
            report.entries_visited <= per_run_cap,
            "run exceeded unified entry budget: {} > {per_run_cap}",
            report.entries_visited
        );
        if dirs.maintain_cursor().exists() {
            saw_partial_cursor = true;
            assert!(
                !report.pressure.complete,
                "partial sliced pass must not mark pressure complete"
            );
            assert!(
                report.stores.is_none(),
                "partial sliced pass must skip unbounded store trims"
            );
        }
        if let Some(reclaim) = &report.reclaim {
            total_reclaimed += reclaim.removals.iter().filter(|r| r.removed).count();
        }
        if targets.iter().all(|t| !t.exists()) && !dirs.maintain_cursor().exists() {
            break;
        }
    }
    assert!(
        saw_partial_cursor,
        "expected at least one partial sliced pass to persist maintain-cursor.json"
    );
    assert_eq!(
        total_reclaimed, 3,
        "successive bounded runs must reclaim all 3 eligible target directories"
    );
    assert!(targets.iter().all(|t| !t.exists()));
    assert!(
        !dirs.maintain_cursor().exists(),
        "cursor must clear once final slice completes"
    );
}

#[test]
fn nested_worktree_cursor_reaches_eligible_child_after_retained_sibling() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let workspace = base.join("workspace");
    let repo = workspace.join("repo");
    init_cargo_repo(&repo, "target/\n.claude/\n");

    let active_wt = repo.join(".claude/worktrees/a-active");
    let eligible_wt = repo.join(".claude/worktrees/b-eligible");
    fs::create_dir_all(active_wt.parent().unwrap()).unwrap();
    run_git(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "active",
            active_wt.to_str().unwrap(),
        ],
    );
    run_git(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "eligible",
            eligible_wt.to_str().unwrap(),
        ],
    );
    let active_target = populate_cargo_target(&active_wt, 25);
    let eligible_target = populate_cargo_target(&eligible_wt, 25);

    let (dirs, settings) = maintain_fixture(&base, workspace, 1024 * 1024 * 1024);
    // Hold the first nested worktree so it stays retained while the later
    // nested worktree has an ended claim and remains eligible for relief.
    registry::register(
        &dirs.registry(),
        &active_wt,
        "bosun",
        Some("wf-active"),
        None,
        0,
    )
    .unwrap();
    registry::append_ended(
        &dirs.registry(),
        &eligible_wt,
        "bosun",
        Some("wf-eligible"),
        None,
        0,
    )
    .unwrap();

    let env = Environment::default();
    let programs = suds_core::stores::Programs::default();
    let git = Git::default();
    let context = suds_core::maintain::Context {
        directories: &dirs,
        environment: &env,
        programs: &programs,
        git: &git,
        now: FUTURE_NOW,
    };

    // Cap each run so run 1 completes the repo root and `a-active` but stops
    // before finishing `b-eligible`, persisting `b-eligible` in pending state.
    let per_run_cap = 175;
    let mut clock_now = FUTURE_NOW;
    let mut saw_pending = false;
    let mut trace = Vec::new();
    for step in 0..6 {
        let mut clock = || {
            clock_now += 1;
            clock_now
        };
        let report = run_maintain_step(&settings, &context, per_run_cap, &mut clock);
        assert!(
            report.entries_visited <= per_run_cap,
            "nested worktree run exceeded cap: {} > {per_run_cap}",
            report.entries_visited
        );
        let cursor = suds_core::state::read_maintain_cursor(&dirs.maintain_cursor());
        if cursor
            .as_ref()
            .is_some_and(|c| c.pending_checkouts.contains(&eligible_wt))
        {
            saw_pending = true;
        }
        trace.push(format!(
            "step={step} outcome={:?} entries={} reclaim={:?} cursor={:?}",
            report.outcome, report.entries_visited, report.reclaim, cursor
        ));
        if !eligible_target.exists() && !dirs.maintain_cursor().exists() {
            break;
        }
    }
    assert!(
        saw_pending,
        "cursor must persist unfinished nested worktree `b-eligible`: {trace:#?}"
    );
    assert!(
        active_target.exists(),
        "held nested worktree target must stay intact: {trace:#?}"
    );
    assert!(
        !eligible_target.exists(),
        "subsequent bounded run must reach and reclaim `b-eligible/target`: {trace:#?}"
    );
    assert!(
        !dirs.maintain_cursor().exists(),
        "cursor must clear after finishing remaining nested worktrees: {trace:#?}"
    );
}

#[test]
fn exhausted_post_observation_preserves_lower_since_and_recovers() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let workspace = base.join("workspace");
    let repo = workspace.join("repo");
    init_cargo_repo(&repo, "target/\n");
    for i in 0..35 {
        fs::write(repo.join(format!("src-{i}.rs")), "fn f() {}\n").unwrap();
    }

    let (dirs, settings) = maintain_fixture(&base, workspace, 100 * 1024 * 1024);
    fs::create_dir_all(&dirs.state).unwrap();
    // Seed an existing Hard reading at t=1000 while current usage stays well below the ceiling.
    let seeded = suds_core::pressure::Pressure {
        schema: suds_core::pressure::PRESSURE_SCHEMA.to_owned(),
        observed_at: suds_core::timestamp::format_utc(1000),
        level: suds_core::pressure::Level::Hard,
        lower_since: None,
        usage_bytes: 100 * 1024 * 1024,
        ceiling_bytes: 100 * 1024 * 1024,
        enforced: false,
        filesystem_available_bytes: Some(1024 * 1024 * 1024),
        complete: true,
    };
    fs::write(dirs.pressure(), serde_json::to_vec(&seeded).unwrap()).unwrap();

    let env = Environment::default();
    let programs = suds_core::stores::Programs::default();
    let git = Git::default();

    // Measure exact single-pass entry count so 1 complete initial scan fits in
    // scan_entry_cap (60%), while 2 full scans exceed max_entries.
    let baseline = inspect(&settings.roots, &env);
    let one_pass = baseline.entries_visited;
    let cap = (one_pass * 175) / 100;
    assert!(cap * 3 / 5 >= one_pass && cap < one_pass * 2);

    let run_at = |ts: i64| {
        let context = suds_core::maintain::Context {
            directories: &dirs,
            environment: &env,
            programs: &programs,
            git: &git,
            now: ts,
        };
        let mut clock = || ts;
        run_maintain_step(&settings, &context, cap, &mut clock)
    };

    // Run 1 at t=1100: initial scan completes and starts lower_since, and
    // post-scan exhausts the remaining budget without erasing lower_since.
    let r1 = run_at(1100);
    assert_eq!(r1.outcome, suds_core::budget::ExecutionOutcome::Exhausted);
    assert_eq!(r1.pressure.level, suds_core::pressure::Level::Hard);
    assert_eq!(
        r1.pressure.lower_since.as_deref(),
        Some(suds_core::timestamp::format_utc(1100).as_str()),
        "exhausted post-scan must preserve lower_since from complete initial scan"
    );

    // Run 2 at t=1405 (past the 300s transition window): initial scan completes
    // and transitions Hard -> Normal even though post-scan exhausts again.
    let r2 = run_at(1405);
    assert_eq!(r2.outcome, suds_core::budget::ExecutionOutcome::Exhausted);
    assert_eq!(r2.pressure.level, suds_core::pressure::Level::Normal);
    assert_eq!(r2.pressure.lower_since, None);
    let persisted = suds_core::maintain::read_pressure(&dirs.pressure()).unwrap();
    assert_eq!(persisted.level, suds_core::pressure::Level::Normal);
}

#[test]
fn verification_debits_shared_budget_and_maintain_skips_external_store_adapters() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let workspace = base.join("workspace");
    let repo = workspace.join("repo");
    init_cargo_repo(&repo, "target/\n");
    let target = populate_cargo_target(&repo, 45);

    // Stub docker and ccache scripts that leave markers if automatic maintain calls them.
    let docker_marker = base.join("docker-called");
    let ccache_marker = base.join("ccache-called");
    let stub_docker = base.join("stub-docker");
    let stub_ccache = base.join("stub-ccache");
    fs::write(
        &stub_docker,
        format!("#!/bin/sh\ntouch \"{}\"\nexit 0\n", docker_marker.display()),
    )
    .unwrap();
    fs::write(
        &stub_ccache,
        format!("#!/bin/sh\ntouch \"{}\"\nexit 0\n", ccache_marker.display()),
    )
    .unwrap();
    fs::set_permissions(&stub_docker, fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(&stub_ccache, fs::Permissions::from_mode(0o755)).unwrap();

    // Create a ccache directory above a 0-byte budget to ensure an external Trim would plan.
    let ccache_dir = base.join("ccache");
    fs::create_dir_all(ccache_dir.join("0")).unwrap();
    fs::write(ccache_dir.join("0/CACHEDIR.TAG"), "Signature: ccache").unwrap();
    fs::write(ccache_dir.join("0/stats"), "0\n").unwrap();
    fs::write(ccache_dir.join("0/entry"), vec![b'a'; 8192]).unwrap();

    let mut env = Environment::default();
    env.variables
        .insert("CCACHE_DIR".to_owned(), ccache_dir.into());
    let programs = suds_core::stores::Programs {
        docker: stub_docker,
        ccache: stub_ccache,
        ..Default::default()
    };

    let (dirs, mut settings) = maintain_fixture(&base, workspace, 1024 * 1024 * 1024);
    settings.budgets.insert("ccache.cache".to_owned(), 0);

    // Give enough entries for the initial scan to complete under the 60% scan
    // cap while leaving fewer entries than verification of the 45-file target
    // requires (~96), so verification itself debits and exhausts the shared tracker.
    let baseline = inspect(&settings.roots, &env);
    let tight_cap = (baseline.entries_visited * 100 / 59).max(baseline.entries_visited + 10);
    assert!(
        tight_cap * 3 / 5 >= baseline.entries_visited && tight_cap < baseline.entries_visited + 90,
        "tight cap must admit initial scan and exhaust during verification: cap={tight_cap} scan={}",
        baseline.entries_visited
    );
    let git = Git::default();
    let context = suds_core::maintain::Context {
        directories: &dirs,
        environment: &env,
        programs: &programs,
        git: &git,
        now: FUTURE_NOW,
    };
    let mut now = FUTURE_NOW;
    let report = run_maintain_step(&settings, &context, tight_cap, &mut || {
        now += 1;
        now
    });
    assert_eq!(
        report.entries_visited, tight_cap,
        "verification must debit and exhaust the shared budget"
    );
    assert!(
        report
            .reclaim
            .as_ref()
            .is_some_and(|r| r.removals.iter().any(|x| !x.removed)),
        "expected budget refusal during verification: {:?}",
        report.reclaim
    );
    assert!(
        target.exists(),
        "candidate whose verification exhausts shared budget must stay intact"
    );

    // Clear cursor so the ample pass starts a fresh full pass and confirms
    // automatic maintenance never invokes docker or external store trims.
    let _ = suds_core::state::clear_maintain_cursor(&dirs.maintain_cursor());
    let full = run_maintain_step(&settings, &context, 10_000, &mut || {
        now += 1;
        now
    });
    assert!(!target.exists());
    assert!(full.stores.as_ref().is_some_and(|s| s.trims.is_empty()));
    assert!(!docker_marker.exists(), "maintain must not run docker");
    assert!(
        !ccache_marker.exists(),
        "maintain must not run external store trim subprocesses"
    );
}

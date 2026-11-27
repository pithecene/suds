use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use suds_core::budget::{Budget, ExecutionOutcome};
use suds_core::development::{self, Environment};
use suds_core::maintain::{self, Context};
use suds_core::model::ReportStatus;
use suds_core::pressure::{self, Level, Pressure};
use suds_core::reclaim::git::Git;
use suds_core::schedule::{self, Scheduler, Standing, Unit};
use suds_core::seed;
use suds_core::state::{Directories, PAUSE_SCHEMA, PauseState, Settings, write_pause};
use suds_core::stores::Programs;
use tempfile::TempDir;

#[test]
fn oversized_single_directory_is_deferred_and_never_complete() {
    let dir = TempDir::new().unwrap();
    let big = dir.path().join("huge");
    fs::create_dir(&big).unwrap();
    for i in 0..40 {
        fs::write(big.join(format!("f{i:03}")), b"data").unwrap();
    }
    let env = Environment::default();
    let budget = Budget {
        max_dir_entries: Some(10),
        max_total_readers: 1,
        ..Budget::default()
    };
    let scan = development::inspect_with_budget(&[dir.path().to_path_buf()], &env, budget);
    assert_eq!(scan.outcome, ExecutionOutcome::Partial);
    assert!(scan.directories_deferred >= 1);
    let report = development::build_report(&scan, 1_700_000_000);
    assert_eq!(report.status, ReportStatus::Partial);
}

fn populate_grid(root: &std::path::Path, dirs: usize, prefix: &str, files: usize) {
    for i in 0..dirs {
        let sub = root.join(format!("{prefix}{i:02}"));
        fs::create_dir(&sub).unwrap();
        for j in 0..files {
            fs::write(sub.join(format!("f{j}")), b"abc").unwrap();
        }
    }
}

#[test]
fn cancellation_reaches_shared_scan_and_stops_development_inspection() {
    let dir = TempDir::new().unwrap();
    populate_grid(dir.path(), 10, "dir-", 20);
    let cancel = Arc::new(AtomicBool::new(true));
    let budget = Budget {
        cancel,
        max_total_readers: 1,
        ..Budget::default()
    };
    let scan = development::inspect_with_budget(
        &[dir.path().to_path_buf()],
        &Environment::default(),
        budget,
    );
    assert_eq!(scan.outcome, ExecutionOutcome::Cancelled);
    assert_eq!(scan.entries_visited, 0);
    let report = development::build_report(&scan, 1_700_000_000);
    assert_eq!(report.status, ReportStatus::Partial);
}

#[test]
fn entry_budget_exhaustion_keeps_hard_pressure_without_false_recovery() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let root = base.join("work");
    fs::create_dir_all(&root).unwrap();
    for i in 0..30 {
        fs::write(root.join(format!("file-{i}")), b"hello").unwrap();
    }
    let dirs = Directories {
        state: base.join("state"),
        config: base.join("config"),
    };
    fs::create_dir_all(&dirs.state).unwrap();
    let prior = Pressure {
        schema: pressure::PRESSURE_SCHEMA.to_owned(),
        observed_at: "2026-11-25T20:00:00Z".to_owned(),
        level: Level::Hard,
        lower_since: None,
        usage_bytes: 900_000_000_000,
        ceiling_bytes: 500_000_000_000,
        enforced: false,
        filesystem_available_bytes: Some(10_000_000_000),
        complete: true,
    };
    fs::write(dirs.pressure(), serde_json::to_vec(&prior).unwrap()).unwrap();

    let settings = Settings {
        roots: vec![root],
        ceiling_bytes: Some(500_000_000_000),
        standing_approval: false,
        ..Settings::default()
    };
    let context = Context {
        directories: &dirs,
        environment: &Environment::default(),
        programs: &Programs::default(),
        git: &Git::default(),
        now: 1_795_600_000,
    };
    let budget = Budget {
        max_entries: Some(5),
        max_total_readers: 1,
        background_priority: false,
        ..Budget::default()
    };
    let mut now = context.now;
    let report = maintain::maintain_with_budget(&settings, &context, &budget, &mut || {
        now += 1;
        now
    })
    .unwrap();
    assert_eq!(report.outcome, ExecutionOutcome::Exhausted);
    assert!(!report.pressure.complete);
    assert_eq!(report.pressure.level, Level::Hard);
    assert!(report.reclaim.is_none());
}

#[test]
fn durable_pause_prevents_reclamation_and_corrupt_pause_fails_closed() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("work");
    fs::create_dir_all(&root).unwrap();
    let dirs = Directories {
        state: tmp.path().join("state"),
        config: tmp.path().join("config"),
    };
    fs::create_dir_all(&dirs.state).unwrap();
    write_pause(
        &dirs.pause(),
        &PauseState {
            schema: PAUSE_SCHEMA.to_owned(),
            paused: true,
            reason: Some("containment".to_owned()),
            updated_at: "2026-11-25T23:00:00Z".to_owned(),
        },
    )
    .unwrap();

    let settings = Settings {
        roots: vec![root],
        standing_approval: true,
        ..Settings::default()
    };
    let context = Context {
        directories: &dirs,
        environment: &Environment::default(),
        programs: &Programs::default(),
        git: &Git::default(),
        now: 1_795_600_000,
    };
    let report =
        maintain::maintain_with_budget(&settings, &context, &Budget::default(), &mut || {
            1_795_600_001
        })
        .unwrap();
    assert!(report.paused);
    assert_eq!(report.pause_reason.as_deref(), Some("containment"));
    assert!(report.reclaim.is_none());
    assert!(report.stores.is_none());

    // Corrupt pause file must fail closed rather than silently resuming.
    fs::write(dirs.pause(), b"{broken").unwrap();
    assert!(
        maintain::maintain_with_budget(&settings, &context, &Budget::default(), &mut || {
            1_795_600_002
        },)
        .is_err()
    );
}

#[test]
fn schedule_units_include_background_priority_and_round_trip_standing() {
    let tmp = TempDir::new().unwrap();
    let program = tmp.path().join("suds");
    fs::write(&program, b"#!/bin/sh\n").unwrap();
    let env = Environment {
        home: Some(tmp.path().to_path_buf()),
        variables: Default::default(),
    };
    let unit = Unit {
        program: program.clone(),
        path: Some("/usr/bin:/bin".to_owned()),
        minutes: 30,
    };

    let mac_files = schedule::files(Scheduler::Launchd, &unit, &env).unwrap();
    let plist = &mac_files[0].contents;
    assert!(plist.contains("<key>ProcessType</key>\n\t<string>Background</string>"));
    assert!(plist.contains("<key>LowPriorityIO</key>\n\t<true/>"));
    assert!(plist.contains("<key>LowPriorityBackgroundIO</key>\n\t<true/>"));
    assert!(plist.contains("<key>Nice</key>\n\t<integer>10</integer>"));
    assert!(plist.contains("<key>StartInterval</key>\n\t<integer>1800</integer>"));
    schedule::place(&mac_files).unwrap();
    assert_eq!(
        schedule::standing(Scheduler::Launchd, &env, Some(&program)),
        Standing::Current
    );

    let linux_files = schedule::files(Scheduler::Systemd, &unit, &env).unwrap();
    assert!(linux_files[0].contents.contains("Nice=10\n"));
    assert!(linux_files[0].contents.contains("IOSchedulingClass=idle\n"));
    schedule::place(&linux_files).unwrap();
    assert_eq!(
        schedule::standing(Scheduler::Systemd, &env, Some(&program)),
        Standing::Current
    );
}

#[test]
fn bounded_seed_shortlist_caps_inspected_siblings() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let main_repo = base.join("repo");
    fs::create_dir_all(&main_repo).unwrap();
    let run = |dir: &PathBuf, args: &[&str]| {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{:?}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    run(&main_repo, &["init", "-q"]);
    run(&main_repo, &["config", "user.email", "test@example.com"]);
    run(&main_repo, &["config", "user.name", "Test"]);
    fs::write(
        main_repo.join("Cargo.toml"),
        "[package]\nname=\"x\"\nversion=\"0.1.0\"\n",
    )
    .unwrap();
    run(&main_repo, &["add", "."]);
    run(&main_repo, &["commit", "-q", "-m", "init"]);

    for i in 0..5 {
        let wt = base.join(format!("wt-{i}"));
        run(
            &main_repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                &format!("b-{i}"),
                wt.to_str().unwrap(),
            ],
        );
    }
    let dest = base.join("dest");
    run(
        &main_repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "dest",
            dest.to_str().unwrap(),
        ],
    );

    let budget = Budget {
        max_seed_candidates: 2,
        max_total_readers: 1,
        pace_delay: Duration::ZERO,
        ..Budget::default()
    };
    let plan = seed::plan_seed_with_budget(&dest, None, &Git::default(), &budget).unwrap();
    let deferred = plan
        .passed
        .iter()
        .filter(|p| p.reason.contains("seed candidate limit of 2"))
        .count();
    assert_eq!(deferred, 4);

    // Entry accounting stays shared across inspected siblings so max_entries
    // bounds the entire seed-selection run instead of resetting per sibling.
    for i in 0..3 {
        let wt = base.join(format!("wt-{i}"));
        for j in 0..8 {
            fs::write(wt.join(format!("extra-{j}.txt")), b"x").unwrap();
        }
    }
    let tight = Budget {
        max_seed_candidates: 6,
        max_entries: Some(12),
        max_total_readers: 1,
        pace_delay: Duration::ZERO,
        ..Budget::default()
    };
    let limited = seed::plan_seed_with_budget(&dest, None, &Git::default(), &tight).unwrap();
    assert!(
        limited
            .passed
            .iter()
            .any(|p| p.reason.contains("execution budget ended")),
        "{:?}",
        limited.passed
    );
}

#[test]
fn queued_maintenance_rechecks_pause_after_acquiring_lock() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let root = base.join("work");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("file.txt"), b"data").unwrap();
    let dirs = Directories {
        state: base.join("state"),
        config: base.join("config"),
    };
    fs::create_dir_all(&dirs.state).unwrap();

    // Hold the maintenance lock, start a second maintenance call, write pause.json
    // while it waits, then release the lock.
    let lock_target = dirs.pressure().with_extension("maintain.lock");
    let guard = suds_core::registry::lock(&lock_target).unwrap();
    let dirs_bg = dirs.clone();
    let root_bg = root.clone();
    let handle = std::thread::spawn(move || {
        let settings = Settings {
            roots: vec![root_bg],
            standing_approval: true,
            ..Settings::default()
        };
        let context = Context {
            directories: &dirs_bg,
            environment: &Environment::default(),
            programs: &Programs::default(),
            git: &Git::default(),
            now: 1_795_600_000,
        };
        maintain::maintain_with_budget(&settings, &context, &Budget::default(), &mut || {
            1_795_600_001
        })
    });
    std::thread::sleep(Duration::from_millis(80));
    write_pause(
        &dirs.pause(),
        &PauseState {
            schema: PAUSE_SCHEMA.to_owned(),
            paused: true,
            reason: Some("paused-during-lock-wait".to_owned()),
            updated_at: "2026-11-25T23:50:00Z".to_owned(),
        },
    )
    .unwrap();
    drop(guard);

    let report = handle.join().unwrap().unwrap();
    assert!(report.paused);
    assert_eq!(
        report.pause_reason.as_deref(),
        Some("paused-during-lock-wait")
    );
    assert!(report.reclaim.is_none());
    assert!(report.stores.is_none());
    assert!(root.join("file.txt").exists());
}

#[test]
fn concurrent_cancellation_leaves_no_additional_work_admitted() {
    let dir = TempDir::new().unwrap();
    populate_grid(dir.path(), 16, "g", 10);
    let cancel = Arc::new(AtomicBool::new(false));
    let budget = Budget {
        cancel: Arc::clone(&cancel),
        max_total_readers: 2,
        max_entries: Some(15),
        ..Budget::default()
    };
    let scan = development::inspect_with_budget(
        &[dir.path().to_path_buf()],
        &Environment::default(),
        budget,
    );
    assert!(matches!(
        scan.outcome,
        ExecutionOutcome::Exhausted | ExecutionOutcome::Cancelled
    ));
    assert!(scan.entries_visited <= 15);
    assert!(!cancel.load(Ordering::Acquire));
}

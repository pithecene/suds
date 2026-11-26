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

fn setup_repo_with_linked_worktree(base: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let workspace = base.join("workspace");
    let repo = workspace.join("repo");
    let remote = base.join("remote.git");
    let linked = repo.join(".worktrees/feature");
    fs::create_dir_all(&repo).unwrap();
    run_git(base, &["init", "--bare", "remote.git"]);
    run_git(&repo, &["init", "-b", "main"]);
    run_git(&repo, &["config", "user.email", "test@example.com"]);
    run_git(&repo, &["config", "user.name", "Test"]);
    fs::write(repo.join("Cargo.toml"), "[package]\n").unwrap();
    fs::write(repo.join(".gitignore"), "target/\n.worktrees/\n").unwrap();
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-m", "initial"]);
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
    let pushed_head = run_git(&linked, &["rev-parse", "HEAD"]);
    fs::write(linked.join("unique.txt"), "precious\n").unwrap();
    run_git(&linked, &["add", "unique.txt"]);
    run_git(&linked, &["commit", "-m", "unique unpushed"]);
    let unique_oid = run_git(&linked, &["rev-parse", "HEAD"]);
    run_git(&linked, &["reset", "--hard", &pushed_head]);
    let git_dir = worktree_git_dir(&linked);
    fs::write(git_dir.join("ORIG_HEAD"), format!("{unique_oid}\n")).unwrap();

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

    let target = repo.join("target");
    fs::create_dir_all(target.join("debug")).unwrap();
    fs::write(target.join("CACHEDIR.TAG"), "Signature: cargo").unwrap();
    for i in 0..25 {
        fs::write(target.join("debug").join(format!("obj-{i}")), "bin").unwrap();
    }

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

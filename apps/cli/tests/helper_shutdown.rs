//! Real parent-process CLI shutdown test verifying that terminating `suds`
//! while a mutating `git worktree remove` helper is active terminates the
//! separate helper process group, prevents post-exit mutations, and releases
//! the coordination lock only after reaping.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use suds_core::development::{Environment, inspect};
use suds_core::organize::journal::{Event, Journal};
use suds_core::reclaim::git::Git;
use suds_core::reclaim::liveness::Liveness;
use suds_core::reclaim::plan::{CheckoutScope, Decision, Observed, Policy, Subject, plan_reclaim};
use suds_core::registry;
use suds_core::state::Directories;
use tempfile::TempDir;

fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

fn run_git(dir: &Path, args: &[&str]) {
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
}

#[test]
fn cli_sigterm_terminates_separate_helper_group_and_preserves_lock_exclusion() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let home = base.join("home");
    let workspace = base.join("workspace");
    let repo = workspace.join("repo");
    let remote = base.join("remote.git");
    let linked = repo.join(".worktrees/feature");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&repo).unwrap();

    run_git(&base, &["init", "--bare", "remote.git"]);
    run_git(&repo, &["init", "-b", "main"]);
    run_git(&repo, &["config", "user.email", "test@example.com"]);
    run_git(&repo, &["config", "user.name", "Test"]);
    fs::write(repo.join("Cargo.toml"), "[package]\n").unwrap();
    fs::write(repo.join(".gitignore"), ".worktrees/\n").unwrap();
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

    let env = Environment {
        home: Some(home.clone()),
        variables: Default::default(),
    };
    let dirs = Directories::of(&env).unwrap();
    let registry_path = dirs.registry();
    registry::append_ended(&registry_path, &linked, "bosun", Some("wf"), None, 0).unwrap();

    // Advance past the fixture creation second so `ctime < plan.created_at`
    // when `suds reclaim --apply` runs with the live wall clock.
    std::thread::sleep(Duration::from_millis(1200));

    // Plan and save a linked-checkout removal into the journal.
    let scan = inspect(&[workspace], &Environment::default());
    let observed = Observed {
        liveness: &Ok(Liveness::default()),
        registry: &registry::reading(&registry_path),
    };
    let policy = Policy {
        idle_seconds: 0,
        checkouts: CheckoutScope::AllLinked,
        tolerate_unreadable_processes: true,
    };
    let plan = plan_reclaim(&scan, &observed, &Git::default(), policy, now_seconds());
    assert!(
        plan.candidates
            .iter()
            .any(|c| c.subject == Subject::LinkedCheckout
                && c.path.as_path() == linked.as_path()
                && c.decision == Decision::Remove),
        "expected removable linked checkout in plan: {:?}",
        plan.candidates
    );
    let journal_dir = dirs.journal();
    let mut journal = Journal::open(&journal_dir).unwrap();
    journal
        .append(Event::ReclaimPlanned { plan: plan.clone() })
        .unwrap();
    drop(journal);

    // Install a stub `git` that delegates everything except `worktree remove`,
    // which spawns a delayed descendant writer and blocks until killed.
    let stub_bin = base.join("stub-bin");
    fs::create_dir_all(&stub_bin).unwrap();
    let started_file = base.join("helper-started.pid");
    let mutation_marker = base.join("post-exit-mutation");
    let real_git = Command::new("which")
        .arg("git")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_else(|| "/usr/bin/git".to_owned());

    let stub_git = stub_bin.join("git");
    fs::write(
        &stub_git,
        format!(
            "#!/bin/sh\n\
             prev=\"\"\n\
             for arg in \"$@\"\n\
             do\n\
               if [ \"$prev\" = \"worktree\" ] && [ \"$arg\" = \"remove\" ]\n\
               then\n\
                 (sleep 0.7 && touch \"{marker}\") &\n\
                 echo \"$$\" > \"{started}\"\n\
                 wait\n\
                 exit 0\n\
               fi\n\
               prev=\"$arg\"\n\
             done\n\
             exec \"{real_git}\" \"$@\"\n",
            marker = mutation_marker.display(),
            started = started_file.display(),
            real_git = real_git,
        ),
    )
    .unwrap();
    fs::set_permissions(&stub_git, fs::Permissions::from_mode(0o755)).unwrap();

    let path_var = format!(
        "{}:{}",
        stub_bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let suds_bin = PathBuf::from(env!("CARGO_BIN_EXE_suds"));
    let mut child = Command::new(&suds_bin)
        .args(["reclaim", "--apply", &plan.id, "--journal"])
        .arg(&journal_dir)
        .env("HOME", &home)
        .env("PATH", &path_var)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    // Wait for the mutating `git worktree remove` helper to start under lock.
    let start_deadline = Instant::now() + Duration::from_secs(5);
    while !started_file.exists() {
        if let Some(status) = child.try_wait().unwrap() {
            let output = child.wait_with_output().unwrap();
            panic!(
                "suds exited early ({status}) before reaching mutating helper\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
        }
        assert!(
            Instant::now() < start_deadline,
            "suds never reached mutating helper"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let helper_pid: i32 = fs::read_to_string(&started_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();

    // While the helper is active, the coordination lock MUST be held.
    let contended = registry::lock_bounded(
        &registry_path,
        &AtomicBool::new(false),
        Duration::from_millis(80),
        "probe-while-active",
    );
    assert!(
        contended.is_err(),
        "coordination lock must remain held while mutating helper is active"
    );

    // Send SIGTERM to the real `suds` CLI parent process.
    let cli_pid = i32::try_from(child.id()).unwrap();
    let _ = Command::new("kill")
        .args(["-TERM", &cli_pid.to_string()])
        .status();

    let exit_deadline = Instant::now() + Duration::from_secs(3);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < exit_deadline,
            "suds CLI did not shut down cleanly after SIGTERM"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(
        !status.success(),
        "signalled suds CLI must exit non-zero, got {status}"
    );

    // The separate helper process group must already be dead on CLI exit.
    let helper_alive = Command::new("kill")
        .args(["-0", &helper_pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    assert!(
        !helper_alive,
        "mutating helper pid {helper_pid} survived CLI SIGTERM shutdown"
    );

    // The coordination lock is now cleanly released.
    let _reacquired = registry::lock_bounded(
        &registry_path,
        &AtomicBool::new(false),
        Duration::from_millis(200),
        "reacquire-after-cli-exit",
    )
    .expect("coordination lock must be available after CLI shutdown and reaping");

    // Wait past the descendant's delay and prove zero post-exit mutation.
    std::thread::sleep(Duration::from_millis(900));
    assert!(
        !mutation_marker.exists(),
        "descendant of mutating helper performed work after CLI shutdown"
    );
    assert!(linked.exists(), "linked worktree must remain intact");
}

//! Tests of reclaim plans against temporary repositories that Git builds.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use suds_core::development::{Environment, inspect};
use suds_core::organize::journal::{Event, Journal, JournalState, RunKind, RunStatus, read};
use suds_core::reclaim::execute::{Authority, Conditions, ReclaimReport, apply};
use suds_core::reclaim::git::Git;
use suds_core::reclaim::liveness::Liveness;
use suds_core::reclaim::plan::{
    Candidate, Decision, Kept, Policy, ReclaimPlan, Subject, plan_reclaim,
};
use tempfile::TempDir;

/// The plan's idle time in these tests.
const IDLE: u64 = 3600;

fn git(directory: &Path, arguments: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args([
            "-c",
            "user.name=Suds",
            "-c",
            "user.email=suds@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(arguments)
        .output()
        .expect("run git");
    assert!(
        status.status.success(),
        "git {arguments:?}: {}",
        String::from_utf8_lossy(&status.stderr)
    );
}

fn write(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn build_output(checkout: &Path) {
    write(&checkout.join("target/CACHEDIR.TAG"), "Signature: cargo");
    write(&checkout.join("target/debug/app"), "binary");
}

/// A main checkout with a remote that holds its branch, and one linked
/// checkout whose branch the remote holds too. Each checkout holds Cargo's
/// build output, which Git ignores.
struct Fixture {
    _dir: TempDir,
    workspace: PathBuf,
    repo: PathBuf,
    linked: PathBuf,
    journal: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let workspace = root.join("workspace");
        let repo = workspace.join("repo");
        let remote = root.join("remote.git");
        let linked = repo.join(".worktrees/feature");
        fs::create_dir_all(&repo).unwrap();
        git(&root, &["init", "--bare", "remote.git"]);
        git(&repo, &["init"]);
        write(&repo.join("Cargo.toml"), "[package]");
        write(&repo.join(".gitignore"), "target/\n.worktrees/\n");
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "first"]);
        git(
            &repo,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(&repo, &["push", "origin", "main"]);
        git(
            &repo,
            &["worktree", "add", "-b", "feature", linked.to_str().unwrap()],
        );
        write(&linked.join("src/lib.rs"), "pub fn f() {}");
        git(&linked, &["add", "."]);
        git(&linked, &["commit", "-m", "feature"]);
        git(&linked, &["push", "origin", "feature"]);
        build_output(&repo);
        build_output(&linked);
        Fixture {
            journal: root.join("journal"),
            _dir: dir,
            workspace,
            repo,
            linked,
        }
    }

    /// Plans as if `IDLE` and one more hour had passed since the fixture's
    /// files changed, so no file counts as recent.
    fn plan(&self, liveness: &Result<Liveness, String>, checkouts: bool) -> ReclaimPlan {
        self.plan_at(liveness, checkouts, now() + 2 * IDLE_SECONDS)
    }

    fn plan_at(
        &self,
        liveness: &Result<Liveness, String>,
        checkouts: bool,
        at: i64,
    ) -> ReclaimPlan {
        let scan = inspect(
            std::slice::from_ref(&self.workspace),
            &Environment::default(),
        );
        let policy = Policy {
            idle_seconds: IDLE,
            checkouts,
            tolerate_unreadable_processes: false,
        };
        plan_reclaim(&scan, liveness, &Git::default(), policy, at)
    }

    /// Applies `plan` at the time for which `plan` planned, with the
    /// processes that `observe` reports before each removal.
    fn apply_with(
        &self,
        plan: &ReclaimPlan,
        observe: &mut dyn FnMut() -> Result<Liveness, String>,
    ) -> ReclaimReport {
        self.apply_at(plan, observe, now() + 2 * IDLE_SECONDS)
    }

    fn apply_at(
        &self,
        plan: &ReclaimPlan,
        observe: &mut dyn FnMut() -> Result<Liveness, String>,
        at: i64,
    ) -> ReclaimReport {
        let mut journal = open_journal(&self.journal);
        let conditions = Conditions {
            authority: Authority::Person,
            observe,
            git: &Git::default(),
            now: at,
        };
        apply(&mut journal, plan, conditions).unwrap()
    }

    fn apply(&self, plan: &ReclaimPlan) -> ReclaimReport {
        self.apply_with(plan, &mut nobody)
    }
}

const IDLE_SECONDS: i64 = 3600;

/// Opens the journal, and waits when it is busy. A test in another thread
/// can start Git between two runs of one test, and the new process holds a
/// copy of this journal's lock until it runs its program.
fn open_journal(directory: &Path) -> Journal {
    for _ in 0..200 {
        match Journal::open(directory) {
            Err(suds_core::organize::journal::JournalError::Busy) => {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            opened => return opened.unwrap(),
        }
    }
    panic!("the journal at {} stayed busy", directory.display());
}

fn now() -> i64 {
    let since = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    i64::try_from(since.as_secs()).unwrap()
}

fn nobody() -> Result<Liveness, String> {
    Ok(Liveness::of(Vec::new()))
}

fn find<'a>(plan: &'a ReclaimPlan, path: &Path) -> &'a Candidate {
    plan.candidates
        .iter()
        .find(|candidate| candidate.path.as_path() == path)
        .unwrap_or_else(|| panic!("no candidate {}", path.display()))
}

#[test]
fn removes_idle_build_state_and_leaves_every_source_file() {
    let fixture = Fixture::new();
    let plan = fixture.plan(&nobody(), false);
    assert_eq!(plan.candidates.len(), 2);
    assert!(
        plan.candidates
            .iter()
            .all(|candidate| candidate.decision == Decision::Remove)
    );
    assert!(matches!(
        plan.candidates[0].subject,
        Subject::BuildState { .. }
    ));

    let report = fixture.apply(&plan);
    assert_eq!(report.status, RunStatus::Complete);
    assert_eq!(report.freed_bytes, plan.removable_bytes());
    assert!(!fixture.repo.join("target").exists());
    assert!(!fixture.linked.join("target").exists());
    assert!(fixture.repo.join("Cargo.toml").exists());
    assert!(fixture.linked.join("src/lib.rs").exists());
    assert!(Git::default().is_clean(&fixture.linked).unwrap());

    let records = read(&fixture.journal).unwrap();
    let removed = records
        .iter()
        .filter(|record| matches!(record.event, Event::Removed { .. }))
        .count();
    assert_eq!(removed, 2);
    assert!(matches!(
        records[1].event,
        Event::ReclaimStarted {
            authority: Authority::Person,
            ..
        }
    ));
}

#[test]
fn keeps_build_state_while_a_process_works_in_its_checkout() {
    let fixture = Fixture::new();
    let busy = Ok(Liveness::of(vec![fixture.linked.join("src")]));
    let plan = fixture.plan(&busy, false);
    let kept = find(&plan, &fixture.linked.join("target"));
    assert_eq!(kept.kept, Some(Kept::InUse));
    // A process in the linked checkout lies beneath the main checkout too.
    assert_eq!(
        find(&plan, &fixture.repo.join("target")).kept,
        Some(Kept::InUse)
    );
}

#[test]
fn keeps_build_state_that_changed_within_the_idle_time() {
    let fixture = Fixture::new();
    let plan = fixture.plan_at(&nobody(), false, now());
    assert!(
        plan.candidates
            .iter()
            .all(|candidate| candidate.kept == Some(Kept::RecentlyModified))
    );
    assert_eq!(plan.removable_bytes(), 0);
}

#[test]
fn keeps_everything_when_the_processes_are_unknown() {
    let fixture = Fixture::new();
    let plan = fixture.plan(&Err("no process table".to_owned()), true);
    assert!(
        plan.candidates
            .iter()
            .all(|candidate| candidate.kept == Some(Kept::Unverified))
    );
}

#[test]
fn never_names_a_main_checkout_for_removal() {
    let fixture = Fixture::new();
    let plan = fixture.plan(&nobody(), true);
    let checkouts: Vec<&Path> = plan
        .candidates
        .iter()
        .filter(|candidate| candidate.subject == Subject::LinkedCheckout)
        .map(|candidate| candidate.path.as_path())
        .collect();
    assert_eq!(checkouts, [fixture.linked.as_path()]);
}

#[test]
fn keeps_a_linked_checkout_with_work_that_no_remote_holds() {
    let fixture = Fixture::new();
    write(&fixture.linked.join("notes.txt"), "unsaved");
    let plan = fixture.plan(&nobody(), true);
    assert_eq!(
        find(&plan, &fixture.linked).kept,
        Some(Kept::UncommittedChanges)
    );

    git(&fixture.linked, &["add", "."]);
    git(&fixture.linked, &["commit", "-m", "local only"]);
    let plan = fixture.plan(&nobody(), true);
    assert_eq!(
        find(&plan, &fixture.linked).kept,
        Some(Kept::UnpushedCommits)
    );
}

#[test]
fn has_git_remove_a_clean_linked_checkout_that_a_remote_holds() {
    let fixture = Fixture::new();
    let plan = fixture.plan(&nobody(), true);
    assert_eq!(find(&plan, &fixture.linked).decision, Decision::Remove);

    let report = fixture.apply(&plan);
    assert_eq!(report.status, RunStatus::Complete);
    assert!(!fixture.linked.exists());
    assert!(fixture.repo.join("Cargo.toml").exists());
    // The branch and its commits stay in the repository and on the remote.
    git(&fixture.repo, &["rev-parse", "--verify", "feature"]);
    git(&fixture.repo, &["rev-parse", "--verify", "origin/feature"]);
}

#[test]
fn refuses_at_apply_what_changed_after_planning() {
    let fixture = Fixture::new();
    let plan = fixture.plan(&nobody(), true);
    // A process starts in the linked checkout, which lies beneath the main
    // one, and a file appears there, after the plan and before the run.
    let busy: Result<Liveness, String> = Ok(Liveness::of(vec![fixture.linked.clone()]));
    let report = fixture.apply_with(&plan, &mut || busy.clone());
    assert_eq!(report.status, RunStatus::Refused);
    assert!(fixture.repo.join("target").exists());
    assert!(fixture.linked.join("target").exists());
    assert!(report.removals.iter().all(|removal| {
        removal
            .refusal
            .as_ref()
            .unwrap()
            .contains("a process works")
    }));

    write(&fixture.linked.join("late.txt"), "late");
    let plan_before = plan;
    let report = fixture.apply(&plan_before);
    assert_eq!(report.status, RunStatus::Partial);
    assert!(fixture.linked.join("late.txt").exists());
    let kept = report
        .removals
        .iter()
        .find(|removal| removal.path.as_path() == fixture.linked)
        .unwrap();
    assert!(!kept.removed);
}

#[test]
fn refuses_build_state_whose_proof_left() {
    let fixture = Fixture::new();
    let plan = fixture.plan(&nobody(), false);
    fs::remove_file(fixture.repo.join("target/CACHEDIR.TAG")).unwrap();
    let report = fixture.apply(&plan);
    assert_eq!(report.status, RunStatus::Partial);
    assert!(fixture.repo.join("target/debug/app").exists());
    assert!(!fixture.linked.join("target").exists());
}

#[test]
fn keeps_build_state_that_holds_a_file_that_git_tracks() {
    let fixture = Fixture::new();
    write(&fixture.repo.join("target/vendored.rs"), "pub fn kept() {}");
    git(&fixture.repo, &["add", "-f", "target/vendored.rs"]);
    git(&fixture.repo, &["commit", "-m", "vendor"]);
    git(&fixture.repo, &["push", "origin", "main"]);

    let plan = fixture.plan(&nobody(), false);
    assert_eq!(
        find(&plan, &fixture.repo.join("target")).kept,
        Some(Kept::TrackedFiles)
    );
    assert_eq!(
        find(&plan, &fixture.linked.join("target")).decision,
        Decision::Remove
    );
    fixture.apply(&plan);
    assert!(fixture.repo.join("target/vendored.rs").exists());
}

#[test]
fn keeps_build_state_that_holds_a_repository() {
    let fixture = Fixture::new();
    let nested = fixture.repo.join("target/checkout");
    fs::create_dir_all(&nested).unwrap();
    git(&nested, &["init"]);
    write(&nested.join("work.txt"), "unpushed");

    let plan = fixture.plan(&nobody(), false);
    assert_eq!(
        find(&plan, &fixture.repo.join("target")).kept,
        Some(Kept::HoldsCheckout)
    );

    // A repository that appears after planning stops the removal as well.
    let late = fixture.linked.join("target/late");
    fs::create_dir_all(&late).unwrap();
    git(&late, &["init"]);
    let report = fixture.apply(&plan);
    assert_eq!(report.status, RunStatus::Refused);
    assert!(late.join(".git").exists());
    assert!(nested.join("work.txt").exists());
}

#[test]
fn keeps_everything_while_the_system_hides_a_process_of_this_user() {
    let fixture = Fixture::new();
    let hidden = Ok(Liveness::of(Vec::new()).with_unreadable(vec!["7 (agent)".to_owned()]));
    let plan = fixture.plan(&hidden, false);
    assert!(
        plan.candidates
            .iter()
            .all(|candidate| candidate.kept == Some(Kept::Unverified))
    );
    assert!(
        plan.candidates[0]
            .evidence
            .last()
            .unwrap()
            .contains("7 (agent)")
    );

    let scan = inspect(
        std::slice::from_ref(&fixture.workspace),
        &Environment::default(),
    );
    let tolerant = Policy {
        idle_seconds: IDLE,
        checkouts: false,
        tolerate_unreadable_processes: true,
    };
    let plan = plan_reclaim(
        &scan,
        &hidden,
        &Git::default(),
        tolerant,
        now() + 2 * IDLE_SECONDS,
    );
    assert_eq!(plan.candidates[0].decision, Decision::Remove);
}

#[test]
fn refuses_at_apply_build_state_that_changed_within_the_idle_time() {
    let fixture = Fixture::new();
    let plan = fixture.plan(&nobody(), false);
    // The run happens now, when the fixture's files are minutes old.
    let report = fixture.apply_at(&plan, &mut nobody, now());
    assert_eq!(report.status, RunStatus::Refused);
    assert!(fixture.repo.join("target/debug/app").exists());
    assert!(
        report.removals[0]
            .refusal
            .as_ref()
            .unwrap()
            .contains("an entry changed at")
    );
}

#[test]
fn observes_the_processes_again_before_each_removal() {
    let fixture = Fixture::new();
    let plan = fixture.plan(&nobody(), false);
    assert_eq!(plan.candidates.len(), 2);
    // A process starts in the linked checkout after the first removal.
    let mut calls = 0;
    let linked = fixture.linked.clone();
    let report = fixture.apply_with(&plan, &mut || {
        calls += 1;
        if calls == 1 {
            nobody()
        } else {
            Ok(Liveness::of(vec![linked.clone()]))
        }
    });
    assert_eq!(report.status, RunStatus::Partial);
    assert!(!fixture.repo.join("target").exists());
    assert!(fixture.linked.join("target").exists());
}

#[test]
fn the_journal_lists_a_reclaim_run_with_its_removals() {
    let fixture = Fixture::new();
    let plan = fixture.plan(&nobody(), false);
    let report = fixture.apply(&plan);
    let summaries = JournalState::from_records(&read(&fixture.journal).unwrap()).summaries();
    assert_eq!(summaries.len(), 1);
    let summary = &summaries[0];
    assert_eq!(summary.kind, RunKind::Reclaim);
    assert_eq!(summary.run, report.run);
    assert_eq!(summary.status, Some(RunStatus::Complete));
    assert!(!summary.undoable);
    assert_eq!(summary.removals.len(), 2);
    assert!(summary.removals.iter().all(|removal| removal.removed));
    assert_eq!(
        summary.removals[0].path.as_path(),
        fixture.repo.join("target")
    );
}

#[test]
fn names_each_directory_once_when_roots_overlap() {
    let fixture = Fixture::new();
    let scan = inspect(
        &[fixture.workspace.clone(), fixture.repo.clone()],
        &Environment::default(),
    );
    let policy = Policy {
        idle_seconds: IDLE,
        checkouts: true,
        tolerate_unreadable_processes: false,
    };
    let plan = plan_reclaim(
        &scan,
        &nobody(),
        &Git::default(),
        policy,
        now() + 2 * IDLE_SECONDS,
    );
    assert_eq!(plan.candidates.len(), 3);
    let report = fixture.apply(&plan);
    assert_eq!(report.status, RunStatus::Complete);
}

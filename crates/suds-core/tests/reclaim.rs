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
    Candidate, CheckoutScope, Decision, Kept, Observed, Policy, ReclaimPlan, Subject, plan_reclaim,
};
use suds_core::registry::{self, Reading, Standings};
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
            "tag.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
            // Git's own maintenance removes files in the background after
            // a commit, which a scan in progress would report as vanished.
            "-c",
            "gc.auto=0",
            "-c",
            "maintenance.auto=false",
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
        let scope = if checkouts {
            CheckoutScope::AllLinked
        } else {
            CheckoutScope::Released
        };
        self.plan_with_scope(liveness, scope, at)
    }

    fn plan_with_scope(
        &self,
        liveness: &Result<Liveness, String>,
        scope: CheckoutScope,
        at: i64,
    ) -> ReclaimPlan {
        let scan = inspect(
            std::slice::from_ref(&self.workspace),
            &Environment::default(),
        );
        let policy = Policy {
            idle_seconds: IDLE,
            checkouts: scope,
            tolerate_unreadable_processes: false,
        };
        let observed = Observed {
            liveness,
            registry: &self.standings(),
        };
        plan_reclaim(&scan, &observed, &Git::default(), policy, at)
    }

    /// The registry's standings in this fixture's state directory.
    fn standings(&self) -> Reading {
        registry::reading(&self.registry())
    }

    fn registry(&self) -> PathBuf {
        self.journal.with_file_name("registry.jsonl")
    }

    fn record(&self, checkout: &Path, event: registry::Event) {
        registry::append(&self.registry(), checkout, "bosun", Some("wf-1"), event, 0).unwrap();
    }

    fn release_landed(&self) -> String {
        let head = Git::default().head(&self.linked).unwrap();
        registry::append_landed(&self.registry(), &self.linked, "bosun", None, &head, 0).unwrap();
        head
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
            registry: &mut || self.standings(),
            git: &Git::default(),
            now: at,
        };
        apply(&mut journal, plan, conditions).unwrap()
    }

    fn apply(&self, plan: &ReclaimPlan) -> ReclaimReport {
        self.apply_with(plan, &mut nobody)
    }

    fn apply_unattended(&self, plan: &ReclaimPlan, at: i64) -> ReclaimReport {
        apply(
            &mut open_journal(&self.journal),
            plan,
            Conditions {
                authority: Authority::StandingApproval,
                observe: &mut nobody,
                registry: &mut || self.standings(),
                git: &Git::default(),
                now: at,
            },
        )
        .unwrap()
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
        checkouts: CheckoutScope::DerivedOnly,
        tolerate_unreadable_processes: true,
    };
    let plan = plan_reclaim(
        &scan,
        &Observed {
            liveness: &hidden,
            registry: &Ok(Standings::new()),
        },
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
        checkouts: CheckoutScope::AllLinked,
        tolerate_unreadable_processes: false,
    };
    let plan = plan_reclaim(
        &scan,
        &Observed {
            liveness: &nobody(),
            registry: &Ok(Standings::new()),
        },
        &Git::default(),
        policy,
        now() + 2 * IDLE_SECONDS,
    );
    assert_eq!(plan.candidates.len(), 3);
    let report = fixture.apply(&plan);
    assert_eq!(report.status, RunStatus::Complete);
}

#[test]
fn keeps_what_a_client_registered_and_reclaims_what_it_released_first() {
    let fixture = Fixture::new();
    fixture.record(&fixture.linked, registry::Event::Registered);
    let plan = fixture.plan(&nobody(), true);
    assert_eq!(
        find(&plan, &fixture.linked.join("target")).kept,
        Some(Kept::Registered)
    );
    assert_eq!(find(&plan, &fixture.linked).kept, Some(Kept::Registered));
    // The registration covers the linked checkout alone.
    assert_eq!(
        find(&plan, &fixture.repo.join("target")).decision,
        Decision::Remove
    );

    // A released checkout enters the plan without the policy for linked
    // checkouts, and before every other checkout.
    fixture.record(&fixture.linked, registry::Event::Released);
    let plan = fixture.plan(&nobody(), false);
    assert_eq!(plan.candidates[0].checkout.as_path(), fixture.linked);
    assert_eq!(find(&plan, &fixture.linked).decision, Decision::Remove);
    let report = fixture.apply(&plan);
    assert_eq!(report.status, RunStatus::Complete);
    assert!(!fixture.linked.exists());
}

#[test]
fn a_registration_during_the_run_keeps_the_checkout() {
    let fixture = Fixture::new();
    fixture.record(&fixture.linked, registry::Event::Released);
    let plan = fixture.plan(&nobody(), false);
    fixture.record(&fixture.linked, registry::Event::Registered);
    let report = fixture.apply(&plan);
    assert!(fixture.linked.join("target").exists());
    assert!(fixture.linked.exists());
    assert!(!fixture.repo.join("target").exists());
    assert_eq!(report.status, RunStatus::Partial);
}

#[test]
fn keeps_everything_while_the_registry_cannot_be_read() {
    let fixture = Fixture::new();
    fs::write(fixture.registry(), b"damaged\n").unwrap();
    let plan = fixture.plan(&nobody(), true);
    assert!(
        plan.candidates
            .iter()
            .all(|candidate| candidate.kept == Some(Kept::Unverified))
    );

    // A registry that breaks after planning stops each removal as well.
    fs::remove_file(fixture.registry()).unwrap();
    let plan = fixture.plan(&nobody(), false);
    fs::write(fixture.registry(), b"damaged\n").unwrap();
    let report = fixture.apply(&plan);
    assert_eq!(report.status, RunStatus::Refused);
    assert!(fixture.repo.join("target").exists());
}

#[test]
fn a_registration_keeps_each_checkout_beneath_its_own() {
    let fixture = Fixture::new();
    // The linked checkout lies beneath the main checkout that a client
    // registered.
    fixture.record(&fixture.repo, registry::Event::Registered);
    let plan = fixture.plan(&nobody(), true);
    assert!(
        plan.candidates
            .iter()
            .all(|candidate| candidate.kept == Some(Kept::Registered))
    );
    assert_eq!(plan.candidates.len(), 3);
}

#[test]
fn reads_a_plan_that_an_earlier_release_recorded() {
    let fixture = Fixture::new();
    let plan = fixture.plan(&nobody(), false);
    let mut document = serde_json::to_value(&plan).unwrap();
    // An earlier release wrote no such field into the journal.
    document
        .as_object_mut()
        .unwrap()
        .remove("tolerate_unreadable_processes");
    let earlier: ReclaimPlan = serde_json::from_value(document).unwrap();
    assert!(!earlier.tolerate_unreadable_processes);
    assert_eq!(earlier.candidates, plan.candidates);
}

#[test]
fn removes_build_state_that_holds_a_repository_that_a_build_fetched() {
    let fixture = Fixture::new();
    let (build, fetched) = cmake_build(&fixture, &fixture.repo);

    let scan = inspect(
        std::slice::from_ref(&fixture.workspace),
        &Environment::default(),
    );
    // The inspection lists the repository as no checkout of its own.
    assert!(
        scan.checkouts
            .iter()
            .all(|checkout| checkout.path != fetched)
    );
    let plan = fixture.plan(&nobody(), false);
    let candidate = find(&plan, &build);
    assert_eq!(
        candidate.decision,
        Decision::Remove,
        "{:?}",
        candidate.evidence
    );
    assert!(
        candidate
            .evidence
            .iter()
            .any(|fact| fact.contains("holds only what a remote holds"))
    );

    // A commit that no remote holds keeps the whole directory.
    write(&fetched.join("patch.txt"), "local");
    git(&fetched, &["add", "."]);
    git(&fetched, &["commit", "-m", "local patch"]);
    let report = fixture.apply(&plan);
    assert!(fetched.join("patch.txt").exists());
    assert!(!fixture.linked.join("target").exists());
    assert_eq!(report.status, RunStatus::Partial);
}

/// A CMake build directory in `checkout` that Git ignores. Its build cloned
/// one dependency in CMake's way, beside the bookkeeping of that fetch. The
/// result is the build directory and the dependency's repository.
fn cmake_build(fixture: &Fixture, checkout: &Path) -> (PathBuf, PathBuf) {
    let remote = fixture.workspace.parent().unwrap().join("remote.git");
    let build = checkout.join("build");
    write(&build.join("CMakeCache.txt"), "");
    write(&build.join("_deps/dependency-subbuild/CMakeLists.txt"), "");
    git(
        &build.join("_deps"),
        &[
            "clone",
            "--quiet",
            remote.to_str().unwrap(),
            "dependency-src",
        ],
    );
    write(&fixture.repo.join(".git/info/exclude"), "build/\n.vcpkg/\n");
    (build.clone(), build.join("_deps/dependency-src"))
}

/// A vcpkg checkout that the linked checkout cloned into its own tree, which
/// Git ignores there.
fn fetched_toolchain(fixture: &Fixture) -> PathBuf {
    let remote = fixture.workspace.parent().unwrap().join("remote.git");
    git(
        &fixture.linked,
        &["clone", "--quiet", remote.to_str().unwrap(), ".vcpkg"],
    );
    let toolchain = fixture.linked.join(".vcpkg");
    write(&toolchain.join(".vcpkg-root"), "");
    write(
        &toolchain.join(".git/info/exclude"),
        ".vcpkg-root\nbuildtrees/\n",
    );
    write(&fixture.repo.join(".git/info/exclude"), "build/\n.vcpkg/\n");
    toolchain
}

#[test]
fn removes_a_toolchain_that_a_checkout_fetched_as_a_repository() {
    let fixture = Fixture::new();
    let toolchain = fetched_toolchain(&fixture);

    let plan = fixture.plan(&nobody(), false);
    let candidate = find(&plan, &toolchain);
    assert!(matches!(
        candidate.subject,
        Subject::FetchedToolchain { .. }
    ));
    assert_eq!(candidate.checkout.as_path(), fixture.linked);
    assert_eq!(
        candidate.decision,
        Decision::Remove,
        "{:?}",
        candidate.evidence
    );

    // A local commit in the toolchain keeps it.
    write(&toolchain.join("port.txt"), "patched");
    git(&toolchain, &["add", "port.txt"]);
    git(&toolchain, &["commit", "-m", "local port"]);
    let report = fixture.apply(&plan);
    assert!(toolchain.join("port.txt").exists());
    let kept = report
        .removals
        .iter()
        .find(|removal| removal.path.as_path() == toolchain)
        .unwrap();
    assert!(!kept.removed);

    // Without the commit, the run removes it.
    git(&toolchain, &["reset", "--quiet", "--hard", "origin/main"]);
    let plan = fixture.plan(&nobody(), false);
    assert_eq!(find(&plan, &toolchain).decision, Decision::Remove);
    fixture.apply(&plan);
    assert!(!toolchain.exists());
    assert!(fixture.linked.join("src/lib.rs").exists());
}

#[test]
fn removes_each_cache_that_a_project_points_into_its_checkout() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    write(&fixture.repo.join(".git/info/exclude"), ".cache/\n");
    let cache = fixture.linked.join(".cache");
    write(
        &cache.join("go-build/README"),
        "This directory holds cached build artifacts from the Go build system.\n",
    );
    write(&cache.join("go-build/trim.txt"), "1");
    write(&cache.join("go-build/00/object-a"), "object");
    write(&cache.join("go-build/ff/object-b"), "object");
    write(
        &cache.join("go-mod/cache/download/example.com/@v/list"),
        "v1",
    );
    // Go lets no one write a module's directory.
    let module = cache.join("go-mod/example.com@v1");
    write(&module.join("lib.go"), "package lib");
    fs::set_permissions(&module, fs::Permissions::from_mode(0o555)).unwrap();
    write(
        &cache.join("ccache/0/CACHEDIR.TAG"),
        "Signature: 8a477f597d28d172789f06886806bc55\n# This file is a cache directory tag created by ccache.\n",
    );
    write(&cache.join("ccache/0/stats"), "0");
    // A directory with a README of its own is no cache, and a marker behind
    // a symbolic link proves nothing.
    write(&fixture.linked.join("docs/README"), "The documents.");
    let outside = fixture.workspace.join("outside");
    write(&outside.join("download/keep"), "");
    write(&outside.join("CACHEDIR.TAG"), "");
    write(&outside.join("stats"), "");
    fs::create_dir_all(cache.join("linked")).unwrap();
    std::os::unix::fs::symlink(&outside, cache.join("linked/cache")).unwrap();
    std::os::unix::fs::symlink(&outside, cache.join("linked/0")).unwrap();

    let plan = fixture.plan(&nobody(), false);
    for (name, label) in [("go-build", "go"), ("go-mod", "go"), ("ccache", "ccache")] {
        let candidate = find(&plan, &cache.join(name));
        assert_eq!(
            candidate.decision,
            Decision::Remove,
            "{:?}",
            candidate.evidence
        );
        let Subject::BuildState { ecosystem } = candidate.subject else {
            panic!("{name} is build state");
        };
        assert_eq!(ecosystem.label(), label);
    }
    for other in [fixture.linked.join("docs"), cache.join("linked")] {
        assert!(
            plan.candidates
                .iter()
                .all(|candidate| candidate.path.as_path() != other)
        );
    }

    let report = fixture.apply(&plan);
    assert_eq!(report.status, RunStatus::Complete, "{:?}", report.removals);
    assert!(!cache.join("go-build").exists());
    assert!(!cache.join("go-mod").exists());
    assert!(!cache.join("ccache").exists());
    assert!(fixture.linked.join("docs/README").exists());
}

#[test]
fn a_fetched_repository_holds_no_branch_tag_stash_or_worktree_of_its_own() {
    let fixture = Fixture::new();
    let (_, fetched) = cmake_build(&fixture, &fixture.repo);
    let git_adapter = Git::default();
    let only_fetched = || git_adapter.holds_only_fetched(&fetched).unwrap();
    assert!(only_fetched());

    // A commit on another local branch is work, though a remote holds the head.
    git(&fetched, &["checkout", "--quiet", "-b", "local"]);
    write(&fetched.join("patch.txt"), "local");
    git(&fetched, &["add", "."]);
    git(&fetched, &["commit", "-m", "local patch"]);
    git(&fetched, &["checkout", "--quiet", "main"]);
    assert!(!only_fetched());

    // A local tag proves no fetch.
    git(&fetched, &["tag", "kept", "local"]);
    git(&fetched, &["branch", "--quiet", "-D", "local"]);
    assert!(!only_fetched());
    git(&fetched, &["tag", "-d", "kept"]);
    assert!(only_fetched());

    // A second worktree borrows the repository.
    let other = fixture.workspace.join("borrower");
    git(
        &fetched,
        &[
            "worktree",
            "add",
            "--quiet",
            "--detach",
            other.to_str().unwrap(),
        ],
    );
    assert!(!only_fetched());
    git(&fetched, &["worktree", "remove", other.to_str().unwrap()]);
    assert!(only_fetched());

    // A directory inside another checkout is no repository of its own.
    assert!(
        !git_adapter
            .holds_only_fetched(&fixture.linked.join("src"))
            .unwrap()
    );
}

#[test]
fn a_shallow_fetch_at_a_tag_holds_only_what_it_fetched() {
    let fixture = Fixture::new();
    let remote = fixture.workspace.parent().unwrap().join("remote.git");
    git(&fixture.repo, &["tag", "v1"]);
    git(&fixture.repo, &["push", "--quiet", "origin", "v1"]);
    let url = format!("file://{}", remote.display());
    git(
        &fixture.workspace,
        &[
            "clone", "--quiet", "--depth", "1", "--branch", "v1", &url, "shallow",
        ],
    );
    let shallow = fixture.workspace.join("shallow");
    let git_adapter = Git::default();
    assert!(git_adapter.holds_only_fetched(&shallow).unwrap());

    write(&shallow.join("patch.txt"), "local");
    git(&shallow, &["add", "."]);
    git(&shallow, &["commit", "-m", "local patch"]);
    assert!(!git_adapter.holds_only_fetched(&shallow).unwrap());
}

#[test]
fn a_repository_that_no_layout_proves_fetched_stays_a_checkout() {
    let fixture = Fixture::new();
    let (build, _) = cmake_build(&fixture, &fixture.repo);
    // A person cloned a repository into the build directory.
    let remote = fixture.workspace.parent().unwrap().join("remote.git");
    write(&build.join("tools/.keep"), "");
    git(
        &build.join("tools"),
        &["clone", "--quiet", remote.to_str().unwrap(), "mine"],
    );
    let mine = build.join("tools/mine");

    let scan = inspect(
        std::slice::from_ref(&fixture.workspace),
        &Environment::default(),
    );
    assert!(scan.checkouts.iter().any(|checkout| checkout.path == mine));
    let plan = fixture.plan(&nobody(), false);
    assert_eq!(find(&plan, &build).kept, Some(Kept::HoldsCheckout));
    fixture.apply(&plan);
    assert!(mine.join(".git").exists());
}

#[test]
fn a_fetched_repository_with_work_keeps_its_linked_checkout() {
    let fixture = Fixture::new();
    let (_, fetched) = cmake_build(&fixture, &fixture.linked);
    let plan = fixture.plan(&nobody(), true);
    assert_eq!(find(&plan, &fixture.linked).decision, Decision::Remove);

    // Git would remove the ignored build directory with the checkout, so
    // work in the dependency keeps the checkout at the removal too.
    write(&fetched.join("notes.txt"), "untracked");
    let kept = fixture.plan(&nobody(), true);
    assert_eq!(find(&kept, &fixture.linked).kept, Some(Kept::HoldsCheckout));
    fixture.apply(&plan);
    assert!(fetched.join("notes.txt").exists());
}

#[test]
fn a_registration_inside_a_candidate_keeps_the_candidate() {
    let fixture = Fixture::new();
    let (build, fetched) = cmake_build(&fixture, &fixture.repo);
    let toolchain = fetched_toolchain(&fixture);
    let plan = fixture.plan(&nobody(), false);
    assert_eq!(find(&plan, &build).decision, Decision::Remove);
    assert_eq!(find(&plan, &toolchain).decision, Decision::Remove);

    fixture.record(&fetched, registry::Event::Registered);
    fixture.record(&toolchain, registry::Event::Registered);
    let held = fixture.plan(&nobody(), false);
    assert_eq!(find(&held, &build).kept, Some(Kept::Registered));
    assert_eq!(find(&held, &toolchain).kept, Some(Kept::Registered));
    // The earlier plan meets the registrations at its removals.
    fixture.apply(&plan);
    assert!(fetched.exists());
    assert!(toolchain.exists());
}

#[test]
fn keeps_a_toolchain_that_is_no_repository_or_that_holds_one() {
    let fixture = Fixture::new();
    let toolchain = fetched_toolchain(&fixture);
    // A build tree of the toolchain holds a repository of its own.
    let nested = toolchain.join("buildtrees/port");
    fs::create_dir_all(&nested).unwrap();
    git(&nested, &["init", "--quiet"]);
    write(&nested.join("work.txt"), "work");
    let plan = fixture.plan(&nobody(), false);
    assert_eq!(find(&plan, &toolchain).kept, Some(Kept::HoldsCheckout));
    fs::remove_dir_all(&nested).unwrap();

    // A checkout that tracks vcpkg's files holds no fetched repository.
    fs::remove_dir_all(toolchain.join(".git")).unwrap();
    let plan = fixture.plan(&nobody(), false);
    assert_eq!(find(&plan, &toolchain).decision, Decision::Keep);
    fixture.apply(&plan);
    assert!(toolchain.join(".vcpkg-root").exists());
}

/// Sets the modification time of `path` and of everything beneath it.
fn backdate(path: &Path, to: SystemTime) {
    let meta = fs::symlink_metadata(path).unwrap();
    if meta.is_symlink() {
        return;
    }
    if meta.is_dir() {
        for entry in fs::read_dir(path).unwrap() {
            backdate(&entry.unwrap().path(), to);
        }
    }
    fs::File::open(path).unwrap().set_modified(to).unwrap();
}

#[test]
fn removes_a_released_checkout_and_its_build_state_in_one_run() {
    let fixture = Fixture::new();
    fixture.record(&fixture.linked, registry::Event::Released);
    // The checkout last changed two idle times before the run, and the run
    // happens at the present, so a change during the run counts as recent.
    let idle = std::time::Duration::from_secs(IDLE);
    backdate(&fixture.linked, SystemTime::now() - 2 * idle);
    let plan = fixture.plan_at(&nobody(), false, now());
    assert_eq!(find(&plan, &fixture.linked).decision, Decision::Remove);
    assert_eq!(
        find(&plan, &fixture.linked.join("target")).decision,
        Decision::Remove
    );

    let report = fixture.apply_at(&plan, &mut nobody, now());
    assert!(!fixture.linked.exists());
    let of_linked: Vec<_> = report
        .removals
        .iter()
        .filter(|removal| removal.path.as_path().starts_with(&fixture.linked))
        .collect();
    assert_eq!(of_linked.len(), 2);
    assert!(of_linked.iter().all(|removal| removal.removed));
    // The report lists the removals in the plan's order.
    assert!(report.removals.is_sorted_by_key(|removal| removal.index));
}

#[test]
fn removes_a_released_checkout_that_holds_a_read_only_go_cache() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    write(&fixture.repo.join(".git/info/exclude"), ".cache/\n");
    let modules = fixture.linked.join(".cache/go-mod");
    write(&modules.join("cache/download/example.com/@v/list"), "v1");
    // Go lets no one write a module's directory.
    let module = modules.join("example.com@v1");
    write(&module.join("lib.go"), "package lib");
    fs::set_permissions(&module, fs::Permissions::from_mode(0o555)).unwrap();
    fixture.record(&fixture.linked, registry::Event::Released);

    let plan = fixture.plan(&nobody(), false);
    assert_eq!(find(&plan, &fixture.linked).decision, Decision::Remove);
    let report = fixture.apply(&plan);
    assert_eq!(report.status, RunStatus::Complete, "{:?}", report.removals);
    assert!(!fixture.linked.exists());
}

#[test]
fn refuses_an_ignored_file_that_appeared_after_planning() {
    let fixture = Fixture::new();
    let head = fixture.release_landed();
    let at = now() + 2 * IDLE_SECONDS;
    let plan = fixture.plan_with_scope(&nobody(), CheckoutScope::Landed, at);
    let candidate = find(&plan, &fixture.linked);
    assert_eq!(candidate.decision, Decision::Remove);
    let proof = format!("the remote-tracking reference refs/remotes/origin/feature holds {head}");
    assert!(candidate.evidence.contains(&proof));

    let added = fixture.linked.join(".worktrees/history.jsonl");
    write(&added, "workflow history");
    assert!(Git::default().is_clean(&fixture.linked).unwrap());
    let reason = format!(
        "Suds could not account for {}",
        added.parent().unwrap().display()
    );

    let report = fixture.apply_unattended(&plan, at);
    let removal = report
        .removals
        .iter()
        .find(|removal| removal.path.as_path() == fixture.linked)
        .unwrap();
    assert!(!removal.removed);
    assert_eq!(removal.refusal.as_deref(), Some(reason.as_str()));
    assert_eq!(fs::read_to_string(added).unwrap(), "workflow history");
    assert!(!fixture.linked.join("target").exists());
    assert!(read(&fixture.journal).unwrap().iter().any(|record| {
        matches!(&record.event, Event::RemovalRefused { run, reason: recorded, .. }
            if run == &report.run && recorded == &reason)
    }));

    let plan = fixture.plan(&nobody(), false);
    let approved = fixture.apply(&plan);
    assert!(!fixture.linked.exists());
    assert!(read(&fixture.journal).unwrap().iter().any(|record| {
        matches!(&record.event, Event::Removed { run, evidence, .. }
            if run == &approved.run && evidence.contains(&proof))
    }));
}

#[test]
fn refuses_a_landed_checkout_when_its_tracking_reference_disappears() {
    let fixture = Fixture::new();
    let head = fixture.release_landed();
    let at = now() + 2 * IDLE_SECONDS;
    let plan = fixture.plan_with_scope(&nobody(), CheckoutScope::Landed, at);
    assert_eq!(find(&plan, &fixture.linked).decision, Decision::Remove);
    git(
        &fixture.repo,
        &["update-ref", "-d", "refs/remotes/origin/feature"],
    );

    let report = fixture.apply_unattended(&plan, at);
    let removal = report
        .removals
        .iter()
        .find(|removal| removal.path.as_path() == fixture.linked)
        .unwrap();
    assert_eq!(
        removal.refusal,
        Some(format!(
            "no remote-tracking reference holds the released head {head}"
        ))
    );
    assert!(fixture.linked.join("src/lib.rs").is_file());
}

#[test]
fn removes_a_released_checkout_whose_head_a_client_reports_as_landed() {
    let fixture = Fixture::new();
    // The forge squashed the branch and deleted it, so no remote holds the
    // head's own commits.
    git(
        &fixture.repo,
        &["push", "--quiet", "origin", "--delete", "feature"],
    );
    git(&fixture.repo, &["fetch", "--quiet", "--prune", "origin"]);
    fixture.record(&fixture.linked, registry::Event::Released);
    let plan = fixture.plan(&nobody(), false);
    assert_eq!(
        find(&plan, &fixture.linked).kept,
        Some(Kept::UnpushedCommits)
    );

    let head = fixture.release_landed();
    let plan = fixture.plan(&nobody(), false);
    let candidate = find(&plan, &fixture.linked);
    assert_eq!(
        candidate.decision,
        Decision::Remove,
        "{:?}",
        candidate.evidence
    );

    // Standing approval needs a reference that contains the released head.
    let unattended = fixture.apply_unattended(&plan, now() + 2 * IDLE_SECONDS);
    assert!(fixture.linked.exists());
    let removal = unattended
        .removals
        .iter()
        .find(|removal| removal.path.as_path() == fixture.linked)
        .unwrap();
    assert_eq!(
        removal.refusal,
        Some(format!(
            "no remote-tracking reference holds the released head {head}"
        ))
    );

    // A commit after the report is work that no remote holds.
    write(&fixture.linked.join("src/more.rs"), "pub fn g() {}");
    git(&fixture.linked, &["add", "."]);
    git(&fixture.linked, &["commit", "-m", "more"]);
    let report = fixture.apply(&plan);
    assert!(fixture.linked.join("src/more.rs").exists());
    assert_ne!(report.status, RunStatus::Complete);

    // A report of the new head lets the checkout leave.
    fixture.release_landed();
    let plan = fixture.plan(&nobody(), false);
    fixture.apply(&plan);
    assert!(!fixture.linked.exists());
    assert!(
        registry::append_landed(
            &fixture.registry(),
            &fixture.linked,
            "bosun",
            None,
            "abc",
            0
        )
        .is_err()
    );
}

//! Tests of an unattended maintenance run against a temporary repository.

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use suds_core::development::Environment;
use suds_core::maintain::{Context, maintain, read_pressure};
use suds_core::organize::journal::{Event, read};
use suds_core::pressure::Level;
use suds_core::reclaim::execute::Authority;
use suds_core::reclaim::git::Git;
use suds_core::reclaim::plan::{Candidate, Decision, Kept, ReclaimPlan, Subject};
use suds_core::registry::{self, Event as RegistryEvent};
use suds_core::state::{Directories, Settings};
use suds_core::stores::Programs;
use tempfile::TempDir;

const GIB: u64 = 1024 * 1024 * 1024;
/// A time long after the fixture's files changed.
const LATER: i64 = 4_000_000_000;
const PULL_HEAD: &str = "refs/pull/42/head";

fn git(directory: &Path, arguments: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args([
            "-c",
            "user.name=Suds",
            "-c",
            "user.email=suds@example.invalid",
        ])
        .args([
            "-c",
            "commit.gpgsign=false",
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
    assert!(output.status.success(), "git {arguments:?}");
}

/// A workspace with one checkout that holds Cargo's build output, an empty
/// home directory, and a stub of Docker that reports no usage.
struct Fixture {
    _dir: TempDir,
    repo: PathBuf,
    directories: Directories,
    environment: Environment,
    programs: Programs,
}

impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let repo = root.join("workspace/repo");
        fs::create_dir_all(repo.join("target/debug")).unwrap();
        git(&repo, &["init"]);
        fs::write(repo.join("Cargo.toml"), "[package]").unwrap();
        fs::write(repo.join(".gitignore"), "target/\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "first"]);
        fs::write(repo.join("target/CACHEDIR.TAG"), "Signature: cargo").unwrap();
        fs::write(repo.join("target/debug/app"), "binary").unwrap();

        let docker = root.join("docker");
        fs::write(&docker, "#!/bin/sh\necho '{}'\n").unwrap();
        fs::set_permissions(&docker, fs::Permissions::from_mode(0o755)).unwrap();
        let environment = Environment {
            home: Some(root.join("home")),
            variables: HashMap::new(),
        };
        Fixture {
            directories: Directories::of(&environment).unwrap(),
            environment,
            programs: Programs {
                docker,
                ..Programs::default()
            },
            repo,
            _dir: dir,
        }
    }

    fn settings(&self, standing_approval: bool) -> Settings {
        Settings {
            roots: vec![self.repo.parent().unwrap().to_path_buf()],
            ceiling_bytes: Some(100 * GIB),
            standing_approval,
            // The fixture's own process tree may hold a process that hides
            // its working directory.
            tolerate_unreadable_processes: true,
            ..Settings::default()
        }
    }

    fn context<'a>(&'a self, git: &'a Git) -> Context<'a> {
        Context {
            directories: &self.directories,
            environment: &self.environment,
            programs: &self.programs,
            git,
            now: LATER,
        }
    }

    fn remote(&self) -> PathBuf {
        let root = self.repo.parent().unwrap().parent().unwrap();
        let remote = root.join("remote.git");
        git(root, &["init", "--bare", "remote.git"]);
        git(
            &self.repo,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(&self.repo, &["push", "origin", "main"]);
        remote
    }

    fn linked(&self, branch: &str) -> PathBuf {
        let checkout = self.repo.with_file_name(branch);
        git(
            &self.repo,
            &["worktree", "add", "-b", branch, checkout.to_str().unwrap()],
        );
        commit_change(&checkout, "feature");
        fs::create_dir(checkout.join("target")).unwrap();
        fs::write(checkout.join("target/CACHEDIR.TAG"), "Signature: cargo").unwrap();
        fs::write(checkout.join("target/app"), "binary").unwrap();
        checkout
    }

    fn release_landed(&self, checkout: &Path) -> String {
        let head = Git::default().head(checkout).unwrap();
        registry::append_landed(
            &self.directories.registry(),
            checkout,
            "bosun",
            Some("wf-1"),
            &head,
            0,
        )
        .unwrap();
        head
    }

    fn reclaim_plan(&self) -> ReclaimPlan {
        let report = maintain(&self.settings(true), &self.context(&Git::default())).unwrap();
        assert_eq!(
            report.reclaim.unwrap().authority,
            Authority::StandingApproval
        );
        read(&self.directories.journal())
            .unwrap()
            .into_iter()
            .find_map(|record| match record.event {
                Event::ReclaimPlanned { plan } => Some(plan),
                _ => None,
            })
            .unwrap()
    }
}

fn commit_change(checkout: &Path, text: &str) {
    fs::write(checkout.join("change.txt"), text).unwrap();
    git(checkout, &["add", "change.txt"]);
    git(checkout, &["commit", "-m", text]);
}

fn linked_candidate<'a>(plan: &'a ReclaimPlan, checkout: &Path) -> Option<&'a Candidate> {
    plan.candidates.iter().find(|candidate| {
        candidate.subject == Subject::LinkedCheckout && candidate.path.as_path() == checkout
    })
}

#[test]
fn only_measures_without_the_standing_approval() {
    let fixture = Fixture::new();
    let git = Git::default();
    let report = maintain(&fixture.settings(false), &fixture.context(&git)).unwrap();
    assert!(!report.standing_approval);
    assert!(report.reclaim.is_none() && report.stores.is_none());
    assert!(fixture.repo.join("target/debug/app").exists());
    assert!(!fixture.directories.journal().exists());

    let reading = read_pressure(&fixture.directories.pressure()).unwrap();
    assert_eq!(reading, report.pressure);
    assert_eq!(reading.level, Level::Normal);
    assert_eq!(reading.ceiling_bytes, 100 * GIB);
    assert!(reading.usage_bytes > 0);
    assert!(!reading.enforced);
    assert!(reading.complete);
    assert_eq!(report.error_count, 0);
    // The run leaves no staged file beside the reading.
    let state: Vec<_> = fs::read_dir(&fixture.directories.state)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(state, ["pressure.json"]);
}

#[test]
fn reports_what_it_could_not_read_and_marks_the_reading_partial() {
    use std::os::unix::fs::MetadataExt;

    let fixture = Fixture::new();
    let hidden = fixture.repo.join("private");
    fs::create_dir(&hidden).unwrap();
    // The superuser reads every directory, so the test holds only for an
    // ordinary user.
    if fs::symlink_metadata(&hidden).unwrap().uid() == 0 {
        return;
    }
    fs::set_permissions(&hidden, fs::Permissions::from_mode(0o000)).unwrap();
    let git = Git::default();
    let report = maintain(&fixture.settings(false), &fixture.context(&git));
    fs::set_permissions(&hidden, fs::Permissions::from_mode(0o700)).unwrap();
    let report = report.unwrap();
    assert_eq!(report.error_count, 1);
    assert_eq!(report.errors[0].path, hidden.to_string_lossy());
    assert!(!report.pressure.complete);
}

#[test]
fn reclaims_under_the_standing_approval_and_records_that_authority() {
    let fixture = Fixture::new();
    let git = Git::default();
    let report = maintain(&fixture.settings(true), &fixture.context(&git)).unwrap();
    let reclaim = report.reclaim.unwrap();
    assert_eq!(reclaim.authority, Authority::StandingApproval);
    assert_eq!(reclaim.removals.len(), 1);
    assert!(reclaim.removals[0].removed);
    assert!(!fixture.repo.join("target").exists());
    assert!(fixture.repo.join("Cargo.toml").exists());
    assert!(report.stores.unwrap().trims.is_empty());

    let records = read(&fixture.directories.journal()).unwrap();
    assert!(records.iter().any(|record| matches!(
        record.event,
        Event::ReclaimStarted {
            authority: Authority::StandingApproval,
            ..
        }
    )));
}

#[test]
fn removes_a_released_landed_checkout_held_by_a_pull_reference() {
    let fixture = Fixture::new();
    fixture.remote();
    let checkout = fixture.linked("feature");
    git(&checkout, &["push", "origin", &format!("HEAD:{PULL_HEAD}")]);
    let head = fixture.release_landed(&checkout);
    assert_eq!(Git::default().unpushed(&checkout).unwrap(), 1);

    let plan = fixture.reclaim_plan();
    let candidate = linked_candidate(&plan, &checkout).unwrap();
    assert_eq!(
        candidate.decision,
        Decision::Remove,
        "{:?}",
        candidate.evidence
    );
    assert!(!checkout.exists());
    assert!(fixture.repo.exists());
    git(
        &fixture.repo,
        &["show-ref", "--verify", "refs/heads/feature"],
    );

    let evidence = format!("git ls-remote found {PULL_HEAD} at {head}");
    assert!(read(&fixture.directories.journal()).unwrap().iter().any(|record| {
        matches!(&record.event, Event::Removed { evidence: facts, .. } if facts.contains(&evidence))
    }));
}

#[test]
fn keeps_a_released_landed_checkout_when_the_remote_holds_no_reference() {
    let fixture = Fixture::new();
    fixture.remote();
    let checkout = fixture.linked("feature");
    let head = fixture.release_landed(&checkout);

    let plan = fixture.reclaim_plan();
    let candidate = linked_candidate(&plan, &checkout).unwrap();
    assert_eq!(candidate.kept, Some(Kept::UnpushedCommits));
    assert_eq!(
        candidate.evidence.last().unwrap(),
        &format!("the remote holds no reference at {head}")
    );
    assert!(checkout.exists());
}

#[test]
fn keeps_a_landed_checkout_whose_head_moved_even_when_the_new_head_is_pushed() {
    let fixture = Fixture::new();
    fixture.remote();
    let checkout = fixture.linked("feature");
    git(&checkout, &["push", "origin", &format!("HEAD:{PULL_HEAD}")]);
    fixture.release_landed(&checkout);
    commit_change(&checkout, "another change");
    git(&checkout, &["push", "origin", "feature"]);

    let plan = fixture.reclaim_plan();
    assert_eq!(
        linked_candidate(&plan, &checkout).unwrap().kept,
        Some(Kept::LandedHeadChanged)
    );
    assert_eq!(
        fs::read_to_string(checkout.join("change.txt")).unwrap(),
        "another change"
    );
}

#[test]
fn keeps_a_landed_checkout_with_an_untracked_file() {
    let fixture = Fixture::new();
    fixture.remote();
    let checkout = fixture.linked("feature");
    git(&checkout, &["push", "origin", &format!("HEAD:{PULL_HEAD}")]);
    fixture.release_landed(&checkout);
    let untracked = checkout.join("notes.txt");
    fs::write(&untracked, "notes").unwrap();

    let plan = fixture.reclaim_plan();
    assert_eq!(
        linked_candidate(&plan, &checkout).unwrap().kept,
        Some(Kept::UncommittedChanges)
    );
    assert!(untracked.exists());
}

#[test]
fn keeps_a_landed_checkout_that_a_second_client_still_holds() {
    let fixture = Fixture::new();
    fixture.remote();
    let checkout = fixture.linked("feature");
    git(&checkout, &["push", "origin", &format!("HEAD:{PULL_HEAD}")]);
    for (client, attempt) in [("bosun", "wf-1"), ("editor", "edit-1")] {
        registry::append(
            &fixture.directories.registry(),
            &checkout,
            client,
            Some(attempt),
            RegistryEvent::Registered,
            0,
        )
        .unwrap();
    }
    fixture.release_landed(&checkout);

    let plan = fixture.reclaim_plan();
    assert!(linked_candidate(&plan, &checkout).is_none());
    let build = plan
        .candidates
        .iter()
        .find(|candidate| candidate.path.as_path() == checkout.join("target"))
        .unwrap();
    assert_eq!(build.kept, Some(Kept::Registered));
    assert!(checkout.join("target/app").exists());
}

#[test]
fn keeps_all_landed_checkouts_when_the_remote_is_unreachable_and_reclaims_build_state() {
    let fixture = Fixture::new();
    let remote = fixture.remote();
    let first = fixture.linked("first");
    let second = fixture.linked("second");
    git(&first, &["push", "origin", &format!("HEAD:{PULL_HEAD}")]);
    git(&second, &["push", "origin", "HEAD:refs/pull/43/head"]);
    fixture.release_landed(&first);
    fixture.release_landed(&second);
    fs::rename(&remote, remote.with_extension("offline")).unwrap();

    let plan = fixture.reclaim_plan();
    for checkout in [first, second] {
        let candidate = linked_candidate(&plan, &checkout).unwrap();
        assert_eq!(candidate.kept, Some(Kept::Unverified));
        assert!(
            candidate
                .evidence
                .last()
                .unwrap()
                .contains("git ls-remote failed")
        );
        assert!(checkout.exists());
        assert!(!checkout.join("target").exists());
    }
    assert!(!fixture.repo.join("target").exists());
}

#[test]
fn leaves_an_unreleased_checkout_out_of_the_landed_rule() {
    let fixture = Fixture::new();
    fixture.remote();
    let checkout = fixture.linked("feature");
    git(&checkout, &["push", "origin", &format!("HEAD:{PULL_HEAD}")]);

    let plan = fixture.reclaim_plan();
    assert!(linked_candidate(&plan, &checkout).is_none());
    assert!(checkout.exists());
}

#[test]
fn removes_a_released_landed_checkout_held_by_a_remote_tracking_branch() {
    let fixture = Fixture::new();
    let remote = fixture.remote();
    let checkout = fixture.linked("feature");
    git(&checkout, &["push", "origin", "feature"]);
    let head = fixture.release_landed(&checkout);
    fs::rename(&remote, remote.with_extension("offline")).unwrap();

    let plan = fixture.reclaim_plan();
    let candidate = linked_candidate(&plan, &checkout).unwrap();
    assert_eq!(
        candidate.evidence.last().unwrap(),
        &format!("the remote-tracking branch refs/remotes/origin/feature holds {head}")
    );
    assert!(!checkout.exists());
}

#[test]
fn refuses_settings_without_a_root() {
    let fixture = Fixture::new();
    let git = Git::default();
    let settings = Settings::default();
    assert!(maintain(&settings, &fixture.context(&git)).is_err());
}

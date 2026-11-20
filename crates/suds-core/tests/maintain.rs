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
        fs::write(repo.join(".gitignore"), "target/\n*.local\n").unwrap();
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

    fn add_remote(&self) {
        let root = self.repo.parent().unwrap().parent().unwrap();
        let remote = root.join("remote.git");
        git(root, &["init", "--bare", "remote.git"]);
        git(
            &self.repo,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(&self.repo, &["push", "origin", "main"]);
    }

    fn linked(&self, branch: &str) -> PathBuf {
        let checkout = self.repo.with_file_name(branch);
        git(
            &self.repo,
            &["worktree", "add", "-b", branch, checkout.to_str().unwrap()],
        );
        fs::write(checkout.join("change.txt"), "feature").unwrap();
        git(&checkout, &["add", "change.txt"]);
        git(&checkout, &["commit", "-m", "feature"]);
        fs::create_dir(checkout.join("target")).unwrap();
        fs::write(checkout.join("target/CACHEDIR.TAG"), "Signature: cargo").unwrap();
        fs::write(checkout.join("target/app"), "binary").unwrap();
        checkout
    }

    fn release_landed(&self, checkout: &Path) {
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
    }

    fn landed_checkout(&self) -> PathBuf {
        self.add_remote();
        let checkout = self.linked("feature");
        let tracked_ref = "refs/remotes/origin/pull/42/head";
        git(&checkout, &["push", "origin", &format!("HEAD:{PULL_HEAD}")]);
        git(
            &checkout,
            &["fetch", "origin", &format!("{PULL_HEAD}:{tracked_ref}")],
        );
        self.release_landed(&checkout);
        checkout
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

fn linked_candidate<'a>(plan: &'a ReclaimPlan, checkout: &Path) -> (usize, &'a Candidate) {
    plan.candidates
        .iter()
        .enumerate()
        .find(|(_, candidate)| {
            candidate.subject == Subject::LinkedCheckout && candidate.path.as_path() == checkout
        })
        .unwrap()
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
fn removes_a_landed_checkout_with_tracked_files_and_build_state() {
    let fixture = Fixture::new();
    let checkout = fixture.landed_checkout();
    let head = Git::default().head(&checkout).unwrap();
    let plan = fixture.reclaim_plan();
    let (index, candidate) = linked_candidate(&plan, &checkout);
    assert_eq!(
        candidate.decision,
        Decision::Remove,
        "{:?}",
        candidate.evidence
    );
    assert!(!checkout.exists());
    git(
        &fixture.repo,
        &["show-ref", "--verify", "refs/heads/feature"],
    );
    let proof =
        format!("the remote-tracking reference refs/remotes/origin/pull/42/head holds {head}");
    assert!(candidate.evidence.contains(&proof));
    assert!(
        read(&fixture.directories.journal())
            .unwrap()
            .iter()
            .any(|record| {
                matches!(&record.event, Event::Removed { index: removed, evidence, .. }
            if *removed == index && evidence.contains(&proof))
            })
    );
}

#[test]
fn keeps_a_landed_checkout_with_an_ignored_file() {
    let fixture = Fixture::new();
    let checkout = fixture.landed_checkout();
    let ignored = checkout.join("secrets.local");
    fs::write(&ignored, "local work").unwrap();
    let git = Git::default();
    assert!(git.is_clean(&checkout).unwrap());
    assert_eq!(git.unpushed(&checkout).unwrap(), 0);

    let plan = fixture.reclaim_plan();
    let (_, candidate) = linked_candidate(&plan, &checkout);
    assert_eq!(candidate.kept, Some(Kept::UnaccountedContent));
    assert_eq!(
        candidate.evidence.last(),
        Some(&format!("Suds could not account for {}", ignored.display()))
    );
    assert_eq!(fs::read_to_string(ignored).unwrap(), "local work");
    assert!(!checkout.join("target").exists());
}

#[test]
fn keeps_a_landed_checkout_with_an_ignored_empty_directory() {
    let fixture = Fixture::new();
    let checkout = fixture.landed_checkout();
    let ignored = checkout.join("empty.local");
    fs::create_dir(&ignored).unwrap();
    assert!(Git::default().is_clean(&checkout).unwrap());

    let plan = fixture.reclaim_plan();
    let (_, candidate) = linked_candidate(&plan, &checkout);
    assert_eq!(candidate.kept, Some(Kept::UnaccountedContent));
    assert_eq!(
        candidate.evidence.last(),
        Some(&format!("Suds could not account for {}", ignored.display()))
    );
    assert!(ignored.is_dir());
}

#[test]
fn keeps_a_landed_checkout_with_a_linked_checkout_beneath_it() {
    let fixture = Fixture::new();
    let checkout = fixture.landed_checkout();
    let nested = checkout.join("target/nested");
    git(
        &fixture.repo,
        &["worktree", "add", "--detach", nested.to_str().unwrap()],
    );
    assert!(Git::default().is_clean(&checkout).unwrap());

    let plan = fixture.reclaim_plan();
    let (_, candidate) = linked_candidate(&plan, &checkout);
    assert_eq!(candidate.kept, Some(Kept::HoldsCheckout));
    assert_eq!(
        candidate.evidence.last(),
        Some(&format!(
            "the checkout {} lies beneath it",
            nested.display()
        ))
    );
    assert!(nested.join(".git").is_file());
}

#[test]
fn keeps_a_pushed_checkout_after_an_ordinary_release() {
    let fixture = Fixture::new();
    fixture.add_remote();
    let checkout = fixture.linked("feature");
    git(&checkout, &["push", "origin", "feature"]);
    registry::append(
        &fixture.directories.registry(),
        &checkout,
        "bosun",
        Some("wf-1"),
        RegistryEvent::Released,
        0,
    )
    .unwrap();

    let plan = fixture.reclaim_plan();
    assert_eq!(
        linked_candidate(&plan, &checkout).1.kept,
        Some(Kept::NoLandedRelease)
    );
    assert!(checkout.join("change.txt").exists());
    assert!(!checkout.join("target").exists());
}

#[test]
fn keeps_a_landed_checkout_that_a_second_client_still_holds() {
    let fixture = Fixture::new();
    let checkout = fixture.linked("feature");
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
    assert_eq!(
        linked_candidate(&plan, &checkout).1.kept,
        Some(Kept::Registered)
    );
    let build = plan
        .candidates
        .iter()
        .find(|candidate| candidate.path.as_path() == checkout.join("target"))
        .unwrap();
    assert_eq!(build.kept, Some(Kept::Registered));
    assert!(checkout.join("target/app").exists());
}

#[test]
fn refuses_settings_without_a_root() {
    let fixture = Fixture::new();
    let git = Git::default();
    let settings = Settings::default();
    assert!(maintain(&settings, &fixture.context(&git)).is_err());
}

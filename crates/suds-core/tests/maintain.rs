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
use suds_core::state::{Directories, Settings};
use suds_core::stores::Programs;
use tempfile::TempDir;

const GIB: u64 = 1024 * 1024 * 1024;
/// A time long after the fixture's files changed.
const LATER: i64 = 4_000_000_000;

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
fn refuses_settings_without_a_root() {
    let fixture = Fixture::new();
    let git = Git::default();
    let settings = Settings::default();
    assert!(maintain(&settings, &fixture.context(&git)).is_err());
}

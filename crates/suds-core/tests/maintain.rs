//! Tests of an unattended maintenance run against a temporary repository.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use suds_core::development::{self, Environment};
use suds_core::maintain::{Context, maintain, read_pressure};
use suds_core::organize::journal::{Event, read};
use suds_core::pressure::Level;
use suds_core::reclaim::execute::Authority;
use suds_core::reclaim::git::Git;
use suds_core::reclaim::liveness::Liveness;
use suds_core::reclaim::plan::{
    Candidate, CheckoutScope, Decision, Kept, Observed, Policy, ReclaimPlan, Subject, plan_reclaim,
};
use suds_core::registry::{self, Event as RegistryEvent};
use suds_core::state::{Directories, Settings};
use suds_core::stores::Programs;
use tempfile::TempDir;

mod support;

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
        support::program(&docker, "#!/bin/sh\necho '{}'\n", "755");
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
        // Each test of another rule starts from an ended claim.
        self.end_claim(&checkout);
        checkout
    }

    fn end_claim(&self, checkout: &Path) {
        let registry = self.directories.registry();
        registry::append_ended(&registry, checkout, "session", None, None, 0).unwrap();
    }

    fn release_landed(&self, checkout: &Path) {
        let head = Git::default().head(checkout).unwrap();
        registry::append_ended(
            &self.directories.registry(),
            checkout,
            "bosun",
            Some("wf-1"),
            Some(&head),
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

    /// The plan by the rules of a landed checkout, as it stands before a run
    /// that reclaims the build state.
    fn landed_plan(&self) -> ReclaimPlan {
        let scan = development::inspect(&self.settings(true).roots, &self.environment);
        let registry = registry::reading(&self.directories.registry());
        let observed = Observed {
            liveness: &Liveness::observe(),
            registry: &registry,
        };
        let policy = Policy {
            idle_seconds: 3600,
            checkouts: CheckoutScope::Landed,
            tolerate_unreadable_processes: true,
        };
        let plan = plan_reclaim(&scan, &observed, &Git::default(), policy, LATER);
        self.reclaim_plan();
        plan
    }

    fn reclaim_plan(&self) -> ReclaimPlan {
        let report = maintain(&self.settings(true), &self.context(&Git::default())).unwrap();
        assert_eq!(
            report.reclaim.unwrap().authority,
            Authority::StandingApproval
        );
        self.journaled_plan()
    }

    fn journaled_plan(&self) -> ReclaimPlan {
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
    // The run leaves no staged file beside the reading and its lock.
    let mut state: Vec<_> = fs::read_dir(&fixture.directories.state)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    state.sort();
    assert_eq!(state, ["pressure.json", "pressure.maintain.lock"]);
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

/// The rules of a landed checkout admit it, and a run that no person approved
/// still names no checkout.
#[test]
fn an_unattended_run_names_no_checkout() {
    let fixture = Fixture::new();
    let checkout = fixture.landed_checkout();
    let head = Git::default().head(&checkout).unwrap();
    let admitted = fixture.landed_plan();
    let (_, candidate) = linked_candidate(&admitted, &checkout);
    assert_eq!(
        candidate.decision,
        Decision::Remove,
        "{:?}",
        candidate.evidence
    );
    let proof =
        format!("the remote-tracking reference refs/remotes/origin/pull/42/head holds {head}");
    assert!(candidate.evidence.contains(&proof));

    let plan = fixture.journaled_plan();
    assert!(
        plan.candidates
            .iter()
            .all(|candidate| candidate.subject != Subject::LinkedCheckout)
    );
    assert!(checkout.join("change.txt").exists());
    assert!(!checkout.join("target").exists());
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

    let plan = fixture.landed_plan();
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

    let plan = fixture.landed_plan();
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

    let plan = fixture.landed_plan();
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

    let plan = fixture.landed_plan();
    assert_eq!(
        linked_candidate(&plan, &checkout).1.kept,
        Some(Kept::Claimed)
    );
    assert!(checkout.join("change.txt").exists());
    assert!(checkout.join("target/app").exists());
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

    let plan = fixture.landed_plan();
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

/// Each path beneath `root` apart from a repository's own data, where Git
/// records what a removal changes.
fn paths(root: &Path) -> BTreeSet<PathBuf> {
    let mut found = BTreeSet::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory).unwrap() {
            let path = entry.unwrap().path();
            if path.file_name().is_some_and(|name| name == ".git") {
                continue;
            }
            if fs::symlink_metadata(&path).unwrap().is_dir() {
                pending.push(path.clone());
            }
            found.insert(path);
        }
    }
    found
}

/// An unattended run over a workstation in small. The test compares every
/// path before and after the run, so a removal that no rule names fails it.
#[test]
fn an_unattended_run_removes_exactly_what_its_rules_name() {
    let mut fixture = Fixture::new();
    let root = fs::canonicalize(fixture._dir.path()).unwrap();
    let cache = root.join("go-build");
    for n in 0..256 {
        fs::create_dir_all(cache.join(format!("{n:02x}"))).unwrap();
    }
    fs::write(cache.join("README"), "cache").unwrap();
    fs::write(cache.join("trim.txt"), "0").unwrap();
    fs::create_dir(cache.join("fuzz")).unwrap();
    fs::write(cache.join("fuzz/corpus"), "kept").unwrap();
    let entry = |first: &str| {
        let path = cache
            .join(first)
            .join(format!("{first}{}-d", "b".repeat(62)));
        fs::write(&path, vec![1u8; 65536]).unwrap();
        path
    };
    let stale = entry("0a");
    let recent = entry("0b");
    let used = SystemTime::UNIX_EPOCH + Duration::from_secs(LATER as u64 - 60);
    fs::File::open(&recent).unwrap().set_modified(used).unwrap();
    fixture
        .environment
        .variables
        .insert("GOCACHE".to_owned(), cache.clone().into());

    let landed = fixture.landed_checkout();
    // Work that only this workstation holds, and a checkout that a client holds.
    let unpushed = fixture.linked("unpushed");
    // Each linked checkout commits the same content, and an equal commit
    // has the pushed commit's identity, so this one needs its own.
    fs::write(unpushed.join("only.txt"), "only here").unwrap();
    git(&unpushed, &["add", "only.txt"]);
    git(&unpushed, &["commit", "-m", "only here"]);
    fixture.release_landed(&unpushed);
    let changed = fixture.linked("changed");
    fs::write(changed.join("change.txt"), "edited").unwrap();
    fixture.release_landed(&changed);
    let held = fixture.linked("held");
    registry::append(
        &fixture.directories.registry(),
        &held,
        "bosun",
        Some("wf-2"),
        RegistryEvent::Registered,
        0,
    )
    .unwrap();

    let mut settings = fixture.settings(true);
    settings.budgets.insert("go.build-cache".to_owned(), 0);
    let before = paths(&root);
    let report = maintain(&settings, &fixture.context(&Git::default())).unwrap();
    let after = paths(&root);

    let trims = report.stores.unwrap().trims;
    assert_eq!(trims.len(), 1, "{trims:?}");
    assert_eq!(trims[0].failure, None);
    let beneath = |tree: &Path| {
        before
            .iter()
            .filter(|path| path.starts_with(tree))
            .cloned()
            .collect()
    };
    let mut expected: BTreeSet<PathBuf> = BTreeSet::new();
    for target in [&fixture.repo, &landed, &unpushed, &changed] {
        let gone: BTreeSet<PathBuf> = beneath(&target.join("target"));
        expected.extend(gone);
    }
    expected.insert(stale);
    let removed: BTreeSet<PathBuf> = before.difference(&after).cloned().collect();
    assert_eq!(removed, expected);
    let state = &fixture.directories.state;
    let added: Vec<_> = after
        .difference(&before)
        .filter(|path| !path.starts_with(state))
        .collect();
    assert!(added.is_empty(), "{added:?}");
    assert!(held.join("target/app").exists() && recent.exists());
    assert_eq!(
        fs::read_to_string(changed.join("change.txt")).unwrap(),
        "edited"
    );

    // A second run finds nothing more to remove.
    maintain(&settings, &fixture.context(&Git::default())).unwrap();
    assert_eq!(paths(&root), after);
}

/// A build that rewrites one file of a checkout's build state until a file
/// appears. It fails as soon as the build state loses an entry.
struct RunningBuild {
    child: std::process::Child,
    stop: PathBuf,
}

impl RunningBuild {
    fn start(target: &Path, directory: &Path, stop: PathBuf) -> Self {
        // A write to a file that exists leaves its directory's time alone, so
        // the file's own time carries the recent write.
        let script = "i=0; while [ ! -e \"$2\" ]; do i=$((i+1)); \
                      [ -e \"$1/app\" ] && [ -e \"$1/CACHEDIR.TAG\" ] || exit 8; \
                      echo $i > \"$1/app\" || exit 7; \
                      [ $i -eq 3 ] && : > \"$2.began\"; sleep 0.02; done";
        let child = Command::new("sh")
            .args(["-c", script, "build"])
            .arg(target)
            .arg(&stop)
            .current_dir(directory)
            .spawn()
            .unwrap();
        let began = std::time::Instant::now();
        while !stop.with_extension("began").exists() {
            assert!(began.elapsed() < Duration::from_secs(20), "no build began");
            std::thread::sleep(Duration::from_millis(10));
        }
        RunningBuild { child, stop }
    }

    fn finish(mut self) -> Option<i32> {
        fs::write(&self.stop, "").unwrap();
        self.child.wait().unwrap().code()
    }
}

impl Drop for RunningBuild {
    /// A failed test leaves no build behind.
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Sets every entry beneath `tree` to a time `hours` before now.
fn age(tree: &Path, hours: u64) {
    let time = SystemTime::now() - Duration::from_secs(hours * 3600);
    for path in paths(tree).iter().rev() {
        fs::File::open(path).unwrap().set_modified(time).unwrap();
    }
    fs::File::open(tree).unwrap().set_modified(time).unwrap();
}

/// Each protection alone spares a running build. Under the far time no write
/// is recent, so only the process's working directory keeps the first build.
/// The second build works elsewhere, so only its recent writes keep it.
#[test]
fn an_unattended_run_spares_a_running_build() {
    for inside in [true, false] {
        let fixture = Fixture::new();
        let root = fs::canonicalize(fixture._dir.path()).unwrap();
        let building = fixture.linked("building");
        let idle = fixture.linked("idle");
        for checkout in [&building, &idle, &fixture.repo] {
            age(&checkout.join("target"), 5);
        }
        let target = building.join("target");
        let directory = if inside {
            building.as_path()
        } else {
            root.as_path()
        };
        let build = RunningBuild::start(&target, directory, root.join("stop"));
        // A change within the plan's own second counts as later.
        std::thread::sleep(Duration::from_millis(1100));
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap();
        let now = if inside {
            LATER
        } else {
            i64::try_from(now.as_secs()).unwrap()
        };
        let git = Git::default();
        let context = Context {
            now,
            ..fixture.context(&git)
        };
        maintain(&fixture.settings(true), &context).unwrap();
        // A build that stops right after a removal would not notice it.
        let survived = ["CACHEDIR.TAG", "app"].map(|name| target.join(name).exists());
        let code = build.finish();
        assert_eq!(survived, [true; 2], "inside: {inside}");
        assert_eq!(code, Some(0), "inside: {inside}");
        // The plan states the rule, so no later refusal stands in for it.
        let kept = if inside {
            Kept::InUse
        } else {
            Kept::RecentlyModified
        };
        let plan = fixture.journaled_plan();
        let candidate = plan
            .candidates
            .iter()
            .find(|candidate| candidate.path.as_path() == target)
            .unwrap();
        assert_eq!(candidate.kept, Some(kept), "{candidate:?}");
        // The same run removes the build state that nothing uses.
        assert!(!idle.join("target").exists(), "inside: {inside}");
        assert!(!fixture.repo.join("target").exists(), "inside: {inside}");
    }
}

#[test]
fn publishes_post_maintenance_pressure_recovery_and_headroom() {
    let fixture = Fixture::new();
    // Seed a large build artifact in the main checkout's target directory so
    // initial pressure hits Hard against a tight ceiling, then drops after
    // maintain removes target/.
    let payload = vec![b'x'; 256 * 1024];
    fs::write(fixture.repo.join("target/big.bin"), &payload).unwrap();
    let mut settings = fixture.settings(true);
    settings.ceiling_bytes = Some(200 * 1024);

    let report = maintain(&settings, &fixture.context(&Git::default())).unwrap();
    assert!(!fixture.repo.join("target").exists());
    assert!(report.filesystem_available_before_bytes.is_some());
    assert!(report.filesystem_available_after_bytes.is_some());
    // Post-maintenance measurement reflects the reduced usage and starts the
    // transition window while preserving the 5-minute recovery delay.
    assert!(
        report.pressure.usage_bytes < 200 * 1024,
        "expected post-cleanup usage below ceiling, got {}",
        report.pressure.usage_bytes
    );
    assert_eq!(report.pressure.level, Level::Hard);
    assert!(
        report.pressure.lower_since.is_some(),
        "expected recovery transition window to start immediately after cleanup"
    );
    let saved = read_pressure(&fixture.directories.pressure()).unwrap();
    assert_eq!(saved, report.pressure);
}

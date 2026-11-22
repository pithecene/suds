//! Tests of seed plans and runs against temporary repositories that Git
//! builds. The repositories lie beneath the build's own temporary directory,
//! whose filesystem may clone a file or may not. Each test of a run holds
//! for both: a filesystem that clones no file must leave nothing behind.

use std::fs::{self, File};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use suds_core::organize::journal::{Journal, JournalState, RunKind, RunStatus};
use suds_core::reclaim::git::Git;
use suds_core::seed::{SeedPlan, SeedReport, apply, plan_seed};
use tempfile::TempDir;

mod support;
use support::git;

fn write(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

/// Writes Cargo's build state into a checkout: the tag, a profile's lock,
/// an artifact with a second name, and a link.
fn build(checkout: &Path, artifact: &str) {
    let target = checkout.join("target");
    write(&target.join("CACHEDIR.TAG"), "Signature: cargo");
    write(&target.join("debug/.cargo-lock"), "");
    write(&target.join("debug/deps/app-1a2b"), artifact);
    fs::hard_link(target.join("debug/deps/app-1a2b"), target.join("debug/app")).unwrap();
    std::os::unix::fs::symlink("deps/app-1a2b", target.join("debug/latest")).unwrap();
}

/// A repository with a main checkout and the linked checkouts `built`,
/// `other`, and `fresh`. Git ignores each `target` directory.
struct Fixture {
    _dir: TempDir,
    root: PathBuf,
    journal: PathBuf,
}

impl Fixture {
    fn new() -> Fixture {
        let dir = TempDir::new_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let root = dir.path().canonicalize().unwrap();
        let repo = root.join("repo");
        fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "--quiet"]);
        write(&repo.join("Cargo.toml"), "[package]\nname = \"app\"\n");
        write(&repo.join("Cargo.lock"), "version = 4\n");
        write(&repo.join(".gitignore"), "target\n");
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "--quiet", "-m", "start"]);
        for name in ["built", "other", "fresh"] {
            let path = root.join(name);
            git(
                &repo,
                &[
                    "worktree",
                    "add",
                    "--quiet",
                    "-b",
                    name,
                    path.to_str().unwrap(),
                ],
            );
        }
        Fixture {
            journal: root.join("journal"),
            root,
            _dir: dir,
        }
    }

    fn checkout(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    fn plan(&self, from: Option<&str>) -> SeedPlan {
        let from = from.map(|name| self.checkout(name));
        plan_seed(&self.checkout("fresh"), from.as_deref(), &Git::default()).unwrap()
    }

    fn apply(&self, plan: &SeedPlan) -> SeedReport {
        let mut journal = Journal::open(&self.journal).unwrap();
        apply(&mut journal, plan, Some("bosun")).unwrap()
    }
}

#[test]
fn a_plan_takes_the_build_state_of_a_built_checkout_and_changes_nothing() {
    let fixture = Fixture::new();
    build(&fixture.checkout("built"), "binary");

    let plan = fixture.plan(None);

    assert_eq!(
        plan.source.as_ref().unwrap().as_path(),
        fixture.checkout("built")
    );
    assert_eq!(plan.clones.len(), 1);
    assert_eq!(
        plan.clones[0].to.as_path(),
        fixture.checkout("fresh").join("target")
    );
    assert!(!fixture.checkout("fresh").join("target").exists());
    assert!(!fixture.journal.exists());
}

#[test]
fn a_run_clones_the_build_state_or_leaves_nothing() {
    let fixture = Fixture::new();
    build(&fixture.checkout("built"), "binary");
    let source = fixture.checkout("built").join("target");
    let target = fixture.checkout("fresh").join("target");

    let report = fixture.apply(&fixture.plan(None));

    let summaries =
        JournalState::from_records(Journal::open(&fixture.journal).unwrap().records()).summaries();
    assert_eq!(summaries[0].kind, RunKind::Seed);
    assert_eq!(summaries[0].units.len(), 1);
    let Some(refusal) = &report.clones[0].refusal else {
        assert_eq!(report.status, RunStatus::Complete);
        assert_eq!(
            fs::read_to_string(target.join("debug/deps/app-1a2b")).unwrap(),
            "binary"
        );
        // The clone keeps the time of each file, which Cargo compares.
        let (old, new) = (
            fs::metadata(source.join("debug/deps/app-1a2b")).unwrap(),
            fs::metadata(target.join("debug/deps/app-1a2b")).unwrap(),
        );
        assert_eq!(
            (old.mtime(), old.mtime_nsec()),
            (new.mtime(), new.mtime_nsec())
        );
        assert_ne!(old.ino(), new.ino());
        // The artifact's two names stay on one file.
        assert_eq!(
            new.ino(),
            fs::metadata(target.join("debug/app")).unwrap().ino()
        );
        assert_eq!(
            fs::read_link(target.join("debug/latest")).unwrap(),
            Path::new("deps/app-1a2b")
        );
        assert_eq!(report.clones[0].files, 3);
        assert!(summaries[0].units[0].reason.is_none());
        return;
    };
    // A filesystem that clones no file takes no copy, and the run leaves
    // neither the directory nor a part of it.
    assert!(refusal.contains("clones no file"), "{refusal}");
    assert_eq!(report.status, RunStatus::Refused);
    assert!(!target.exists());
    let left: Vec<_> = fs::read_dir(fixture.checkout("fresh"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name.to_string_lossy().starts_with(".suds-seed"))
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

#[test]
fn a_plan_changes_no_existing_build_state() {
    let fixture = Fixture::new();
    build(&fixture.checkout("built"), "binary");
    build(&fixture.checkout("fresh"), "its own");

    let plan = fixture.plan(None);

    assert!(plan.source.is_none());
    assert!(plan.clones.is_empty());
    assert!(
        plan.passed[0].reason.contains("exists"),
        "{:?}",
        plan.passed
    );
    let report = fixture.apply(&plan);
    assert!(report.clones.is_empty());
    let kept = fixture.checkout("fresh").join("target/debug/deps/app-1a2b");
    assert_eq!(fs::read_to_string(kept).unwrap(), "its own");
}

#[test]
fn a_plan_passes_a_build_state_that_a_build_holds() {
    let fixture = Fixture::new();
    build(&fixture.checkout("built"), "binary");
    let lock = File::open(fixture.checkout("built").join("target/debug/.cargo-lock")).unwrap();
    lock.lock().unwrap();

    let plan = fixture.plan(None);

    assert!(plan.clones.is_empty());
    assert!(
        plan.passed[0].reason.contains("a build holds the lock"),
        "{:?}",
        plan.passed
    );
    drop(lock);
    assert_eq!(fixture.plan(None).clones.len(), 1);
}

#[test]
fn a_plan_prefers_the_checkout_with_the_destination_s_lock_file() {
    let fixture = Fixture::new();
    // The other checkout holds more build state, for other dependencies.
    build(
        &fixture.checkout("other"),
        "a much larger binary of another version",
    );
    write(
        &fixture.checkout("other").join("target/debug/deps/more"),
        &"x".repeat(1 << 16),
    );
    write(
        &fixture.checkout("other").join("Cargo.lock"),
        "version = 4\n# other\n",
    );
    build(&fixture.checkout("built"), "binary");

    let plan = fixture.plan(None);

    assert_eq!(
        plan.source.as_ref().unwrap().as_path(),
        fixture.checkout("built")
    );
    let passed: Vec<_> = plan
        .passed
        .iter()
        .map(|passed| passed.path.as_path())
        .collect();
    assert_eq!(passed, [fixture.checkout("other")]);
    // A named checkout decides the source.
    let named = fixture.plan(Some("other"));
    assert_eq!(
        named.source.as_ref().unwrap().as_path(),
        fixture.checkout("other")
    );
}

#[test]
fn a_plan_refuses_a_directory_that_is_no_checkout_and_a_source_of_another_repository() {
    let fixture = Fixture::new();
    let plain = fixture.root.join("plain");
    fs::create_dir_all(&plain).unwrap();

    let refused = plan_seed(&plain, None, &Git::default()).unwrap_err();
    assert!(refused.contains("holds no checkout"), "{refused}");

    let foreign = plan_seed(&fixture.checkout("fresh"), Some(&plain), &Git::default()).unwrap_err();
    assert!(
        foreign.contains("is no checkout of the repository"),
        "{foreign}"
    );
}

#[test]
fn a_run_passes_a_directory_whose_condition_changed_after_the_plan() {
    let fixture = Fixture::new();
    build(&fixture.checkout("built"), "binary");
    let plan = fixture.plan(None);
    build(&fixture.checkout("fresh"), "its own");

    let report = fixture.apply(&plan);

    assert_eq!(report.status, RunStatus::Refused);
    assert!(
        report.clones[0]
            .refusal
            .as_ref()
            .unwrap()
            .contains("exists")
    );
    let kept = fixture.checkout("fresh").join("target/debug/deps/app-1a2b");
    assert_eq!(fs::read_to_string(kept).unwrap(), "its own");
}

#[test]
fn a_plan_follows_no_link_on_the_way_to_the_destination() {
    let fixture = Fixture::new();
    // Each checkout holds a package in `pkg`, and the built one's package
    // holds build state.
    for name in ["built", "fresh"] {
        write(
            &fixture.checkout(name).join("pkg/Cargo.toml"),
            "[package]\n",
        );
    }
    let built = fixture.checkout("built").join("pkg");
    write(&built.join("target/CACHEDIR.TAG"), "Signature: cargo");
    write(&built.join("target/debug/app"), "binary");
    // The destination's `pkg` becomes a link to a directory outside it.
    let outside = fixture.root.join("outside");
    write(&outside.join("Cargo.toml"), "[package]\n");
    let link = fixture.checkout("fresh").join("pkg");
    fs::remove_dir_all(&link).unwrap();
    std::os::unix::fs::symlink(&outside, &link).unwrap();

    let plan = fixture.plan(None);

    assert!(plan.clones.is_empty(), "{:?}", plan.clones);
    assert!(
        plan.passed
            .iter()
            .any(|passed| passed.reason.contains("follows no link")),
        "{:?}",
        plan.passed
    );
    assert!(!outside.join("target").exists());
}

#[test]
fn a_plan_passes_a_build_state_that_holds_a_checkout() {
    let fixture = Fixture::new();
    build(&fixture.checkout("built"), "binary");
    let nested = fixture.checkout("built").join("target/vendor/dep");
    fs::create_dir_all(&nested).unwrap();
    git(&nested, &["init", "--quiet"]);

    let plan = fixture.plan(None);

    assert!(plan.clones.is_empty(), "{:?}", plan.clones);
    assert!(
        plan.passed
            .iter()
            .any(|passed| passed.reason.contains("holds 1 checkouts")),
        "{:?}",
        plan.passed
    );
}

#[test]
fn a_plan_names_what_it_could_not_read() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    build(&fixture.checkout("built"), "binary");
    let closed = fixture.checkout("built").join("closed");
    fs::create_dir_all(&closed).unwrap();
    fs::set_permissions(&closed, fs::Permissions::from_mode(0o000)).unwrap();
    // The administrator reads every directory, so the test proves nothing there.
    let unreadable = fs::read_dir(&closed).is_err();

    let plan = fixture.plan(None);
    fs::set_permissions(&closed, fs::Permissions::from_mode(0o755)).unwrap();

    if unreadable {
        assert!(
            plan.passed
                .iter()
                .any(|passed| passed.reason.contains("could not read 1 entries")),
            "{:?}",
            plan.passed
        );
    }
}

#[test]
fn a_clone_keeps_the_permissions_of_each_directory() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    build(&fixture.checkout("built"), "binary");
    let source = fixture.checkout("built").join("target");
    fs::set_permissions(source.join("debug"), fs::Permissions::from_mode(0o700)).unwrap();

    let report = fixture.apply(&fixture.plan(None));

    if report.clones[0].cloned {
        let target = fixture.checkout("fresh").join("target");
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&target.join("debug")), 0o700);
        assert_eq!(mode(&target), mode(&source));
    }
}

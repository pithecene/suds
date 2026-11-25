//! A reclaim plan's arrangement and its run, against a workstation that Git builds.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use suds_core::development::Environment;
use suds_core::organize::journal::{Event, read};
use suds_core::reclaim::plan::{Decision, Kept, Subject};
use suds_core::registry;
use suds_core::state::Directories;
use suds_view::{IDLE_SECONDS, Proposal, apply, propose};
use tempfile::TempDir;

fn git(directory: &Path, arguments: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(["-c", "user.name=Suds"])
        .args(["-c", "user.email=suds@example.invalid"])
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        // Git's own maintenance removes files in the background after a
        // commit, which a scan in progress would report as vanished.
        .args(["-c", "gc.auto=0", "-c", "maintenance.auto=false"])
        .args(arguments)
        .output()
        .expect("run git");
    let errors = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "git {arguments:?}: {errors}");
}

fn write(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

/// A home directory whose settings name one root. The root holds a main
/// checkout and a linked checkout, a remote holds the branch of each, and
/// each holds Cargo's build output.
struct Workstation {
    _temp: TempDir,
    environment: Environment,
    repo: PathBuf,
    linked: PathBuf,
}

impl Workstation {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let home = temp.path().canonicalize().unwrap();
        let environment = Environment {
            home: Some(home.clone()),
            variables: HashMap::new(),
        };
        let root = home.join("workspace");
        let repo = root.join("repo");
        let linked = repo.join(".worktrees/feature");
        fs::create_dir_all(&repo).unwrap();
        let remote = home.join("remote.git");
        let (remote, branch) = (remote.to_str().unwrap(), linked.to_str().unwrap());
        write(&repo.join("Cargo.toml"), "[package]");
        write(&repo.join(".gitignore"), "target/\n.worktrees/\n");
        // A remote holds the main branch before the linked checkout leaves it.
        let first: [(&Path, &[&str]); 7] = [
            (&home, &["init", "--bare", "remote.git"]),
            (&repo, &["init"]),
            (&repo, &["add", "."]),
            (&repo, &["commit", "-m", "first"]),
            (&repo, &["remote", "add", "origin", remote]),
            (&repo, &["push", "origin", "main"]),
            (&repo, &["worktree", "add", "-b", "feature", branch]),
        ];
        first.iter().for_each(|(at, arguments)| git(at, arguments));
        write(&linked.join("src/lib.rs"), "pub fn f() {}");
        for arguments in [
            &["add", "."][..],
            &["commit", "-m", "feature"],
            &["push", "origin", "feature"],
        ] {
            git(&linked, arguments);
        }
        for checkout in [&repo, &linked] {
            write(&checkout.join("target/CACHEDIR.TAG"), "Signature: cargo");
            write(&checkout.join("target/debug/app"), "binary");
        }
        let settings = Directories::of(&environment).unwrap().settings();
        write(
            &settings,
            &serde_json::json!({ "roots": [root] }).to_string(),
        );
        Workstation {
            _temp: temp,
            environment,
            repo,
            linked,
        }
    }

    fn directories(&self) -> Directories {
        Directories::of(&self.environment).unwrap()
    }

    /// Records that the linked checkout's client released it.
    fn release(&self) {
        let registry = self.directories().registry();
        registry::append_ended(&registry, &self.linked, "bosun", None, None, 0).unwrap();
    }

    /// Plans as if the idle time and one more hour had passed since the
    /// files changed, so no file counts as recent. The plan tolerates a
    /// hidden process, which the machine of a test may run.
    fn propose(&self) -> Proposal {
        propose(&self.environment, true, later()).unwrap()
    }

    fn position(&self, proposal: &Proposal, path: &Path) -> usize {
        let entries = proposal.removals.iter();
        let mut found = entries.filter(|entry| Path::new(&entry.path) == path);
        found.next().expect("the plan removes the path").candidate
    }
}

fn later() -> i64 {
    let since = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    i64::try_from(since.as_secs() + 2 * IDLE_SECONDS).unwrap()
}

#[test]
fn a_proposal_lists_what_the_plan_removes_and_why_it_keeps_the_rest() {
    let workstation = Workstation::new();
    let proposal = workstation.propose();

    // Nobody ended the claim on the linked checkout, so it keeps its build state.
    let paths: BTreeSet<&str> = proposal.removals.iter().map(|e| e.path.as_str()).collect();
    let build = |checkout: &Path| checkout.join("target").to_string_lossy().into_owned();
    let expected = [build(&workstation.repo)];
    assert_eq!(paths, expected.iter().map(String::as_str).collect());
    for entry in &proposal.removals {
        assert!(matches!(entry.subject, Subject::BuildState { .. }));
        // The remote names the project, as it names one in the outline.
        assert!(entry.project.ends_with("/remote"), "{}", entry.project);
        assert_eq!(entry.within, None);
        assert!(!entry.evidence.is_empty());
    }
    let sizes: Vec<u64> = proposal.removals.iter().map(|entry| entry.bytes).collect();
    assert!(sizes.windows(2).all(|pair| pair[0] >= pair[1]), "{sizes:?}");
    assert_eq!(proposal.kept.len(), 1);
    assert_eq!(proposal.kept[0].reason, Kept::Claimed);
    assert_eq!(proposal.kept[0].count, 1);

    assert!(proposal.complete());

    let all = proposal.removing(&BTreeSet::new());
    assert_eq!(all, (1, proposal.plan.removable_bytes()));
}

#[test]
fn a_recent_change_keeps_a_directory_and_the_proposal_counts_it() {
    let workstation = Workstation::new();
    workstation.release();
    let now = later() - 2 * i64::try_from(IDLE_SECONDS).unwrap();
    let proposal = propose(&workstation.environment, true, now).unwrap();

    assert_eq!(proposal.removals, []);
    assert_eq!(proposal.kept.len(), 1);
    assert_eq!(proposal.kept[0].reason, Kept::RecentlyModified);
    assert_eq!(proposal.kept[0].count, 3);
}

#[test]
fn a_checkout_that_stays_keeps_its_build_state() {
    let workstation = Workstation::new();
    workstation.release();
    let proposal = workstation.propose();
    let checkout = workstation.position(&proposal, &workstation.linked);
    let inner = workstation.position(&proposal, &workstation.linked.join("target"));
    let other = workstation.position(&proposal, &workstation.repo.join("target"));
    let entry = |candidate| {
        let mut entries = proposal.removals.iter();
        entries.find(|entry| entry.candidate == candidate).unwrap()
    };
    assert_eq!(entry(inner).within, Some(checkout));
    assert_eq!(entry(checkout).within, None);
    assert_eq!(entry(other).within, None);

    // Leaving the build state out leaves its checkout out.
    let mut excluded = BTreeSet::new();
    proposal.toggle(&mut excluded, inner);
    assert_eq!(excluded, BTreeSet::from([inner, checkout]));
    assert_eq!(proposal.removing(&excluded).0, 1);
    // Taking the checkout in takes its build state in.
    proposal.toggle(&mut excluded, checkout);
    assert_eq!(excluded, BTreeSet::new());
    // A checkout alone leaves the run, and its build state may still go.
    proposal.toggle(&mut excluded, checkout);
    assert_eq!(excluded, BTreeSet::from([checkout]));

    let chosen = proposal.chosen(&excluded);
    assert_eq!(chosen.id, proposal.plan.id);
    assert_eq!(chosen.candidates.len(), proposal.plan.candidates.len() - 1);
    let removed = |candidate: &&suds_core::reclaim::plan::Candidate| {
        candidate.decision == Decision::Remove && candidate.path.as_path() == workstation.linked
    };
    assert_eq!(chosen.candidates.iter().filter(removed).count(), 0);
}

#[test]
fn an_approved_run_removes_the_chosen_entries_and_the_journal_records_it() {
    let workstation = Workstation::new();
    workstation.release();
    let proposal = workstation.propose();
    let stays = workstation.position(&proposal, &workstation.repo.join("target"));
    let mut excluded = BTreeSet::new();
    proposal.toggle(&mut excluded, stays);
    let checkout = workstation.position(&proposal, &workstation.linked);
    proposal.toggle(&mut excluded, checkout);

    let plan = proposal.chosen(&excluded);
    let report = apply(&workstation.environment, &plan, later()).unwrap();

    assert_eq!(report.removals.len(), 1);
    assert!(report.removals[0].removed, "{:?}", report.removals[0]);
    assert!(!workstation.linked.join("target").exists());
    assert!(workstation.repo.join("target/debug/app").exists());
    assert!(workstation.linked.join("src/lib.rs").exists());

    let records = read(&workstation.directories().journal()).unwrap();
    let removed = |record: &&suds_core::organize::journal::Record| {
        matches!(record.event, Event::Removed { .. })
    };
    assert_eq!(records.iter().filter(removed).count(), 1);
    assert!(matches!(records[0].event, Event::ReclaimPlanned { .. }));
}

#[cfg(unix)]
#[test]
fn a_proposal_states_what_the_inspection_missed() {
    use std::os::unix::fs::PermissionsExt;

    let workstation = Workstation::new();
    workstation.release();
    let root = workstation.repo.parent().unwrap();
    // No directory holds the second root, and the first holds a directory that refuses a reading.
    let settings = workstation.directories().settings();
    let roots = [root.to_owned(), root.with_file_name("absent")];
    write(
        &settings,
        &serde_json::json!({ "roots": roots }).to_string(),
    );
    let closed = root.join("closed");
    fs::create_dir_all(&closed).unwrap();
    fs::set_permissions(&closed, fs::Permissions::from_mode(0o000)).unwrap();
    let refused = fs::read_dir(&closed).is_err();

    let proposal = workstation.propose();
    fs::set_permissions(&closed, fs::Permissions::from_mode(0o755)).unwrap();

    assert_eq!(proposal.unmeasured, 1);
    // A superuser reads the closed directory, and the inspection then misses no entry.
    assert_eq!(proposal.unread > 0, refused);
    assert!(!proposal.complete());
    assert_eq!(proposal.removals.len(), 3);
}

// The file systems of macOS refuse a name that no text encodes.
#[cfg(target_os = "linux")]
#[test]
fn a_repository_whose_path_is_no_text_keeps_its_own_project() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let workstation = Workstation::new();
    // The directory's name holds a byte that no text encodes.
    let root = workstation.repo.parent().unwrap();
    let odd = root.join(OsStr::from_bytes(b"odd\xff"));
    fs::create_dir_all(&odd).unwrap();
    git(&odd, &["init"]);
    write(&odd.join(".gitignore"), "target/\n");
    git(
        &odd,
        &[
            "remote",
            "add",
            "origin",
            "https://example.invalid/team/odd.git",
        ],
    );
    write(&odd.join("Cargo.toml"), "[package]");
    write(&odd.join("target/CACHEDIR.TAG"), "Signature: cargo");

    let proposal = workstation.propose();
    let build = odd.join("target").to_string_lossy().into_owned();
    let mut entries = proposal.removals.iter();
    let entry = entries.find(|entry| entry.path == build);
    assert_eq!(
        entry.expect("the plan removes the path").project,
        "team/odd"
    );
}

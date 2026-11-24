//! Command-line tests of `suds reclaim` in a temporary workspace with a home
//! directory and a journal of its own.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, SystemTime};

use tempfile::TempDir;

struct Fixture {
    _dir: TempDir,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        fs::create_dir_all(root.join("home")).unwrap();
        Fixture { _dir: dir, root }
    }

    /// A repository with idle build output, and the path of that output.
    fn project(&self, name: &str) -> PathBuf {
        let project = self.root.join("workspace").join(name);
        let target = project.join("target");
        fs::create_dir_all(&target).unwrap();
        fs::write(project.join("Cargo.toml"), "[package]").unwrap();
        fs::write(project.join(".gitignore"), "target/\n").unwrap();
        fs::write(target.join("CACHEDIR.TAG"), "Signature: cargo").unwrap();
        fs::write(target.join("app"), "binary").unwrap();
        for arguments in [&["init", "-q"][..], &["add", "."]] {
            let status = Command::new("git")
                .current_dir(&project)
                .args(arguments)
                .status();
            assert!(status.unwrap().success());
        }
        age(&target, 5 * 3600);
        target
    }

    fn suds(&self, arguments: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_suds"))
            .arg("reclaim")
            .args(arguments)
            .arg("--journal")
            .arg(self.root.join("journal"))
            .current_dir(&self.root)
            .env("HOME", self.root.join("home"))
            .env_remove("XDG_STATE_HOME")
            .env_remove("XDG_CONFIG_HOME")
            .output()
            .unwrap()
    }

    fn plan(&self, idle_minutes: &str) -> Output {
        self.suds(&[
            "--root",
            "workspace",
            "--idle-minutes",
            idle_minutes,
            "--tolerate-unreadable-processes",
            "--save",
        ])
    }

    /// Saves a plan whose time lies after each change of the fixture.
    fn saved(&self, idle_minutes: &str) -> String {
        std::thread::sleep(Duration::from_millis(1100));
        let planned = self.plan(idle_minutes);
        assert!(planned.status.success(), "{}", text(&planned.stderr));
        let shown = text(&planned.stdout);
        let (_, id) = shown.rsplit_once("suds reclaim --apply ").unwrap();
        id.trim().to_owned()
    }
}

/// Sets a directory and each file in it to a time before now.
fn age(directory: &Path, seconds: u64) {
    let time = SystemTime::now() - Duration::from_secs(seconds);
    let entries = fs::read_dir(directory).unwrap().map(|e| e.unwrap().path());
    for path in entries.chain([directory.to_path_buf()]) {
        fs::File::open(path).unwrap().set_modified(time).unwrap();
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn applies_the_saved_plan_and_nothing_else() {
    let fixture = Fixture::new();
    let named = fixture.project("named");
    let id = fixture.saved("60");
    assert!(named.exists());

    // Another project becomes removable after the plan, and a later plan
    // names it.
    let later = fixture.project("later");
    fixture.saved("60");
    let applied = fixture.suds(&["--apply", &id]);
    assert!(applied.status.success(), "{}", text(&applied.stderr));
    assert!(!named.exists());
    assert!(named.with_file_name("Cargo.toml").exists());
    assert!(later.join("app").exists());
}

#[test]
fn keeps_a_file_that_a_restore_replaced_with_its_earlier_time() {
    let fixture = Fixture::new();
    let target = fixture.project("project");
    let id = fixture.saved("60");
    fs::write(target.join("app"), "restored").unwrap();
    age(&target, 5 * 3600);
    let applied = fixture.suds(&["--apply", &id]);
    assert!(!applied.status.success());
    assert_eq!(fs::read_to_string(target.join("app")).unwrap(), "restored");
}

#[test]
fn builds_no_plan_at_an_apply() {
    let fixture = Fixture::new();
    let target = fixture.project("project");
    let id = fixture.saved("60");
    let cases: [&[&str]; 4] = [
        // The form of earlier releases, which applied a plan that nobody read.
        &["--root", "workspace", "--apply"],
        &["--root", "workspace", "--apply", &id],
        &["--apply", "reclaim-absent"],
        &[],
    ];
    for arguments in cases {
        assert!(!fixture.suds(arguments).status.success(), "{arguments:?}");
    }
    assert!(target.join("app").exists());
}

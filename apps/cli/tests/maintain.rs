//! Command-line tests of `suds maintain` under a durable pause, in a temporary
//! workspace with a home directory of its own.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output};

use tempfile::TempDir;

struct Fixture {
    _dir: TempDir,
    root: PathBuf,
}

impl Fixture {
    /// A home directory whose settings name one root that holds one file, with no standing approval.
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let workspace = root.join("workspace");
        fs::create_dir_all(workspace.join("project")).unwrap();
        fs::write(workspace.join("project/file.txt"), "data").unwrap();
        let config = if cfg!(target_os = "macos") {
            root.join("home/Library/Application Support/Suds")
        } else {
            root.join("home/.config/suds")
        };
        fs::create_dir_all(&config).unwrap();
        let settings = serde_json::json!({
            "roots": [workspace],
            "tolerate_unreadable_processes": true,
        });
        fs::write(config.join("settings.json"), settings.to_string()).unwrap();
        Fixture { _dir: dir, root }
    }

    fn maintain(&self, arguments: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_suds"))
            .arg("maintain")
            .args(arguments)
            .current_dir(&self.root)
            .env("HOME", self.root.join("home"))
            .env_remove("XDG_STATE_HOME")
            .env_remove("XDG_CONFIG_HOME")
            .output()
            .unwrap()
    }
}

#[test]
fn a_paused_run_measures_and_its_status_follows_the_measurement() {
    let fixture = Fixture::new();
    let paused = fixture.maintain(&["--pause", "--reason", "incident-hold"]);
    assert!(paused.status.success(), "{paused:?}");

    // A paused scheduled run that measures everything succeeds, as the explicit observation does.
    let scheduled = fixture.maintain(&[]);
    let text = String::from_utf8_lossy(&scheduled.stdout);
    assert!(
        text.contains("Suds paused maintenance: incident-hold"),
        "{text}"
    );
    assert_eq!(scheduled.status.code(), Some(0), "{scheduled:?}");
    assert_eq!(fixture.maintain(&["--observe"]).status.code(), Some(0));
    let status = fixture.maintain(&["--status", "--format", "json"]);
    let status: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["paused"], true);
    assert_eq!(status["pressure"]["complete"], true);

    // A directory that the run cannot read makes the measurement partial, and the pause hides none of it.
    let closed = fixture.root.join("workspace/project/closed");
    fs::create_dir(&closed).unwrap();
    fs::set_permissions(&closed, fs::Permissions::from_mode(0o000)).unwrap();
    let observed = fixture.maintain(&["--observe"]);
    let scheduled = fixture.maintain(&[]);
    fs::set_permissions(&closed, fs::Permissions::from_mode(0o755)).unwrap();
    assert_ne!(observed.status.code(), Some(0), "{observed:?}");
    assert_eq!(
        scheduled.status.code(),
        observed.status.code(),
        "{scheduled:?}"
    );
}

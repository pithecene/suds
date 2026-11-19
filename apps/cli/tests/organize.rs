//! Command-line tests of `suds plan`, `suds apply`, `suds journal`, and
//! `suds undo` against temporary fixture trees and a temporary journal.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

struct Fixture {
    _dir: TempDir,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        fs::create_dir(root.join("Downloads")).unwrap();
        fs::create_dir(root.join("Research")).unwrap();
        Fixture { _dir: dir, root }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    /// Runs `suds` with the fixture's journal.
    fn suds(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_suds"))
            .args(args)
            .arg("--journal")
            .arg(self.path("journal"))
            .current_dir(&self.root)
            .output()
            .expect("run suds")
    }

    fn plan(&self, sources: &[&str], output: &str) -> Output {
        let mut args = vec!["plan", "move"];
        for source in sources {
            args.push("--from");
            args.push(source);
        }
        args.extend(["--to", "Research", "--output", output]);
        self.suds(&args)
    }
}

fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|err| {
        panic!(
            "parse {err}: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn snapshot(path: &Path) -> Vec<(PathBuf, u64)> {
    let mut entries = Vec::new();
    let mut pending = vec![path.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let meta = fs::symlink_metadata(entry.path()).unwrap();
            if meta.is_dir() {
                pending.push(entry.path());
            }
            entries.push((entry.path(), meta.len()));
        }
    }
    entries.sort();
    entries
}

#[test]
fn plans_applies_lists_and_undoes_a_move() {
    let fixture = Fixture::new();
    fs::write(fixture.path("Downloads/report.pdf"), b"report").unwrap();
    fs::create_dir(fixture.path("Downloads/photos")).unwrap();
    fs::write(fixture.path("Downloads/photos/1.jpg"), b"jpeg").unwrap();

    let planned = fixture.plan(&["Downloads/report.pdf", "Downloads/photos"], "plan.json");
    assert_eq!(planned.status.code(), Some(0), "{planned:?}");
    let preview = String::from_utf8(planned.stdout).unwrap();
    assert!(preview.contains("report.pdf"), "{preview}");
    assert!(
        preview.contains("Immediately before each move"),
        "{preview}"
    );
    let plan: Value =
        serde_json::from_slice(&fs::read(fixture.path("plan.json")).unwrap()).unwrap();
    assert_eq!(plan["schema"], "suds.plan/v1");
    assert_eq!(plan["moves"].as_array().unwrap().len(), 2);
    assert!(plan["moves"][0]["source"]["unix_bytes"].is_string());
    // Planning moves nothing.
    assert!(fixture.path("Downloads/report.pdf").exists());

    let applied = fixture.suds(&["apply", "plan.json"]);
    assert_eq!(applied.status.code(), Some(0));
    let run = json(&applied);
    assert_eq!(run["schema"], "suds.run/v1");
    assert_eq!(run["status"], "complete");
    assert!(fixture.path("Research/report.pdf").exists());
    assert!(fixture.path("Research/photos/1.jpg").exists());
    assert!(!fixture.path("Downloads/photos").exists());

    let listed = json(&fixture.suds(&["journal", "--format", "json"]));
    assert_eq!(listed[0]["run"], run["run"]);
    assert_eq!(listed[0]["undoable"], true);
    let table = fixture.suds(&["journal"]);
    assert!(String::from_utf8(table.stdout).unwrap().contains("applied"));

    let undone = fixture.suds(&["undo", run["run"].as_str().unwrap()]);
    assert_eq!(undone.status.code(), Some(0));
    let undo = json(&undone);
    assert_eq!(undo["schema"], "suds.undo/v1");
    assert_eq!(undo["status"], "complete");
    assert_eq!(
        fs::read(fixture.path("Downloads/report.pdf")).unwrap(),
        b"report"
    );
    assert!(fixture.path("Downloads/photos/1.jpg").exists());
}

#[test]
fn planning_changes_nothing_but_the_plan_file_and_the_journal() {
    let fixture = Fixture::new();
    fs::write(fixture.path("Downloads/a"), b"a").unwrap();
    let downloads = snapshot(&fixture.path("Downloads"));
    let research = snapshot(&fixture.path("Research"));
    assert_eq!(
        fixture.plan(&["Downloads/a"], "plan.json").status.code(),
        Some(0)
    );
    assert_eq!(downloads, snapshot(&fixture.path("Downloads")));
    assert_eq!(research, snapshot(&fixture.path("Research")));
}

#[test]
fn plan_refuses_to_replace_its_output_or_to_mix_directories() {
    let fixture = Fixture::new();
    fs::write(fixture.path("Downloads/a"), b"a").unwrap();
    fs::write(fixture.path("plan.json"), b"keep").unwrap();
    let exists = fixture.plan(&["Downloads/a"], "plan.json");
    assert_eq!(exists.status.code(), Some(2));
    assert_eq!(fs::read(fixture.path("plan.json")).unwrap(), b"keep");

    fs::write(fixture.path("elsewhere"), b"b").unwrap();
    let mixed = fixture.plan(&["Downloads/a", "elsewhere"], "other.json");
    assert_eq!(mixed.status.code(), Some(2));
    assert!(!fixture.path("other.json").exists());

    let missing = fixture.plan(&["Downloads/missing"], "missing.json");
    assert_eq!(missing.status.code(), Some(1));
    assert!(
        String::from_utf8(missing.stderr)
            .unwrap()
            .contains("no longer exists")
    );
    assert!(!fixture.path("missing.json").exists());
}

#[test]
fn apply_refuses_an_edited_plan_file_and_moves_nothing() {
    let fixture = Fixture::new();
    fs::write(fixture.path("Downloads/a"), b"a").unwrap();
    assert_eq!(
        fixture.plan(&["Downloads/a"], "plan.json").status.code(),
        Some(0)
    );
    let mut plan: Value =
        serde_json::from_slice(&fs::read(fixture.path("plan.json")).unwrap()).unwrap();
    plan["moves"][0]["expected"]["logical_size"] = 99.into();
    fs::write(fixture.path("edited.json"), plan.to_string()).unwrap();

    let refused = fixture.suds(&["apply", "edited.json"]);
    assert_eq!(refused.status.code(), Some(2));
    assert!(refused.stdout.is_empty());
    assert!(
        String::from_utf8(refused.stderr)
            .unwrap()
            .contains("differs from the plan that Suds built")
    );
    assert!(fixture.path("Downloads/a").exists());
}

#[test]
fn apply_exits_one_and_reports_each_refusal() {
    let fixture = Fixture::new();
    fs::write(fixture.path("Downloads/a"), b"a").unwrap();
    fs::write(fixture.path("Downloads/b"), b"b").unwrap();
    assert_eq!(
        fixture
            .plan(&["Downloads/a", "Downloads/b"], "plan.json")
            .status
            .code(),
        Some(0)
    );
    fs::write(fixture.path("Research/a"), b"occupied").unwrap();

    let applied = fixture.suds(&["apply", "plan.json"]);
    assert_eq!(applied.status.code(), Some(1));
    let run = json(&applied);
    assert_eq!(run["status"], "partial");
    assert_eq!(run["moves"][0]["status"], "refused");
    assert_eq!(run["moves"][0]["refusal"]["code"], "destination_exists");
    assert_eq!(run["moves"][1]["status"], "applied");
    assert_eq!(fs::read(fixture.path("Research/a")).unwrap(), b"occupied");
}

#[test]
fn undo_of_an_unknown_run_exits_two() {
    let fixture = Fixture::new();
    let output = fixture.suds(&["undo", "run-unknown"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("no run run-unknown")
    );
}

#[test]
fn an_empty_journal_lists_no_run_and_creates_nothing() {
    let fixture = Fixture::new();
    let output = fixture.suds(&["journal"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(!fixture.path("journal").exists());
    let listed = json(&fixture.suds(&["journal", "--format", "json"]));
    assert_eq!(listed, Value::Array(Vec::new()));
}

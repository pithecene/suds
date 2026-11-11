//! Command-line tests of `suds inspect` against temporary fixture trees.

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

fn suds(args: &[&str], roots: &[&Path]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_suds"));
    command.arg("inspect").args(args);
    for root in roots {
        command.arg("--root").arg(root);
    }
    command.output().expect("run suds")
}

/// Runs a JSON inspection and returns the report and the exit status.
fn inspect(roots: &[&Path]) -> (Value, i32) {
    let output = suds(&["--format", "json"], roots);
    let report = serde_json::from_slice(&output.stdout).expect("parse report");
    (report, output.status.code().expect("exit code"))
}

fn write(path: &Path, size: usize) {
    fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
    fs::write(path, vec![b'x'; size]).expect("write fixture");
}

fn child<'a>(root: &'a Value, name: &str) -> &'a Value {
    root["children"]
        .as_array()
        .expect("children")
        .iter()
        .find(|child| child["name"] == name)
        .unwrap_or_else(|| panic!("no child {name}"))
}

fn names(root: &Value) -> Vec<String> {
    root["children"]
        .as_array()
        .expect("children")
        .iter()
        .map(|child| child["name"].as_str().expect("name").to_owned())
        .collect()
}

#[test]
fn nested_files_aggregate_into_root_and_child() {
    let dir = TempDir::new().unwrap();
    write(&dir.path().join("a/b/c.txt"), 100);
    write(&dir.path().join("a/d.txt"), 50);
    write(&dir.path().join("e.txt"), 10);

    let (report, code) = inspect(&[dir.path()]);
    assert_eq!(code, 0);
    let root = &report["roots"][0];
    assert_eq!(root["status"], "complete");
    assert_eq!(root["logical_bytes"], 160);
    assert_eq!(root["files"], 3);
    assert_eq!(root["directories"], 2);
    assert!(root["allocated_bytes"].as_u64().unwrap() > 0);

    let a = child(root, "a");
    assert_eq!(a["kind"], "directory");
    assert_eq!(a["logical_bytes"], 150);
    assert_eq!(a["files"], 2);
    // A child's measured size includes the child's own entry.
    assert_eq!(a["directories"], 2);
    let e = child(root, "e.txt");
    assert_eq!(e["kind"], "file");
    assert_eq!(e["logical_bytes"], 10);
    assert_eq!(e["files"], 1);
}

#[test]
fn empty_directories_report_zero() {
    let dir = TempDir::new().unwrap();
    let empty_root = dir.path().join("empty-root");
    let populated = dir.path().join("populated");
    fs::create_dir_all(&empty_root).unwrap();
    fs::create_dir_all(populated.join("hollow")).unwrap();

    let (report, code) = inspect(&[&empty_root, &populated]);
    assert_eq!(code, 0);
    let empty = &report["roots"][0];
    assert_eq!(empty["status"], "complete");
    assert_eq!(empty["children"], Value::Array(Vec::new()));
    assert_eq!(empty["files"], 0);
    assert_eq!(empty["directories"], 0);
    assert_eq!(empty["logical_bytes"], 0);
    assert_eq!(empty["oldest_modified"], Value::Null);

    let hollow = child(&report["roots"][1], "hollow");
    assert_eq!(hollow["kind"], "directory");
    assert_eq!(hollow["files"], 0);
    assert_eq!(hollow["directories"], 1);
    assert_eq!(report["roots"][1]["directories"], 1);
}

#[test]
fn symlink_to_outside_directory_is_counted_and_not_followed() {
    let dir = TempDir::new().unwrap();
    let outside = dir.path().join("outside");
    let root = dir.path().join("root");
    write(&outside.join("large.bin"), 100_000);
    fs::create_dir_all(&root).unwrap();
    symlink(&outside, root.join("link")).unwrap();

    let (report, code) = inspect(&[&root]);
    assert_eq!(code, 0);
    let root = &report["roots"][0];
    assert_eq!(root["symlinks"], 1);
    assert_eq!(root["files"], 0);
    assert_eq!(root["logical_bytes"], 0);
    assert_eq!(child(root, "link")["kind"], "symlink");
}

#[test]
fn broken_symlink_is_counted_without_aborting() {
    let dir = TempDir::new().unwrap();
    symlink(dir.path().join("missing"), dir.path().join("broken")).unwrap();
    write(&dir.path().join("present.txt"), 5);

    let (report, code) = inspect(&[dir.path()]);
    assert_eq!(code, 0);
    let root = &report["roots"][0];
    assert_eq!(root["status"], "complete");
    assert_eq!(root["symlinks"], 1);
    assert_eq!(root["files"], 1);
    assert_eq!(root["error_count"], 0);
}

#[test]
fn hard_linked_file_contributes_storage_once() {
    let dir = TempDir::new().unwrap();
    let original = dir.path().join("x/file.bin");
    write(&original, 4096);
    fs::create_dir_all(dir.path().join("y")).unwrap();
    fs::hard_link(&original, dir.path().join("y/link.bin")).unwrap();

    let (report, code) = inspect(&[dir.path()]);
    assert_eq!(code, 0);
    let root = &report["roots"][0];
    assert_eq!(root["files"], 2);
    assert_eq!(root["logical_bytes"], 4096);
    let allocated = fs::metadata(&original).unwrap().blocks() * 512;
    let directories: u64 = ["x", "y"]
        .iter()
        .map(|name| {
            fs::symlink_metadata(dir.path().join(name))
                .unwrap()
                .blocks()
                * 512
        })
        .sum();
    assert_eq!(root["allocated_bytes"], allocated + directories);
    // The traversal reaches x before y, so the report assigns the shared bytes
    // to x.
    assert_eq!(child(root, "x")["logical_bytes"], 4096);
    assert_eq!(child(root, "y")["logical_bytes"], 0);
    assert_eq!(child(root, "y")["files"], 1);
}

#[test]
fn missing_root_is_absent() {
    let dir = TempDir::new().unwrap();
    let missing = dir.path().join("missing");

    let (report, code) = inspect(&[&missing]);
    assert_eq!(code, 0);
    assert_eq!(report["status"], "complete");
    assert_eq!(report["roots"][0]["status"], "absent");
    assert_eq!(report["roots"][0]["error_count"], 0);
}

#[test]
fn ordering_is_deterministic() {
    let dir = TempDir::new().unwrap();
    let first = dir.path().join("first");
    let second = dir.path().join("second");
    write(&second.join("b"), 10);
    write(&second.join("a"), 10);
    write(&second.join("c"), 50_000);
    write(&first.join("only"), 1);

    let strip = |mut report: Value| {
        report["observed_at"] = Value::Null;
        for root in report["roots"].as_array_mut().unwrap() {
            root["elapsed_ms"] = Value::Null;
        }
        report
    };
    let (one, _) = inspect(&[&second, &first]);
    let (two, _) = inspect(&[&second, &first]);
    assert_eq!(strip(one.clone()), strip(two));

    assert_eq!(one["roots"][0]["id"], "explicit.1");
    assert_eq!(one["roots"][0]["path"], second.to_str().unwrap());
    assert_eq!(one["roots"][1]["path"], first.to_str().unwrap());
    // The report orders entries by allocated bytes, then by logical bytes,
    // then by name.
    assert_eq!(names(&one["roots"][0]), ["c", "a", "b"]);
}

#[test]
fn json_output_is_one_versioned_document() {
    let dir = TempDir::new().unwrap();
    write(&dir.path().join("file"), 1);

    let output = suds(&["--format", "json"], &[dir.path()]);
    assert!(output.stderr.is_empty());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["schema"], "suds.inspection/v1");
    assert_eq!(report["platform"]["os"], std::env::consts::OS);
    assert!(report["observed_at"].as_str().unwrap().ends_with('Z'));
    assert_eq!(report["roots"][0]["kind"], "unclassified");

    // The JSON value type sorts its keys, so the order check reads the
    // document's raw text.
    let text = String::from_utf8(output.stdout).unwrap();
    let root = &text[text.find("\"roots\"").unwrap()..];
    let positions: Vec<usize> = [
        "id",
        "kind",
        "path",
        "status",
        "elapsed_ms",
        "logical_bytes",
        "allocated_bytes",
        "files",
        "directories",
        "symlinks",
        "others",
        "oldest_modified",
        "newest_modified",
        "children",
        "skipped_mount_count",
        "skipped_mounts",
        "error_count",
        "errors",
    ]
    .iter()
    .map(|key| root.find(&format!("\"{key}\":")).unwrap())
    .collect();
    assert!(positions.is_sorted(), "{text}");
}

#[test]
fn table_uses_binary_units_and_honors_top() {
    let dir = TempDir::new().unwrap();
    write(&dir.path().join("large"), 3 * 1024 * 1024);
    write(&dir.path().join("medium"), 2048);
    write(&dir.path().join("small"), 10);

    let output = suds(&["--top", "2"], &[dir.path()]);
    assert_eq!(output.status.code(), Some(0));
    let table = String::from_utf8(output.stdout).unwrap();
    assert!(table.contains("3.0 MiB"), "{table}");
    assert!(table.contains("2.0 KiB"), "{table}");
    assert!(table.contains("  large "), "{table}");
    assert!(table.contains("  medium "), "{table}");
    assert!(!table.contains("  small "), "{table}");
    assert!(table.contains("Omitted entries: 1."), "{table}");
    assert!(table.contains("estimate"), "{table}");

    let json = suds(&["--top", "1", "--format", "json"], &[dir.path()]);
    let report: Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(report["roots"][0]["children"].as_array().unwrap().len(), 3);
}

#[test]
fn unreadable_directory_yields_partial_report_and_status_one() {
    let dir = TempDir::new().unwrap();
    let locked = dir.path().join("locked");
    write(&locked.join("hidden"), 10);
    write(&dir.path().join("open"), 10);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let readable = fs::read_dir(&locked).is_ok();

    let (report, code) = inspect(&[dir.path()]);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    if readable {
        // A superuser reads the directory regardless of its mode.
        eprintln!("skipped: the process reads mode 000 directories");
        return;
    }
    assert_eq!(code, 1);
    assert_eq!(report["status"], "partial");
    let root = &report["roots"][0];
    assert_eq!(root["status"], "partial");
    assert_eq!(root["error_count"], 1);
    assert_eq!(root["errors"][0]["kind"], "permission_denied");
    assert_eq!(root["errors"][0]["path"], locked.to_str().unwrap());
    assert_eq!(root["files"], 1);
    assert_eq!(child(root, "locked")["directories"], 1);
}

#[test]
fn paths_with_spaces_work() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("with space");
    write(&root.join("inner dir/file name.txt"), 7);

    let (report, code) = inspect(&[&root]);
    assert_eq!(code, 0);
    let root_value = &report["roots"][0];
    assert_eq!(root_value["path"], root.to_str().unwrap());
    assert_eq!(child(root_value, "inner dir")["logical_bytes"], 7);
}

#[test]
fn relative_root_resolves_against_invocation_directory() {
    let dir = TempDir::new().unwrap();
    write(&dir.path().join("tree/file"), 3);

    let output = Command::new(env!("CARGO_BIN_EXE_suds"))
        .current_dir(dir.path())
        .args(["inspect", "--format", "json", "--root", "./tree/../tree/"])
        .output()
        .unwrap();
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    // On macOS, /var is a symbolic link to /private/var, so the test expects
    // the canonical path that the child process reports as its directory.
    let expected: PathBuf = fs::canonicalize(dir.path()).unwrap().join("tree");
    assert_eq!(report["roots"][0]["path"], expected.to_str().unwrap());
    assert_eq!(report["roots"][0]["logical_bytes"], 3);
}

#[test]
fn inspection_leaves_the_tree_unchanged() {
    let dir = TempDir::new().unwrap();
    write(&dir.path().join("a/b/file"), 100);
    symlink("a/b/file", dir.path().join("link")).unwrap();

    let snapshot = |root: &Path| {
        let mut entries = Vec::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(path) = pending.pop() {
            let meta = fs::symlink_metadata(&path).unwrap();
            entries.push((path.clone(), meta.len(), meta.mtime(), meta.mode()));
            if meta.is_dir() {
                for entry in fs::read_dir(&path).unwrap() {
                    pending.push(entry.unwrap().path());
                }
            }
        }
        entries.sort();
        entries
    };
    let before = snapshot(dir.path());
    let (_, code) = inspect(&[dir.path()]);
    assert_eq!(code, 0);
    assert_eq!(before, snapshot(dir.path()));
}

#[test]
fn mount_points_are_reported_and_not_crossed() {
    // On Linux runners, /dev holds nested mounts such as /dev/shm and /dev/pts.
    let dev = Path::new("/dev");
    let Ok(dev_meta) = fs::symlink_metadata(dev) else {
        eprintln!("skipped: no /dev");
        return;
    };
    let mut expected: Vec<String> = fs::read_dir(dev)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            fs::symlink_metadata(entry.path()).is_ok_and(|meta| meta.dev() != dev_meta.dev())
        })
        .map(|entry| entry.path().to_string_lossy().into_owned())
        .collect();
    expected.sort();
    if expected.is_empty() {
        eprintln!("skipped: /dev holds no mount point");
        return;
    }

    let (report, _) = inspect(&[dev]);
    let root = &report["roots"][0];
    let skipped: Vec<String> = root["skipped_mounts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|path| path.as_str().unwrap().to_owned())
        .collect();
    for mount in expected.iter().take(10) {
        assert!(skipped.contains(mount), "{mount} missing from {skipped:?}");
    }
    for name in &expected {
        let name = Path::new(name).file_name().unwrap().to_str().unwrap();
        assert!(!names(root).iter().any(|child| child == name));
    }
}

#[cfg(target_os = "linux")]
#[test]
fn linux_without_roots_exits_two() {
    let output = suds(&[], &[]);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let message = String::from_utf8(output.stderr).unwrap();
    assert!(message.contains("--root"), "{message}");
}

#[cfg(target_os = "macos")]
#[test]
fn macos_default_profile_reads_the_home_directory() {
    let home = TempDir::new().unwrap();
    write(&home.path().join("Library/Caches/app/blob"), 2048);

    let output = Command::new(env!("CARGO_BIN_EXE_suds"))
        .env("HOME", home.path())
        .args(["inspect", "--format", "json"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let ids: Vec<_> = report["roots"]
        .as_array()
        .unwrap()
        .iter()
        .map(|root| {
            (
                root["id"].clone(),
                root["kind"].clone(),
                root["status"].clone(),
            )
        })
        .collect();
    let expect = |id: &str, kind: &str, status: &str| {
        (Value::from(id), Value::from(kind), Value::from(status))
    };
    assert_eq!(
        ids,
        [
            expect("macos.user-caches", "cache", "complete"),
            expect("macos.user-logs", "logs", "absent"),
            expect("macos.xcode-derived-data", "generated", "absent"),
            expect("macos.downloads", "inbox", "absent"),
            expect("macos.trash", "trash", "absent"),
        ]
    );
    assert_eq!(report["roots"][0]["logical_bytes"], 2048);
}

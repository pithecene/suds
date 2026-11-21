//! The script that installs the window for one user.
#![cfg(target_os = "linux")]

use std::fs;
use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

/// Runs the script for a home directory, with the data directory that the desktop reads.
fn install(home: &Path, data: Option<&Path>) {
    let binary = home.join("built");
    fs::write(&binary, "#!/bin/sh\n").unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("install.sh");
    let mut command = Command::new("sh");
    command.arg(script).arg(&binary).env("HOME", home);
    match data {
        Some(data) => command.env("XDG_DATA_HOME", data),
        None => command.env_remove("XDG_DATA_HOME"),
    };
    assert!(command.status().unwrap().success());
}

#[test]
fn the_entry_names_the_installed_binary_by_its_full_path() {
    // The home's name holds a space and a dollar sign, which the entry must quote and escape.
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("a $home");
    fs::create_dir_all(&home).unwrap();
    install(&home, None);

    assert!(home.join(".local/bin/suds-desktop").is_file());
    let data = home.join(".local/share");
    assert!(data.join("icons/hicolor/scalable/apps/suds.svg").is_file());
    let entry = fs::read_to_string(data.join("applications/suds.desktop")).unwrap();
    let exec = entry
        .lines()
        .find(|line| line.starts_with("Exec="))
        .unwrap();
    let path = home.join(".local/bin/suds-desktop");
    let escaped = path.to_string_lossy().replace('$', "\\$");
    assert_eq!(exec, format!("Exec=\"{escaped}\""));
    assert!(entry.contains("\nIcon=suds\n"), "{entry}");
}

#[test]
fn the_entry_and_the_icon_follow_the_data_directory() {
    let temp = TempDir::new().unwrap();
    let (home, data) = (temp.path().join("home"), temp.path().join("data"));
    fs::create_dir_all(&home).unwrap();
    install(&home, Some(&data));

    assert!(data.join("applications/suds.desktop").is_file());
    assert!(data.join("icons/hicolor/scalable/apps/suds.svg").is_file());
    assert!(!home.join(".local/share").exists());
}

/// Lays out a release archive beneath `root`: the binary, and the script with its assets.
fn archive(root: &Path, binary: bool) -> std::path::PathBuf {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"));
    let desktop = root.join("share/suds/desktop");
    fs::create_dir_all(desktop.join("assets")).unwrap();
    fs::create_dir_all(root.join("bin")).unwrap();
    fs::copy(source.join("install.sh"), desktop.join("install.sh")).unwrap();
    for asset in ["suds.desktop", "suds.svg"] {
        let held = Path::new("assets").join(asset);
        fs::copy(source.join(&held), desktop.join(&held)).unwrap();
    }
    if binary {
        fs::write(root.join("bin/suds-desktop"), "#!/bin/sh\n").unwrap();
    }
    desktop.join("install.sh")
}

#[test]
fn the_script_of_a_release_archive_installs_the_archive_s_binary() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let script = archive(&temp.path().join("release"), true);
    let status = Command::new("sh")
        .arg(script)
        .env("HOME", &home)
        .env_remove("XDG_DATA_HOME")
        .status()
        .unwrap();

    assert!(status.success());
    assert!(home.join(".local/bin/suds-desktop").is_file());
    assert!(
        home.join(".local/share/applications/suds.desktop")
            .is_file()
    );
}

#[test]
fn the_script_installs_nothing_without_a_binary() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let script = archive(&temp.path().join("release"), false);
    let output = Command::new("sh")
        .arg(script)
        .env("HOME", &home)
        .env_remove("XDG_DATA_HOME")
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no binary at"));
    assert!(!home.join(".local").exists());
}

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

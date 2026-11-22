//! What the tests of the adapters and of the repositories share.
#![allow(dead_code, reason = "each test binary uses a part of the module")]

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// Writes a stub program at `path`, with `mode` as its permissions. A shell
/// writes the file, because the system runs no program that a process holds
/// open for writing. Another test's new process would hold a file that this
/// process opened, until that process runs its own program.
pub fn program(path: &Path, script: &str, mode: &str) {
    let mut shell = Command::new("sh")
        .args(["-c", "cat > \"$0\" && chmod \"$1\" \"$0\""])
        .arg(path)
        .arg(mode)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = shell.stdin.take().unwrap();
    input.write_all(script.as_bytes()).unwrap();
    drop(input);
    assert!(shell.wait().unwrap().success());
}

/// Runs Git in `directory` with a fixed identity and without signatures.
pub fn git(directory: &Path, arguments: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args([
            "-c",
            "user.name=Suds",
            "-c",
            "user.email=suds@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "tag.gpgsign=false",
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
    assert!(
        output.status.success(),
        "git {arguments:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

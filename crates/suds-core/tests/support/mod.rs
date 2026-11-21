//! What the tests of the adapters share.

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

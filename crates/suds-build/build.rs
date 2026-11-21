//! Reads the commit of this build from Git, for the text that the build states about itself.

use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=SUDS_BUILD_COMMIT");
    println!("cargo:rerun-if-env-changed=SUDS_BUILD_DATE");
    for path in ["HEAD", "logs/HEAD"] {
        if let Some(found) = git(&["rev-parse", "--git-path", path]) {
            println!("cargo:rerun-if-changed={}", absolute(&found));
        }
    }
    // A build outside a checkout names its commit and its date through the two variables.
    let commit = std::env::var("SUDS_BUILD_COMMIT")
        .ok()
        .or_else(|| git(&["rev-parse", "--short=12", "HEAD"]));
    let date = std::env::var("SUDS_BUILD_DATE")
        .ok()
        .or_else(|| git(&["log", "-1", "--format=%cs"]));
    if let Some(commit) = commit.filter(|text| !text.is_empty()) {
        println!("cargo:rustc-env=SUDS_COMMIT={commit}");
    }
    if let Some(date) = date.filter(|text| !text.is_empty()) {
        println!("cargo:rustc-env=SUDS_COMMIT_DATE={date}");
    }
}

fn git(arguments: &[&str]) -> Option<String> {
    let output = Command::new("git").args(arguments).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    Some(text.trim().to_owned())
}

fn absolute(path: &str) -> String {
    let path = Path::new(path);
    match std::env::current_dir() {
        Ok(here) if path.is_relative() => here.join(path).display().to_string(),
        _ => path.display().to_string(),
    }
}

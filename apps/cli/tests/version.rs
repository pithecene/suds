//! Command-line tests of the two forms of the version.

use std::process::Command;

fn printed(flag: &str) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_suds"))
        .arg(flag)
        .output()
        .expect("run suds");
    assert!(output.status.success());
    String::from_utf8(output.stdout).expect("text")
}

#[test]
fn the_short_form_prints_the_version_alone() {
    assert_eq!(
        printed("-V"),
        format!("suds {}\n", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn the_long_form_opens_with_the_short_form_and_states_the_owner_and_the_licence() {
    let text = printed("--version");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], format!("suds {}", env!("CARGO_PKG_VERSION")));
    assert!(lines.contains(&"Copyright © 2026 Pithecene LLC"));
    assert!(lines.contains(&"Apache License 2.0"));
    assert!(lines.contains(&"contact@pithecene.sh"));
}

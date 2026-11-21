//! What one build of Suds states about itself.
//!
//! The command prints these lines for `--version`, and the desktop
//! application shows them in About, so both state one text.

use std::sync::LazyLock;

/// The version of the release that this build belongs to.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The commit that this build came from, or nothing for a build outside a checkout.
pub const COMMIT: Option<&str> = option_env!("SUDS_COMMIT");

/// The date of that commit, as a calendar day.
pub const COMMIT_DATE: Option<&str> = option_env!("SUDS_COMMIT_DATE");

pub const COPYRIGHT: &str = "Copyright © 2026 Pithecene LLC";

pub const LICENSE: &str = "Apache License 2.0";

pub const LICENSE_ADDRESS: &str = "https://www.apache.org/licenses/LICENSE-2.0";

pub const CONTACT: &str = "contact@pithecene.sh";

/// The version with the commit and its date, as far as the build knows them.
pub fn build() -> String {
    described(VERSION, COMMIT, COMMIT_DATE)
}

/// The owner, the licence, the licence's address, and the contact address, one for each line.
pub fn notice() -> [&'static str; 4] {
    [COPYRIGHT, LICENSE, LICENSE_ADDRESS, CONTACT]
}

/// The text that follows the program's name for `--version`. Its first line is the version
/// alone, as the short form prints it, so a program that reads the first line reads the same.
pub fn long_version() -> &'static str {
    static TEXT: LazyLock<String> = LazyLock::new(|| long(VERSION, source(COMMIT, COMMIT_DATE)));
    &TEXT
}

fn long(version: &str, source: Option<String>) -> String {
    let mut lines = vec![version.to_owned()];
    lines.extend(source.map(|source| format!("Commit {source}")));
    lines.extend(notice().map(str::to_owned));
    lines.join("\n")
}

/// The commit with its date, or nothing for a build that knows no commit.
fn source(commit: Option<&str>, date: Option<&str>) -> Option<String> {
    match (commit, date) {
        (Some(commit), Some(date)) => Some(format!("{commit}, {date}")),
        (Some(commit), None) => Some(commit.to_owned()),
        _ => None,
    }
}

fn described(version: &str, commit: Option<&str>, date: Option<&str>) -> String {
    match source(commit, date) {
        Some(source) => format!("{version} ({source})"),
        None => version.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::{described, long, long_version, notice, source};

    #[test]
    fn a_build_states_its_commit_and_date_when_it_knows_them() {
        assert_eq!(
            described("0.1.4", Some("35d72f3a1b2c"), Some("2026-11-21")),
            "0.1.4 (35d72f3a1b2c, 2026-11-21)"
        );
        assert_eq!(
            described("0.1.4", Some("35d72f3a1b2c"), None),
            "0.1.4 (35d72f3a1b2c)"
        );
        assert_eq!(described("0.1.4", None, Some("2026-11-21")), "0.1.4");
        assert_eq!(described("0.1.4", None, None), "0.1.4");
    }

    #[test]
    fn the_long_version_opens_with_the_version_alone() {
        let text = long("0.1.4", source(Some("35d72f3a1b2c"), Some("2026-11-21")));
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "0.1.4");
        assert_eq!(lines[1], "Commit 35d72f3a1b2c, 2026-11-21");
        assert_eq!(lines[2], "Copyright © 2026 Pithecene LLC");
        assert_eq!(lines[2..], notice());
        assert_eq!(
            long("0.1.4", None).lines().nth(1),
            Some("Copyright © 2026 Pithecene LLC")
        );
        assert!(long_version().starts_with(env!("CARGO_PKG_VERSION")));
    }
}

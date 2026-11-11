//! The macOS profile: user-owned roots beneath the home directory.
//!
//! The profile excludes system-owned paths, application data, containers,
//! archives, simulators, browser profiles, mail, messages, and synchronized
//! documents.

use std::env;

use super::{ProfileError, RootSpec};
use crate::model::RootKind;

/// Each default root with its stable identifier, its kind, and its path
/// relative to the home directory.
const ROOTS: &[(&str, RootKind, &str)] = &[
    ("macos.user-caches", RootKind::Cache, "Library/Caches"),
    ("macos.user-logs", RootKind::Logs, "Library/Logs"),
    (
        "macos.xcode-derived-data",
        RootKind::Generated,
        "Library/Developer/Xcode/DerivedData",
    ),
    ("macos.downloads", RootKind::Inbox, "Downloads"),
    ("macos.trash", RootKind::Trash, ".Trash"),
];

pub fn default_roots() -> Result<Vec<RootSpec>, ProfileError> {
    let home = env::home_dir()
        .filter(|home| home.is_absolute())
        .ok_or(ProfileError::NoHome)?;
    Ok(ROOTS
        .iter()
        .map(|&(id, kind, relative)| RootSpec {
            id: id.to_owned(),
            kind,
            path: home.join(relative),
        })
        .collect())
}

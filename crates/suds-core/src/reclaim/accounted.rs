//! Checks the contents that Git would remove with a landed checkout.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::development::{self, Ecosystem};
use crate::model::{EntryKind, Measure};
use crate::reclaim::git::Git;
use crate::reclaim::plan::Kept;
use crate::scan::{self, SharedScan};

pub(super) struct Accounted {
    pub(super) measure: Measure,
    pub(super) go_caches: Vec<PathBuf>,
}

pub(super) fn survey(
    git: &Git,
    checkout: &Path,
    commit: &str,
) -> Result<Accounted, (Kept, String)> {
    let unreadable = |path: &Path, reason: &str| {
        (
            Kept::Unverified,
            format!("Suds could not read {}: {reason}", path.display()),
        )
    };
    let mount = |path: &Path| {
        (
            Kept::HoldsMountPoint,
            format!("a mount point stands at {}", path.display()),
        )
    };
    if scan::mount_at(checkout).map_err(|err| unreadable(checkout, &err.to_string()))? {
        return Err(mount(checkout));
    }
    let mut scan = SharedScan::for_removal();
    let mut entries = Vec::new();
    let (measure, _, repositories) =
        development::survey_tree_with(checkout, &mut scan, &mut |path, kind| {
            entries.push((path.to_path_buf(), kind));
        });
    if let Some(error) = scan.errors().first() {
        return Err(unreadable(&error.path, &error.message));
    }
    if let Some(path) = scan.first_mount() {
        return Err(mount(path));
    }
    if let Some(path) = scan.first_foreign_directory() {
        return Err((
            Kept::OwnedByAnotherUser,
            format!("another user owns {}", path.display()),
        ));
    }
    if let Some(nested) = repositories.first() {
        return Err((
            Kept::HoldsCheckout,
            format!("the checkout {} lies beneath it", nested.display()),
        ));
    }

    let tracked = git
        .tracked_at(checkout, commit)
        .map_err(|reason| (Kept::Unverified, reason))?;
    let mut derived = HashSet::new();
    let mut go_caches = Vec::new();
    let git_pointer = checkout.join(".git");
    for (path, kind) in entries {
        let relative = path
            .strip_prefix(checkout)
            .expect("the survey stays beneath the checkout");
        if (path == git_pointer && kind == EntryKind::File)
            || tracked.get(relative) == Some(&kind)
            || path
                .ancestors()
                .skip(1)
                .any(|parent| derived.contains(parent))
        {
            continue;
        }
        if kind == EntryKind::Directory
            && let Some(ecosystem) = development::build_state_at(&path)
            && git
                .ignores_all_entries(checkout, &path)
                .map_err(|reason| (Kept::Unverified, reason))?
        {
            if ecosystem == Ecosystem::Go {
                go_caches.push(path.clone());
            }
            derived.insert(path);
            continue;
        }
        return Err((
            Kept::UnaccountedContent,
            format!("Suds could not account for {}", path.display()),
        ));
    }
    Ok(Accounted { measure, go_caches })
}

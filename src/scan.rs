//! Metadata-only traversal of one root.
//!
//! The scan reads entry metadata and directory listings and never opens a
//! regular file. It examines each entry through `symlink_metadata`, so a
//! symbolic link counts as one entry and the scan never follows it.

use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::{self, FileType, Metadata};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::Instant;

use crate::model::{Child, EntryKind, ErrorKind, Measure, RootResult, RootStatus, TraversalError};
use crate::platform::RootSpec;
use crate::timestamp;

/// The maximum number of errors and of mount points that a result lists by
/// path. The count beside each list includes the entries beyond that maximum.
const SAMPLE_LIMIT: usize = 10;

/// The unit of `st_blocks` on Linux and on macOS, whatever the block size of
/// the filesystem.
const BLOCK_UNIT: u64 = 512;

/// Scans one root and returns its result.
pub fn scan_root(spec: &RootSpec) -> RootResult {
    let started = Instant::now();
    let (status, walker, measure, children) = walk_root(&spec.path);
    RootResult {
        id: spec.id.clone(),
        kind: spec.kind,
        path: spec.path.to_string_lossy().into_owned(),
        status,
        elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        measure,
        children,
        skipped_mount_count: walker.mount_count,
        skipped_mounts: walker.mounts,
        error_count: walker.error_count,
        errors: walker.errors,
    }
}

fn walk_root(root: &Path) -> (RootStatus, Walker, Measure, Vec<Child>) {
    let meta = match fs::symlink_metadata(root) {
        Ok(meta) => meta,
        Err(err) => {
            let mut walker = Walker::new(0);
            let status = if err.kind() == io::ErrorKind::NotFound {
                RootStatus::Absent
            } else {
                walker.record_error(root, &err);
                RootStatus::Unreadable
            };
            return (status, walker, Measure::default(), Vec::new());
        }
    };

    let mut walker = Walker::new(meta.dev());
    let mut totals = Tally::default();
    let mut children = Vec::new();
    if meta.is_dir() {
        let Some(names) = walker.list(root) else {
            return (
                RootStatus::Unreadable,
                walker,
                Measure::default(),
                Vec::new(),
            );
        };
        for name in names {
            let mut tally = Tally::default();
            if let Some(kind) = walker.walk(&root.join(&name), &mut tally) {
                totals.add(&tally);
                children.push(Child {
                    name: name.to_string_lossy().into_owned(),
                    kind,
                    measure: tally.into_measure(),
                });
            }
        }
        children.sort_by(|a, b| {
            b.measure
                .allocated_bytes
                .cmp(&a.measure.allocated_bytes)
                .then(b.measure.logical_bytes.cmp(&a.measure.logical_bytes))
                .then_with(|| a.name.cmp(&b.name))
        });
    } else {
        walker.walk(root, &mut totals);
    }

    let status = if walker.error_count == 0 {
        RootStatus::Complete
    } else {
        RootStatus::Partial
    };
    (status, walker, totals.into_measure(), children)
}

/// The state that persists across one root's traversal.
struct Walker {
    /// The root's device, which bounds the traversal to one filesystem.
    device: u64,
    /// Each hard-linked regular file whose storage the scan has counted under
    /// the root.
    linked: HashSet<(u64, u64)>,
    error_count: u64,
    errors: Vec<TraversalError>,
    mount_count: u64,
    mounts: Vec<String>,
}

impl Walker {
    fn new(device: u64) -> Self {
        Walker {
            device,
            linked: HashSet::new(),
            error_count: 0,
            errors: Vec::new(),
            mount_count: 0,
            mounts: Vec::new(),
        }
    }

    /// Adds the sums for the entry at `start` and everything beneath it to
    /// `tally`. The function returns the entry's type, or `None` when the scan
    /// cannot measure the entry or the entry lies on another filesystem.
    fn walk(&mut self, start: &Path, tally: &mut Tally) -> Option<EntryKind> {
        let kind = self.visit(start, tally)?;
        if kind == EntryKind::Directory {
            // The listings arrive sorted, and the traversal pushes subdirectories
            // onto the stack in reverse order, so the traversal order, and with
            // it the sampled errors and hard-link attribution, depends only on
            // the tree.
            let mut pending = vec![start.to_path_buf()];
            while let Some(dir) = pending.pop() {
                let Some(names) = self.list(&dir) else {
                    continue;
                };
                let mut subdirs = Vec::new();
                for name in names {
                    let path = dir.join(name);
                    if self.visit(&path, tally) == Some(EntryKind::Directory) {
                        subdirs.push(path);
                    }
                }
                pending.extend(subdirs.into_iter().rev());
            }
        }
        Some(kind)
    }

    /// Measures one entry without descending into it.
    fn visit(&mut self, path: &Path, tally: &mut Tally) -> Option<EntryKind> {
        let meta = match fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(err) => {
                self.record_error(path, &err);
                return None;
            }
        };
        let kind = entry_kind(meta.file_type());
        if meta.dev() != self.device || (kind == EntryKind::Directory && is_mount_root(path)) {
            self.mount_count = self.mount_count.saturating_add(1);
            if self.mounts.len() < SAMPLE_LIMIT {
                self.mounts.push(path.to_string_lossy().into_owned());
            }
            return None;
        }
        // A regular file with several links stores its data once, so only the
        // first link that the traversal reaches under the root contributes bytes.
        let counts_storage = kind != EntryKind::File
            || meta.nlink() <= 1
            || self.linked.insert((meta.dev(), meta.ino()));
        tally.count(kind, &meta, counts_storage);
        Some(kind)
    }

    /// Lists a directory's entry names in byte order.
    fn list(&mut self, dir: &Path) -> Option<Vec<OsString>> {
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(err) => {
                self.record_error(dir, &err);
                return None;
            }
        };
        let mut names = Vec::new();
        for entry in entries {
            match entry {
                Ok(entry) => names.push(entry.file_name()),
                Err(err) => self.record_error(dir, &err),
            }
        }
        names.sort();
        Some(names)
    }

    fn record_error(&mut self, path: &Path, err: &io::Error) {
        self.error_count = self.error_count.saturating_add(1);
        if self.errors.len() < SAMPLE_LIMIT {
            let kind = match err.kind() {
                io::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
                io::ErrorKind::NotFound => ErrorKind::Vanished,
                _ => ErrorKind::Other,
            };
            self.errors.push(TraversalError {
                path: path.to_string_lossy().into_owned(),
                kind,
                message: err.to_string(),
            });
        }
    }
}

/// Reports whether a directory is a mount point, including a bind mount on
/// the scanned root's own device. A kernel older than Linux 5.8 omits the
/// mount-root attribute, and the device comparison alone then bounds the scan.
#[cfg(target_os = "linux")]
fn is_mount_root(path: &Path) -> bool {
    use rustix::fs::{AtFlags, CWD, StatxAttributes, StatxFlags, statx};

    let flags = AtFlags::SYMLINK_NOFOLLOW | AtFlags::NO_AUTOMOUNT;
    statx(CWD, path, flags, StatxFlags::empty()).is_ok_and(|stat| {
        stat.stx_attributes_mask
            .contains(StatxAttributes::MOUNT_ROOT)
            && stat.stx_attributes.contains(StatxAttributes::MOUNT_ROOT)
    })
}

/// macOS has no bind mount, so a mount point always lies on a different device
/// from its parent directory.
#[cfg(target_os = "macos")]
fn is_mount_root(_path: &Path) -> bool {
    false
}

fn entry_kind(file_type: FileType) -> EntryKind {
    if file_type.is_dir() {
        EntryKind::Directory
    } else if file_type.is_file() {
        EntryKind::File
    } else if file_type.is_symlink() {
        EntryKind::Symlink
    } else {
        EntryKind::Other
    }
}

/// Running sums for one measurement.
///
/// Each sum saturates at `u64::MAX`. No real filesystem reaches that total,
/// and saturation keeps an impossible input from aborting the report.
#[derive(Default)]
struct Tally {
    logical: u64,
    allocated: u64,
    files: u64,
    directories: u64,
    symlinks: u64,
    others: u64,
    oldest: Option<i64>,
    newest: Option<i64>,
}

impl Tally {
    fn count(&mut self, kind: EntryKind, meta: &Metadata, counts_storage: bool) {
        let counter = match kind {
            EntryKind::Directory => &mut self.directories,
            EntryKind::File => &mut self.files,
            EntryKind::Symlink => &mut self.symlinks,
            EntryKind::Other => &mut self.others,
        };
        *counter = counter.saturating_add(1);
        if counts_storage {
            if kind == EntryKind::File {
                self.logical = self.logical.saturating_add(meta.len());
            }
            let allocated = meta.blocks().saturating_mul(BLOCK_UNIT);
            self.allocated = self.allocated.saturating_add(allocated);
        }
        let modified = meta.mtime();
        self.oldest = Some(self.oldest.map_or(modified, |t| t.min(modified)));
        self.newest = Some(self.newest.map_or(modified, |t| t.max(modified)));
    }

    fn add(&mut self, other: &Tally) {
        self.logical = self.logical.saturating_add(other.logical);
        self.allocated = self.allocated.saturating_add(other.allocated);
        self.files = self.files.saturating_add(other.files);
        self.directories = self.directories.saturating_add(other.directories);
        self.symlinks = self.symlinks.saturating_add(other.symlinks);
        self.others = self.others.saturating_add(other.others);
        self.oldest = match (self.oldest, other.oldest) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        self.newest = match (self.newest, other.newest) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
    }

    fn into_measure(self) -> Measure {
        Measure {
            logical_bytes: self.logical,
            allocated_bytes: self.allocated,
            files: self.files,
            directories: self.directories,
            symlinks: self.symlinks,
            others: self.others,
            oldest_modified: self.oldest.map(timestamp::format_utc),
            newest_modified: self.newest.map(timestamp::format_utc),
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::path::Path;

    use rustix::fs::{AtFlags, CWD, StatxAttributes, StatxFlags, statx};
    use tempfile::TempDir;

    use super::is_mount_root;

    /// The test reads existing mounts rather than creating a bind mount, so it
    /// leaves the host's mount table unchanged and needs no privilege.
    #[test]
    fn detects_mount_roots_by_attribute() {
        let dir = TempDir::new().unwrap();
        assert!(!is_mount_root(dir.path()));

        let root = Path::new("/");
        let stat = statx(CWD, root, AtFlags::SYMLINK_NOFOLLOW, StatxFlags::empty()).unwrap();
        if !stat.stx_attributes_mask.contains(StatxAttributes::MOUNT_ROOT) {
            eprintln!("skipped: the kernel omits the mount-root attribute");
            return;
        }
        assert!(is_mount_root(root));
    }
}

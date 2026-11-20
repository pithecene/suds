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
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use crate::inspection::{ChildScan, EntryRevision, RootScan, ScanError};
use crate::model::{EntryKind, ErrorKind, Measure, RootStatus};
use crate::platform::RootSpec;
use crate::timestamp;

/// The maximum number of errors and of mount points that a result lists by
/// path. The count beside each list includes the entries beyond that maximum.
const SAMPLE_LIMIT: usize = 10;

/// The unit of `st_blocks` on Linux and on macOS, whatever the block size of
/// the filesystem.
const BLOCK_UNIT: u64 = 512;

/// The scan stops early once `cancel` holds `true`. The result then records
/// the cancellation and lists only the children whose measurement finished
/// before the cancellation.
pub fn scan_root(spec: &RootSpec, cancel: &AtomicBool) -> RootScan {
    let started = Instant::now();
    let (status, walker, measure, children) = match &spec.anchor {
        Some(anchor) => check_ancestors(anchor, &spec.path),
        None => None,
    }
    .unwrap_or_else(|| walk_root(&spec.path, cancel));
    RootScan {
        spec: spec.clone(),
        status,
        cancelled: walker.cancelled,
        elapsed: started.elapsed(),
        measure,
        children,
        skipped_mount_count: walker.mount_count,
        skipped_mounts: walker.mounts,
        error_count: walker.error_count,
        errors: walker.errors,
    }
}

type Outcome = (RootStatus, Walker, Measure, Vec<ChildScan>);

/// Examines each directory between a default root's anchor and the root
/// without following links. The function returns an outcome when a directory
/// is absent, unreadable, or a symbolic link, and `None` when the scan may
/// proceed.
fn check_ancestors(anchor: &Path, root: &Path) -> Option<Outcome> {
    let relative = root.strip_prefix(anchor).ok()?;
    let mut path = anchor.to_path_buf();
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        // The root's own final component is the scan's first entry.
        if components.peek().is_none() {
            break;
        }
        path.push(component);
        let mut walker = Walker::new(0);
        let status = match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let err = io::Error::other(
                    "this symbolic link lies on the root's path, and the scan follows no symbolic link",
                );
                walker.record_error(&path, &err);
                RootStatus::Unreadable
            }
            Ok(_) => continue,
            Err(err) if err.kind() == io::ErrorKind::NotFound => RootStatus::Absent,
            Err(err) => {
                walker.record_error(&path, &err);
                RootStatus::Unreadable
            }
        };
        return Some((status, walker, Measure::default(), Vec::new()));
    }
    None
}

fn walk_root(root: &Path, cancel: &AtomicBool) -> Outcome {
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
            if cancel.load(Ordering::Relaxed) {
                walker.cancelled = true;
                break;
            }
            let mut tally = Tally::default();
            let visited = walker.walk(&root.join(&name), &mut tally, cancel);
            // A cancellation during the child's walk leaves its measure short,
            // so the result omits that child rather than list a wrong figure.
            if walker.cancelled {
                break;
            }
            if let Some(revision) = visited {
                totals.add(&tally);
                children.push(ChildScan {
                    name,
                    kind: revision.kind,
                    revision,
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
        walker.walk(root, &mut totals, cancel);
    }

    let status = if walker.error_count == 0 && !walker.cancelled {
        RootStatus::Complete
    } else {
        RootStatus::Partial
    };
    (status, walker, totals.into_measure(), children)
}

/// The measurement of one entry and everything beneath it, as a move plan
/// shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryMeasure {
    pub measure: Measure,
    pub skipped_mount_count: u64,
    pub error_count: u64,
    /// The first traversal errors, in traversal order.
    pub errors: Vec<ScanError>,
}

/// Measures the entry at `path`, which lies on `device`, with the same rules
/// as a root's direct child.
pub fn measure_entry(path: &Path, device: u64) -> EntryMeasure {
    let mut walker = Walker::new(device);
    let mut tally = Tally::default();
    // A plan measures only the entries that a person selected, so the
    // measurement runs to its end.
    let never = AtomicBool::new(false);
    walker.walk(path, &mut tally, &never);
    EntryMeasure {
        measure: tally.into_measure(),
        skipped_mount_count: walker.mount_count,
        error_count: walker.error_count,
        errors: walker.errors,
    }
}

/// One traversal that several measurements share. A regular file with
/// several links counts toward the first measurement that reaches it, so the
/// measurements of one scan sum to the storage that their trees occupy.
pub struct SharedScan {
    walker: Walker,
}

/// One measurement of a shared scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundedMeasure {
    /// The entry and everything beneath it, without the directories that the
    /// measurement set aside.
    pub measure: Measure,
    /// Each directory that the caller's boundary claimed, in traversal order.
    /// The measurement neither counted nor entered it.
    pub set_aside: Vec<PathBuf>,
}

impl Default for SharedScan {
    fn default() -> Self {
        SharedScan {
            walker: Walker::new(0),
        }
    }
}

impl SharedScan {
    /// Measures the entry at `start` and everything beneath it on its
    /// filesystem. The measurement asks `boundary` about each directory
    /// beneath `start` and sets aside each one that the boundary claims. It
    /// returns `None` when it cannot measure `start` itself.
    pub fn measure(
        &mut self,
        start: &Path,
        boundary: &mut dyn FnMut(&Path) -> bool,
    ) -> Option<BoundedMeasure> {
        match fs::symlink_metadata(start) {
            Ok(meta) => self.walker.device = meta.dev(),
            Err(err) => {
                self.walker.record_error(start, &err);
                return None;
            }
        }
        let mut tally = Tally::default();
        let mut set_aside = Vec::new();
        let never = AtomicBool::new(false);
        self.walker
            .walk_within(start, &mut tally, &never, Some(boundary), &mut set_aside)?;
        Some(BoundedMeasure {
            measure: tally.into_measure(),
            set_aside,
        })
    }

    /// Records that the caller could not examine `path`, so the scan's
    /// errors name every path that an inspection could not read.
    pub fn record_error(&mut self, path: &Path, err: &io::Error) {
        self.walker.record_error(path, err);
    }

    pub fn error_count(&self) -> u64 {
        self.walker.error_count
    }

    /// The first traversal errors, in traversal order.
    pub fn errors(&self) -> &[ScanError] {
        &self.walker.errors
    }

    pub fn skipped_mount_count(&self) -> u64 {
        self.walker.mount_count
    }
}

/// The state that persists across one root's traversal.
/// One entry's metadata, its kind, and whether a mount point stands there.
struct Probed {
    meta: fs::Metadata,
    kind: EntryKind,
    mount: bool,
}

impl Probed {
    /// Whether the entry is a directory of the scanned filesystem.
    fn is_directory_here(&self) -> bool {
        self.kind == EntryKind::Directory && !self.mount
    }
}

struct Walker {
    /// The root's device, which bounds the traversal to one filesystem.
    device: u64,
    /// Each hard-linked regular file whose storage the scan has counted under
    /// the root.
    linked: HashSet<(u64, u64)>,
    error_count: u64,
    errors: Vec<ScanError>,
    mount_count: u64,
    mounts: Vec<PathBuf>,
    /// Whether the scan stopped at a cancellation request.
    cancelled: bool,
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
            cancelled: false,
        }
    }

    /// Adds the sums for the entry at `start` and everything beneath it to
    /// `tally`. The function returns the entry's revision, or `None` when the
    /// scan cannot measure the entry or the entry lies on another filesystem.
    fn walk(
        &mut self,
        start: &Path,
        tally: &mut Tally,
        cancel: &AtomicBool,
    ) -> Option<EntryRevision> {
        self.walk_within(start, tally, cancel, None, &mut Vec::new())
    }

    /// Walks as `walk` does, and sets aside each directory beneath `start`
    /// that `boundary` claims, without counting or entering it.
    fn walk_within(
        &mut self,
        start: &Path,
        tally: &mut Tally,
        cancel: &AtomicBool,
        mut boundary: Option<&mut dyn FnMut(&Path) -> bool>,
        set_aside: &mut Vec<PathBuf>,
    ) -> Option<EntryRevision> {
        // A bounded measurement starts where its caller chose, so its start
        // may be a mount point. A mount point beneath the start still ends
        // the traversal there.
        let probed = self.probe(start, boundary.is_some())?;
        let revision = self.count(start, &probed, tally)?;
        if revision.kind == EntryKind::Directory {
            // The listings arrive sorted, and the traversal pushes subdirectories
            // onto the stack in reverse order, so the traversal order, and with
            // it the sampled errors and hard-link attribution, depends only on
            // the tree.
            let mut pending = vec![start.to_path_buf()];
            while let Some(dir) = pending.pop() {
                if cancel.load(Ordering::Relaxed) {
                    self.cancelled = true;
                    break;
                }
                let Some(names) = self.list(&dir) else {
                    continue;
                };
                let mut subdirs = Vec::new();
                for name in names {
                    // A single directory can hold many entries, so a
                    // cancellation request takes effect between entries.
                    if cancel.load(Ordering::Relaxed) {
                        self.cancelled = true;
                        break;
                    }
                    let path = dir.join(name);
                    // One probe serves the boundary and the count, so each
                    // entry costs one metadata call.
                    let Some(probed) = self.probe(&path, false) else {
                        continue;
                    };
                    if let Some(boundary) = boundary.as_deref_mut()
                        && probed.is_directory_here()
                        && boundary(&path)
                    {
                        set_aside.push(path);
                        continue;
                    }
                    let visited = self.count(&path, &probed, tally);
                    if visited.is_some_and(|revision| revision.kind == EntryKind::Directory) {
                        subdirs.push(path);
                    }
                }
                pending.extend(subdirs.into_iter().rev());
            }
        }
        Some(revision)
    }

    /// Reads the metadata of one entry and decides whether a mount point
    /// stands there. With `chosen`, the entry is the start that a caller
    /// named, which may be a mount point.
    fn probe(&mut self, path: &Path, chosen: bool) -> Option<Probed> {
        let meta = match fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(err) => {
                self.record_error(path, &err);
                return None;
            }
        };
        let kind = entry_kind(meta.file_type());
        let mut mount = !chosen && meta.dev() != self.device;
        if !chosen && !mount && kind == EntryKind::Directory {
            // A failed check leaves the entry unmeasured and the root partial,
            // because the scan cannot rule out a mount boundary.
            match is_mount_root(path) {
                Ok(root) => mount = root,
                Err(err) => {
                    self.record_error(path, &err);
                    return None;
                }
            }
        }
        Some(Probed { meta, kind, mount })
    }

    /// Measures one probed entry without descending into it. A mount point
    /// counts as skipped, so no boundary sets one aside.
    fn count(&mut self, path: &Path, probed: &Probed, tally: &mut Tally) -> Option<EntryRevision> {
        let Probed { meta, kind, mount } = probed;
        if *mount {
            self.mount_count = self.mount_count.saturating_add(1);
            if self.mounts.len() < SAMPLE_LIMIT {
                self.mounts.push(path.to_path_buf());
            }
            return None;
        }
        // A regular file with several links stores its data once, so only the
        // first link that the traversal reaches under the root contributes bytes.
        let counts_storage = *kind != EntryKind::File
            || meta.nlink() <= 1
            || self.linked.insert((meta.dev(), meta.ino()));
        tally.count(*kind, meta, counts_storage);
        Some(EntryRevision::from_metadata(meta))
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
            self.errors.push(ScanError {
                path: path.to_path_buf(),
                kind,
                message: err.to_string(),
            });
        }
    }
}

/// Reports whether a directory is a mount point, including a bind mount on
/// the scanned root's own device. A kernel older than Linux 5.8 omits the
/// mount-root attribute, and a kernel older than Linux 4.11 lacks `statx`.
/// On either kernel, the device comparison alone bounds the scan.
#[cfg(target_os = "linux")]
fn is_mount_root(path: &Path) -> io::Result<bool> {
    use rustix::fs::{AtFlags, CWD, StatxAttributes, StatxFlags, statx};
    use rustix::io::Errno;

    let flags = AtFlags::SYMLINK_NOFOLLOW | AtFlags::NO_AUTOMOUNT;
    match statx(CWD, path, flags, StatxFlags::empty()) {
        Ok(stat) => Ok(stat
            .stx_attributes_mask
            .contains(StatxAttributes::MOUNT_ROOT)
            && stat.stx_attributes.contains(StatxAttributes::MOUNT_ROOT)),
        Err(Errno::NOSYS) => Ok(false),
        Err(errno) => Err(errno.into()),
    }
}

/// macOS has no bind mount, so a mount point always lies on a different device
/// from its parent directory.
#[cfg(target_os = "macos")]
fn is_mount_root(_path: &Path) -> io::Result<bool> {
    Ok(false)
}

pub(crate) fn entry_kind(file_type: FileType) -> EntryKind {
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
        assert!(!is_mount_root(dir.path()).unwrap());
        assert!(is_mount_root(&dir.path().join("missing")).is_err());

        let root = Path::new("/");
        let stat = statx(CWD, root, AtFlags::SYMLINK_NOFOLLOW, StatxFlags::empty()).unwrap();
        if !stat
            .stx_attributes_mask
            .contains(StatxAttributes::MOUNT_ROOT)
        {
            eprintln!("skipped: the kernel omits the mount-root attribute");
            return;
        }
        assert!(is_mount_root(root).unwrap());
    }
}

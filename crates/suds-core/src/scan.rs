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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use crate::ahead::{Ahead, Body, Claims, Entry, Head, Layout, START_AT, Slot};
use crate::clones::{self, Extent};
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

/// A rule that sees each entry beneath a measurement's start, in the
/// traversal's order, before the measurement counts it.
pub(crate) type Note<'a> = dyn FnMut(&Entry<'_>) + 'a;

/// The rules of a bounded measurement.
struct Rules<'a, 'b> {
    claims: Claims,
    note: &'a mut Note<'b>,
}

/// The state of one traversal that the count of a directory's entries
/// reads and changes.
struct Replay<'a, 'b, 'c> {
    ahead: &'a Ahead,
    tally: &'a mut Tally,
    cancel: &'a AtomicBool,
    note: &'a mut Option<&'b mut Note<'c>>,
    set_aside: &'a mut Vec<PathBuf>,
    pending: &'a mut Vec<(PathBuf, Arc<Slot>)>,
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
    pub(crate) fn for_removal() -> Self {
        let mut scan = Self::default();
        scan.walker.check_file_mounts = true;
        scan
    }

    /// Measures the entry at `start` and everything beneath it on its
    /// filesystem. The measurement asks `claims` about each directory
    /// beneath `start` and sets aside each one that the rule claims. It
    /// returns `None` when it cannot measure `start` itself.
    pub(crate) fn measure(&mut self, start: &Path, claims: Claims) -> Option<BoundedMeasure> {
        self.measure_with(start, claims, &mut |_| {})
    }

    /// Measures as `measure` does, and shows `note` each entry beneath
    /// `start` in the traversal's order.
    pub(crate) fn measure_with(
        &mut self,
        start: &Path,
        claims: Claims,
        note: &mut Note<'_>,
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
        let rules = Rules { claims, note };
        self.walker
            .walk_within(start, &mut tally, &never, Some(rules), &mut set_aside)?;
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

    pub(crate) fn first_mount(&self) -> Option<&Path> {
        self.walker.mounts.first().map(PathBuf::as_path)
    }

    pub(crate) fn first_foreign_directory(&self) -> Option<&Path> {
        self.walker.first_foreign_directory.as_deref()
    }

    /// The directories so far that belong to another user.
    pub fn foreign_directory_count(&self) -> u64 {
        self.walker.foreign_directory_count
    }
}

/// The state that persists across one root's traversal.
/// One entry's metadata, its kind, and whether a mount point stands there.
pub(crate) struct Probed {
    meta: fs::Metadata,
    /// Each extent that a regular file shares with another file.
    shared: Vec<Extent>,
    pub(crate) kind: EntryKind,
    pub(crate) mount: bool,
}

struct Walker {
    /// The root's device, which bounds the traversal to one filesystem.
    device: u64,
    check_file_mounts: bool,
    /// Each hard-linked regular file whose storage the scan has counted under
    /// the root.
    linked: HashSet<(u64, u64)>,
    /// Each shared extent whose storage the scan has counted under the
    /// root.
    cloned: HashSet<Extent>,
    error_count: u64,
    errors: Vec<ScanError>,
    mount_count: u64,
    /// The directories that belong to another user than the one who runs
    /// the scan. That user cannot remove what such a directory holds.
    foreign_directory_count: u64,
    first_foreign_directory: Option<PathBuf>,
    user: u32,
    mounts: Vec<PathBuf>,
    /// Whether the scan stopped at a cancellation request.
    cancelled: bool,
}

impl Walker {
    fn new(device: u64) -> Self {
        Walker {
            device,
            check_file_mounts: false,
            linked: HashSet::new(),
            cloned: HashSet::new(),
            error_count: 0,
            errors: Vec::new(),
            mount_count: 0,
            foreign_directory_count: 0,
            first_foreign_directory: None,
            user: rustix::process::geteuid().as_raw(),
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
    /// that `claims` claims, without counting or entering it. The walk shows
    /// `note` each entry beneath `start` before it counts the entry.
    fn walk_within(
        &mut self,
        start: &Path,
        tally: &mut Tally,
        cancel: &AtomicBool,
        rules: Option<Rules<'_, '_>>,
        set_aside: &mut Vec<PathBuf>,
    ) -> Option<EntryRevision> {
        // A bounded measurement starts where its caller chose, so its start
        // may be a mount point. A mount point beneath the start still ends
        // the traversal there.
        let probed = self.probe(start, rules.is_some())?;
        let revision = self.count(start, &probed, tally)?;
        if revision.kind != EntryKind::Directory {
            return Some(revision);
        }
        let (claims, mut note): (Claims, Option<&mut Note<'_>>) = match rules {
            Some(rules) => (rules.claims, Some(rules.note)),
            None => (Arc::new(|_| false), None),
        };
        let ahead = Ahead::new(claims, self.device, self.check_file_mounts);
        // Each listing arrives sorted, and the stack takes subdirectories
        // in reverse. The order of the count therefore depends on the tree
        // alone, whichever thread read a directory.
        let mut pending = vec![(start.to_path_buf(), Slot::start(start.to_path_buf()))];
        // Whether the traversal has asked the threads to join, which it asks once.
        let mut asked = false;
        while let Some((dir, slot)) = pending.pop() {
            if cancel.load(Ordering::Relaxed) {
                self.cancelled = true;
                break;
            }
            let (head, body) = ahead.body(&slot);
            if !asked && pending.len() + body.entered() >= START_AT {
                asked = true;
                let listed: Vec<_> = pending.iter().map(|(_, slot)| Arc::clone(slot)).collect();
                ahead.join(&listed, &slot, &body);
            }
            let mut rest = Replay {
                ahead: &ahead,
                tally: &mut *tally,
                cancel,
                note: &mut note,
                set_aside: &mut *set_aside,
                pending: &mut pending,
            };
            self.replay(&dir, &head, &body, &mut rest);
            ahead.counted(&body);
            if self.cancelled {
                break;
            }
        }
        ahead.end();
        Some(revision)
    }

    /// Counts the entries of one directory from its reading, in the
    /// listing's order, and queues each directory to enter.
    fn replay(&mut self, dir: &Path, head: &Head, body: &Body, rest: &mut Replay<'_, '_, '_>) {
        let Replay {
            ahead,
            tally,
            cancel,
            note,
            set_aside,
            pending,
        } = rest;
        if let Some(err) = &head.unlisted {
            self.record_error(dir, err);
            return;
        }
        for err in &head.unnamed {
            self.record_error(dir, err);
        }
        let mut subdirs = Vec::new();
        for child in &body.children {
            // A single directory can hold many entries, so a cancellation
            // request takes effect between entries.
            if cancel.load(Ordering::Relaxed) {
                self.cancelled = true;
                break;
            }
            let path = child.path.as_path();
            let probed = match &child.probed {
                Ok(probed) => probed,
                Err(err) => {
                    self.record_error(path, err);
                    continue;
                }
            };
            let entered = child.slot.as_ref().map(|slot| ahead.head(slot));
            if !probed.mount {
                if let Some(note) = note.as_deref_mut() {
                    note(&Entry {
                        path,
                        kind: probed.kind,
                        own: entered
                            .as_ref()
                            .map_or(Layout::Asked(path), |own| own.layout(path)),
                        beside: head.layout(dir),
                    });
                }
                if entered.as_ref().is_some_and(|entered| entered.claimed) {
                    set_aside.push(child.path.clone());
                    continue;
                }
            }
            let visited = self.count(path, probed, tally);
            if visited.is_some_and(|revision| revision.kind == EntryKind::Directory)
                && let Some(slot) = &child.slot
            {
                subdirs.push((child.path.clone(), Arc::clone(slot)));
            }
        }
        pending.extend(subdirs.into_iter().rev());
    }

    /// Reads the metadata of one entry and decides whether a mount point
    /// stands there. With `chosen`, the entry is the start that a caller
    /// named, which may be a mount point.
    fn probe(&mut self, path: &Path, chosen: bool) -> Option<Probed> {
        match probe_at(path, chosen, self.device, self.check_file_mounts) {
            Ok(probed) => Some(probed),
            Err(err) => {
                self.record_error(path, &err);
                None
            }
        }
    }

    /// Measures one probed entry without descending into it. A mount point
    /// counts as skipped, so no boundary sets one aside.
    fn count(&mut self, path: &Path, probed: &Probed, tally: &mut Tally) -> Option<EntryRevision> {
        let Probed {
            meta,
            shared,
            kind,
            mount,
        } = probed;
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
        if *kind == EntryKind::Directory && meta.uid() != self.user {
            self.foreign_directory_count = self.foreign_directory_count.saturating_add(1);
            self.first_foreign_directory
                .get_or_insert_with(|| path.to_path_buf());
        }
        // A cloned extent stores its data once, so only the first file that
        // the traversal reaches under the root contributes its bytes.
        let mut counted_before = 0_u64;
        if counts_storage {
            for extent in shared {
                if !self.cloned.insert(*extent) {
                    counted_before = counted_before.saturating_add(extent.1);
                }
            }
        }
        tally.count(*kind, meta, counts_storage, counted_before);
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

/// Reads the metadata of one entry on a filesystem of `device`, and decides
/// whether a mount point stands there. A failed check of the mount point
/// leaves the entry unmeasured and the root partial, because the scan cannot
/// rule out a mount boundary.
pub(crate) fn probe_at(
    path: &Path,
    chosen: bool,
    device: u64,
    check_file_mounts: bool,
) -> io::Result<Probed> {
    let meta = fs::symlink_metadata(path)?;
    let kind = entry_kind(meta.file_type());
    let mut mount = !chosen && meta.dev() != device;
    if !chosen && !mount && (kind == EntryKind::Directory || check_file_mounts) {
        mount = is_mount_root(path)?;
    }
    let shared = if kind == EntryKind::File && !mount {
        clones::shared(path, &meta)
    } else {
        Vec::new()
    };
    Ok(Probed {
        meta,
        shared,
        kind,
        mount,
    })
}

/// Checks the root of a removal, which a bounded measurement may enter even
/// when it is a mount point.
pub(crate) fn mount_at(path: &Path) -> io::Result<bool> {
    let Some(parent) = path.parent() else {
        return Ok(true);
    };
    let meta = fs::symlink_metadata(path)?;
    Ok(meta.dev() != fs::symlink_metadata(parent)?.dev() || is_mount_root(path)?)
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
    /// Counts one entry. With `counts_storage`, its bytes join the sums, less
    /// the `counted_before` bytes of its blocks that another file counted.
    fn count(
        &mut self,
        kind: EntryKind,
        meta: &Metadata,
        counts_storage: bool,
        counted_before: u64,
    ) {
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
            let allocated = meta
                .blocks()
                .saturating_mul(BLOCK_UNIT)
                .saturating_sub(counted_before);
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

    use super::{is_mount_root, measure_entry};

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

    #[test]
    fn a_cloned_block_counts_once_in_one_scan() {
        use std::fs::{self, File};
        use std::os::unix::fs::MetadataExt;

        let dir = TempDir::new().unwrap();
        let source = dir.path().join("a.rlib");
        let bytes: Vec<u8> = (0..512 * 1024_u32).map(|n| (n % 251) as u8).collect();
        fs::write(&source, &bytes).unwrap();
        let opened = File::open(&source).unwrap();
        opened.sync_all().unwrap();
        let created = File::create(dir.path().join("b.rlib")).unwrap();
        if rustix::fs::ioctl_ficlone(&created, &opened).is_err() {
            // This filesystem clones no file, so no block has two holders.
            return;
        }
        created.sync_all().unwrap();
        let one = fs::metadata(&source).unwrap().blocks() * 512;
        let device = fs::metadata(dir.path()).unwrap().dev();
        let both = measure_entry(dir.path(), device).measure;
        assert_eq!(both.files, 2);
        assert_eq!(both.logical_bytes, 2 * bytes.len() as u64);
        // The directory's own blocks join the sum, and they stay far below
        // the second file's.
        assert!(both.allocated_bytes >= one, "{both:?}");
        assert!(both.allocated_bytes < one + one / 2, "{both:?}");
        // A scan of the clone alone counts the clone's blocks in full.
        let alone = measure_entry(&dir.path().join("b.rlib"), device).measure;
        assert_eq!(alone.allocated_bytes, one);
    }
}

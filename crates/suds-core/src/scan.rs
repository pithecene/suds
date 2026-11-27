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
use crate::budget::{Budget, BudgetTracker, ExecutionOutcome};
use crate::clones::{self, Counted, Extent};
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

    let status = if walker.error_count == 0
        && !walker.cancelled
        && walker.outcome() == ExecutionOutcome::Complete
    {
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

/// Shared-storage deduplication state (hard-linked regular files and cloned
/// extents) preserved across completed bounded inspection slices.
#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub(crate) struct ScanDedup {
    pub(crate) linked: HashSet<(u64, u64)>,
    pub(crate) cloned: Counted,
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
        Self::with_budget(Budget::default())
    }
}

impl SharedScan {
    /// Creates a shared scan governed by `budget`.
    pub fn with_budget(budget: Budget) -> Self {
        Self::with_tracker(BudgetTracker::new(budget))
    }

    /// Creates a shared scan sharing an existing `tracker`.
    pub fn with_tracker(tracker: Arc<BudgetTracker>) -> Self {
        SharedScan {
            walker: Walker::with_budget(0, tracker),
        }
    }

    /// Creates a shared scan seeded with prior committed deduplication state
    /// and transactional staging enabled so incomplete trees never pollute
    /// committed deduplication across slices.
    pub(crate) fn with_tracker_and_dedup(tracker: Arc<BudgetTracker>, dedup: ScanDedup) -> Self {
        let mut walker = Walker::with_budget(0, tracker);
        walker.linked = dedup.linked;
        walker.cloned = dedup.cloned;
        walker.staging = true;
        SharedScan { walker }
    }

    /// Commits staged deduplication entries from a completed tree.
    pub(crate) fn commit_dedup(&mut self) {
        self.walker.linked.extend(self.walker.staged_linked.drain());
        let staged = std::mem::take(&mut self.walker.staged_cloned);
        self.walker.cloned.merge(staged);
    }

    /// Discards staged deduplication entries from an incomplete tree.
    pub(crate) fn discard_dedup(&mut self) {
        self.walker.staged_linked.clear();
        self.walker.staged_cloned.clear();
    }

    /// Monotonic count of newly staged hardlink or shared-extent claims.
    pub(crate) fn staged_insertions(&self) -> u64 {
        self.walker.staged_insertions
    }

    /// Extracts only the committed deduplication state for cursor persistence.
    pub(crate) fn into_committed_dedup(self) -> ScanDedup {
        ScanDedup {
            linked: self.walker.linked,
            cloned: self.walker.cloned,
        }
    }

    pub(crate) fn for_removal() -> Self {
        let mut scan = Self::default();
        scan.walker.check_file_mounts = true;
        scan
    }

    /// Whether the scan's budget or cancellation flag has stopped further work.
    pub fn is_stopped(&self) -> bool {
        self.walker.cancelled || self.walker.tracker.is_stopped()
    }

    /// Final outcome of this scan's execution budget.
    pub fn outcome(&self) -> ExecutionOutcome {
        self.walker.outcome()
    }

    pub fn entries_visited(&self) -> u64 {
        self.walker.tracker.entries_visited()
    }

    pub fn directories_deferred(&self) -> u64 {
        self.walker.tracker.directories_deferred()
    }

    pub fn draining_workers(&self) -> usize {
        self.walker.tracker.draining_workers()
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
        if self.is_stopped() {
            return None;
        }
        match fs::symlink_metadata(start) {
            Ok(meta) => self.walker.device = meta.dev(),
            Err(err) => {
                self.walker.record_error(start, &err);
                return None;
            }
        }
        let mut tally = Tally::default();
        let mut set_aside = Vec::new();
        let cancel = Arc::clone(&self.walker.tracker.budget().cancel);
        let rules = Rules { claims, note };
        self.walker
            .walk_within(start, &mut tally, &cancel, Some(rules), &mut set_aside)?;
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

/// Metadata fields that traversal accounting and revision checks read from an
/// entry, obtained either from `symlink_metadata` or macOS `getattrlistbulk`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EntryMeta {
    pub(crate) dev: u64,
    pub(crate) ino: u64,
    pub(crate) nlink: u64,
    pub(crate) uid: u32,
    pub(crate) logical_bytes: u64,
    pub(crate) allocated_bytes: u64,
    pub(crate) mtime: i64,
    pub(crate) mtime_nsec: i64,
}

impl EntryMeta {
    pub(crate) fn from_metadata(meta: &Metadata) -> Self {
        EntryMeta {
            dev: meta.dev(),
            ino: meta.ino(),
            nlink: meta.nlink(),
            uid: meta.uid(),
            logical_bytes: meta.len(),
            allocated_bytes: meta.blocks().saturating_mul(BLOCK_UNIT),
            mtime: meta.mtime(),
            mtime_nsec: meta.mtime_nsec(),
        }
    }
}

/// One entry's metadata, its kind, and whether a mount point stands there.
pub(crate) struct Probed {
    pub(crate) meta: EntryMeta,
    /// Each extent that a regular file shares with another file, or the
    /// reason that the scan could not read the file's extents.
    pub(crate) shared: io::Result<Vec<Extent>>,
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
    /// Hard-linked files counted anywhere in the active slice (including
    /// incomplete trees) so later trees in the same slice never double-count
    /// them, without polluting committed cross-slice state.
    slice_linked: HashSet<(u64, u64)>,
    /// Hard-linked files staged for the active tree until it finishes cleanly.
    staged_linked: HashSet<(u64, u64)>,
    /// The shared extents whose storage the scan has counted under the
    /// root.
    cloned: Counted,
    /// Shared extents counted anywhere in the active slice (including
    /// incomplete trees) to prevent intra-slice double counting.
    slice_cloned: Counted,
    /// Shared extents staged for the active tree until it finishes cleanly.
    staged_cloned: Counted,
    /// Whether new deduplication claims go into staged sets awaiting commit.
    staging: bool,
    /// Monotonic counter of newly staged hardlink or shared-extent claims.
    staged_insertions: u64,
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
    tracker: Arc<BudgetTracker>,
}

impl Walker {
    fn new(device: u64) -> Self {
        Self::with_budget(device, BudgetTracker::new(Budget::default()))
    }

    fn with_budget(device: u64, tracker: Arc<BudgetTracker>) -> Self {
        Walker {
            device,
            check_file_mounts: false,
            linked: HashSet::new(),
            slice_linked: HashSet::new(),
            staged_linked: HashSet::new(),
            cloned: Counted::default(),
            slice_cloned: Counted::default(),
            staged_cloned: Counted::default(),
            staging: false,
            staged_insertions: 0,
            error_count: 0,
            errors: Vec::new(),
            mount_count: 0,
            foreign_directory_count: 0,
            first_foreign_directory: None,
            user: rustix::process::geteuid().as_raw(),
            mounts: Vec::new(),
            cancelled: false,
            tracker,
        }
    }

    fn outcome(&self) -> ExecutionOutcome {
        if self.cancelled {
            return ExecutionOutcome::Cancelled;
        }
        self.tracker.outcome(self.error_count > 0)
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
        let ahead = Ahead::new(
            claims,
            self.device,
            self.check_file_mounts,
            Arc::clone(&self.tracker),
        );
        // Each listing arrives sorted, and the stack takes subdirectories
        // in reverse. The order of the count therefore depends on the tree
        // alone, whichever thread read a directory.
        let mut pending = vec![(start.to_path_buf(), Slot::start(start.to_path_buf()))];
        // Whether the traversal has asked the threads to join, which it asks once.
        let mut asked = false;
        while let Some((dir, slot)) = pending.pop() {
            if cancel.load(Ordering::Relaxed) {
                self.cancelled = true;
                self.tracker.cancel();
                break;
            }
            if self.tracker.is_stopped() {
                if self.tracker.stop_reason() == Some(ExecutionOutcome::Cancelled) {
                    self.cancelled = true;
                }
                break;
            }
            let Some((head, body)) = ahead.body(&slot) else {
                if self.tracker.stop_reason() == Some(ExecutionOutcome::Cancelled) {
                    self.cancelled = true;
                }
                break;
            };
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
            if self.cancelled || self.tracker.is_stopped() {
                if self.tracker.stop_reason() == Some(ExecutionOutcome::Cancelled) {
                    self.cancelled = true;
                }
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
                self.tracker.cancel();
                break;
            }
            if self.tracker.is_stopped() {
                if self.tracker.stop_reason() == Some(ExecutionOutcome::Cancelled) {
                    self.cancelled = true;
                }
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
            let entered = match child.slot.as_ref() {
                Some(slot) => match ahead.head(slot) {
                    Some(head) => Some(head),
                    None => {
                        if self.tracker.stop_reason() == Some(ExecutionOutcome::Cancelled) {
                            self.cancelled = true;
                        }
                        break;
                    }
                },
                None => None,
            };
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
            || meta.nlink <= 1
            || if self.staging {
                let key = (meta.dev, meta.ino);
                if self.linked.contains(&key) || self.slice_linked.contains(&key) {
                    false
                } else {
                    self.slice_linked.insert(key);
                    if self.staged_linked.insert(key) {
                        self.staged_insertions = self.staged_insertions.saturating_add(1);
                    }
                    true
                }
            } else {
                self.linked.insert((meta.dev, meta.ino))
            };
        if *kind == EntryKind::Directory && meta.uid != self.user {
            self.foreign_directory_count = self.foreign_directory_count.saturating_add(1);
            self.first_foreign_directory
                .get_or_insert_with(|| path.to_path_buf());
        }
        // A cloned extent stores its data once, so only the first file that
        // the traversal reaches under the root contributes its bytes.
        let mut counted_before = 0_u64;
        match shared {
            Ok(shared) if counts_storage => {
                for extent in shared {
                    let before = if self.staging {
                        let (_, len) = *extent;
                        let uncovered = self.cloned.uncovered(meta.dev, *extent);
                        let uncovered_len: u64 = uncovered.iter().map(|(_, l)| *l).sum();
                        let in_committed = len.saturating_sub(uncovered_len);
                        let mut in_slice = 0_u64;
                        for sub in uncovered {
                            let fresh = self.slice_cloned.uncovered(meta.dev, sub);
                            let prior = self.slice_cloned.claim(meta.dev, sub);
                            for piece in fresh {
                                self.staged_cloned.claim(meta.dev, piece);
                                self.staged_insertions = self.staged_insertions.saturating_add(1);
                            }
                            in_slice = in_slice.saturating_add(prior);
                        }
                        in_committed.saturating_add(in_slice)
                    } else {
                        self.cloned.claim(meta.dev, *extent)
                    };
                    counted_before = counted_before.saturating_add(before);
                }
            }
            Ok(_) => {}
            // The file's blocks count in full, and the result states
            // that the scan could not read which of them it shares.
            Err(err) => self.record_error(path, err),
        }
        tally.count(*kind, *meta, counts_storage, counted_before);
        Some(EntryRevision::from_entry_meta(*meta, *kind))
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
    crate::bench::record_entry_probe();
    let meta = fs::symlink_metadata(path)?;
    let kind = entry_kind(meta.file_type());
    let mut mount = !chosen && meta.dev() != device;
    if !chosen && !mount && (kind == EntryKind::Directory || check_file_mounts) {
        mount = is_mount_root(path)?;
    }
    let shared = if kind == EntryKind::File && !mount {
        clones::shared(path, &meta)
    } else {
        Ok(Vec::new())
    };
    Ok(Probed {
        meta: EntryMeta::from_metadata(&meta),
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
        meta: EntryMeta,
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
                self.logical = self.logical.saturating_add(meta.logical_bytes);
            }
            let allocated = meta.allocated_bytes.saturating_sub(counted_before);
            self.allocated = self.allocated.saturating_add(allocated);
        }
        let modified = meta.mtime;
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

    #[test]
    fn a_file_that_the_scan_cannot_open_counts_in_full_with_an_error() {
        use std::fs::{self, Permissions};
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        use crate::model::ErrorKind;

        let dir = TempDir::new().unwrap();
        let file = dir.path().join("a.rlib");
        fs::write(&file, vec![7_u8; 64 * 1024]).unwrap();
        fs::set_permissions(&file, Permissions::from_mode(0o000)).unwrap();
        if fs::File::open(&file).is_ok() {
            // An administrator opens every file, so no open fails here.
            return;
        }
        let device = fs::metadata(dir.path()).unwrap().dev();
        let measured = measure_entry(&file, device);
        assert_eq!(measured.measure.files, 1);
        assert_eq!(measured.measure.logical_bytes, 64 * 1024);
        assert_eq!(measured.error_count, 1);
        assert_eq!(measured.errors[0].kind, ErrorKind::PermissionDenied);
        assert_eq!(measured.errors[0].path, file);
    }
}

#[cfg(all(test, target_os = "macos"))]
mod macos_tests {
    use std::fs;
    use std::io::{Seek, SeekFrom, Write};
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;

    use rustix::fs::{CWD, CloneFlags, fclonefileat};
    use tempfile::TempDir;

    use super::measure_entry;

    const MIB: u64 = 1 << 20;

    /// Clones a file, or states that this filesystem clones none.
    fn clone(source: &Path, target: &Path) -> bool {
        let opened = fs::File::open(source).unwrap();
        opened.sync_all().unwrap();
        fclonefileat(&opened, CWD, target, CloneFlags::empty()).is_ok()
    }

    #[test]
    fn a_cloned_file_counts_once_until_it_changes() {
        let dir = TempDir::new().unwrap();
        let device = fs::metadata(dir.path()).unwrap().dev();
        let source = dir.path().join("a.rlib");
        let bytes: Vec<u8> = (0..4 * MIB as u32).map(|n| (n % 251) as u8).collect();
        fs::write(&source, &bytes).unwrap();
        let copy = dir.path().join("b.rlib");
        if !clone(&source, &copy) || !clone(&source, &dir.path().join("c.rlib")) {
            eprintln!("skipped: the filesystem clones no file");
            return;
        }
        // A file with equal content is no clone.
        fs::write(dir.path().join("d.rlib"), &bytes).unwrap();
        let one = fs::metadata(&source).unwrap().blocks() * 512;
        let measured = measure_entry(dir.path(), device);
        assert_eq!(measured.error_count, 0);
        assert_eq!(measured.measure.files, 4);
        assert_eq!(measured.measure.logical_bytes, 4 * bytes.len() as u64);
        let sum = measured.measure.allocated_bytes;
        assert!((2 * one..2 * one + MIB).contains(&sum), "{sum} of {one}");
        // A scan of one clone alone counts it in full.
        let alone = measure_entry(&copy, device).measure;
        assert_eq!(alone.allocated_bytes, one);

        // Each resource fork belongs to one clone alone, so both count.
        let forks = [&copy, &dir.path().join("c.rlib")].map(|file| file.join("..namedfork/rsrc"));
        for (fork, byte) in forks.iter().zip([3u8, 5]) {
            fs::write(fork, vec![byte; MIB as usize]).unwrap();
        }
        let forked = measure_entry(dir.path(), device).measure.allocated_bytes;
        assert!(forked >= sum + 2 * MIB, "{forked} after {sum}");
        for fork in &forks {
            fs::write(fork, []).unwrap();
        }

        // A changed clone shares a part of its blocks, and it counts in full.
        let mut changed = fs::OpenOptions::new().write(true).open(&copy).unwrap();
        changed.seek(SeekFrom::Start(0)).unwrap();
        changed.write_all(&vec![1u8; MIB as usize]).unwrap();
        changed.sync_all().unwrap();
        let sum = measure_entry(dir.path(), device).measure.allocated_bytes;
        assert!((3 * one..3 * one + MIB).contains(&sum), "{sum} of {one}");
    }
}

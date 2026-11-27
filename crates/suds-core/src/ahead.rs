//! Reads directories ahead of a traversal, on several threads.
//!
//! A traversal counts each entry in one order, so its sums and its
//! attribution of a hard-linked file depend on the tree alone. Any thread
//! may read a directory, and the traversal takes each reading in its order.
//! A reading first lists the names and asks the caller's rule, and then
//! reads each entry's metadata. The same threads serve every traversal of
//! the process.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread;
use std::time::Duration;

use crate::budget::{BudgetTracker, apply_background_thread_policy};
use crate::model::EntryKind;
use crate::scan::{Probed, entry_kind, probe_at};

/// The most threads that read ahead.
const WORKER_LIMIT: usize = 8;

/// The count of waiting directories at which the threads join a traversal.
/// A smaller tree costs less to read than the threads cost to wake.
pub(crate) const START_AT: usize = 4;

/// The most entries that the threads hold read and not yet counted. The
/// threads wait above it, so a wide tree cannot fill the memory.
const HELD_LIMIT: usize = 100_000;

/// The names that one directory holds, in byte order, each with its kind
/// as the listing states it.
pub(crate) struct Listing {
    names: Vec<OsString>,
    kinds: Vec<Option<EntryKind>>,
    /// Whether each name holds ASCII alone.
    plain: bool,
}

impl Listing {
    fn kind(&self, dir: &Path, name: &str) -> Option<EntryKind> {
        let found = self
            .names
            .binary_search_by(|held| held.as_os_str().cmp(OsStr::new(name)));
        match found {
            // A filesystem may state no kind in its listing, and the
            // entry's own metadata then decides.
            Ok(at) => self.kinds[at].or_else(|| probed_kind(&dir.join(name))),
            // A filesystem that ignores case finds an entry under another
            // spelling of its name. The filesystem then decides, as it
            // decides for a caller that asks it for the name.
            Err(_) if self.respelled(name) => probed_kind(&dir.join(name)),
            Err(_) => None,
        }
    }

    /// Whether the listing holds another spelling of `name`: a name that
    /// differs from it in case alone.
    fn respelled(&self, name: &str) -> bool {
        self.names.iter().any(|held| {
            let held = held.as_encoded_bytes();
            if held.len() == name.len() {
                return held != name.as_bytes() && held.eq_ignore_ascii_case(name.as_bytes());
            }
            // A letter outside ASCII may fold to an ASCII letter, as the
            // Kelvin sign folds to the letter k.
            !self.plain
                && str::from_utf8(held).is_ok_and(|held| {
                    held.to_lowercase() == name.to_lowercase()
                        || held.to_uppercase() == name.to_uppercase()
                })
        })
    }
}

fn probed_kind(path: &Path) -> Option<EntryKind> {
    fs::symlink_metadata(path)
        .ok()
        .map(|meta| entry_kind(meta.file_type()))
}

/// What a directory holds, by name. A traversal answers from the listing
/// that it read, and any other caller asks the filesystem for each name.
/// Neither follows a symbolic link.
pub(crate) enum Layout<'a> {
    Listed(&'a Path, &'a Listing),
    Asked(&'a Path),
}

impl Layout<'_> {
    fn kind(&self, name: &str) -> Option<EntryKind> {
        match self {
            Layout::Listed(dir, listing) => listing.kind(dir, name),
            Layout::Asked(dir) => probed_kind(&dir.join(name)),
        }
    }

    /// Whether the directory holds a regular file of this name.
    pub(crate) fn file(&self, name: &str) -> bool {
        self.kind(name) == Some(EntryKind::File)
    }

    /// Whether the directory holds a directory of this name.
    pub(crate) fn dir(&self, name: &str) -> bool {
        self.kind(name) == Some(EntryKind::Directory)
    }

    /// Whether a file, a directory, a link, or another entry holds this
    /// name. An absent name gives nothing.
    pub(crate) fn entry(&self, name: &str) -> Option<EntryKind> {
        self.kind(name)
    }
}

/// One entry beneath a traversal's start, as a caller's rule sees it.
pub(crate) struct Entry<'a> {
    pub(crate) path: &'a Path,
    pub(crate) kind: EntryKind,
    /// What the entry holds, when it is a directory.
    pub(crate) own: Layout<'a>,
    /// What the entry's parent directory holds.
    pub(crate) beside: Layout<'a>,
}

/// A rule that claims a directory, so the traversal sets it aside. Any
/// thread may ask it, so it changes nothing and borrows nothing.
pub(crate) type Claims = Arc<dyn for<'a, 'b> Fn(&'a Entry<'b>) -> bool + Send + Sync>;

/// One entry of a directory that a thread read.
pub(crate) struct Child {
    pub(crate) path: PathBuf,
    pub(crate) probed: io::Result<Probed>,
    /// The reading of the entry itself, for a directory that the traversal
    /// may enter.
    pub(crate) slot: Option<Arc<Slot>>,
}

/// The first step of a directory's reading: its names, and the rule's
/// answer about it.
pub(crate) struct Head {
    /// Whether the caller's rule claims the directory, so the traversal
    /// never enters it.
    pub(crate) claimed: bool,
    /// Whether the reader deferred the directory after reaching the
    /// per-directory entry limit.
    pub(crate) deferred: bool,
    listing: Option<Arc<Listing>>,
    /// Pre-probed entry metadata collected during directory listing on macOS,
    /// taken once when building the directory's body.
    preprobed: Mutex<Option<Vec<Option<Probed>>>>,
    /// The error that kept the directory's listing from the traversal.
    pub(crate) unlisted: Option<io::Error>,
    /// The error of each name that the listing could not state.
    pub(crate) unnamed: Vec<io::Error>,
}

impl Head {
    pub(crate) fn layout<'a>(&'a self, dir: &'a Path) -> Layout<'a> {
        match &self.listing {
            Some(listing) => Layout::Listed(dir, listing),
            None => Layout::Asked(dir),
        }
    }
}

/// The second step of a directory's reading: each entry with its metadata.
/// A claimed directory and an unlisted one have no entries here.
pub(crate) struct Body {
    pub(crate) children: Vec<Child>,
    /// Whether the entries count against the limit of held entries.
    weighed: bool,
}

impl Body {
    /// The count that this reading holds against the limit of held entries.
    fn weight(&self) -> usize {
        self.children.len() + 1
    }

    /// The count of directories beneath this one that the traversal may enter.
    pub(crate) fn entered(&self) -> usize {
        self.children
            .iter()
            .filter(|child| child.slot.is_some())
            .count()
    }
}

enum State {
    Waiting,
    Listing,
    Listed(Arc<Head>),
    Probing(Arc<Head>),
    Read(Arc<Head>, Arc<Body>),
}

/// One directory that awaits its reading, or holds it.
pub(crate) struct Slot {
    dir: PathBuf,
    /// The directory's place: for each step from the start, the index
    /// that the parent's listing gives the step. The traversal enters the
    /// directories by ascending place.
    place: Box<[u32]>,
    /// The listing of the parent directory. The traversal's start has none,
    /// and no rule claims the start.
    beside: Option<Arc<Listing>>,
    state: Mutex<State>,
    changed: Condvar,
}

impl Slot {
    pub(crate) fn start(dir: PathBuf) -> Arc<Slot> {
        Slot::new(dir, Box::default(), None)
    }

    fn new(dir: PathBuf, place: Box<[u32]>, beside: Option<Arc<Listing>>) -> Arc<Slot> {
        Arc::new(Slot {
            dir,
            place,
            beside,
            state: Mutex::new(State::Waiting),
            changed: Condvar::new(),
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn set(&self, state: State) {
        *self.state() = state;
        self.changed.notify_all();
    }

    fn wait_bounded<'a>(
        &self,
        state: MutexGuard<'a, State>,
        tracker: &BudgetTracker,
    ) -> Option<MutexGuard<'a, State>> {
        if tracker.is_stopped() {
            return None;
        }
        let (guard, _) = self
            .changed
            .wait_timeout(state, Duration::from_millis(25))
            .unwrap_or_else(PoisonError::into_inner);
        if tracker.is_stopped() {
            return None;
        }
        Some(guard)
    }
}

/// Decrements active worker counts for a traversal and its tracker on drop.
struct ActiveWorker<'a>(&'a Ahead);

impl Drop for ActiveWorker<'_> {
    fn drop(&mut self) {
        self.0.active_workers.fetch_sub(1, Ordering::AcqRel);
        self.0.tracker.worker_exit();
    }
}

/// Returns a directory to its last finished step when a step panics, so
/// the traversal runs the step itself and meets the same panic.
struct Taken<'a>(&'a Slot);

impl Drop for Taken<'_> {
    fn drop(&mut self) {
        let mut state = self.0.state();
        let back = match &*state {
            State::Listing => State::Waiting,
            State::Probing(head) => State::Listed(Arc::clone(head)),
            _ => return,
        };
        *state = back;
        drop(state);
        self.0.changed.notify_all();
    }
}

/// The waiting directories, grouped by parent directory. A key holds the
/// traversal and the parent's place, so the map's order is the order of
/// each traversal's need. Each group keeps its first directory last.
type Waiting = BTreeMap<(u64, Box<[u32]>), Vec<Arc<Slot>>>;

#[derive(Default)]
struct Shared {
    waiting: Waiting,
    /// Each traversal that the threads have joined.
    joined: Vec<Arc<Ahead>>,
}

/// The threads that read ahead, for every traversal of the process.
struct Pool {
    shared: Mutex<Shared>,
    /// Signals a new directory, a counted reading, and a joined traversal.
    changed: Condvar,
    threads: usize,
}

impl Pool {
    /// The pool of the process, which starts its threads at the first call.
    fn get() -> &'static Pool {
        static POOL: OnceLock<Pool> = OnceLock::new();
        static STARTED: OnceLock<()> = OnceLock::new();
        let pool = POOL.get_or_init(|| {
            let count = thread::available_parallelism().map_or(1, usize::from);
            let count = count.min(WORKER_LIMIT);
            Pool {
                shared: Mutex::default(),
                changed: Condvar::new(),
                threads: if count < 2 { 0 } else { count },
            }
        });
        STARTED.get_or_init(|| {
            for _ in 0..pool.threads {
                // A system that refuses a thread leaves the reading to the
                // other threads and to each traversal itself.
                let spawned = thread::Builder::new()
                    .name("suds-read-ahead".to_owned())
                    .spawn(|| pool.work());
                drop(spawned);
            }
        });
        pool
    }

    fn shared(&self) -> MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn work(&self) {
        apply_background_thread_policy();
        let mut shared = self.shared();
        loop {
            match next(&mut shared) {
                Some((ahead, slot)) => {
                    drop(shared);
                    let _active = ActiveWorker(&ahead);
                    if !ahead.tracker.is_stopped() {
                        ahead.list(&slot);
                        if !ahead.tracker.is_stopped() {
                            ahead.probe(&slot);
                        }
                    }
                    drop(_active);
                    drop((ahead, slot));
                    shared = self.shared();
                }
                None => {
                    shared = self
                        .changed
                        .wait(shared)
                        .unwrap_or_else(PoisonError::into_inner);
                }
            }
        }
    }
}

/// Takes the waiting directory that a joined traversal needs first. A
/// traversal that holds its limit of read entries, reached its helper-reader
/// cap, or stopped offers none.
fn next(shared: &mut Shared) -> Option<(Arc<Ahead>, Arc<Slot>)> {
    let Shared { waiting, joined } = shared;
    for ahead in joined.iter() {
        if ahead.tracker.is_stopped() || ahead.held.load(Ordering::Relaxed) >= HELD_LIMIT {
            continue;
        }
        let limit = ahead.helper_limit();
        if limit == 0 || ahead.active_workers.load(Ordering::Acquire) >= limit {
            continue;
        }
        let range = (ahead.id, Box::default())..(ahead.id + 1, Box::default());
        let Some((place, group)) = waiting.range_mut(range).next() else {
            continue;
        };
        let slot = group.pop();
        if group.is_empty() {
            let place = place.clone();
            waiting.remove(&place);
        }
        if let Some(slot) = slot {
            ahead.active_workers.fetch_add(1, Ordering::AcqRel);
            ahead.tracker.worker_enter();
            return Some((Arc::clone(ahead), slot));
        }
    }
    None
}

/// The reading ahead of one traversal.
pub(crate) struct Ahead {
    id: u64,
    claims: Claims,
    device: u64,
    check_file_mounts: bool,
    tracker: Arc<BudgetTracker>,
    /// Whether the threads have joined. Before they join, the traversal
    /// reads each directory itself and no directory waits in the pool.
    joined: AtomicBool,
    held: AtomicUsize,
    active_workers: AtomicUsize,
}

impl Ahead {
    pub(crate) fn new(
        claims: Claims,
        device: u64,
        check_file_mounts: bool,
        tracker: Arc<BudgetTracker>,
    ) -> Arc<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        Arc::new(Ahead {
            id: NEXT.fetch_add(1, Ordering::Relaxed),
            claims,
            device,
            check_file_mounts,
            tracker,
            joined: AtomicBool::new(false),
            held: AtomicUsize::new(0),
            active_workers: AtomicUsize::new(0),
        })
    }

    fn helper_limit(&self) -> usize {
        self.tracker.budget().helper_readers().min(WORKER_LIMIT)
    }

    fn joined(&self) -> bool {
        self.joined.load(Ordering::Relaxed)
    }

    /// Lets the threads join when the traversal's total-reader budget allows
    /// helper readers beyond the caller thread.
    pub(crate) fn join(self: &Arc<Self>, listed: &[Arc<Slot>], now: &Slot, body: &Body) {
        if self.helper_limit() == 0 || self.tracker.is_stopped() {
            return;
        }
        let pool = Pool::get();
        if pool.threads == 0 {
            return;
        }
        let mut shared = pool.shared();
        for slot in listed {
            let parent = &slot.place[..slot.place.len().saturating_sub(1)];
            let group = shared.waiting.entry((self.id, parent.into())).or_default();
            group.push(Arc::clone(slot));
        }
        offer(&mut shared.waiting, self.id, now, body);
        shared.joined.push(Arc::clone(self));
        self.joined.store(true, Ordering::Relaxed);
        drop(shared);
        pool.changed.notify_all();
    }

    /// Ends the reading ahead. It drops queued directories immediately and
    /// keeps any mid-directory worker counted in `tracker.draining_workers()`.
    pub(crate) fn end(&self) {
        if !self.joined() {
            return;
        }
        let pool = Pool::get();
        let mut shared = pool.shared();
        shared.joined.retain(|ahead| ahead.id != self.id);
        let mut own = shared.waiting.split_off(&(self.id, Box::default()));
        let later = own.split_off(&(self.id + 1, Box::default()));
        shared.waiting.extend(later);
        self.joined.store(false, Ordering::Relaxed);
        drop(shared);
        pool.changed.notify_all();
    }

    /// The first step of a directory's reading. Returns `None` if stopped.
    pub(crate) fn head(&self, slot: &Slot) -> Option<Arc<Head>> {
        loop {
            if self.tracker.is_stopped() {
                return None;
            }
            self.list(slot);
            let mut state = slot.state();
            loop {
                match &*state {
                    State::Listed(head) | State::Probing(head) | State::Read(head, _) => {
                        return Some(Arc::clone(head));
                    }
                    // The thread that listed the directory panicked or stopped.
                    State::Waiting => break,
                    State::Listing => {
                        state = slot.wait_bounded(state, &self.tracker)?;
                    }
                }
            }
        }
    }

    /// Both steps of a directory's reading. Returns `None` if stopped.
    pub(crate) fn body(&self, slot: &Slot) -> Option<(Arc<Head>, Arc<Body>)> {
        loop {
            if self.tracker.is_stopped() {
                return None;
            }
            self.list(slot);
            if self.tracker.is_stopped() {
                return None;
            }
            self.probe(slot);
            let mut state = slot.state();
            loop {
                match &*state {
                    State::Read(head, body) => return Some((Arc::clone(head), Arc::clone(body))),
                    // The thread that ran a step panicked or stopped.
                    State::Waiting | State::Listed(_) => break,
                    State::Listing | State::Probing(_) => {
                        state = slot.wait_bounded(state, &self.tracker)?;
                    }
                }
            }
        }
    }

    /// Records that the traversal has counted a reading, so the threads may
    /// read further ahead.
    pub(crate) fn counted(&self, body: &Body) {
        if !body.weighed {
            return;
        }
        let before = self.held.fetch_sub(body.weight(), Ordering::Relaxed);
        if before >= HELD_LIMIT && before - body.weight() < HELD_LIMIT {
            let pool = Pool::get();
            drop(pool.shared());
            pool.changed.notify_all();
        }
    }

    /// Runs the first step for a directory that no thread has taken.
    fn list(&self, slot: &Slot) {
        {
            let mut state = slot.state();
            if !matches!(*state, State::Waiting) {
                return;
            }
            *state = State::Listing;
        }
        let taken = Taken(slot);
        let head = Arc::new(self.listed(slot));
        slot.set(State::Listed(head));
        drop(taken);
    }

    /// Runs the second step for a listed directory that no thread has taken.
    fn probe(&self, slot: &Slot) {
        let head = {
            let mut state = slot.state();
            let State::Listed(head) = &*state else {
                return;
            };
            let head = Arc::clone(head);
            *state = State::Probing(Arc::clone(&head));
            head
        };
        let taken = Taken(slot);
        let mut body = self.probed(slot, &head);
        // The traversal counts no reading of a claimed directory.
        body.weighed = self.joined() && !head.claimed;
        if body.weighed {
            self.held.fetch_add(body.weight(), Ordering::Relaxed);
            let pool = Pool::get();
            let mut shared = pool.shared();
            // A traversal that has ended takes no further directory.
            let added = if self.joined() {
                offer(&mut shared.waiting, self.id, slot, &body)
            } else {
                0
            };
            drop(shared);
            match added {
                0 => {}
                1 => pool.changed.notify_one(),
                _ => pool.changed.notify_all(),
            }
        }
        slot.set(State::Read(head, Arc::new(body)));
        drop(taken);
    }

    fn listed(&self, slot: &Slot) -> Head {
        let dir = slot.dir.as_path();
        let mut head = Head {
            claimed: false,
            deferred: false,
            listing: None,
            preprobed: Mutex::new(None),
            unlisted: None,
            unnamed: Vec::new(),
        };
        match list_bounded(
            dir,
            self.device,
            self.tracker.budget().max_dir_entries,
            &self.tracker,
        ) {
            Ok(ListOutcome::Listed(listing, preprobed, unnamed)) => {
                head.listing = Some(Arc::new(listing));
                head.preprobed = Mutex::new(preprobed);
                head.unnamed = unnamed;
            }
            Ok(ListOutcome::Deferred(limit)) => {
                head.deferred = true;
                self.tracker.record_deferred_directory();
                head.unlisted = Some(io::Error::other(format!(
                    "directory exceeded per-directory entry limit of {limit}"
                )));
            }
            Ok(ListOutcome::Stopped) => {
                return head;
            }
            Err(err) => head.unlisted = Some(err),
        }
        if !head.deferred
            && let (Some(beside), Some(parent)) = (&slot.beside, dir.parent())
        {
            let entry = Entry {
                path: dir,
                kind: EntryKind::Directory,
                own: head.layout(dir),
                beside: Layout::Listed(parent, beside),
            };
            head.claimed = (self.claims)(&entry);
            if head.claimed {
                *head
                    .preprobed
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = None;
            }
        }
        head
    }

    fn probed(&self, slot: &Slot, head: &Head) -> Body {
        let mut body = Body {
            children: Vec::new(),
            weighed: false,
        };
        let Some(listing) = head
            .listing
            .as_ref()
            .filter(|_| !head.claimed && !head.deferred)
        else {
            return body;
        };
        let mut preprobed = head
            .preprobed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .map(Vec::into_iter);
        let dir = slot.dir.as_path();
        body.children.reserve(listing.names.len().min(4096));
        for (index, name) in listing.names.iter().enumerate() {
            if !self.tracker.claim_entry() {
                break;
            }
            let path = dir.join(name);
            let cached = preprobed.as_mut().and_then(|iter| iter.next()).flatten();
            let probed = match cached {
                Some(probed) => Ok(probed),
                None => probe_at(&path, false, self.device, self.check_file_mounts),
            };
            let enters = probed
                .as_ref()
                .is_ok_and(|probed| probed.kind == EntryKind::Directory && !probed.mount);
            let slot = enters.then(|| {
                let index = u32::try_from(index).unwrap_or(u32::MAX);
                let place = slot.place.iter().copied().chain([index]).collect();
                Slot::new(path.clone(), place, Some(Arc::clone(listing)))
            });
            body.children.push(Child { path, probed, slot });
        }
        body
    }
}

/// Queues the directories beneath a read one, and returns their count.
fn offer(waiting: &mut Waiting, id: u64, slot: &Slot, body: &Body) -> usize {
    let beneath: Vec<Arc<Slot>> = body
        .children
        .iter()
        .rev()
        .filter_map(|child| child.slot.clone())
        .collect();
    let count = beneath.len();
    if count > 0 {
        waiting.insert((id, slot.place.clone()), beneath);
    }
    count
}

enum ListOutcome {
    Listed(Listing, Option<Vec<Option<Probed>>>, Vec<io::Error>),
    Deferred(usize),
    Stopped,
}

/// Lists a directory's names in byte order while enforcing per-directory entry
/// limits and cancellation before allocating or probing an oversized directory.
fn list_bounded(
    dir: &Path,
    device: u64,
    max_entries: Option<usize>,
    tracker: &BudgetTracker,
) -> io::Result<ListOutcome> {
    #[cfg(target_os = "macos")]
    {
        match crate::bulk_macos::read_dir_bulk(dir, device, max_entries, tracker)? {
            crate::bulk_macos::BulkOutcome::Listed(entries) => {
                let mut names = Vec::with_capacity(entries.len());
                let mut kinds = Vec::with_capacity(entries.len());
                let mut preprobed = Vec::with_capacity(entries.len());
                for entry in entries {
                    names.push(entry.name);
                    kinds.push(entry.kind);
                    preprobed.push(entry.probed);
                }
                let plain = names.iter().all(|name| name.as_encoded_bytes().is_ascii());
                let listing = Listing {
                    names,
                    kinds,
                    plain,
                };
                return Ok(ListOutcome::Listed(listing, Some(preprobed), Vec::new()));
            }
            crate::bulk_macos::BulkOutcome::Deferred(limit) => {
                return Ok(ListOutcome::Deferred(limit));
            }
            crate::bulk_macos::BulkOutcome::Stopped => return Ok(ListOutcome::Stopped),
            crate::bulk_macos::BulkOutcome::Unsupported => {}
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = device;
    let mut found = Vec::new();
    let mut unnamed = Vec::new();
    let mut seen = 0_usize;
    for entry in fs::read_dir(dir)? {
        if seen.is_multiple_of(64) && tracker.is_stopped() {
            return Ok(ListOutcome::Stopped);
        }
        seen = seen.saturating_add(1);
        if let Some(limit) = max_entries
            && seen > limit
        {
            return Ok(ListOutcome::Deferred(limit));
        }
        match entry {
            Ok(entry) => {
                let kind = entry.file_type().ok().map(entry_kind);
                found.push((entry.file_name(), kind));
            }
            Err(err) => unnamed.push(err),
        }
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    let (names, kinds): (Vec<OsString>, Vec<_>) = found.into_iter().unzip();
    let plain = names.iter().all(|name| name.as_encoded_bytes().is_ascii());
    let listing = Listing {
        names,
        kinds,
        plain,
    };
    Ok(ListOutcome::Listed(listing, None, unnamed))
}

#[cfg(test)]
fn list(dir: &Path) -> io::Result<(Listing, Vec<io::Error>)> {
    let tracker = BudgetTracker::new(crate::budget::Budget::default());
    match list_bounded(dir, 0, None, &tracker)? {
        ListOutcome::Listed(listing, _, unnamed) => Ok((listing, unnamed)),
        ListOutcome::Deferred(_) | ListOutcome::Stopped => {
            Err(io::Error::other("unexpected stop in unbounded list"))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::MetadataExt;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use tempfile::TempDir;

    use crate::scan::SharedScan;

    const GROUPS: usize = 12;
    const PARTS: usize = 6;

    /// A tree wide enough for the threads to join: twelve directories of six
    /// directories, each with files of 100, 200, and 300 bytes. The first
    /// file of each group has a second link in the next group.
    fn wide() -> TempDir {
        let dir = TempDir::new().unwrap();
        for group in 0..GROUPS {
            for part in 0..PARTS {
                let at = dir.path().join(format!("d{group:02}/s{part}"));
                fs::create_dir_all(&at).unwrap();
                for (file, size) in [100, 200, 300].into_iter().enumerate() {
                    fs::write(at.join(format!("f{file}")), vec![0_u8; size]).unwrap();
                }
            }
        }
        for group in 0..GROUPS {
            let next = (group + 1) % GROUPS;
            fs::hard_link(
                dir.path().join(format!("d{group:02}/s0/f0")),
                dir.path().join(format!("d{next:02}/s1/link")),
            )
            .unwrap();
        }
        dir
    }

    fn named(name: &'static str) -> super::Claims {
        Arc::new(move |entry| entry.path.file_name().is_some_and(|held| held == name))
    }

    #[test]
    fn a_wide_tree_counts_each_entry_once() {
        let dir = wide();
        let mut scan = SharedScan::default();
        let whole = scan.measure(dir.path(), Arc::new(|_| false)).unwrap();
        assert_eq!(whole.measure.directories, 1 + 12 + 72);
        assert_eq!(whole.measure.files, 72 * 3 + 12);
        // A second link adds no bytes to the tree that holds both links.
        assert_eq!(whole.measure.logical_bytes, 72 * 600);
        assert!(whole.set_aside.is_empty());
        assert_eq!(scan.error_count(), 0);
    }

    #[test]
    fn a_wide_tree_sets_directories_aside_in_the_order_of_their_names() {
        let dir = wide();
        let mut scan = SharedScan::default();
        let bounded = scan.measure(dir.path(), named("s3")).unwrap();
        let expected: Vec<PathBuf> = (0..GROUPS)
            .map(|group| dir.path().join(format!("d{group:02}/s3")))
            .collect();
        assert_eq!(bounded.set_aside, expected);
        assert_eq!(bounded.measure.directories, 1 + 12 + 60);
        assert_eq!(bounded.measure.logical_bytes, 60 * 600);
    }

    #[test]
    fn a_linked_file_counts_toward_the_first_tree_that_reaches_it() {
        let dir = wide();
        let group = |index: usize| dir.path().join(format!("d{index:02}"));
        let measure = |scan: &mut SharedScan, at: &Path| {
            let bounded = scan.measure(at, Arc::new(|_| false)).unwrap();
            bounded.measure.logical_bytes
        };
        // Each group holds 3600 bytes of its own files and a link to the
        // first file of the group before it, of 100 bytes. The group that a
        // scan measures first counts the bytes of a file that both hold.
        let mut scan = SharedScan::default();
        assert_eq!(measure(&mut scan, &group(1)), 3600 + 100);
        assert_eq!(measure(&mut scan, &group(0)), 3600 - 100 + 100);
        let mut scan = SharedScan::default();
        assert_eq!(measure(&mut scan, &group(0)), 3600 + 100);
        assert_eq!(measure(&mut scan, &group(1)), 3600);
    }

    #[test]
    fn a_listing_and_the_filesystem_state_the_same_kinds() {
        let dir = wide();
        let at = dir.path().join("d00");
        let (listing, unnamed) = super::list(&at).unwrap();
        assert!(unnamed.is_empty());
        let listed = super::Layout::Listed(&at, &listing);
        let asked = super::Layout::Asked(&at);
        for name in ["s0", "s5", "absent"] {
            assert_eq!(listed.entry(name), asked.entry(name));
        }
        assert!(listed.dir("s0") && !listed.file("s0") && listed.entry("absent").is_none());
        let part = at.join("s1");
        let (listing, _) = super::list(&part).unwrap();
        assert!(super::Layout::Listed(&part, &listing).file("link"));
    }

    /// A filesystem that ignores case finds `.git` under the name `.GIT`,
    /// so a listing with another spelling asks the filesystem.
    #[test]
    fn a_listing_with_another_spelling_of_a_name_asks_the_filesystem() {
        let dir = TempDir::new().unwrap();
        for name in [".GIT", "\u{17f}tats", "trim.txt", "caf\u{e9}"] {
            fs::create_dir(dir.path().join(name)).unwrap();
        }
        let (listing, _) = super::list(dir.path()).unwrap();
        assert!(listing.respelled(".git"));
        // The long s folds to the letter s.
        assert!(listing.respelled("stats"));
        assert!(!listing.respelled("trim.txt"));
        assert!(!listing.respelled("README"));
        let listed = super::Layout::Listed(dir.path(), &listing);
        let asked = super::Layout::Asked(dir.path());
        for name in [".git", "stats", "trim.txt", "README"] {
            assert_eq!(listed.entry(name), asked.entry(name));
        }
    }

    #[test]
    fn a_traversal_reads_the_metadata_of_one_directory_at_a_time_before_the_threads_join() {
        let dir = wide();
        let device = fs::metadata(dir.path()).unwrap().dev();
        let tracker = crate::budget::BudgetTracker::new(crate::budget::Budget {
            max_total_readers: 4,
            ..crate::budget::Budget::default()
        });
        let ahead = super::Ahead::new(Arc::new(|_| false), device, false, tracker);
        let start = super::Slot::start(dir.path().to_path_buf());
        let (_, body) = ahead.body(&start).unwrap();
        for child in &body.children {
            let slot = child.slot.as_ref().unwrap();
            assert!(!ahead.head(slot).unwrap().claimed);
            // The first step lists the directory and reads no entry of it.
            assert!(matches!(*slot.state(), super::State::Listed(_)));
        }
    }
}

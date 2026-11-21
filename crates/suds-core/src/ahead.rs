//! Reads directories ahead of a traversal, on several threads.
//!
//! A traversal counts each entry in one order, so that its sums, its sampled
//! errors, and its attribution of a hard-linked file depend on the tree
//! alone. The reading of a directory has no such order. This module lists a
//! directory and reads its entries' metadata on any thread, and the
//! traversal takes each result in its own order. One set of threads serves
//! every traversal of the process.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread;

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
}

impl Listing {
    fn kind(&self, dir: &Path, name: &str) -> Option<EntryKind> {
        let at = self
            .names
            .binary_search_by(|held| held.as_os_str().cmp(OsStr::new(name)))
            .ok()?;
        // A filesystem may state no kind in its listing, and the entry's own
        // metadata then decides.
        self.kinds[at].or_else(|| probed_kind(&dir.join(name)))
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

/// What one reading of a directory found.
pub(crate) struct Reading {
    /// Whether the caller's rule claims the directory. A claimed directory
    /// has no entries here, because the traversal never enters it.
    pub(crate) claimed: bool,
    pub(crate) listing: Option<Arc<Listing>>,
    /// The error that kept the directory's listing from the traversal.
    pub(crate) unlisted: Option<io::Error>,
    /// The error of each name that the listing could not state.
    pub(crate) unnamed: Vec<io::Error>,
    pub(crate) children: Vec<Child>,
    /// Whether the reading counts against the limit of held entries.
    weighed: bool,
}

impl Reading {
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

    pub(crate) fn layout<'a>(&'a self, dir: &'a Path) -> Layout<'a> {
        match &self.listing {
            Some(listing) => Layout::Listed(dir, listing),
            None => Layout::Asked(dir),
        }
    }
}

enum State {
    Waiting,
    Running,
    Read(Arc<Reading>),
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
    read: Condvar,
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
            read: Condvar::new(),
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Takes the reading for the calling thread, unless another thread has it.
    fn take(&self) -> bool {
        let mut state = self.state();
        let waiting = matches!(*state, State::Waiting);
        if waiting {
            *state = State::Running;
        }
        waiting
    }
}

/// Returns a taken directory to the waiting ones when its reading panics,
/// so the traversal reads it itself and meets the same panic.
struct Taken<'a>(&'a Slot);

impl Drop for Taken<'_> {
    fn drop(&mut self) {
        let mut state = self.0.state();
        if matches!(*state, State::Running) {
            *state = State::Waiting;
            drop(state);
            self.0.read.notify_all();
        }
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
        let mut shared = self.shared();
        loop {
            match next(&mut shared) {
                Some((ahead, slot)) => {
                    drop(shared);
                    if slot.take() {
                        ahead.fill(&slot);
                    }
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
/// traversal that holds its limit of read entries offers none.
fn next(shared: &mut Shared) -> Option<(Arc<Ahead>, Arc<Slot>)> {
    let Shared { waiting, joined } = shared;
    for ahead in joined.iter() {
        if ahead.held.load(Ordering::Relaxed) >= HELD_LIMIT {
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
    /// Whether the threads have joined. Before they join, the traversal
    /// reads each directory itself and no directory waits in the pool.
    joined: AtomicBool,
    held: AtomicUsize,
}

impl Ahead {
    pub(crate) fn new(claims: Claims, device: u64, check_file_mounts: bool) -> Arc<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        Arc::new(Ahead {
            id: NEXT.fetch_add(1, Ordering::Relaxed),
            claims,
            device,
            check_file_mounts,
            joined: AtomicBool::new(false),
            held: AtomicUsize::new(0),
        })
    }

    pub(crate) fn joined(&self) -> bool {
        self.joined.load(Ordering::Relaxed)
    }

    /// Lets the threads join, and hands them the directories that wait
    /// beneath each of `read`, the directories that the traversal has read
    /// and has yet to count.
    pub(crate) fn join(self: &Arc<Self>, read: &[(&Slot, &Reading)]) {
        let pool = Pool::get();
        if pool.threads == 0 {
            return;
        }
        let mut shared = pool.shared();
        for (slot, reading) in read {
            offer(&mut shared.waiting, self.id, slot, reading);
        }
        shared.joined.push(Arc::clone(self));
        self.joined.store(true, Ordering::Relaxed);
        drop(shared);
        pool.changed.notify_all();
    }

    /// Ends the reading ahead. A thread finishes the directory that it
    /// reads, and its result goes unused.
    pub(crate) fn end(&self) {
        if !self.joined() {
            return;
        }
        let mut shared = Pool::get().shared();
        shared.joined.retain(|ahead| ahead.id != self.id);
        let later = shared.waiting.split_off(&(self.id, Box::default()));
        let mut later = later;
        let rest = later.split_off(&(self.id + 1, Box::default()));
        shared.waiting.extend(rest);
        self.joined.store(false, Ordering::Relaxed);
    }

    /// The reading of a directory. The traversal reads a directory that no
    /// thread has taken, and waits for one that a thread reads.
    pub(crate) fn reading(&self, slot: &Slot) -> Arc<Reading> {
        loop {
            if slot.take() {
                self.fill(slot);
            }
            let mut state = slot.state();
            loop {
                match &*state {
                    State::Read(reading) => return Arc::clone(reading),
                    // The thread that read the directory panicked.
                    State::Waiting => break,
                    State::Running => {
                        state = slot
                            .read
                            .wait(state)
                            .unwrap_or_else(PoisonError::into_inner);
                    }
                }
            }
        }
    }

    /// Records that the traversal has counted a reading, so the threads may
    /// read further ahead.
    pub(crate) fn counted(&self, reading: &Reading) {
        if !reading.weighed {
            return;
        }
        let before = self.held.fetch_sub(reading.weight(), Ordering::Relaxed);
        if before >= HELD_LIMIT && before - reading.weight() < HELD_LIMIT {
            let pool = Pool::get();
            drop(pool.shared());
            pool.changed.notify_all();
        }
    }

    fn fill(&self, slot: &Slot) {
        let taken = Taken(slot);
        let mut reading = self.read(slot);
        reading.weighed = self.joined();
        let reading = Arc::new(reading);
        if reading.weighed {
            self.held.fetch_add(reading.weight(), Ordering::Relaxed);
            let pool = Pool::get();
            let mut shared = pool.shared();
            // A traversal that has ended takes no further directory.
            let added = if self.joined() {
                offer(&mut shared.waiting, self.id, slot, &reading)
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
        *slot.state() = State::Read(reading);
        drop(taken);
        slot.read.notify_all();
    }

    fn read(&self, slot: &Slot) -> Reading {
        let dir = slot.dir.as_path();
        let mut reading = Reading {
            claimed: false,
            listing: None,
            unlisted: None,
            unnamed: Vec::new(),
            children: Vec::new(),
            weighed: false,
        };
        match list(dir) {
            Ok((listing, unnamed)) => {
                reading.listing = Some(Arc::new(listing));
                reading.unnamed = unnamed;
            }
            Err(err) => reading.unlisted = Some(err),
        }
        if let (Some(beside), Some(parent)) = (&slot.beside, dir.parent()) {
            let entry = Entry {
                path: dir,
                kind: EntryKind::Directory,
                own: reading.layout(dir),
                beside: Layout::Listed(parent, beside),
            };
            reading.claimed = (self.claims)(&entry);
        }
        let Some(listing) = reading.listing.clone() else {
            return reading;
        };
        if reading.claimed {
            return reading;
        }
        reading.children.reserve_exact(listing.names.len());
        for (index, name) in listing.names.iter().enumerate() {
            let path = dir.join(name);
            let probed = probe_at(&path, false, self.device, self.check_file_mounts);
            let enters = probed
                .as_ref()
                .is_ok_and(|probed| probed.kind == EntryKind::Directory && !probed.mount);
            let slot = enters.then(|| {
                let index = u32::try_from(index).unwrap_or(u32::MAX);
                let place = slot.place.iter().copied().chain([index]).collect();
                Slot::new(path.clone(), place, Some(Arc::clone(&listing)))
            });
            reading.children.push(Child { path, probed, slot });
        }
        reading
    }
}

/// Queues the directories beneath a read one, and returns their count.
fn offer(waiting: &mut Waiting, id: u64, slot: &Slot, reading: &Reading) -> usize {
    let beneath: Vec<Arc<Slot>> = reading
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

/// Lists a directory's names in byte order, with the error of each name
/// that the listing could not state.
fn list(dir: &Path) -> io::Result<(Listing, Vec<io::Error>)> {
    let mut found = Vec::new();
    let mut unnamed = Vec::new();
    for entry in fs::read_dir(dir)? {
        match entry {
            Ok(entry) => {
                let kind = entry.file_type().ok().map(entry_kind);
                found.push((entry.file_name(), kind));
            }
            Err(err) => unnamed.push(err),
        }
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    let (names, kinds) = found.into_iter().unzip();
    Ok((Listing { names, kinds }, unnamed))
}

#[cfg(test)]
mod tests {
    use std::fs;
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
}

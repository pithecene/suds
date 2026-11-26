//! Seeds a checkout's build state from another checkout of its repository.
//!
//! A new linked checkout holds no build state, so its first build compiles
//! each dependency again. This module clones the Cargo build state of a
//! built checkout into a checkout that holds none. A clone shares its
//! blocks with its source until one of the two changes. A seed changes no
//! existing file and copies no block: where the filesystem clones no file,
//! it writes nothing.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use std::sync::Arc;
use std::time::SystemTime;

use rustix::fs::OFlags;
use serde::{Deserialize, Serialize};

use crate::budget::{Budget, BudgetTracker};
use crate::development::{self, Ecosystem};
use crate::organize::journal::{Event, Journal, JournalError, RunStatus};
use crate::organize::native::NativePath;
use crate::organize::plan::new_id;
use crate::reclaim::git::Git;
use crate::scan::SharedScan;

/// The schema tag of a seed plan.
pub const SEED_PLAN_SCHEMA: &str = "suds.seed-plan/v1";

/// The schema tag of a seed run's result.
pub const SEED_RUN_SCHEMA: &str = "suds.seed-run/v1";

/// The unit of `st_blocks` on Linux and on macOS.
const BLOCK_UNIT: u64 = 512;

/// One directory of build state that the plan clones.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeedClone {
    pub from: NativePath,
    pub to: NativePath,
    pub ecosystem: Ecosystem,
    /// The allocated bytes of the source, which both directories share
    /// after the clone.
    pub shared_bytes: u64,
}

/// A checkout or a directory that the plan takes nothing from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Passed {
    pub path: NativePath,
    pub reason: String,
}

/// What a seed would clone into one checkout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeedPlan {
    pub schema: String,
    pub id: String,
    /// The checkout that receives the build state.
    pub destination: NativePath,
    /// The checkout whose build state the plan clones, or none when no
    /// checkout of the repository offers any.
    pub source: Option<NativePath>,
    pub clones: Vec<SeedClone>,
    pub passed: Vec<Passed>,
}

impl SeedPlan {
    pub fn shared_bytes(&self) -> u64 {
        self.clones
            .iter()
            .fold(0, |sum, clone| sum.saturating_add(clone.shared_bytes))
    }
}

/// The longest lock file that a plan compares.
const LOCK_LIMIT: u64 = 16 * 1024 * 1024;

/// The build state of one checkout that could seed another.
struct Offer {
    checkout: PathBuf,
    clones: Vec<SeedClone>,
    /// The count of directories whose lock file equals the destination's,
    /// so that the destination's build asks for the same dependencies.
    same_locks: usize,
    bytes: u64,
    newest: Option<String>,
}

/// Whether the two directories hold a `Cargo.lock` of the same bytes. The
/// lock file names each dependency's version, and with it each artifact
/// that the destination's build can take from the clone.
fn same_lock(a: &Path, b: &Path) -> bool {
    let read = |dir: &Path| {
        let path = dir.join("Cargo.lock");
        let meta = fs::symlink_metadata(&path).ok()?;
        if !meta.is_file() || meta.len() > LOCK_LIMIT {
            return None;
        }
        fs::read(&path).ok()
    };
    match (read(a), read(b)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// Plans the seed of the checkout at `destination`. With `from`, the plan
/// takes build state from that checkout alone. Without it, the plan takes
/// the checkout of the same repository whose build state changed last,
/// bounded by [`Budget::for_seeding`].
pub fn plan_seed(destination: &Path, from: Option<&Path>, git: &Git) -> Result<SeedPlan, String> {
    plan_seed_with_budget(destination, from, git, &Budget::for_seeding())
}

/// Like [`plan_seed`], bounded by `budget` so sibling worktree enumeration
/// never scans an unbounded number of checkouts.
pub fn plan_seed_with_budget(
    destination: &Path,
    from: Option<&Path>,
    git: &Git,
    budget: &Budget,
) -> Result<SeedPlan, String> {
    plan_seed_with_tracker(destination, from, git, BudgetTracker::new(budget.clone()))
}

/// Like [`plan_seed_with_budget`], sharing an existing `tracker` across
/// planning and subsequent cloning.
pub fn plan_seed_with_tracker(
    destination: &Path,
    from: Option<&Path>,
    git: &Git,
    tracker: Arc<BudgetTracker>,
) -> Result<SeedPlan, String> {
    if !development::is_checkout(destination) {
        return Err(format!(
            "{} holds no checkout, so Suds seeds nothing there",
            destination.display()
        ));
    }
    let checkouts = git.worktrees(destination)?;
    let mut passed = Vec::new();
    let candidates: Vec<PathBuf> = match from {
        Some(from) => {
            if !checkouts.iter().any(|checkout| checkout == from) {
                return Err(format!(
                    "{} is no checkout of the repository of {}",
                    from.display(),
                    destination.display()
                ));
            }
            vec![from.to_path_buf()]
        }
        None => shortlist_candidates(
            checkouts,
            destination,
            tracker.budget().max_seed_candidates,
            &mut passed,
        ),
    };
    let mut offers = Vec::new();
    for checkout in candidates {
        if checkout == destination {
            continue;
        }
        if tracker.is_stopped() {
            passed.push(Passed {
                path: NativePath::new(checkout),
                reason: "seed selection stopped because the execution budget ended".to_owned(),
            });
            continue;
        }
        let offer = offer_of(&checkout, destination, &tracker, &mut passed);
        if !offer.clones.is_empty() {
            offers.push(offer);
        }
    }
    // A build takes an artifact only for the same version of a dependency,
    // so a checkout with the destination's lock file comes first. A larger
    // source holds more and costs nothing, because a clone occupies no space.
    offers.sort_by(|a, b| {
        b.same_locks
            .cmp(&a.same_locks)
            .then(b.bytes.cmp(&a.bytes))
            .then(b.newest.cmp(&a.newest))
            .then(a.checkout.cmp(&b.checkout))
    });
    let mut offers = offers.into_iter();
    let chosen = offers.next();
    for other in offers {
        passed.push(Passed {
            path: NativePath::new(other.checkout),
            reason: "the plan takes the build state of another checkout".to_owned(),
        });
    }
    let (source, clones) = match chosen {
        Some(offer) => (Some(NativePath::new(offer.checkout)), offer.clones),
        None => (None, Vec::new()),
    };
    Ok(SeedPlan {
        schema: SEED_PLAN_SCHEMA.to_owned(),
        id: new_id("seed"),
        destination: NativePath::new(destination.to_path_buf()),
        source,
        clones,
        passed,
    })
}

/// Selects a bounded shortlist of sibling checkouts without traversing every
/// sibling's tree: the main checkout comes first, followed by siblings ranked
/// cheaply by `target` (or checkout) modification time.
fn shortlist_candidates(
    checkouts: Vec<PathBuf>,
    destination: &Path,
    limit: usize,
    passed: &mut Vec<Passed>,
) -> Vec<PathBuf> {
    let mut siblings: Vec<(bool, Option<SystemTime>, PathBuf)> = checkouts
        .into_iter()
        .enumerate()
        .filter(|(_, checkout)| checkout != destination)
        .map(|(index, checkout)| {
            let mtime = fs::symlink_metadata(checkout.join("target"))
                .or_else(|_| fs::symlink_metadata(&checkout))
                .ok()
                .and_then(|meta| meta.modified().ok());
            (index == 0, mtime, checkout)
        })
        .collect();
    if limit == 0 || siblings.len() <= limit {
        return siblings.into_iter().map(|(_, _, path)| path).collect();
    }
    siblings.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)));
    let deferred = siblings.split_off(limit);
    for (_, _, checkout) in deferred {
        passed.push(Passed {
            path: NativePath::new(checkout),
            reason: format!("seed candidate limit of {limit} checkouts reached"),
        });
    }
    siblings.into_iter().map(|(_, _, path)| path).collect()
}

/// The build state of `checkout` that the destination can take, with the
/// reason for each directory that it cannot.
fn offer_of(
    checkout: &Path,
    destination: &Path,
    tracker: &std::sync::Arc<crate::budget::BudgetTracker>,
    passed: &mut Vec<Passed>,
) -> Offer {
    let mut offer = Offer {
        checkout: checkout.to_path_buf(),
        clones: Vec::new(),
        same_locks: 0,
        bytes: 0,
        newest: None,
    };
    let (states, unread, first) =
        development::build_states_with_tracker(checkout, std::sync::Arc::clone(tracker));
    if let Some(first) = first {
        // An entry that the scan could not read may hold build state, so
        // the plan names what it could not establish.
        passed.push(Passed {
            path: NativePath::new(checkout.to_path_buf()),
            reason: format!(
                "Suds could not read {unread} entries beneath it, the first {}: {}",
                first.path.display(),
                first.message
            ),
        });
    }
    for state in states {
        let pass = |reason: String, passed: &mut Vec<Passed>| {
            passed.push(Passed {
                path: NativePath::new(state.path.clone()),
                reason,
            });
        };
        if state.ecosystem != Ecosystem::Cargo {
            pass(
                format!(
                    "Suds seeds the build state of Cargo alone, and this is the build state of {}",
                    state.ecosystem.label()
                ),
                passed,
            );
            continue;
        }
        let Ok(relative) = state.path.strip_prefix(checkout) else {
            continue;
        };
        let to = destination.join(relative);
        if let Some(reason) = unclear(&state.survey) {
            pass(reason, passed);
            continue;
        }
        match refusal(&state.path, &to, destination) {
            Some(reason) => pass(reason, passed),
            None => {
                if let (Some(source), Some(target)) = (state.path.parent(), to.parent())
                    && same_lock(source, target)
                {
                    offer.same_locks += 1;
                }
                offer.bytes = offer.bytes.saturating_add(state.measure.allocated_bytes);
                offer.newest = offer.newest.max(state.measure.newest_modified.clone());
                offer.clones.push(SeedClone {
                    from: NativePath::new(state.path.clone()),
                    to: NativePath::new(to),
                    ecosystem: state.ecosystem,
                    shared_bytes: state.measure.allocated_bytes,
                });
            }
        }
    }
    offer
}

/// The reason that a directory of build state is no derived state as a
/// whole, so that Suds clones none of it. A checkout beneath it holds work
/// that a build cannot regenerate.
fn unclear(survey: &development::Survey) -> Option<String> {
    if survey.checkouts_beneath > 0 {
        return Some(format!(
            "it holds {} checkouts, and a seed clones derived state alone",
            survey.checkouts_beneath
        ));
    }
    if survey.skipped_mount_count > 0 {
        return Some("a mount point lies beneath it, and no clone crosses one".to_owned());
    }
    if survey.error_count > 0 {
        return Some(format!(
            "Suds could not read {} entries beneath it",
            survey.error_count
        ));
    }
    None
}

/// The reason that a part of the path from `checkout` to `to` is no plain
/// directory. A symbolic link there would lead the clone outside the
/// checkout.
fn linked(checkout: &Path, to: &Path) -> Option<String> {
    let Ok(relative) = to.strip_prefix(checkout) else {
        return Some(format!(
            "{} lies outside the checkout {}",
            to.display(),
            checkout.display()
        ));
    };
    let mut at = checkout.to_path_buf();
    let mut parts = relative.components().peekable();
    while let Some(part) = parts.next() {
        // The last part is the clone's own name, which must hold nothing.
        if parts.peek().is_none() {
            break;
        }
        at.push(part);
        match fs::symlink_metadata(&at) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => {
                return Some(format!(
                    "{} is no directory, and a seed follows no link",
                    at.display()
                ));
            }
            Err(err) => return Some(format!("could not examine {}: {err}", at.display())),
        }
    }
    None
}

/// The reason that Suds clones nothing from `from` to `to` beneath the
/// checkout `checkout`, or none when each condition of a clone holds.
fn refusal(from: &Path, to: &Path, checkout: &Path) -> Option<String> {
    if let Some(reason) = linked(checkout, to) {
        return Some(reason);
    }
    match fs::symlink_metadata(to) {
        Ok(_) => {
            return Some(format!(
                "{} exists, and a seed changes no existing entry",
                to.display()
            ));
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Some(format!("could not examine {}: {err}", to.display())),
    }
    let parent = to.parent()?;
    // Cargo writes its build state beside the manifest, so the manifest
    // proves that the destination's build would use the directory.
    let manifest = parent.join("Cargo.toml");
    if !fs::symlink_metadata(&manifest).is_ok_and(|meta| meta.is_file()) {
        return Some(format!(
            "{} holds no Cargo.toml, so no build there would use the directory",
            parent.display()
        ));
    }
    let devices = fs::symlink_metadata(from)
        .and_then(|source| fs::symlink_metadata(parent).map(|target| (source.dev(), target.dev())));
    match devices {
        Ok((source, target)) if source != target => {
            return Some(
                "the two checkouts lie on different filesystems, and no clone crosses one"
                    .to_owned(),
            );
        }
        Ok(_) => {}
        Err(err) => return Some(format!("could not examine {}: {err}", from.display())),
    }
    Quiet::of(from).err()
}

/// The locks that keep Cargo from writing a build state during a clone.
/// Cargo locks a file of each profile's directory for a whole build. A
/// shared lock on each file proves that no build runs, and it holds a new
/// build back until the clone ends.
struct Quiet {
    _locks: Vec<File>,
}

impl Quiet {
    fn of(build_state: &Path) -> Result<Quiet, String> {
        const LOCK: &str = ".cargo-lock";
        let mut paths = vec![build_state.join(LOCK)];
        let entries = fs::read_dir(build_state)
            .map_err(|err| format!("could not list {}: {err}", build_state.display()))?;
        for entry in entries.flatten() {
            paths.push(entry.path().join(LOCK));
            // A build for another target keeps its profiles one level deeper.
            if let Ok(inner) = fs::read_dir(entry.path()) {
                paths.extend(inner.flatten().map(|entry| entry.path().join(LOCK)));
            }
        }
        let mut locks = Vec::new();
        for path in paths {
            let opened = OpenOptions::new()
                .read(true)
                .custom_flags(OFlags::NOFOLLOW.bits().cast_signed())
                .open(&path);
            let Ok(file) = opened else {
                continue;
            };
            if file.try_lock_shared().is_err() {
                return Err(format!(
                    "a build holds the lock {}, so the directory may change during a clone",
                    path.display()
                ));
            }
            locks.push(file);
        }
        Ok(Quiet { _locks: locks })
    }
}

/// The result of one seed run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SeedReport {
    pub schema: String,
    pub run: String,
    pub plan: String,
    pub status: RunStatus,
    /// The allocated bytes that the cloned directories share with their
    /// sources.
    pub shared_bytes: u64,
    pub clones: Vec<Cloned>,
}

/// The outcome of one directory of the plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Cloned {
    pub to: NativePath,
    pub cloned: bool,
    pub files: u64,
    pub shared_bytes: u64,
    /// The reason that the run cloned nothing to this path.
    pub refusal: Option<String>,
}

/// Records `plan` and clones each directory that it names within
/// [`Budget::for_seeding`]. The run checks each directory's conditions again,
/// and one refusal leaves the others to proceed. A journal failure stops the run.
pub fn apply(
    journal: &mut Journal,
    plan: &SeedPlan,
    client: Option<&str>,
) -> Result<SeedReport, JournalError> {
    apply_with_budget(journal, plan, client, &Budget::for_seeding())
}

/// Like [`apply`], governed by an explicit execution `budget`.
pub fn apply_with_budget(
    journal: &mut Journal,
    plan: &SeedPlan,
    client: Option<&str>,
    budget: &Budget,
) -> Result<SeedReport, JournalError> {
    apply_with_tracker(journal, plan, client, BudgetTracker::new(budget.clone()))
}

/// Like [`apply_with_budget`], sharing an existing `tracker`.
pub fn apply_with_tracker(
    journal: &mut Journal,
    plan: &SeedPlan,
    client: Option<&str>,
    tracker: Arc<BudgetTracker>,
) -> Result<SeedReport, JournalError> {
    let run = new_id("run");
    journal.append(Event::SeedStarted {
        run: run.clone(),
        plan: plan.clone(),
        client: client.map(str::to_owned),
    })?;
    let mut clones = Vec::new();
    let mut shared_bytes = 0_u64;
    for clone in &plan.clones {
        let (from, to) = (clone.from.as_path(), clone.to.as_path());
        let outcome = if tracker.is_stopped() {
            Err("seed cloning stopped by execution budget".to_owned())
        } else {
            match refusal(from, to, plan.destination.as_path()) {
                Some(reason) => Err(reason),
                None => clone_build_state(from, to, &run, &tracker),
            }
        };
        match outcome {
            Ok(counts) => {
                journal.append(Event::SeedCloned {
                    run: run.clone(),
                    path: clone.to.clone(),
                    files: counts.files,
                    shared_bytes: counts.bytes,
                })?;
                shared_bytes = shared_bytes.saturating_add(counts.bytes);
                clones.push(Cloned {
                    to: clone.to.clone(),
                    cloned: true,
                    files: counts.files,
                    shared_bytes: counts.bytes,
                    refusal: None,
                });
            }
            Err(reason) => {
                journal.append(Event::SeedRefused {
                    run: run.clone(),
                    path: clone.to.clone(),
                    reason: reason.clone(),
                })?;
                clones.push(Cloned {
                    to: clone.to.clone(),
                    cloned: false,
                    files: 0,
                    shared_bytes: 0,
                    refusal: Some(reason),
                });
            }
        }
    }
    let done = clones.iter().filter(|clone| clone.cloned).count();
    let status = RunStatus::from_counts(done, clones.len());
    journal.append(Event::SeedCompleted {
        run: run.clone(),
        status,
    })?;
    Ok(SeedReport {
        schema: SEED_RUN_SCHEMA.to_owned(),
        run,
        plan: plan.id.clone(),
        status,
        shared_bytes,
        clones,
    })
}

#[derive(Default)]
struct Counts {
    files: u64,
    bytes: u64,
}

/// Clones the build state at `from` to `to`. The run builds the clone
/// under another name and renames it once it is whole, so a build in the
/// destination never meets a part of one. A failure removes what the run
/// built, and it leaves `to` absent.
fn clone_build_state(
    from: &Path,
    to: &Path,
    run: &str,
    tracker: &Arc<BudgetTracker>,
) -> Result<Counts, String> {
    if tracker.is_stopped() {
        return Err("seed cloning stopped by execution budget".to_owned());
    }
    // The source is build state by the layout of its own checkout, as at
    // the plan's time.
    if development::build_state_at(from) != Some(Ecosystem::Cargo) {
        return Err(format!(
            "{} is no longer the build state of Cargo",
            from.display()
        ));
    }
    let quiet = Quiet::of(from)?;
    // The run surveys the source again, because a checkout or a mount point
    // may have entered it since the plan.
    let mut scan = SharedScan::with_tracker(Arc::clone(tracker));
    let (_, survey, _) = development::survey_build_state_with(from, Ecosystem::Cargo, &mut scan);
    if scan.is_stopped() {
        return Err("seed verification stopped by execution budget".to_owned());
    }
    if let Some(reason) = unclear(&survey) {
        return Err(format!("{}: {reason}", from.display()));
    }
    let parent = to.parent().ok_or("the destination has no parent")?;
    let staged = parent.join(format!(".suds-seed-{run}"));
    let cloned = clone_tree(from, &staged, tracker).and_then(|counts| {
        fs::rename(&staged, to)
            .map(|()| counts)
            .map_err(|err| format!("could not name the clone {}: {err}", to.display()))
    });
    drop(quiet);
    if cloned.is_err() {
        // The directory holds only what this run wrote.
        let _ = fs::remove_dir_all(&staged);
    }
    cloned
}

/// Clones each directory, each regular file, and each symbolic link beneath
/// `from` to `to`, which must not exist. It follows no link, crosses no
/// mount point, and stops cleanly when `tracker` exhausts or cancels.
fn clone_tree(from: &Path, to: &Path, tracker: &BudgetTracker) -> Result<Counts, String> {
    let root = fs::symlink_metadata(from)
        .map_err(|err| format!("could not examine {}: {err}", from.display()))?;
    let mut counts = Counts::default();
    // Cargo gives an artifact a second name through a hard link. The clone
    // keeps the two names on one file, and it counts the file once.
    let mut linked: HashMap<(u64, u64), PathBuf> = HashMap::new();
    // The run creates each directory for this user alone and gives it the
    // source's permissions once its entries exist, so a clone never opens
    // what its source closes to another user.
    let mut made: Vec<(PathBuf, fs::Permissions)> = Vec::new();
    let mut pending = vec![(from.to_path_buf(), to.to_path_buf())];
    while let Some((source, target)) = pending.pop() {
        if !tracker.claim_entry() {
            return Err("seed cloning stopped by execution budget".to_owned());
        }
        let permissions = fs::symlink_metadata(&source)
            .map_err(|err| format!("could not examine {}: {err}", source.display()))?
            .permissions();
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&target)
            .map_err(|err| format!("could not create {}: {err}", target.display()))?;
        made.push((target.clone(), permissions));
        let entries = fs::read_dir(&source)
            .map_err(|err| format!("could not list {}: {err}", source.display()))?;
        for entry in entries {
            if !tracker.claim_entry() {
                return Err("seed cloning stopped by execution budget".to_owned());
            }
            let entry =
                entry.map_err(|err| format!("could not list {}: {err}", source.display()))?;
            let (source, target) = (entry.path(), target.join(entry.file_name()));
            let meta = fs::symlink_metadata(&source)
                .map_err(|err| format!("could not examine {}: {err}", source.display()))?;
            if meta.dev() != root.dev() {
                return Err(format!(
                    "{} lies on another filesystem, and no clone crosses one",
                    source.display()
                ));
            }
            let kind = meta.file_type();
            if kind.is_dir() {
                pending.push((source, target));
            } else if kind.is_file() {
                if meta.nlink() > 1 {
                    if let Some(first) = linked.get(&(meta.dev(), meta.ino())) {
                        fs::hard_link(first, &target)
                            .map_err(|err| format!("could not link {}: {err}", target.display()))?;
                        continue;
                    }
                    linked.insert((meta.dev(), meta.ino()), target.clone());
                }
                clone_file(&source, &target, &meta)?;
                counts.files = counts.files.saturating_add(1);
                counts.bytes = counts
                    .bytes
                    .saturating_add(meta.blocks().saturating_mul(BLOCK_UNIT));
            } else if kind.is_symlink() {
                let link = fs::read_link(&source)
                    .map_err(|err| format!("could not read {}: {err}", source.display()))?;
                std::os::unix::fs::symlink(&link, &target)
                    .map_err(|err| format!("could not create {}: {err}", target.display()))?;
            }
            // A socket or a pipe holds no build output, so the clone leaves
            // it out.
        }
    }
    // A directory comes after each directory beneath it, so a source that
    // forbids its own listing closes last.
    for (target, permissions) in made.into_iter().rev() {
        fs::set_permissions(&target, permissions).map_err(|err| {
            format!(
                "could not set the permissions of {}: {err}",
                target.display()
            )
        })?;
    }
    Ok(counts)
}

/// The times of the source, which Cargo compares with each source file's
/// time to decide what it compiles again. A clone with a new time would
/// read as newer than a source file that changed since the seed's build.
fn times_of(meta: &fs::Metadata) -> rustix::fs::Timestamps {
    let time = |seconds: i64, nanoseconds: i64| rustix::fs::Timespec {
        tv_sec: seconds,
        tv_nsec: nanoseconds as _,
    };
    rustix::fs::Timestamps {
        last_access: time(meta.atime(), meta.atime_nsec()),
        last_modification: time(meta.mtime(), meta.mtime_nsec()),
    }
}

/// The text of a failed clone. A filesystem that clones no file answers
/// with one of a few errors, and the text names that cause.
fn unclonable(source: &Path, err: &io::Error) -> String {
    use rustix::io::Errno;
    let unsupported = [
        Errno::OPNOTSUPP,
        Errno::NOTTY,
        Errno::XDEV,
        Errno::INVAL,
        Errno::NOSYS,
    ]
    .iter()
    .any(|errno| err.raw_os_error() == Some(errno.raw_os_error()));
    if unsupported {
        format!(
            "the filesystem of {} clones no file, and a seed copies none: {err}",
            source.display()
        )
    } else {
        format!("could not clone {}: {err}", source.display())
    }
}

#[cfg(target_os = "linux")]
fn clone_file(source: &Path, target: &Path, meta: &fs::Metadata) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    let opened = OpenOptions::new()
        .read(true)
        .custom_flags(OFlags::NOFOLLOW.bits().cast_signed())
        .open(source)
        .map_err(|err| format!("could not open {}: {err}", source.display()))?;
    let created = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(meta.permissions().mode())
        .open(target)
        .map_err(|err| format!("could not create {}: {err}", target.display()))?;
    rustix::fs::ioctl_ficlone(&created, &opened)
        .map_err(|errno| unclonable(source, &errno.into()))?;
    rustix::fs::futimens(&created, &times_of(meta))
        .map_err(|errno| format!("could not set the times of {}: {errno}", target.display()))
}

#[cfg(target_os = "macos")]
fn clone_file(source: &Path, target: &Path, meta: &fs::Metadata) -> Result<(), String> {
    use rustix::fs::{AtFlags, CWD, CloneFlags};

    let opened = OpenOptions::new()
        .read(true)
        .custom_flags(OFlags::NOFOLLOW.bits().cast_signed())
        .open(source)
        .map_err(|err| format!("could not open {}: {err}", source.display()))?;
    rustix::fs::fclonefileat(&opened, CWD, target, CloneFlags::empty())
        .map_err(|errno| unclonable(source, &errno.into()))?;
    rustix::fs::utimensat(CWD, target, &times_of(meta), AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|errno| format!("could not set the times of {}: {errno}", target.display()))
}

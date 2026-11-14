//! The `suds.plan/v1` move plan and the planner that builds each plan.
//!
//! A plan moves direct children of one source directory into one
//! destination directory on the same filesystem. Planning reads metadata and
//! changes no file. A plan names every source and destination by its native
//! path and records the revision of each entry, so that applying the plan
//! acts only on the entries that a person reviewed.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::inspection::EntryRevision;
use crate::model::{EntryKind, Measure};
use crate::organize::native::NativePath;
use crate::scan;
use crate::timestamp;

/// The schema tag of every plan.
pub const PLAN_SCHEMA: &str = "suds.plan/v1";

/// The conditions that Suds verifies for each move immediately before it
/// renames the entry. A preview lists them, and a refusal names the one that
/// failed.
pub const PRECONDITIONS: &[&str] = &[
    "The source is still a direct child of the source directory, with the device, inode, type, size, and modification time that the plan records.",
    "Neither directory has moved, no other directory has taken either one's place, and no component of either directory's path has become a symbolic link.",
    "No entry exists at the destination path. Suds overwrites nothing, merges no directory, and never renames an entry to avoid a conflict.",
    "The destination directory lies outside the source.",
    "The source and the destination lie on one filesystem, so the move is one atomic rename.",
    "Each path lies within the plan's two directories.",
];

/// A plan to move direct children of one directory into another.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MovePlan {
    pub schema: String,
    pub id: String,
    /// The time at which Suds built the plan, in RFC 3339 form and UTC.
    pub created_at: String,
    pub source_directory: PlannedDirectory,
    pub destination_directory: PlannedDirectory,
    pub moves: Vec<PlannedMove>,
}

/// A directory of a plan, with the identity that it had at planning.
///
/// The path is canonical: it holds no symbolic link, no `.`, and no `..`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedDirectory {
    pub path: NativePath,
    pub device: u64,
    pub inode: u64,
}

/// One entry's move.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedMove {
    pub source: NativePath,
    pub destination: NativePath,
    /// The source's revision at planning.
    pub expected: EntryRevision,
    pub measure: Measure,
    /// The number of entries beneath the source that the measurement could
    /// not read. A positive count makes the measure a lower bound.
    pub measure_error_count: u64,
}

impl PlannedMove {
    /// The entry's name, which the source and the destination share.
    pub fn name(&self) -> Option<&OsStr> {
        self.source.as_path().file_name()
    }
}

/// One entry that a plan should move: its name in the source directory and,
/// when an inspection showed the entry, the revision that the inspection
/// recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveRequest {
    pub name: OsString,
    pub expected: Option<EntryRevision>,
}

/// The class of a refusal, which automation can match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusalCode {
    /// The plan names no entry.
    NothingSelected,
    /// The source entry no longer exists.
    SourceMissing,
    /// The source entry's metadata differs from the plan's record.
    SourceChanged,
    /// The source entry is a symbolic link or a special file, which this
    /// release does not move.
    UnsupportedKind,
    /// A path lies outside the plan's two directories, or an entry's name is
    /// not a single path component.
    OutsideScope,
    /// A directory of the plan moved, another entry took its place, or a
    /// component of its path became a symbolic link.
    DirectoryChanged,
    /// An entry already exists at the destination path.
    DestinationExists,
    /// The destination directory lies inside the source.
    DestinationInsideSource,
    /// The destination directory is the source directory.
    SameDirectory,
    /// The source and the destination lie on different filesystems.
    CrossFilesystem,
    /// A mount point lies beneath the source.
    ContainsMountPoint,
    /// The entry that Suds moved is no longer at the destination.
    DestinationMissing,
    /// The entry at the destination differs from the entry that Suds moved.
    DestinationChanged,
    /// An entry now occupies the source path that an undo would restore.
    SourceOccupied,
    /// Suds could not record the moved entry's identity, so it cannot prove
    /// that an undo would restore the same entry.
    IdentityUnrecorded,
    /// Reading metadata failed.
    Unreadable,
    /// The operating system refused the rename.
    RenameFailed,
    /// Reconciliation could not establish whether an interrupted rename
    /// happened.
    Unresolved,
}

/// The reason that Suds declined to plan, apply, or reverse one move.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refusal {
    pub code: RefusalCode,
    pub message: String,
}

impl Refusal {
    pub fn new(code: RefusalCode, message: String) -> Self {
        Refusal { code, message }
    }

    pub(crate) fn unreadable(path: &Path, err: &io::Error) -> Self {
        Refusal::new(
            RefusalCode::Unreadable,
            format!("could not read the metadata of {}: {err}", path.display()),
        )
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

/// The reasons that Suds declined to build a plan. Planning reports every
/// refusal rather than the first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanRefused {
    pub refusals: Vec<Refusal>,
}

impl fmt::Display for PlanRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let messages: Vec<&str> = self
            .refusals
            .iter()
            .map(|refusal| refusal.message.as_str())
            .collect();
        f.write_str(&messages.join("\n"))
    }
}

/// Builds a plan to move the requested entries of `source_directory` into
/// `destination_directory`. The `created_at` argument holds the time of
/// planning, in seconds since the Unix epoch.
///
/// Planning resolves each directory to its canonical path, because the
/// person who chose the directory named it, and the plan shows the resolved
/// path. It follows no symbolic link beneath the source directory.
pub fn plan_move(
    source_directory: &Path,
    requests: &[MoveRequest],
    destination_directory: &Path,
    created_at: i64,
) -> Result<MovePlan, PlanRefused> {
    let mut refusals = Vec::new();
    if requests.is_empty() {
        refusals.push(Refusal::new(
            RefusalCode::NothingSelected,
            "the plan names no entry to move".to_owned(),
        ));
    }
    let source = resolve_directory(source_directory).map_err(|refusal| PlanRefused {
        refusals: vec![refusal],
    })?;
    let destination = resolve_directory(destination_directory).map_err(|refusal| PlanRefused {
        refusals: vec![refusal],
    })?;
    let source_path = source.path.as_path().to_path_buf();
    let destination_path = destination.path.as_path().to_path_buf();
    if source_path == destination_path {
        refusals.push(Refusal::new(
            RefusalCode::SameDirectory,
            format!(
                "the destination {} is the source directory",
                destination_path.display()
            ),
        ));
    }
    if source.device != destination.device {
        refusals.push(Refusal::new(
            RefusalCode::CrossFilesystem,
            format!(
                "{} and {} lie on different filesystems, and this release moves an entry only by an atomic rename",
                source_path.display(),
                destination_path.display()
            ),
        ));
    }
    if !refusals.is_empty() {
        return Err(PlanRefused { refusals });
    }

    let mut moves = Vec::new();
    let mut seen = HashSet::new();
    for request in requests {
        if !seen.insert(request.name.clone()) {
            continue;
        }
        match plan_entry(&source, &destination, request) {
            Ok(planned) => moves.push(planned),
            Err(refusal) => refusals.push(refusal),
        }
    }
    if !refusals.is_empty() {
        return Err(PlanRefused { refusals });
    }
    Ok(MovePlan {
        schema: PLAN_SCHEMA.to_owned(),
        id: new_id("plan"),
        created_at: timestamp::format_utc(created_at),
        source_directory: source,
        destination_directory: destination,
        moves,
    })
}

fn plan_entry(
    source: &PlannedDirectory,
    destination: &PlannedDirectory,
    request: &MoveRequest,
) -> Result<PlannedMove, Refusal> {
    let source_path = entry_path(source.path.as_path(), &request.name)?;
    let destination_path = destination.path.as_path().join(&request.name);
    let meta = match fs::symlink_metadata(&source_path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Err(Refusal::new(
                RefusalCode::SourceMissing,
                format!("{} no longer exists", source_path.display()),
            ));
        }
        Err(err) => return Err(Refusal::unreadable(&source_path, &err)),
    };
    let revision = EntryRevision::from_metadata(&meta);
    if !matches!(revision.kind, EntryKind::File | EntryKind::Directory) {
        return Err(Refusal::new(
            RefusalCode::UnsupportedKind,
            format!(
                "{} is a symbolic link or a special file, and this release moves only files and directories",
                source_path.display()
            ),
        ));
    }
    if let Some(expected) = request.expected
        && expected != revision
    {
        return Err(Refusal::new(
            RefusalCode::SourceChanged,
            format!(
                "{} changed after the inspection. Inspect the location again.",
                source_path.display()
            ),
        ));
    }
    if meta.dev() != source.device {
        return Err(Refusal::new(
            RefusalCode::CrossFilesystem,
            format!(
                "{} is a mount point on another filesystem",
                source_path.display()
            ),
        ));
    }
    if revision.kind == EntryKind::Directory && destination.path.as_path().starts_with(&source_path)
    {
        return Err(Refusal::new(
            RefusalCode::DestinationInsideSource,
            format!(
                "the destination {} lies inside {}",
                destination.path.display(),
                source_path.display()
            ),
        ));
    }
    check_destination_free(&destination_path)?;

    let measured = scan::measure_entry(&source_path, source.device);
    if measured.skipped_mount_count > 0 {
        return Err(Refusal::new(
            RefusalCode::ContainsMountPoint,
            format!(
                "{} contains {} mount points, and this release moves no directory that contains one",
                source_path.display(),
                measured.skipped_mount_count
            ),
        ));
    }
    Ok(PlannedMove {
        source: NativePath::new(source_path),
        destination: NativePath::new(destination_path),
        expected: revision,
        measure: measured.measure,
        measure_error_count: measured.error_count,
    })
}

/// Resolves a directory of a plan to its canonical path and identity.
fn resolve_directory(path: &Path) -> Result<PlannedDirectory, Refusal> {
    let canonical = fs::canonicalize(path).map_err(|err| {
        Refusal::new(
            RefusalCode::DirectoryChanged,
            format!("could not resolve the directory {}: {err}", path.display()),
        )
    })?;
    let meta =
        fs::symlink_metadata(&canonical).map_err(|err| Refusal::unreadable(&canonical, &err))?;
    if !meta.is_dir() {
        return Err(Refusal::new(
            RefusalCode::DirectoryChanged,
            format!("{} is not a directory", canonical.display()),
        ));
    }
    Ok(PlannedDirectory {
        path: NativePath::new(canonical),
        device: meta.dev(),
        inode: meta.ino(),
    })
}

/// The path of the entry `name` in `directory`, or a refusal when `name` is
/// not one ordinary path component.
pub(crate) fn entry_path(directory: &Path, name: &OsStr) -> Result<PathBuf, Refusal> {
    let mut components = Path::new(name).components();
    let single = matches!(
        (components.next(), components.next()),
        (Some(Component::Normal(component)), None) if component == name
    );
    if !single {
        return Err(Refusal::new(
            RefusalCode::OutsideScope,
            format!(
                "{} is not the name of an entry in {}",
                Path::new(name).display(),
                directory.display()
            ),
        ));
    }
    Ok(directory.join(name))
}

/// Refuses when any entry, even a broken symbolic link, occupies `path`.
pub(crate) fn check_destination_free(path: &Path) -> Result<(), Refusal> {
    match fs::symlink_metadata(path) {
        Ok(_) => Err(Refusal::new(
            RefusalCode::DestinationExists,
            format!("{} already exists", path.display()),
        )),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(Refusal::unreadable(path, &err)),
    }
}

/// An identifier that no earlier plan or run of this machine holds: the
/// time in nanoseconds, the process, and a counter within the process.
pub(crate) fn new_id(prefix: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{nanos:x}-{:x}-{count:x}", process::id())
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::fs;
    use std::os::unix::fs::symlink;

    use tempfile::TempDir;

    use super::{MoveRequest, PLAN_SCHEMA, RefusalCode, plan_move};

    fn request(name: &str) -> MoveRequest {
        MoveRequest {
            name: OsString::from(name),
            expected: None,
        }
    }

    fn codes(result: Result<super::MovePlan, super::PlanRefused>) -> Vec<RefusalCode> {
        result
            .unwrap_err()
            .refusals
            .iter()
            .map(|refusal| refusal.code)
            .collect()
    }

    #[test]
    fn plans_files_and_directories_with_their_measures() {
        let dir = TempDir::new().unwrap();
        let from = dir.path().join("from");
        let to = dir.path().join("to");
        fs::create_dir_all(from.join("folder/inner")).unwrap();
        fs::write(from.join("folder/inner/a"), b"12345").unwrap();
        fs::write(from.join("report.pdf"), b"pdf").unwrap();
        fs::create_dir(&to).unwrap();

        let plan = plan_move(&from, &[request("report.pdf"), request("folder")], &to, 0).unwrap();
        assert_eq!(plan.schema, PLAN_SCHEMA);
        assert!(plan.id.starts_with("plan-"));
        assert_eq!(plan.created_at, "1970-01-01T00:00:00Z");
        let canonical_to = fs::canonicalize(&to).unwrap();
        assert_eq!(plan.destination_directory.path.as_path(), canonical_to);
        assert_eq!(plan.moves.len(), 2);
        assert_eq!(
            plan.moves[0].destination.as_path(),
            canonical_to.join("report.pdf")
        );
        assert_eq!(plan.moves[0].measure.logical_bytes, 3);
        assert_eq!(plan.moves[1].measure.logical_bytes, 5);
        assert_eq!(plan.moves[1].measure.directories, 2);
        assert!(from.join("report.pdf").exists());
    }

    #[test]
    fn refuses_each_unsafe_request() {
        let dir = TempDir::new().unwrap();
        let from = dir.path().join("from");
        fs::create_dir_all(from.join("folder/sub")).unwrap();
        fs::write(from.join("taken"), b"").unwrap();
        fs::create_dir(dir.path().join("to")).unwrap();
        fs::write(dir.path().join("to/taken"), b"").unwrap();
        symlink("/etc", from.join("link")).unwrap();

        let to = dir.path().join("to");
        assert_eq!(
            codes(plan_move(&from, &[], &to, 0)),
            [RefusalCode::NothingSelected]
        );
        assert_eq!(
            codes(plan_move(&from, &[request("missing")], &to, 0)),
            [RefusalCode::SourceMissing]
        );
        assert_eq!(
            codes(plan_move(&from, &[request("taken")], &to, 0)),
            [RefusalCode::DestinationExists]
        );
        assert_eq!(
            codes(plan_move(&from, &[request("link")], &to, 0)),
            [RefusalCode::UnsupportedKind]
        );
        assert_eq!(
            codes(plan_move(&from, &[request("../to")], &to, 0)),
            [RefusalCode::OutsideScope]
        );
        assert_eq!(
            codes(plan_move(
                &from,
                &[request("folder")],
                &from.join("folder/sub"),
                0
            )),
            [RefusalCode::DestinationInsideSource]
        );
        assert_eq!(
            codes(plan_move(&from, &[request("taken")], &from, 0)),
            [RefusalCode::SameDirectory]
        );
    }

    #[test]
    fn refuses_an_entry_that_changed_after_the_inspection() {
        let dir = TempDir::new().unwrap();
        let from = dir.path().join("from");
        fs::create_dir_all(&from).unwrap();
        fs::create_dir(dir.path().join("to")).unwrap();
        fs::write(from.join("file"), b"one").unwrap();
        let meta = fs::symlink_metadata(from.join("file")).unwrap();
        let seen = crate::inspection::EntryRevision::from_metadata(&meta);
        fs::write(from.join("file"), b"three").unwrap();

        let stale = MoveRequest {
            name: OsString::from("file"),
            expected: Some(seen),
        };
        assert_eq!(
            codes(plan_move(&from, &[stale], &dir.path().join("to"), 0)),
            [RefusalCode::SourceChanged]
        );
    }
}

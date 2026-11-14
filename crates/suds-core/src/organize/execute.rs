//! Applying a plan, undoing a run, and reconciling an interrupted one.
//!
//! Each move verifies its preconditions immediately before its rename,
//! records `move_started`, renames the entry with the kernel's no-replace
//! flag, and records the outcome. The rename is one system call on one
//! filesystem, so an entry is at its source or at its destination and never
//! in between. Suds never copies, overwrites, merges, or deletes.

use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, File};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use rustix::fs::{Mode, OFlags, RenameFlags, renameat_with};
use rustix::io::Errno;
use serde::Serialize;

use crate::inspection::EntryRevision;
use crate::model::EntryKind;
use crate::organize::journal::{
    Event, Finding, Journal, JournalError, JournalState, MoveStatus, RunStatus,
};
use crate::organize::native::NativePath;
use crate::organize::plan::{
    MovePlan, PLAN_SCHEMA, PlannedDirectory, PlannedMove, Refusal, RefusalCode,
    check_destination_free, entry_path, new_id,
};

/// The schema tag of an apply's result.
pub const RUN_SCHEMA: &str = "suds.run/v1";
/// The schema tag of an undo's result.
pub const UNDO_SCHEMA: &str = "suds.undo/v1";

/// The result of applying a plan or undoing a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunReport {
    pub schema: String,
    pub run: String,
    pub plan: String,
    pub status: RunStatus,
    /// The interrupted renames that this invocation examined before it began.
    pub reconciled: Vec<Reconciliation>,
    pub moves: Vec<MoveOutcome>,
}

/// One move's outcome in a run or an undo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MoveOutcome {
    /// The move's position in the plan.
    pub index: usize,
    pub source: NativePath,
    pub destination: NativePath,
    pub status: MoveStatus,
    pub refusal: Option<Refusal>,
}

/// One interrupted rename that reconciliation examined.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Reconciliation {
    pub run: String,
    pub index: usize,
    pub source: NativePath,
    pub destination: NativePath,
    pub finding: Finding,
    /// For an unresolved finding, what Suds found at each path.
    pub detail: Option<String>,
}

/// The reason that Suds applied or undid nothing.
#[derive(Debug)]
pub enum OperationError {
    Journal(JournalError),
    /// The plan's schema is not one that this release applies.
    UnsupportedPlan(String),
    /// The journal records no plan with this identifier, so Suds did not
    /// build it.
    UnknownPlan(String),
    /// The plan differs from the plan that the journal records.
    AlteredPlan(String),
    /// A run of this plan exists already.
    AlreadyApplied {
        plan: String,
        run: String,
    },
    UnknownRun(String),
    /// The run holds no move that remains applied.
    NothingToUndo(String),
}

impl fmt::Display for OperationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OperationError::Journal(err) => err.fmt(f),
            OperationError::UnsupportedPlan(schema) => write!(
                f,
                "the plan's schema is {schema}, and this release applies only {PLAN_SCHEMA}"
            ),
            OperationError::UnknownPlan(plan) => write!(
                f,
                "the journal records no plan {plan}, so Suds did not build it. Build a plan with suds plan."
            ),
            OperationError::AlteredPlan(plan) => write!(
                f,
                "the plan {plan} differs from the plan that Suds built and recorded"
            ),
            OperationError::AlreadyApplied { plan, run } => {
                write!(f, "the plan {plan} already ran as {run}")
            }
            OperationError::UnknownRun(run) => write!(f, "the journal records no run {run}"),
            OperationError::NothingToUndo(run) => {
                write!(f, "the run {run} holds no move that remains applied")
            }
        }
    }
}

impl std::error::Error for OperationError {}

impl From<JournalError> for OperationError {
    fn from(err: JournalError) -> Self {
        OperationError::Journal(err)
    }
}

/// Records a plan that the planner built, which makes the plan eligible to
/// run.
pub fn record_plan(journal: &mut Journal, plan: &MovePlan) -> Result<(), OperationError> {
    journal.append(Event::PlanCreated { plan: plan.clone() })?;
    Ok(())
}

/// The plan that the journal records under `plan`.
pub fn recorded_plan(journal: &Journal, plan: &str) -> Option<MovePlan> {
    journal.state().plans.remove(plan)
}

/// Applies `plan`, calling `on_move` after each move. Each move verifies its
/// own preconditions, so one refusal leaves the others to proceed. A journal
/// failure stops the run, and the next invocation reconciles its rename.
pub fn apply(
    journal: &mut Journal,
    plan: &MovePlan,
    on_move: &mut dyn FnMut(&MoveOutcome),
) -> Result<RunReport, OperationError> {
    if plan.schema != PLAN_SCHEMA {
        return Err(OperationError::UnsupportedPlan(plan.schema.clone()));
    }
    let reconciled = reconcile(journal)?;
    let state = journal.state();
    match state.plans.get(&plan.id) {
        None => return Err(OperationError::UnknownPlan(plan.id.clone())),
        Some(recorded) if recorded != plan => {
            return Err(OperationError::AlteredPlan(plan.id.clone()));
        }
        Some(_) => {}
    }
    if let Some(previous) = state.runs.iter().find(|run| run.plan == plan.id) {
        return Err(OperationError::AlreadyApplied {
            plan: plan.id.clone(),
            run: previous.run.clone(),
        });
    }

    let run = new_id("run");
    journal.append(Event::RunStarted {
        run: run.clone(),
        plan: plan.id.clone(),
    })?;
    let mut moves = Vec::new();
    for (index, planned) in plan.moves.iter().enumerate() {
        let outcome = apply_move(journal, plan, &run, index, planned)?;
        on_move(&outcome);
        moves.push(outcome);
    }
    let applied = moves
        .iter()
        .filter(|outcome| outcome.status == MoveStatus::Applied)
        .count();
    let status = RunStatus::from_counts(applied, moves.len());
    journal.append(Event::RunCompleted {
        run: run.clone(),
        status,
        interrupted: false,
    })?;
    Ok(RunReport {
        schema: RUN_SCHEMA.to_owned(),
        run,
        plan: plan.id.clone(),
        status,
        reconciled,
        moves,
    })
}

fn apply_move(
    journal: &mut Journal,
    plan: &MovePlan,
    run: &str,
    index: usize,
    planned: &PlannedMove,
) -> Result<MoveOutcome, OperationError> {
    let outcome = |status, refusal| MoveOutcome {
        index,
        source: planned.source.clone(),
        destination: planned.destination.clone(),
        status,
        refusal,
    };
    let prepared = match check_move(plan, planned) {
        Ok(prepared) => prepared,
        Err(refusal) => {
            journal.append(Event::MoveRefused {
                run: run.to_owned(),
                index,
                refusal: refusal.clone(),
            })?;
            return Ok(outcome(MoveStatus::Refused, Some(refusal)));
        }
    };
    journal.append(Event::MoveStarted {
        run: run.to_owned(),
        index,
    })?;
    match rename_without_replacing(
        &prepared.source,
        &prepared.destination,
        prepared.name,
        planned,
    ) {
        Ok(()) => {
            // The rename succeeded whether or not this read does. Without the
            // revision, an undo refuses, because it cannot prove identity.
            let revision = fs::symlink_metadata(planned.destination.as_path())
                .ok()
                .map(|meta| EntryRevision::from_metadata(&meta));
            journal.append(Event::MoveApplied {
                run: run.to_owned(),
                index,
                revision,
            })?;
            Ok(outcome(MoveStatus::Applied, None))
        }
        Err(refusal) => {
            journal.append(Event::MoveRefused {
                run: run.to_owned(),
                index,
                refusal: refusal.clone(),
            })?;
            Ok(outcome(MoveStatus::Refused, Some(refusal)))
        }
    }
}

/// The directories of a move, opened and verified, and the entry's name.
struct Prepared<'a> {
    source: File,
    destination: File,
    name: &'a OsStr,
}

/// Verifies every precondition of a forward move.
fn check_move<'a>(plan: &MovePlan, planned: &'a PlannedMove) -> Result<Prepared<'a>, Refusal> {
    let name = check_scope(plan, planned)?;
    let source = open_directory(&plan.source_directory)?;
    let destination = open_directory(&plan.destination_directory)?;
    check_same_filesystem(plan)?;

    let source_path = planned.source.as_path();
    let current = match fs::symlink_metadata(source_path) {
        Ok(meta) => EntryRevision::from_metadata(&meta),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Err(Refusal::new(
                RefusalCode::SourceMissing,
                format!("{} no longer exists", source_path.display()),
            ));
        }
        Err(err) => return Err(Refusal::unreadable(source_path, &err)),
    };
    if current != planned.expected {
        return Err(Refusal::new(
            RefusalCode::SourceChanged,
            format!(
                "{} changed after Suds built the plan",
                source_path.display()
            ),
        ));
    }
    if current.kind == EntryKind::Directory
        && plan
            .destination_directory
            .path
            .as_path()
            .starts_with(source_path)
    {
        return Err(Refusal::new(
            RefusalCode::DestinationInsideSource,
            format!(
                "the destination {} lies inside {}",
                plan.destination_directory.path.display(),
                source_path.display()
            ),
        ));
    }
    check_destination_free(planned.destination.as_path())?;
    Ok(Prepared {
        source,
        destination,
        name,
    })
}

/// Verifies that a move's two paths are the plan's directories joined with
/// one shared name, and returns the name.
fn check_scope<'a>(plan: &MovePlan, planned: &'a PlannedMove) -> Result<&'a OsStr, Refusal> {
    let outside = || {
        Refusal::new(
            RefusalCode::OutsideScope,
            format!(
                "{} or {} lies outside the plan's directories",
                planned.source.display(),
                planned.destination.display()
            ),
        )
    };
    let name = planned.name().ok_or_else(outside)?;
    let source = entry_path(plan.source_directory.path.as_path(), name)?;
    let destination = entry_path(plan.destination_directory.path.as_path(), name)?;
    if source != planned.source.as_path() || destination != planned.destination.as_path() {
        return Err(outside());
    }
    Ok(name)
}

fn check_same_filesystem(plan: &MovePlan) -> Result<(), Refusal> {
    if plan.source_directory.device == plan.destination_directory.device {
        return Ok(());
    }
    Err(Refusal::new(
        RefusalCode::CrossFilesystem,
        format!(
            "{} and {} lie on different filesystems",
            plan.source_directory.path.display(),
            plan.destination_directory.path.display()
        ),
    ))
}

/// Opens a plan's directory without following a link at its final
/// component, and verifies that it is the directory that the plan recorded.
/// The canonical-path comparison detects a symbolic link that replaced any
/// other component.
fn open_directory(planned: &PlannedDirectory) -> Result<File, Refusal> {
    let path = planned.path.as_path();
    let changed = |detail: String| {
        Refusal::new(
            RefusalCode::DirectoryChanged,
            format!("the directory {} {detail}", path.display()),
        )
    };
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let descriptor = rustix::fs::open(path, flags, Mode::empty())
        .map_err(|errno| changed(format!("refused to open: {}", io::Error::from(errno))))?;
    let directory = File::from(descriptor);
    let meta = directory
        .metadata()
        .map_err(|err| Refusal::unreadable(path, &err))?;
    if meta.dev() != planned.device || meta.ino() != planned.inode {
        return Err(changed(
            "moved, or another directory took its place, after Suds built the plan".to_owned(),
        ));
    }
    match fs::canonicalize(path) {
        Ok(canonical) if canonical == path => Ok(directory),
        Ok(_) => Err(changed("now lies behind a symbolic link".to_owned())),
        Err(err) => Err(changed(format!("has no canonical path: {err}"))),
    }
}

/// Renames `name` from one open directory to the other, and fails rather
/// than replace an entry that already exists at the destination.
fn rename_without_replacing(
    from: &File,
    to: &File,
    name: &OsStr,
    planned: &PlannedMove,
) -> Result<(), Refusal> {
    renameat_with(from, name, to, name, RenameFlags::NOREPLACE).map_err(|errno| {
        let (code, detail) = match errno {
            Errno::EXIST => (
                RefusalCode::DestinationExists,
                "an entry appeared at the target",
            ),
            Errno::XDEV => (
                RefusalCode::CrossFilesystem,
                "the paths lie on different filesystems",
            ),
            Errno::NOENT => (RefusalCode::SourceMissing, "the entry disappeared"),
            _ => (
                RefusalCode::RenameFailed,
                "the operating system refused the rename",
            ),
        };
        Refusal::new(
            code,
            format!(
                "could not move {} to {}: {detail} ({})",
                planned.source.display(),
                planned.destination.display(),
                io::Error::from(errno)
            ),
        )
    })
}

/// Reverses every move of `run` that remains applied, in reverse order.
pub fn undo(
    journal: &mut Journal,
    run: &str,
    on_move: &mut dyn FnMut(&MoveOutcome),
) -> Result<RunReport, OperationError> {
    let reconciled = reconcile(journal)?;
    let state = journal.state();
    let run_state = state
        .run(run)
        .ok_or_else(|| OperationError::UnknownRun(run.to_owned()))?;
    let plan = state
        .plans
        .get(&run_state.plan)
        .ok_or_else(|| OperationError::UnknownPlan(run_state.plan.clone()))?;
    let applied: Vec<(usize, Option<EntryRevision>)> = run_state
        .moves
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, state)| state.status == MoveStatus::Applied)
        .map(|(index, state)| (index, state.revision))
        .collect();
    if applied.is_empty() {
        return Err(OperationError::NothingToUndo(run.to_owned()));
    }

    journal.append(Event::UndoStarted {
        run: run.to_owned(),
    })?;
    let mut moves = Vec::new();
    for (index, revision) in applied {
        let Some(planned) = plan.moves.get(index) else {
            continue;
        };
        let outcome = reverse_move(journal, plan, run, index, planned, revision)?;
        on_move(&outcome);
        moves.push(outcome);
    }
    let reversed = moves
        .iter()
        .filter(|outcome| outcome.status == MoveStatus::Reversed)
        .count();
    let status = RunStatus::from_counts(reversed, moves.len());
    journal.append(Event::UndoCompleted {
        run: run.to_owned(),
        status,
        interrupted: false,
    })?;
    Ok(RunReport {
        schema: UNDO_SCHEMA.to_owned(),
        run: run.to_owned(),
        plan: plan.id.clone(),
        status,
        reconciled,
        moves,
    })
}

fn reverse_move(
    journal: &mut Journal,
    plan: &MovePlan,
    run: &str,
    index: usize,
    planned: &PlannedMove,
    revision: Option<EntryRevision>,
) -> Result<MoveOutcome, OperationError> {
    let outcome = |status, refusal| MoveOutcome {
        index,
        source: planned.source.clone(),
        destination: planned.destination.clone(),
        status,
        refusal,
    };
    let refuse = |journal: &mut Journal, refusal: Refusal| {
        journal.append(Event::ReverseRefused {
            run: run.to_owned(),
            index,
            refusal: refusal.clone(),
        })?;
        Ok(outcome(MoveStatus::Applied, Some(refusal)))
    };
    let prepared = match check_reverse(plan, planned, revision) {
        Ok(prepared) => prepared,
        Err(refusal) => return refuse(journal, refusal),
    };
    journal.append(Event::ReverseStarted {
        run: run.to_owned(),
        index,
    })?;
    let reversed = rename_without_replacing(
        &prepared.destination,
        &prepared.source,
        prepared.name,
        planned,
    );
    match reversed {
        Ok(()) => {
            journal.append(Event::MoveReversed {
                run: run.to_owned(),
                index,
            })?;
            Ok(outcome(MoveStatus::Reversed, None))
        }
        Err(mut refusal) => {
            if refusal.code == RefusalCode::DestinationExists {
                refusal.code = RefusalCode::SourceOccupied;
            }
            refuse(journal, refusal)
        }
    }
}

/// Verifies every precondition of a reversal: the destination still holds
/// the entry that Suds moved, and the source path is free.
fn check_reverse<'a>(
    plan: &MovePlan,
    planned: &'a PlannedMove,
    revision: Option<EntryRevision>,
) -> Result<Prepared<'a>, Refusal> {
    let name = check_scope(plan, planned)?;
    let destination_path = planned.destination.as_path();
    let Some(moved) = revision else {
        return Err(Refusal::new(
            RefusalCode::IdentityUnrecorded,
            format!(
                "Suds could not record the identity of {} after the move, so it cannot prove that the entry there is the one it moved",
                destination_path.display()
            ),
        ));
    };
    let source = open_directory(&plan.source_directory)?;
    let destination = open_directory(&plan.destination_directory)?;
    check_same_filesystem(plan)?;
    match fs::symlink_metadata(destination_path) {
        Ok(meta) if EntryRevision::from_metadata(&meta) == moved => {}
        Ok(_) => {
            return Err(Refusal::new(
                RefusalCode::DestinationChanged,
                format!("{} changed after Suds moved it", destination_path.display()),
            ));
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Err(Refusal::new(
                RefusalCode::DestinationMissing,
                format!("{} no longer exists", destination_path.display()),
            ));
        }
        Err(err) => return Err(Refusal::unreadable(destination_path, &err)),
    }
    check_destination_free(planned.source.as_path()).map_err(|mut refusal| {
        if refusal.code == RefusalCode::DestinationExists {
            refusal.code = RefusalCode::SourceOccupied;
            refusal.message = format!(
                "another entry now occupies {}, so the moved entry cannot return there",
                planned.source.display()
            );
        }
        refusal
    })?;
    Ok(Prepared {
        source,
        destination,
        name,
    })
}

/// Examines each rename that the journal started without an outcome, and
/// closes each run and undo that an interruption left open. Only an entry at
/// exactly one of its two paths settles a rename. Suds changes no path here.
pub fn reconcile(journal: &mut Journal) -> Result<Vec<Reconciliation>, OperationError> {
    let state = journal.state();
    let mut findings = Vec::new();
    for run in &state.runs {
        let Some(plan) = state.plans.get(&run.plan) else {
            continue;
        };
        for (index, move_state) in run.moves.iter().enumerate() {
            let Some(planned) = plan.moves.get(index) else {
                continue;
            };
            let (finding, revision, detail) = match move_state.status {
                MoveStatus::Started => probe(
                    planned.source.as_path(),
                    planned.destination.as_path(),
                    &planned.expected,
                    Finding::NotApplied,
                    Finding::Applied,
                ),
                MoveStatus::Reversing => match move_state.revision {
                    Some(moved) => probe(
                        planned.destination.as_path(),
                        planned.source.as_path(),
                        &moved,
                        Finding::NotReversed,
                        Finding::Reversed,
                    ),
                    None => (
                        Finding::Unresolved,
                        None,
                        Some(
                            "Suds recorded no identity for the moved entry, so it cannot tell where the entry lies"
                                .to_owned(),
                        ),
                    ),
                },
                _ => continue,
            };
            // A finding that the forward move happened carries the entry's
            // revision at the destination, which a later undo verifies.
            let revision = if finding == Finding::Applied {
                revision
            } else {
                None
            };
            journal.append(Event::Reconciled {
                run: run.run.clone(),
                index,
                finding,
                revision,
                detail: detail.clone(),
            })?;
            findings.push(Reconciliation {
                run: run.run.clone(),
                index,
                source: planned.source.clone(),
                destination: planned.destination.clone(),
                finding,
                detail,
            });
        }
    }
    close_interrupted(journal)?;
    Ok(findings)
}

/// What reconciliation found at one path of an interrupted rename.
enum Presence {
    Expected(EntryRevision),
    Other,
    Absent,
    Unreadable(io::Error),
}

impl Presence {
    fn at(path: &Path, expected: &EntryRevision) -> Self {
        match fs::symlink_metadata(path) {
            Ok(meta) => {
                let revision = EntryRevision::from_metadata(&meta);
                if revision.is_same_entry(expected) {
                    Presence::Expected(revision)
                } else {
                    Presence::Other
                }
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => Presence::Absent,
            Err(err) => Presence::Unreadable(err),
        }
    }

    fn describe(&self, path: &Path) -> String {
        match self {
            Presence::Expected(_) => format!("{} holds the planned entry", path.display()),
            Presence::Other => format!("{} holds another entry", path.display()),
            Presence::Absent => format!("{} holds no entry", path.display()),
            Presence::Unreadable(err) => {
                format!(
                    "Suds could not read the metadata of {}: {err}",
                    path.display()
                )
            }
        }
    }
}

/// Classifies an interrupted rename from `from` to `to` of the entry that
/// `expected` identifies. The result carries the entry's revision at `to`
/// when it lies there, and a description of both paths when it is
/// unresolved.
fn probe(
    from: &Path,
    to: &Path,
    expected: &EntryRevision,
    stayed: Finding,
    arrived: Finding,
) -> (Finding, Option<EntryRevision>, Option<String>) {
    let at_from = Presence::at(from, expected);
    let at_to = Presence::at(to, expected);
    match (&at_from, &at_to) {
        (Presence::Expected(_), Presence::Absent) => (stayed, None, None),
        (Presence::Absent, Presence::Expected(revision)) => (arrived, Some(*revision), None),
        _ => (
            Finding::Unresolved,
            None,
            Some(format!(
                "{}, and {}, which matches neither outcome",
                at_from.describe(from),
                at_to.describe(to)
            )),
        ),
    }
}

fn close_interrupted(journal: &mut Journal) -> Result<(), OperationError> {
    let state: JournalState = journal.state();
    for run in &state.runs {
        if run.completed.is_none() {
            let applied = run
                .moves
                .iter()
                .filter(|state| state.status == MoveStatus::Applied)
                .count();
            journal.append(Event::RunCompleted {
                run: run.run.clone(),
                status: RunStatus::from_counts(applied, run.moves.len()),
                interrupted: true,
            })?;
        }
        if run.undo_open {
            let reversed = run
                .moves
                .iter()
                .filter(|state| state.status == MoveStatus::Reversed)
                .count();
            let remaining = run
                .moves
                .iter()
                .filter(|state| state.status == MoveStatus::Applied)
                .count();
            journal.append(Event::UndoCompleted {
                run: run.run.clone(),
                status: RunStatus::from_counts(reversed, reversed + remaining),
                interrupted: true,
            })?;
        }
    }
    Ok(())
}

//! The append-only operation journal.
//!
//! The journal is the only state that Suds keeps between runs. It holds one
//! JSON record per line in `journal.jsonl`, and Suds never rewrites or
//! removes a record. Every move writes a record before its rename and another
//! after it, so the next run can tell whether an interrupted rename happened.
//! An exclusive lock on `journal.lock` keeps two processes from writing at
//! once.

use std::collections::HashMap;
use std::fmt;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::inspection::EntryRevision;
use crate::organize::native::NativePath;
use crate::organize::plan::{MovePlan, Refusal, RefusalCode};
use crate::timestamp;

/// The schema tag of every journal record.
pub const JOURNAL_SCHEMA: &str = "suds.journal/v1";

const JOURNAL_FILE: &str = "journal.jsonl";
const LOCK_FILE: &str = "journal.lock";

/// One immutable record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub schema: String,
    /// The record's position in the journal, from 1.
    pub sequence: u64,
    /// The time of the record, in RFC 3339 form and UTC.
    pub at: String,
    #[serde(flatten)]
    pub event: Event,
}

/// What a record states. Each `index` names a move by its position in the
/// run's plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// Suds built this plan. Only a plan that the journal records can run.
    PlanCreated {
        plan: MovePlan,
    },
    RunStarted {
        run: String,
        plan: String,
    },
    /// Suds verified the move's preconditions and is about to rename.
    MoveStarted {
        run: String,
        index: usize,
    },
    /// The rename succeeded. The revision describes the entry at its
    /// destination, or is absent when Suds could not read it.
    MoveApplied {
        run: String,
        index: usize,
        revision: Option<EntryRevision>,
    },
    MoveRefused {
        run: String,
        index: usize,
        refusal: Refusal,
    },
    /// A rename or a reversal succeeded, but Suds cannot establish where the
    /// planned entry lies. A person must examine both paths.
    MoveUnresolved {
        run: String,
        index: usize,
        detail: String,
    },
    /// A later run examined a rename that an interruption left without an
    /// outcome.
    Reconciled {
        run: String,
        index: usize,
        finding: Finding,
        revision: Option<EntryRevision>,
        /// For an unresolved finding, what Suds found at each path.
        detail: Option<String>,
    },
    RunCompleted {
        run: String,
        status: RunStatus,
        /// Whether reconciliation, rather than the run itself, closed the run.
        interrupted: bool,
    },
    UndoStarted {
        run: String,
    },
    /// Suds verified the reversal's preconditions and is about to rename.
    ReverseStarted {
        run: String,
        index: usize,
    },
    MoveReversed {
        run: String,
        index: usize,
    },
    ReverseRefused {
        run: String,
        index: usize,
        refusal: Refusal,
    },
    UndoCompleted {
        run: String,
        status: RunStatus,
        interrupted: bool,
    },
}

/// What reconciliation found at an interrupted rename's two paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Finding {
    /// The entry lies at its destination, so the move happened.
    Applied,
    /// The entry lies at its source, and the destination is free.
    NotApplied,
    /// The entry lies at its source again, so the reversal happened.
    Reversed,
    /// The entry still lies at its destination, and the source is free.
    NotReversed,
    /// Neither state holds. A person must examine both paths.
    Unresolved,
}

/// The outcome of a run or an undo as a whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// Every move succeeded.
    Complete,
    /// Some moves succeeded and others did not.
    Partial,
    /// No move succeeded.
    Refused,
}

impl RunStatus {
    pub fn from_counts(succeeded: usize, total: usize) -> Self {
        if succeeded == total && total > 0 {
            RunStatus::Complete
        } else if succeeded > 0 {
            RunStatus::Partial
        } else {
            RunStatus::Refused
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            RunStatus::Complete => "complete",
            RunStatus::Partial => "partial",
            RunStatus::Refused => "refused",
        }
    }
}

/// The reason that Suds could not use the journal.
#[derive(Debug)]
pub enum JournalError {
    /// The platform has no default journal directory.
    #[cfg(target_os = "linux")]
    NoDefault,
    /// The process has no home directory in which to keep the journal.
    #[cfg(target_os = "macos")]
    NoHome,
    /// Another Suds process holds the journal's lock.
    Busy,
    Io {
        path: PathBuf,
        source: io::Error,
    },
    /// A line of the journal is not a record that this release reads.
    Corrupt {
        line: usize,
        message: String,
    },
}

impl fmt::Display for JournalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            #[cfg(target_os = "linux")]
            JournalError::NoDefault => write!(
                f,
                "no default journal directory exists for linux. Name one with --journal DIR."
            ),
            #[cfg(target_os = "macos")]
            JournalError::NoHome => write!(
                f,
                "no home directory exists for this process, so Suds has no journal directory. Name one with --journal DIR."
            ),
            JournalError::Busy => write!(
                f,
                "another Suds process is applying or undoing a plan. Try again when it finishes."
            ),
            JournalError::Io { path, source } => {
                write!(
                    f,
                    "could not use the journal at {}: {source}",
                    path.display()
                )
            }
            JournalError::Corrupt { line, message } => write!(
                f,
                "line {line} of the journal is not a record that this release reads: {message}"
            ),
        }
    }
}

impl std::error::Error for JournalError {}

/// The journal directory beneath the user's Application Support folder.
#[cfg(target_os = "macos")]
pub fn default_directory() -> Result<PathBuf, JournalError> {
    let home = std::env::home_dir()
        .filter(|home| home.is_absolute())
        .ok_or(JournalError::NoHome)?;
    Ok(home.join("Library/Application Support/Suds/journal"))
}

/// Linux has no default journal directory, as it has no default inspection
/// profile, so the operator names one.
#[cfg(target_os = "linux")]
pub fn default_directory() -> Result<PathBuf, JournalError> {
    Err(JournalError::NoDefault)
}

/// The journal, opened for writing under its exclusive lock.
pub struct Journal {
    path: PathBuf,
    file: File,
    records: Vec<Record>,
    /// The lock lasts as long as this handle.
    _lock: File,
}

impl Journal {
    /// Opens the journal in `dir`, creating the directory and the journal
    /// when they do not exist. The call fails at once, rather than waiting,
    /// when another process holds the lock.
    pub fn open(dir: &Path) -> Result<Journal, JournalError> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|source| io_error(dir, source))?;
        let lock = open_lock(dir)?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(JournalError::Busy),
            Err(TryLockError::Error(source)) => return Err(io_error(&dir.join(LOCK_FILE), source)),
        }
        let path = dir.join(JOURNAL_FILE);
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .mode(0o600)
            .open(&path)
            .map_err(|source| io_error(&path, source))?;
        let records = read_records(&file, &path)?;
        Ok(Journal {
            path,
            file,
            records,
            _lock: lock,
        })
    }

    pub fn records(&self) -> &[Record] {
        &self.records
    }

    /// Appends one record and waits until the storage device holds it, so
    /// that no rename can precede its record on disk.
    pub fn append(&mut self, event: Event) -> Result<(), JournalError> {
        let record = Record {
            schema: JOURNAL_SCHEMA.to_owned(),
            sequence: u64::try_from(self.records.len())
                .unwrap_or(u64::MAX)
                .saturating_add(1),
            at: timestamp::format_utc(now_seconds()),
            event,
        };
        let mut line = serde_json::to_vec(&record).map_err(|err| JournalError::Corrupt {
            line: self.records.len() + 1,
            message: err.to_string(),
        })?;
        line.push(b'\n');
        // One write of the whole line keeps a record contiguous, because the
        // file opens in append mode.
        self.file
            .write_all(&line)
            .and_then(|()| self.file.sync_data())
            .map_err(|source| io_error(&self.path, source))?;
        self.records.push(record);
        Ok(())
    }

    pub fn state(&self) -> JournalState {
        JournalState::from_records(&self.records)
    }
}

/// Reads the journal in `dir` under a shared lock, without creating anything.
/// A directory without a journal holds no record.
pub fn read(dir: &Path) -> Result<Vec<Record>, JournalError> {
    let path = dir.join(JOURNAL_FILE);
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(io_error(&path, source)),
    };
    let lock = open_lock(dir)?;
    match lock.try_lock_shared() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Err(JournalError::Busy),
        Err(TryLockError::Error(source)) => return Err(io_error(&dir.join(LOCK_FILE), source)),
    }
    read_records(&file, &path)
}

fn open_lock(dir: &Path) -> Result<File, JournalError> {
    let path = dir.join(LOCK_FILE);
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)
        .map_err(|source| io_error(&path, source))
}

fn read_records(file: &File, path: &Path) -> Result<Vec<Record>, JournalError> {
    let mut records = Vec::new();
    for (index, line) in BufReader::new(file).lines().enumerate() {
        let number = index + 1;
        let line = line.map_err(|source| io_error(path, source))?;
        let record: Record = serde_json::from_str(&line).map_err(|err| JournalError::Corrupt {
            line: number,
            message: err.to_string(),
        })?;
        if record.schema != JOURNAL_SCHEMA {
            return Err(JournalError::Corrupt {
                line: number,
                message: format!("the schema {} is not {JOURNAL_SCHEMA}", record.schema),
            });
        }
        if usize::try_from(record.sequence).ok() != Some(number) {
            return Err(JournalError::Corrupt {
                line: number,
                message: format!("the record's sequence is {}", record.sequence),
            });
        }
        records.push(record);
    }
    Ok(records)
}

fn io_error(path: &Path, source: io::Error) -> JournalError {
    JournalError::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
        })
}

/// The state of one move, as the journal's records establish it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MoveStatus {
    /// The run ended before it reached the move.
    NotStarted,
    /// The rename's outcome is unrecorded. Reconciliation resolves it.
    Started,
    Applied,
    Refused,
    /// An interruption stopped the run before the rename.
    NotApplied,
    /// The paths match neither outcome. A person must examine them.
    Unresolved,
    /// The reversal's outcome is unrecorded. Reconciliation resolves it.
    Reversing,
    Reversed,
}

impl MoveStatus {
    pub fn label(self) -> &'static str {
        match self {
            MoveStatus::NotStarted => "not started",
            MoveStatus::Started => "started",
            MoveStatus::Applied => "applied",
            MoveStatus::Refused => "refused",
            MoveStatus::NotApplied => "not applied",
            MoveStatus::Unresolved => "unresolved",
            MoveStatus::Reversing => "reversing",
            MoveStatus::Reversed => "reversed",
        }
    }
}

/// One move's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveState {
    pub status: MoveStatus,
    /// The entry's revision at its destination after the move.
    pub revision: Option<EntryRevision>,
    /// The most recent refusal of the move or of its reversal.
    pub refusal: Option<Refusal>,
}

/// One run's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunState {
    pub run: String,
    pub plan: String,
    pub started_at: String,
    pub moves: Vec<MoveState>,
    pub completed: Option<RunStatus>,
    pub interrupted: bool,
    /// Whether an undo started and has no completion record.
    pub undo_open: bool,
    /// The outcome of the most recent completed undo.
    pub undo: Option<RunStatus>,
}

/// The plans and runs that the journal's records establish.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JournalState {
    pub plans: HashMap<String, MovePlan>,
    /// The runs in the order in which they started.
    pub runs: Vec<RunState>,
}

impl JournalState {
    pub fn from_records(records: &[Record]) -> Self {
        let mut state = JournalState::default();
        for record in records {
            state.apply(record);
        }
        state
    }

    pub fn run(&self, run: &str) -> Option<&RunState> {
        self.runs.iter().find(|state| state.run == run)
    }

    fn apply(&mut self, record: &Record) {
        match &record.event {
            Event::PlanCreated { plan } => {
                self.plans.insert(plan.id.clone(), plan.clone());
            }
            Event::RunStarted { run, plan } => {
                let count = self.plans.get(plan).map_or(0, |plan| plan.moves.len());
                self.runs.push(RunState {
                    run: run.clone(),
                    plan: plan.clone(),
                    started_at: record.at.clone(),
                    moves: vec![
                        MoveState {
                            status: MoveStatus::NotStarted,
                            revision: None,
                            refusal: None,
                        };
                        count
                    ],
                    completed: None,
                    interrupted: false,
                    undo_open: false,
                    undo: None,
                });
            }
            Event::MoveStarted { run, index } => {
                self.update_move(run, *index, |state| state.status = MoveStatus::Started);
            }
            Event::MoveApplied {
                run,
                index,
                revision,
            } => self.update_move(run, *index, |state| {
                state.status = MoveStatus::Applied;
                state.revision = *revision;
            }),
            Event::MoveRefused {
                run,
                index,
                refusal,
            } => self.update_move(run, *index, |state| {
                state.status = MoveStatus::Refused;
                state.refusal = Some(refusal.clone());
            }),
            Event::MoveUnresolved { run, index, detail } => {
                self.update_move(run, *index, |state| {
                    state.status = MoveStatus::Unresolved;
                    state.refusal = Some(Refusal::new(RefusalCode::Unresolved, detail.clone()));
                });
            }
            Event::Reconciled {
                run,
                index,
                finding,
                revision,
                detail,
            } => self.update_move(run, *index, |state| {
                state.status = match finding {
                    Finding::Applied | Finding::NotReversed => MoveStatus::Applied,
                    Finding::NotApplied => MoveStatus::NotApplied,
                    Finding::Reversed => MoveStatus::Reversed,
                    Finding::Unresolved => MoveStatus::Unresolved,
                };
                if revision.is_some() {
                    state.revision = *revision;
                }
                if let Some(detail) = detail {
                    state.refusal = Some(Refusal::new(RefusalCode::Unresolved, detail.clone()));
                }
            }),
            Event::RunCompleted {
                run,
                status,
                interrupted,
            } => {
                if let Some(state) = self.run_mut(run) {
                    state.completed = Some(*status);
                    state.interrupted = *interrupted;
                }
            }
            Event::UndoStarted { run } => {
                if let Some(state) = self.run_mut(run) {
                    state.undo_open = true;
                }
            }
            Event::ReverseStarted { run, index } => {
                self.update_move(run, *index, |state| state.status = MoveStatus::Reversing);
            }
            Event::MoveReversed { run, index } => {
                self.update_move(run, *index, |state| {
                    state.status = MoveStatus::Reversed;
                    state.refusal = None;
                });
            }
            Event::ReverseRefused {
                run,
                index,
                refusal,
            } => self.update_move(run, *index, |state| {
                state.status = MoveStatus::Applied;
                state.refusal = Some(refusal.clone());
            }),
            Event::UndoCompleted { run, status, .. } => {
                if let Some(state) = self.run_mut(run) {
                    state.undo_open = false;
                    state.undo = Some(*status);
                }
            }
        }
    }

    fn run_mut(&mut self, run: &str) -> Option<&mut RunState> {
        self.runs.iter_mut().find(|state| state.run == run)
    }

    fn update_move(&mut self, run: &str, index: usize, change: impl FnOnce(&mut MoveState)) {
        if let Some(state) = self.run_mut(run)
            && let Some(entry) = state.moves.get_mut(index)
        {
            change(entry);
        }
    }

    /// Each run with its plan's paths, most recent first.
    pub fn summaries(&self) -> Vec<RunSummary> {
        self.runs
            .iter()
            .rev()
            .map(|run| {
                let plan = self.plans.get(&run.plan);
                let moves = run
                    .moves
                    .iter()
                    .enumerate()
                    .filter_map(|(index, state)| {
                        let planned = plan?.moves.get(index)?;
                        Some(MoveSummary {
                            source: planned.source.clone(),
                            destination: planned.destination.clone(),
                            status: state.status,
                            refusal: state.refusal.clone(),
                        })
                    })
                    .collect();
                RunSummary {
                    run: run.run.clone(),
                    plan: run.plan.clone(),
                    started_at: run.started_at.clone(),
                    status: run.completed,
                    interrupted: run.interrupted,
                    undo: run.undo,
                    undoable: run
                        .moves
                        .iter()
                        .any(|state| state.status == MoveStatus::Applied),
                    moves,
                }
            })
            .collect()
    }
}

/// One run as `suds journal` and the desktop application list it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunSummary {
    pub run: String,
    pub plan: String,
    pub started_at: String,
    /// The run's outcome, or `None` while the run is in progress or until
    /// reconciliation closes an interrupted run.
    pub status: Option<RunStatus>,
    pub interrupted: bool,
    /// The outcome of the most recent undo.
    pub undo: Option<RunStatus>,
    /// Whether any move remains applied, so that an undo could reverse it.
    pub undoable: bool,
    pub moves: Vec<MoveSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MoveSummary {
    pub source: NativePath,
    pub destination: NativePath,
    pub status: MoveStatus,
    pub refusal: Option<Refusal>,
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::{Event, Journal, JournalError, read};

    #[test]
    fn appends_records_in_sequence_and_reads_them_back() {
        let dir = TempDir::new().unwrap();
        let journal_dir = dir.path().join("journal");
        assert!(read(&journal_dir).unwrap().is_empty());
        assert!(!journal_dir.exists());

        let mut journal = Journal::open(&journal_dir).unwrap();
        journal
            .append(Event::UndoStarted {
                run: "run-1".to_owned(),
            })
            .unwrap();
        journal
            .append(Event::MoveReversed {
                run: "run-1".to_owned(),
                index: 0,
            })
            .unwrap();
        drop(journal);

        let records = read(&journal_dir).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[1].sequence, 2);
        let text = fs::read_to_string(journal_dir.join("journal.jsonl")).unwrap();
        assert!(
            text.lines()
                .all(|line| line.contains("\"schema\":\"suds.journal/v1\""))
        );
        assert!(text.contains("\"event\":\"move_reversed\""));
    }

    #[test]
    fn a_second_writer_is_refused_while_the_first_holds_the_lock() {
        let dir = TempDir::new().unwrap();
        let first = Journal::open(dir.path()).unwrap();
        assert!(matches!(Journal::open(dir.path()), Err(JournalError::Busy)));
        drop(first);
        assert!(Journal::open(dir.path()).is_ok());
    }

    #[test]
    fn an_unreadable_line_stops_the_journal() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("journal.jsonl"),
            "{\"schema\":\"suds.journal/v1\"\n",
        )
        .unwrap();
        assert!(matches!(
            Journal::open(dir.path()),
            Err(JournalError::Corrupt { line: 1, .. })
        ));
    }
}

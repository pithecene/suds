//! The journal's runs as the window lists them, most recent first.
//!
//! The core reads the journal and sums each run up. This module adds the
//! figures of a run that a person reads first: how many steps succeeded, how
//! many bytes the run freed, and whose approval admitted it.

use std::collections::HashMap;

use suds_core::development::Environment;
use suds_core::organize::journal::{
    self, Event, JournalState, MoveStatus, Record, RunKind, RunStatus, RunSummary,
};
use suds_core::reclaim::execute::Authority;
use suds_core::state::Directories;

/// One step of a run: a move, a removal, a store's trim, or a step that placed a unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// The path that the step moved, removed, or wrote, the store that it trimmed, or the
    /// command that it ran.
    pub subject: String,
    /// For a move, the path that the entry took.
    pub destination: Option<String>,
    pub succeeded: bool,
    /// The bytes that the step freed. A move frees none.
    pub freed_bytes: u64,
    /// The reason that a step did not succeed, or the state that the journal
    /// records for it.
    pub note: Option<String>,
}

/// One run of the journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub run: String,
    pub kind: RunKind,
    /// The time at which the run started, in RFC 3339 form and UTC.
    pub started_at: String,
    /// The run's outcome, or nothing for a run without a completion record.
    pub status: Option<RunStatus>,
    pub interrupted: bool,
    /// The outcome of the most recent undo of a move run.
    pub undo: Option<RunStatus>,
    /// Whose approval admitted a reclaim run or a stores run.
    pub authority: Option<Authority>,
    pub steps: Vec<Step>,
}

impl Run {
    /// The count of the steps that succeeded.
    pub fn succeeded(&self) -> usize {
        self.steps.iter().filter(|step| step.succeeded).count()
    }

    /// The bytes that the run's steps freed.
    pub fn freed_bytes(&self) -> u64 {
        let freed = self.steps.iter().map(|step| step.freed_bytes);
        freed.fold(0, u64::saturating_add)
    }
}

/// Each run that the journal records, most recent first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct History {
    pub runs: Vec<Run>,
}

impl History {
    /// The bytes that every run freed.
    pub fn freed_bytes(&self) -> u64 {
        let freed = self.runs.iter().map(Run::freed_bytes);
        freed.fold(0, u64::saturating_add)
    }
}

fn steps(summary: &RunSummary) -> Vec<Step> {
    let moves = summary.moves.iter().map(|planned| Step {
        subject: planned.source.display(),
        destination: Some(planned.destination.display()),
        succeeded: planned.status == MoveStatus::Applied,
        freed_bytes: 0,
        note: match (&planned.refusal, planned.status) {
            (Some(refusal), _) => Some(refusal.message.clone()),
            (None, MoveStatus::Applied) => None,
            (None, status) => Some(status.label().to_owned()),
        },
    });
    let removals = summary.removals.iter().map(|removal| Step {
        subject: removal.path.display(),
        destination: None,
        succeeded: removal.removed,
        freed_bytes: removal.freed_bytes,
        note: match (&removal.reason, removal.removed) {
            (Some(reason), _) => Some(reason.clone()),
            (None, true) => None,
            (None, false) => Some("started".to_owned()),
        },
    });
    let trims = summary.trims.iter().map(|trim| {
        let sizes = trim.before_bytes.zip(trim.after_bytes);
        Step {
            subject: trim.store.clone(),
            destination: None,
            succeeded: sizes.is_some() && trim.reason.is_none(),
            freed_bytes: sizes.map_or(0, |(before, after)| before.saturating_sub(after)),
            note: match (&trim.reason, sizes) {
                (Some(reason), _) => Some(reason.clone()),
                (None, Some(_)) => None,
                (None, None) => Some("started".to_owned()),
            },
        }
    });
    let units = summary.units.iter().map(|unit| Step {
        subject: unit.step.clone(),
        destination: None,
        succeeded: unit.reason.is_none(),
        freed_bytes: 0,
        note: unit.reason.clone(),
    });
    moves.chain(removals).chain(trims).chain(units).collect()
}

/// The authority of each reclaim run and of each stores run, by the run's name.
fn authorities(records: &[Record]) -> HashMap<&str, Authority> {
    let started = records.iter().filter_map(|record| match &record.event {
        Event::ReclaimStarted { run, authority, .. }
        | Event::StoresStarted { run, authority, .. } => Some((run.as_str(), *authority)),
        _ => None,
    });
    started.collect()
}

impl History {
    /// Arranges the runs that `records` establish.
    pub fn of(records: &[Record]) -> Self {
        let authorities = authorities(records);
        let summaries = JournalState::from_records(records).summaries();
        let runs = summaries.iter().map(|summary| Run {
            run: summary.run.clone(),
            kind: summary.kind,
            started_at: summary.started_at.clone(),
            status: summary.status,
            interrupted: summary.interrupted,
            undo: summary.undo,
            authority: authorities.get(summary.run.as_str()).copied(),
            steps: steps(summary),
        });
        History {
            runs: runs.collect(),
        }
    }
}

/// Reads the journal in the user's state directory and changes nothing. A
/// workstation without a journal has an empty history.
pub fn history(environment: &Environment) -> Result<History, String> {
    let directories = Directories::of(environment)
        .ok_or("the environment names no home directory, so Suds has no journal to read")?;
    let records = journal::read(&directories.journal()).map_err(|err| err.to_string())?;
    Ok(History::of(&records))
}

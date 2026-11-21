//! The standing of the scheduled maintenance, which runs without a person.
//!
//! A scheduled run that fails tells nobody. This module reads what such a
//! run leaves behind: the scheduler's units, the time of the last pressure
//! reading, and the journal's last stores run under the standing approval.

use std::time::UNIX_EPOCH;

use suds_core::development::Environment;
use suds_core::organize::journal::{self, RunKind};
use suds_core::reclaim::execute::Authority;
use suds_core::schedule::{self, Scheduler, Standing};
use suds_core::state::Directories;
use suds_core::timestamp;

use crate::history::History;

/// The time after which a workstation with a schedule should have run its
/// maintenance again. A workstation may sleep through a night, so the time
/// is a whole day.
pub const OVERDUE_SECONDS: i64 = 24 * 60 * 60;

/// One store that the last scheduled run could not trim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedTrim {
    pub store: String,
    pub reason: String,
}

/// What the scheduled maintenance left behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upkeep {
    /// The standing of the scheduler's units.
    pub schedule: Standing,
    /// The time of the last maintenance run, in RFC 3339 form and UTC. Each
    /// run writes the pressure reading, so the reading's file tells it.
    pub last_run: Option<String>,
    /// Whether the units stand in place and no run came within a day.
    pub overdue: bool,
    /// The stores that the last scheduled stores run could not trim.
    pub failed_trims: Vec<FailedTrim>,
    /// The reason that Suds could not read the journal. The failed trims
    /// are then unknown, and the maintenance is not sound.
    pub journal_error: Option<String>,
}

impl Upkeep {
    /// Whether the maintenance runs as this release schedules it.
    pub fn sound(&self) -> bool {
        let run = !self.overdue && self.failed_trims.is_empty();
        self.schedule == Standing::Current && run && self.journal_error.is_none()
    }
}

/// The trims that failed in the most recent stores run of the standing approval.
fn failed_trims(history: &History) -> Vec<FailedTrim> {
    let scheduled = Some(Authority::StandingApproval);
    let mut runs = history.runs.iter();
    let last = runs.find(|run| run.kind == RunKind::Stores && run.authority == scheduled);
    let steps = last.map_or(&[][..], |run| &run.steps);
    let failed = steps.iter().filter(|step| !step.succeeded);
    let failed = failed.filter_map(|step| {
        Some(FailedTrim {
            store: step.subject.clone(),
            reason: step.note.clone()?,
        })
    });
    failed.collect()
}

/// Reads the standing of the scheduled maintenance at `now`, in seconds
/// since the Unix epoch, and changes nothing.
pub fn upkeep(environment: &Environment, now: i64) -> Upkeep {
    let schedule = schedule::standing(Scheduler::native(), environment, None);
    let directories = Directories::of(environment);
    let reading = directories.as_ref().map(Directories::pressure);
    let written = reading.and_then(|path| path.metadata().ok()?.modified().ok());
    let written = written.and_then(|time| time.duration_since(UNIX_EPOCH).ok());
    let written = written.and_then(|since| i64::try_from(since.as_secs()).ok());
    // A workstation without a journal has an empty one, and another failure stays a failure.
    let records = match directories {
        Some(directories) => journal::read(&directories.journal()),
        None => Ok(Vec::new()),
    };
    let journal_error = records.as_ref().err().map(ToString::to_string);
    let history = History::of(&records.unwrap_or_default());
    Upkeep {
        overdue: schedule != Standing::Absent
            && written.is_none_or(|written| now.saturating_sub(written) > OVERDUE_SECONDS),
        schedule,
        last_run: written.map(timestamp::format_utc),
        failed_trims: failed_trims(&history),
        journal_error,
    }
}

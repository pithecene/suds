//! Move plans that the window builds, applies, and undoes.
//!
//! The Rust process holds every native path. The webview receives display
//! strings and opaque identifiers, and it names a plan by its identifier
//! alone, so no string from the webview can name a file.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use suds_core::model::EntryKind;
use suds_core::organize::execute::{self, MoveOutcome, RunReport};
use suds_core::organize::journal::{
    self, Journal, JournalState, MoveStatus, RunStatus, RunSummary,
};
use suds_core::organize::plan::{MovePlan, MoveRequest, PRECONDITIONS, Refusal, plan_move};

/// The journal directory and the plans that this window built.
pub struct Operations {
    journal: Result<PathBuf, String>,
    /// A plan leaves this set when the window applies it, so the window can
    /// apply only a plan that it built and only once.
    created: Mutex<HashSet<String>>,
}

/// The answer to a request for a plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PlanResponse {
    /// The person closed the folder picker.
    Cancelled,
    /// Suds declined to build the plan, for each of these reasons.
    Refused {
        refusals: Vec<Refusal>,
    },
    Planned {
        plan: PlanView,
    },
}

/// A plan as the confirmation sheet shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlanView {
    pub id: String,
    pub created_at: String,
    pub source_directory: String,
    pub destination_directory: String,
    pub moves: Vec<PlannedMoveView>,
    pub preconditions: Vec<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlannedMoveView {
    pub name: String,
    pub kind: EntryKind,
    pub source: String,
    pub destination: String,
    pub logical_bytes: u64,
    pub allocated_bytes: u64,
    pub newest_modified: Option<String>,
    pub measure_error_count: u64,
}

/// A run's or an undo's result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunView {
    pub run: String,
    pub status: RunStatus,
    pub reconciled: usize,
    pub moves: Vec<MoveView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MoveView {
    pub source: String,
    pub destination: String,
    pub status: MoveStatus,
    pub refusal: Option<Refusal>,
}

/// A run as the history sheet lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunSummaryView {
    pub run: String,
    pub started_at: String,
    pub status: Option<RunStatus>,
    pub interrupted: bool,
    pub undo: Option<RunStatus>,
    pub undoable: bool,
    pub moves: Vec<MoveView>,
}

impl Operations {
    pub fn new(journal: Result<PathBuf, String>) -> Self {
        Operations {
            journal,
            created: Mutex::new(HashSet::new()),
        }
    }

    fn journal_directory(&self) -> Result<&Path, String> {
        self.journal.as_deref().map_err(Clone::clone)
    }

    /// Builds a plan, records it in the journal, and returns its view.
    pub fn create_plan(
        &self,
        source_directory: &Path,
        requests: &[MoveRequest],
        destination_directory: &Path,
    ) -> Result<PlanResponse, String> {
        let plan = match plan_move(
            source_directory,
            requests,
            destination_directory,
            now_seconds(),
        ) {
            Ok(plan) => plan,
            Err(refused) => {
                return Ok(PlanResponse::Refused {
                    refusals: refused.refusals,
                });
            }
        };
        let mut journal =
            Journal::open(self.journal_directory()?).map_err(|err| err.to_string())?;
        execute::record_plan(&mut journal, &plan).map_err(|err| err.to_string())?;
        lock(&self.created).insert(plan.id.clone());
        Ok(PlanResponse::Planned {
            plan: plan_view(&plan),
        })
    }

    /// Applies a plan that this window built. The function calls `on_move`
    /// after each move.
    pub fn apply(&self, plan: &str, on_move: &mut dyn FnMut(MoveView)) -> Result<RunView, String> {
        if !lock(&self.created).remove(plan) {
            return Err(format!(
                "this window built no plan {plan} that awaits application"
            ));
        }
        let mut journal =
            Journal::open(self.journal_directory()?).map_err(|err| err.to_string())?;
        let recorded = execute::recorded_plan(&journal, plan)
            .ok_or_else(|| format!("the journal records no plan {plan}"))?;
        let report = execute::apply(&mut journal, &recorded, &mut |outcome| {
            on_move(move_view(outcome));
        })
        .map_err(|err| err.to_string())?;
        Ok(run_view(&report))
    }

    pub fn undo(&self, run: &str) -> Result<RunView, String> {
        let mut journal =
            Journal::open(self.journal_directory()?).map_err(|err| err.to_string())?;
        let report =
            execute::undo(&mut journal, run, &mut |_| {}).map_err(|err| err.to_string())?;
        Ok(run_view(&report))
    }

    pub fn list(&self) -> Result<Vec<RunSummaryView>, String> {
        let records = journal::read(self.journal_directory()?).map_err(|err| err.to_string())?;
        Ok(JournalState::from_records(&records)
            .summaries()
            .iter()
            .map(summary_view)
            .collect())
    }
}

fn plan_view(plan: &MovePlan) -> PlanView {
    PlanView {
        id: plan.id.clone(),
        created_at: plan.created_at.clone(),
        source_directory: plan.source_directory.path.display(),
        destination_directory: plan.destination_directory.path.display(),
        moves: plan
            .moves
            .iter()
            .map(|planned| PlannedMoveView {
                name: planned
                    .name()
                    .map_or_else(String::new, |name| name.to_string_lossy().into_owned()),
                kind: planned.expected.kind,
                source: planned.source.display(),
                destination: planned.destination.display(),
                logical_bytes: planned.measure.logical_bytes,
                allocated_bytes: planned.measure.allocated_bytes,
                newest_modified: planned.measure.newest_modified.clone(),
                measure_error_count: planned.measure_error_count,
            })
            .collect(),
        preconditions: PRECONDITIONS.to_vec(),
    }
}

fn move_view(outcome: &MoveOutcome) -> MoveView {
    MoveView {
        source: outcome.source.display(),
        destination: outcome.destination.display(),
        status: outcome.status,
        refusal: outcome.refusal.clone(),
    }
}

fn run_view(report: &RunReport) -> RunView {
    RunView {
        run: report.run.clone(),
        status: report.status,
        reconciled: report.reconciled.len(),
        moves: report.moves.iter().map(move_view).collect(),
    }
}

fn summary_view(summary: &RunSummary) -> RunSummaryView {
    RunSummaryView {
        run: summary.run.clone(),
        started_at: summary.started_at.clone(),
        status: summary.status,
        interrupted: summary.interrupted,
        undo: summary.undo,
        undoable: summary.undoable,
        moves: summary
            .moves
            .iter()
            .map(|planned| MoveView {
                source: planned.source.display(),
                destination: planned.destination.display(),
                status: planned.status,
                refusal: planned.refusal.clone(),
            })
            .collect(),
    }
}

fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
        })
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::fs;

    use suds_core::organize::journal::{MoveStatus, RunStatus};
    use suds_core::organize::plan::MoveRequest;
    use tempfile::TempDir;

    use super::{Operations, PlanResponse};

    fn request(name: &str) -> MoveRequest {
        MoveRequest {
            name: OsString::from(name),
            expected: None,
        }
    }

    #[test]
    fn applies_and_undoes_a_plan_that_the_window_built() {
        let dir = TempDir::new().unwrap();
        let from = dir.path().join("Downloads");
        let to = dir.path().join("Documents");
        fs::create_dir(&from).unwrap();
        fs::create_dir(&to).unwrap();
        fs::write(from.join("a.pdf"), b"a").unwrap();
        let operations = Operations::new(Ok(dir.path().join("journal")));

        let PlanResponse::Planned { plan } = operations
            .create_plan(&from, &[request("a.pdf")], &to)
            .unwrap()
        else {
            panic!("expected a plan");
        };
        assert_eq!(plan.moves[0].name, "a.pdf");
        assert!(!plan.preconditions.is_empty());

        let mut streamed = Vec::new();
        let run = operations
            .apply(&plan.id, &mut |outcome| streamed.push(outcome.status))
            .unwrap();
        assert_eq!(run.status, RunStatus::Complete);
        assert_eq!(streamed, [MoveStatus::Applied]);
        assert!(to.join("a.pdf").exists());
        assert!(operations.apply(&plan.id, &mut |_| {}).is_err());

        let history = operations.list().unwrap();
        assert_eq!(history[0].run, run.run);
        assert!(history[0].undoable);
        assert_eq!(
            operations.undo(&run.run).unwrap().status,
            RunStatus::Complete
        );
        assert!(from.join("a.pdf").exists());
    }

    #[test]
    fn reports_refusals_and_applies_no_plan_from_elsewhere() {
        let dir = TempDir::new().unwrap();
        let operations = Operations::new(Ok(dir.path().join("journal")));
        let refused = operations
            .create_plan(dir.path(), &[request("missing")], dir.path())
            .unwrap();
        assert!(matches!(refused, PlanResponse::Refused { .. }));
        assert!(operations.apply("plan-forged", &mut |_| {}).is_err());
        assert!(!dir.path().join("journal").exists());
    }
}

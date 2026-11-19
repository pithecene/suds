//! Applying a reclaim plan.
//!
//! The executor records the plan, then judges each candidate again
//! immediately before it acts, because a process can start or a file can
//! change after planning. For each removal it observes the processes,
//! measures the directory, and asks Git anew. The executor removes build
//! state itself, without following a symbolic link, and it has Git remove a
//! linked checkout.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::development;
use crate::inspection::EntryRevision;
use crate::organize::journal::{Event, Journal, JournalError, RunStatus};
use crate::organize::native::NativePath;
use crate::organize::plan::new_id;
use crate::reclaim::git::Git;
use crate::reclaim::liveness::Liveness;
use crate::reclaim::plan::{Candidate, Decision, Judge, ReclaimPlan, Subject, idle_since};
use crate::registry::Reading;

/// The schema tag of a reclaim run's result.
pub const RECLAIM_RUN_SCHEMA: &str = "suds.reclaim-run/v1";

/// Who approved a reclaim run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Authority {
    /// A person ran the command and asked it to apply the plan.
    Person,
    /// The owner's standing approval for derived state admitted the plan.
    StandingApproval,
}

/// The result of one reclaim run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReclaimReport {
    pub schema: String,
    pub run: String,
    pub plan: String,
    pub authority: Authority,
    pub status: RunStatus,
    /// The allocated bytes of the directories that the run removed, as the
    /// plan measured them.
    pub freed_bytes: u64,
    pub removals: Vec<Removal>,
}

/// The outcome of one candidate that the plan removes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Removal {
    /// The candidate's position in the plan.
    pub index: usize,
    pub path: NativePath,
    pub removed: bool,
    pub freed_bytes: u64,
    /// The reason that the run removed nothing at this path.
    pub refusal: Option<String>,
}

/// What a run needs beside its plan: who approved it, how to observe the
/// processes, the Git adapter, and the time in seconds since the Unix epoch.
pub struct Conditions<'a> {
    pub authority: Authority,
    /// The run calls this before each removal, so a process that starts
    /// during the run protects its checkout.
    pub observe: &'a mut dyn FnMut() -> Result<Liveness, String>,
    /// The registry's standing of each checkout, read anew before each
    /// removal, so a client that registers during the run keeps its
    /// checkout.
    pub registry: &'a mut dyn FnMut() -> Reading,
    pub git: &'a Git,
    pub now: i64,
}

/// Records `plan` and removes each candidate that it decided to remove. One
/// refusal leaves the other candidates to proceed, and a journal failure
/// stops the run.
pub fn apply(
    journal: &mut Journal,
    plan: &ReclaimPlan,
    conditions: Conditions<'_>,
) -> Result<ReclaimReport, JournalError> {
    let Conditions {
        authority,
        observe,
        registry,
        git,
        now,
    } = conditions;
    journal.append(Event::ReclaimPlanned { plan: plan.clone() })?;
    let run = new_id("run");
    journal.append(Event::ReclaimStarted {
        run: run.clone(),
        plan: plan.id.clone(),
        authority,
    })?;
    let mut removals = Vec::new();
    for (index, candidate) in plan.candidates.iter().enumerate() {
        if candidate.decision != Decision::Remove {
            continue;
        }
        let liveness = observe();
        let standings = registry();
        let judge = Judge {
            liveness: &liveness,
            registry: &standings,
            git,
            idle_since: idle_since(now, plan.idle_seconds),
            tolerate_unreadable_processes: plan.tolerate_unreadable_processes,
        };
        let outcome = match check(journal.directory(), candidate, &judge) {
            Err(reason) => Err(reason),
            Ok(()) => {
                journal.append(Event::RemovalStarted {
                    run: run.clone(),
                    index,
                })?;
                remove(candidate, git)
            }
        };
        let refusal = outcome.err();
        journal.append(match &refusal {
            None => Event::Removed {
                run: run.clone(),
                index,
                freed_bytes: candidate.allocated_bytes,
            },
            Some(reason) => Event::RemovalRefused {
                run: run.clone(),
                index,
                reason: reason.clone(),
            },
        })?;
        removals.push(Removal {
            index,
            path: candidate.path.clone(),
            removed: refusal.is_none(),
            freed_bytes: if refusal.is_none() {
                candidate.allocated_bytes
            } else {
                0
            },
            refusal,
        });
    }
    let removed = removals.iter().filter(|removal| removal.removed).count();
    let freed_bytes = removals.iter().map(|removal| removal.freed_bytes).sum();
    let status = RunStatus::from_counts(removed, removals.len());
    journal.append(Event::ReclaimCompleted {
        run: run.clone(),
        status,
        freed_bytes,
    })?;
    Ok(ReclaimReport {
        schema: RECLAIM_RUN_SCHEMA.to_owned(),
        run,
        plan: plan.id.clone(),
        authority,
        status,
        freed_bytes,
        removals,
    })
}

/// Verifies every precondition of one removal. The directory must be the
/// entry that the plan measured, and the plan's own rules must still decide
/// to remove it as it stands now.
fn check(journal: &Path, candidate: &Candidate, judge: &Judge<'_>) -> Result<(), String> {
    let path = candidate.path.as_path();
    if journal.starts_with(path) {
        return Err(format!("{} holds the journal", path.display()));
    }
    let meta = fs::symlink_metadata(path)
        .map_err(|err| format!("could not read the metadata of {}: {err}", path.display()))?;
    if !EntryRevision::from_metadata(&meta).is_same_entry(&candidate.expected) {
        return Err(format!(
            "another entry took the place of {} after Suds built the plan",
            path.display()
        ));
    }
    match fs::canonicalize(path) {
        Ok(canonical) if canonical == path => {}
        Ok(_) => {
            return Err(format!(
                "{} now lies behind a symbolic link",
                path.display()
            ));
        }
        Err(err) => return Err(format!("{} has no canonical path: {err}", path.display())),
    }
    let (kept, mut evidence) = match candidate.subject {
        Subject::BuildState { ecosystem } => {
            if development::build_state_at(path) != Some(ecosystem) {
                return Err(format!(
                    "the layout of {} no longer proves that {} is build state",
                    ecosystem.label(),
                    path.display()
                ));
            }
            let (measure, survey) = development::survey_build_state(path);
            judge.build_state(
                candidate.checkout.as_path(),
                path,
                ecosystem,
                &measure,
                &survey,
            )
        }
        Subject::LinkedCheckout => {
            let (measure, survey) = development::survey_tree(path);
            judge.checkout(path, measure.newest_modified.as_deref(), &survey)
        }
    };
    match kept {
        // The judgment's last fact is the one that keeps the directory.
        Some(_) => Err(evidence.pop().unwrap_or_default()),
        None => Ok(()),
    }
}

fn remove(candidate: &Candidate, git: &Git) -> Result<(), String> {
    let path = candidate.path.as_path();
    match candidate.subject {
        // The standard library removes a tree without following a symbolic
        // link, so a link inside build state loses only itself.
        Subject::BuildState { .. } => fs::remove_dir_all(path)
            .map_err(|err| format!("could not remove {}: {err}", path.display())),
        Subject::LinkedCheckout => {
            let repository = candidate
                .repository
                .as_ref()
                .ok_or_else(|| format!("the plan names no repository for {}", path.display()))?;
            git.remove_worktree(repository.as_path(), path)
        }
    }
}

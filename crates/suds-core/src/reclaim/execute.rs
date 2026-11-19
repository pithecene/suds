//! Applying a reclaim plan.
//!
//! The executor records the plan, then verifies each removal's evidence
//! again immediately before it acts, because a process can start or a file
//! can change after planning. It removes build state itself, without
//! following a symbolic link, and it has Git remove a linked checkout.

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
use crate::reclaim::plan::{Candidate, Decision, ReclaimPlan, Subject, unsaved_work};

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

/// Records `plan` and removes each candidate that it decided to remove. One
/// refusal leaves the other candidates to proceed, and a journal failure
/// stops the run.
pub fn apply(
    journal: &mut Journal,
    plan: &ReclaimPlan,
    authority: Authority,
    liveness: &Result<Liveness, String>,
    git: &Git,
) -> Result<ReclaimReport, JournalError> {
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
        let path = candidate.path.as_path();
        let checked = check(journal.directory(), candidate, liveness, git);
        let outcome = match checked {
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
            path: NativePath::new(path.to_path_buf()),
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

/// Verifies every precondition of one removal.
fn check(
    journal: &Path,
    candidate: &Candidate,
    liveness: &Result<Liveness, String>,
    git: &Git,
) -> Result<(), String> {
    let path = candidate.path.as_path();
    let checkout = candidate.checkout.as_path();
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
    match liveness {
        Err(reason) => {
            return Err(format!(
                "Suds could not read the working directories of the processes: {reason}"
            ));
        }
        Ok(liveness) => {
            if let Some(directory) = liveness.within(checkout) {
                return Err(format!("a process works in {}", directory.display()));
            }
        }
    }
    match candidate.subject {
        Subject::BuildState { ecosystem } => {
            if development::build_state_at(path) != Some(ecosystem) {
                return Err(format!(
                    "the layout of {} no longer proves that {} is build state",
                    ecosystem.label(),
                    path.display()
                ));
            }
        }
        Subject::LinkedCheckout => {
            let mut evidence = Vec::new();
            if unsaved_work(git, path, &mut evidence).is_some() {
                return Err(evidence.pop().unwrap_or_default());
            }
        }
    }
    Ok(())
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

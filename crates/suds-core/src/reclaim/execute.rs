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

use crate::development::{self, Ecosystem};
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
    // Git removes a linked checkout with the build state in it, so each
    // checkout goes first. A removal of its build state would change the
    // checkout and make it count as recently modified.
    let mut order: Vec<(usize, &Candidate)> = plan
        .candidates
        .iter()
        .enumerate()
        .filter(|(_, candidate)| candidate.decision == Decision::Remove)
        .collect();
    order.sort_by_key(|(_, candidate)| candidate.subject != Subject::LinkedCheckout);
    let mut gone: Vec<&Path> = Vec::new();
    for (index, candidate) in order {
        if gone
            .iter()
            .any(|checkout| candidate.path.as_path().starts_with(checkout))
        {
            journal.append(Event::Removed {
                run: run.clone(),
                index,
                freed_bytes: candidate.allocated_bytes,
            })?;
            removals.push(Removal {
                index,
                path: candidate.path.clone(),
                removed: true,
                freed_bytes: candidate.allocated_bytes,
                refusal: None,
            });
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
            // The standing approval covers derived state alone, and a
            // landed head still holds commits that no remote holds.
            admit_landed: authority == Authority::Person,
        };
        let outcome = match check(journal.directory(), candidate, &judge) {
            Err(reason) => Err(reason),
            Ok(()) => {
                journal.append(Event::RemovalStarted {
                    run: run.clone(),
                    index,
                })?;
                if candidate.subject == Subject::LinkedCheckout {
                    // Git removes the checkout's Go caches with it, and Git
                    // cannot enter a directory that no one may write.
                    for cache in go_caches_beneath(plan, candidate.path.as_path()) {
                        grant_owner_write(cache);
                    }
                }
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
        if refusal.is_none() && candidate.subject == Subject::LinkedCheckout {
            gone.push(candidate.path.as_path());
        }
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
    removals.sort_by_key(|removal| removal.index);
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
            let (measure, survey, repositories) = development::survey_build_state(path, ecosystem);
            judge.build_state(
                candidate.checkout.as_path(),
                path,
                ecosystem,
                &measure,
                &survey,
                &repositories,
            )
        }
        Subject::LinkedCheckout => {
            let (measure, survey, repositories) = development::survey_tree(path);
            judge.checkout(
                path,
                measure.newest_modified.as_deref(),
                &survey,
                &repositories,
            )
        }
        Subject::FetchedToolchain { ecosystem } => {
            if development::toolchain_at(path) != Some(ecosystem) {
                return Err(format!(
                    "the layout of {} no longer proves that {} is a fetched toolchain",
                    ecosystem.label(),
                    path.display()
                ));
            }
            let (measure, survey, repositories) = development::survey_tree(path);
            judge.fetched_toolchain(
                candidate.checkout.as_path(),
                path,
                ecosystem,
                measure.newest_modified.as_deref(),
                &survey,
                &repositories,
            )
        }
    };
    match kept {
        // The judgment's last fact is the one that keeps the directory.
        Some(_) => Err(evidence.pop().unwrap_or_default()),
        None => Ok(()),
    }
}

/// Each Go cache of the plan that lies beneath `checkout`.
fn go_caches_beneath<'a>(
    plan: &'a ReclaimPlan,
    checkout: &'a Path,
) -> impl Iterator<Item = &'a Path> {
    plan.candidates
        .iter()
        .filter(|candidate| {
            matches!(candidate.subject, Subject::BuildState { ecosystem } if ecosystem == Ecosystem::Go)
        })
        .map(|candidate| candidate.path.as_path())
        .filter(move |path| path.starts_with(checkout))
}

/// Lets the owner write each directory beneath `path`, without following a
/// symbolic link. A directory that refuses the change makes the removal
/// fail, and the removal reports it.
fn grant_owner_write(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(meta) = fs::symlink_metadata(path) else {
        return;
    };
    if !meta.is_dir() {
        return;
    }
    let mode = meta.permissions().mode();
    if mode & 0o700 != 0o700 {
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o700));
    }
    let Ok(entries) = fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        grant_owner_write(&entry.path());
    }
}

fn remove(candidate: &Candidate, git: &Git) -> Result<(), String> {
    let path = candidate.path.as_path();
    match candidate.subject {
        // The standard library removes a tree without following a symbolic
        // link, so a link inside build state loses only itself.
        Subject::BuildState { .. } | Subject::FetchedToolchain { .. } => {
            // Go writes each module beneath directories that no one may
            // write, so the removal first grants the owner that right.
            if matches!(candidate.subject, Subject::BuildState { ecosystem } if ecosystem == Ecosystem::Go)
            {
                grant_owner_write(path);
            }
            fs::remove_dir_all(path)
                .map_err(|err| format!("could not remove {}: {err}", path.display()))
        }
        Subject::LinkedCheckout => {
            let repository = candidate
                .repository
                .as_ref()
                .ok_or_else(|| format!("the plan names no repository for {}", path.display()))?;
            git.remove_worktree(repository.as_path(), path)
        }
    }
}

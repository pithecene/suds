//! A reclaim plan as the window shows it, and the run that a person approves.
//!
//! The core builds the plan and applies it. This module arranges the plan's
//! candidates for a person to read, and it hands the approved part back to
//! the core, so the journal records the run as it records one from the
//! command line.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use suds_core::development::{self, Environment};
use suds_core::organize::journal::Journal;
use suds_core::reclaim::plan::{self, Decision, Kept, ReclaimPlan, Subject};
use suds_core::reclaim::{execute, git::Git, liveness::Liveness};
use suds_core::registry::{self, Reading};
use suds_core::state::{Directories, load_settings};

use crate::project;
use crate::remote;

/// The time for which a directory must have stood unchanged, as the command
/// `suds reclaim` sets it without an option.
pub const IDLE_SECONDS: u64 = 60 * 60;

/// One directory that the plan removes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The entry's position among the plan's candidates.
    pub candidate: usize,
    pub path: String,
    /// The checkout that owns the build state, or the checkout itself.
    pub checkout: String,
    /// The project of the checkout's repository, named as the outline names it.
    pub project: String,
    pub subject: Subject,
    pub bytes: u64,
    /// The position of the removed checkout that holds this entry.
    pub within: Option<usize>,
    /// Each fact that supports the removal.
    pub evidence: Vec<String>,
}

/// The candidates that the plan keeps for one reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeptGroup {
    pub reason: Kept,
    pub count: usize,
    pub bytes: u64,
}

/// A reclaim plan, arranged for a person to read and approve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proposal {
    /// The plan as the core built it.
    pub plan: ReclaimPlan,
    /// Each directory that the plan removes, largest first.
    pub removals: Vec<Entry>,
    /// The kept candidates by reason, with the largest sum first.
    pub kept: Vec<KeptGroup>,
    /// Each process of this user whose working directory the system hides.
    pub hidden: Vec<String>,
    /// The entries that the inspection could not read. A directory beneath
    /// one stands in no list of the plan.
    pub unread: u64,
    /// The roots that the inspection could not measure.
    pub unmeasured: usize,
}

/// The name of the project that holds the repository at `repository`.
fn project_of(repository: &Path) -> String {
    match remote::origin(repository) {
        Some(origin) => project::name(&origin, true),
        None => project::name(&repository.to_string_lossy(), false),
    }
}

impl Proposal {
    /// Arranges `plan`, with each process whose working directory the system
    /// hid at planning. A proposal of a whole inspection leaves nothing unread.
    pub fn of(plan: ReclaimPlan, hidden: Vec<String>) -> Self {
        let removed = |position: &usize| plan.candidates[*position].decision == Decision::Remove;
        // A removed checkout takes its build state with it, so each entry
        // beneath one names it.
        let checkouts: HashMap<&Path, usize> = (0..plan.candidates.len())
            .filter(removed)
            .filter(|position| plan.candidates[*position].subject == Subject::LinkedCheckout)
            .map(|position| (plan.candidates[position].path.as_path(), position))
            .collect();
        // The native path names the repository, so two paths with one lossy text stay apart.
        let mut projects: HashMap<PathBuf, String> = HashMap::new();
        let mut removals: Vec<Entry> = (0..plan.candidates.len())
            .filter(removed)
            .map(|position| {
                let candidate = &plan.candidates[position];
                let repository = candidate.repository.as_ref().unwrap_or(&candidate.checkout);
                let repository = repository.as_path();
                let project = projects
                    .entry(repository.to_owned())
                    .or_insert_with(|| project_of(repository))
                    .clone();
                let holder = checkouts.get(candidate.checkout.as_path()).copied();
                Entry {
                    candidate: position,
                    path: candidate.path.display(),
                    checkout: candidate.checkout.display(),
                    project,
                    subject: candidate.subject,
                    bytes: candidate.allocated_bytes,
                    within: holder.filter(|holder| *holder != position),
                    evidence: candidate.evidence.clone(),
                }
            })
            .collect();
        removals.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.path.cmp(&b.path)));

        let mut kept: Vec<KeptGroup> = Vec::new();
        for candidate in &plan.candidates {
            let Some(reason) = candidate.kept else {
                continue;
            };
            let group = match kept.iter_mut().find(|group| group.reason == reason) {
                Some(group) => group,
                None => {
                    kept.push(KeptGroup {
                        reason,
                        count: 0,
                        bytes: 0,
                    });
                    kept.last_mut()
                        .expect("the group that the line above added")
                }
            };
            group.count += 1;
            group.bytes = group.bytes.saturating_add(candidate.allocated_bytes);
        }
        kept.sort_by(|a, b| {
            b.bytes
                .cmp(&a.bytes)
                .then_with(|| a.reason.label().cmp(b.reason.label()))
        });
        Proposal {
            plan,
            removals,
            kept,
            hidden,
            unread: 0,
            unmeasured: 0,
        }
    }

    /// Whether the inspection read each root whole. A plan of a partial
    /// inspection can miss a directory.
    pub fn complete(&self) -> bool {
        self.unread == 0 && self.unmeasured == 0
    }

    /// Whether the plan kept a candidate only because the system hides a
    /// process's working directory. A person can then plan again and
    /// tolerate those processes.
    pub fn held_by_hidden(&self) -> bool {
        !self.hidden.is_empty() && !self.plan.tolerate_unreadable_processes
    }

    /// Leaves the entry at `candidate` out of the run, or takes it back in.
    /// A checkout that stays keeps its build state, so leaving build state
    /// out leaves its removed checkout out, and taking a checkout in takes
    /// in the build state beneath it.
    pub fn toggle(&self, excluded: &mut BTreeSet<usize>, candidate: usize) {
        let Some(entry) = self.removals.iter().find(|e| e.candidate == candidate) else {
            return;
        };
        if excluded.remove(&candidate) {
            for inner in self.removals.iter().filter(|e| e.within == Some(candidate)) {
                excluded.remove(&inner.candidate);
            }
        } else {
            excluded.insert(candidate);
            if let Some(holder) = entry.within {
                excluded.insert(holder);
            }
        }
    }

    /// The count and the bytes of the entries that a run without `excluded`
    /// removes.
    pub fn removing(&self, excluded: &BTreeSet<usize>) -> (usize, u64) {
        let chosen = self
            .removals
            .iter()
            .filter(|entry| !excluded.contains(&entry.candidate));
        chosen.fold((0, 0), |(count, bytes), entry| {
            (count + 1, bytes + entry.bytes)
        })
    }

    /// The plan that a run applies: the core's plan without each removal
    /// that a person left out. The plan still names each kept candidate.
    pub fn chosen(&self, excluded: &BTreeSet<usize>) -> ReclaimPlan {
        let mut plan = self.plan.clone();
        let mut position = 0;
        plan.candidates.retain(|_| {
            let keep = !excluded.contains(&position);
            position += 1;
            keep
        });
        plan
    }
}

/// The registry's standing of each checkout.
fn standings(directories: &Directories) -> Reading {
    registry::reading(&directories.registry())
}

fn directories(environment: &Environment) -> Result<Directories, String> {
    Directories::of(environment)
        .ok_or_else(|| "the environment names no home directory, so Suds has no state".to_owned())
}

/// Plans a reclaim of the settings' roots as `suds reclaim` plans one, and
/// changes nothing. A true `tolerate` proceeds while the system hides a
/// process's working directory.
pub fn propose(environment: &Environment, tolerate: bool, now: i64) -> Result<Proposal, String> {
    let directories = directories(environment)?;
    let settings = load_settings(&directories.settings())?;
    if settings.roots.is_empty() {
        return Err("the settings name no root, so Suds has nothing to plan".to_owned());
    }
    // The command inspects without the stores, and the plan names none.
    let scan = development::inspect(&settings.roots, &Environment::default());
    let liveness = Liveness::observe();
    let hidden = liveness
        .as_ref()
        .map(|liveness| liveness.unreadable().to_vec())
        .unwrap_or_default();
    let observed = plan::Observed {
        liveness: &liveness,
        registry: &standings(&directories),
    };
    let policy = plan::Policy {
        idle_seconds: IDLE_SECONDS,
        checkouts: plan::CheckoutScope::Released,
        tolerate_unreadable_processes: tolerate,
    };
    let plan = plan::plan_reclaim(&scan, &observed, &Git::default(), policy, now);
    // An unread entry may hold a checkout, so the proposal states what the inspection missed.
    Ok(Proposal {
        unread: scan.error_count,
        unmeasured: scan.workspaces.iter().filter(|w| !w.measured).count(),
        ..Proposal::of(plan, hidden)
    })
}

/// Applies `plan` on a person's approval. The core verifies each removal
/// again, and the journal in the user's state directory records the run.
pub fn apply(
    environment: &Environment,
    plan: &ReclaimPlan,
    now: i64,
) -> Result<execute::ReclaimReport, String> {
    let directories = directories(environment)?;
    let mut journal = Journal::open(&directories.journal()).map_err(|err| err.to_string())?;
    let git = Git::default();
    let conditions = execute::Conditions {
        authority: execute::Authority::Person,
        observe: &mut Liveness::observe,
        registry: &mut || standings(&directories),
        git: &git,
        now,
    };
    execute::apply(&mut journal, plan, conditions).map_err(|err| err.to_string())
}

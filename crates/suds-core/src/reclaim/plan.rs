//! The `suds.reclaim/v1` plan and its planner.
//!
//! Planning reads a development inspection, the processes' working
//! directories, and Git's own account of each checkout. It changes nothing.
//! Each candidate carries its decision and the evidence for it, so a person
//! or a journal reader can see why Suds removed or kept each directory.

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::development::{Checkout, CheckoutKind, DevelopmentScan, Ecosystem, Survey};
use crate::inspection::EntryRevision;
use crate::model::Measure;
use crate::organize::native::NativePath;
use crate::organize::plan::new_id;
use crate::reclaim::git::Git;
use crate::reclaim::liveness::Liveness;
use crate::registry::{self, Standings};
use crate::timestamp;

/// The schema tag of every reclaim plan.
pub const RECLAIM_SCHEMA: &str = "suds.reclaim/v1";

/// What a caller allows a plan to remove.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    /// The time for which a directory must have stood unchanged. A recent
    /// change suggests a build in progress, so it protects the directory. No
    /// age ever causes a removal.
    pub idle_seconds: u64,
    /// Whether the plan considers linked checkouts as well as build state.
    pub checkouts: bool,
    /// Whether a removal may proceed while the system hides the working
    /// directory of one of this user's processes.
    pub tolerate_unreadable_processes: bool,
}

/// A plan to remove derived development state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReclaimPlan {
    pub schema: String,
    pub id: String,
    /// The time at which Suds built the plan, in RFC 3339 form and UTC.
    pub created_at: String,
    pub idle_seconds: u64,
    pub tolerate_unreadable_processes: bool,
    pub candidates: Vec<Candidate>,
}

impl ReclaimPlan {
    /// The allocated bytes of the candidates that the plan removes.
    pub fn removable_bytes(&self) -> u64 {
        self.candidates
            .iter()
            .filter(|candidate| candidate.decision == Decision::Remove)
            .map(|candidate| candidate.allocated_bytes)
            .sum()
    }
}

/// One directory that the plan considered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub path: NativePath,
    pub subject: Subject,
    /// The checkout that owns the build state, or the checkout itself.
    pub checkout: NativePath,
    /// For a linked checkout, the main checkout that holds its repository.
    pub repository: Option<NativePath>,
    /// The directory's revision at planning.
    pub expected: EntryRevision,
    pub allocated_bytes: u64,
    pub logical_bytes: u64,
    pub decision: Decision,
    /// The reason for a kept candidate, in a form that automation matches.
    pub kept: Option<Kept>,
    /// Each fact that supports the decision.
    pub evidence: Vec<String>,
}

/// The class of a candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Subject {
    BuildState { ecosystem: Ecosystem },
    LinkedCheckout,
}

impl Subject {
    pub fn label(self) -> &'static str {
        match self {
            Subject::BuildState { ecosystem } => ecosystem.label(),
            Subject::LinkedCheckout => "checkout",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Remove,
    Keep,
}

/// The reason that a plan keeps a candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kept {
    /// A process works in the checkout.
    InUse,
    /// An entry changed within the plan's idle time.
    RecentlyModified,
    /// The checkout holds a changed, staged, or untracked file.
    UncommittedChanges,
    /// The checkout's head holds a commit that no remote holds.
    UnpushedCommits,
    /// Git tracks a file in the build state, or would track one.
    TrackedFiles,
    /// A checkout lies beneath the directory.
    HoldsCheckout,
    /// A mount point lies beneath the directory.
    HoldsMountPoint,
    /// A client registered the checkout and has not released it.
    Registered,
    /// Suds could not establish a fact that a removal needs.
    Unverified,
}

/// What the machine reports at planning: the processes' working
/// directories, or the reason that Suds could not read them, and the
/// registry's standing of each checkout.
pub struct Observed<'a> {
    pub liveness: &'a Result<Liveness, String>,
    pub registry: &'a Standings,
}

/// Plans the removal of the derived state that `scan` found. A failure to
/// read the processes' working directories keeps every candidate. A
/// checkout that a client released comes first, and the plan considers it
/// whatever the policy says of linked checkouts.
pub fn plan_reclaim(
    scan: &DevelopmentScan,
    observed: &Observed<'_>,
    git: &Git,
    policy: Policy,
    now: i64,
) -> ReclaimPlan {
    let judge = Judge {
        liveness: observed.liveness,
        registry: observed.registry,
        git,
        idle_since: idle_since(now, policy.idle_seconds),
        tolerate_unreadable_processes: policy.tolerate_unreadable_processes,
    };
    let released = |checkout: &Checkout| {
        observed
            .registry
            .get(&checkout.path)
            .is_some_and(|standing| standing.event == registry::Event::Released)
    };
    let mut ordered: Vec<&Checkout> = scan.checkouts.iter().collect();
    ordered.sort_by_key(|checkout| !released(checkout));
    let mut candidates = Vec::new();
    // Two routes of an inspection can reach one checkout, and a plan names
    // each directory once.
    let mut named = HashSet::new();
    for checkout in ordered {
        for build in &checkout.build_state {
            if !named.insert(build.path.clone()) {
                continue;
            }
            let subject = Subject::BuildState {
                ecosystem: build.ecosystem,
            };
            let (kept, evidence) = judge.build_state(
                &checkout.path,
                &build.path,
                build.ecosystem,
                &build.measure,
                &build.survey,
            );
            candidates.extend(candidate(
                &build.path,
                subject,
                checkout,
                &build.measure,
                kept,
                evidence,
            ));
        }
        let linked =
            checkout.kind == CheckoutKind::Linked && (policy.checkouts || released(checkout));
        if linked && named.insert(checkout.path.clone()) {
            let (kept, evidence) = judge_linked(&judge, checkout, scan);
            let subject = Subject::LinkedCheckout;
            candidates.extend(candidate(
                &checkout.path,
                subject,
                checkout,
                &checkout.own,
                kept,
                evidence,
            ));
        }
    }
    ReclaimPlan {
        schema: RECLAIM_SCHEMA.to_owned(),
        id: new_id("reclaim"),
        created_at: timestamp::format_utc(now),
        idle_seconds: policy.idle_seconds,
        tolerate_unreadable_processes: policy.tolerate_unreadable_processes,
        candidates,
    }
}

/// The time before which a directory counts as idle. Timestamps in this
/// form sort in the order of their times.
pub(crate) fn idle_since(now: i64, idle_seconds: u64) -> String {
    let idle = i64::try_from(idle_seconds).unwrap_or(i64::MAX);
    timestamp::format_utc(now.saturating_sub(idle))
}

/// Judges a linked checkout of an inspection. A checkout beneath it keeps
/// it, and the inspection's measures date its newest entry.
fn judge_linked(
    judge: &Judge<'_>,
    checkout: &Checkout,
    scan: &DevelopmentScan,
) -> (Option<Kept>, Vec<String>) {
    let nested = scan
        .checkouts
        .iter()
        .find(|other| other.path != checkout.path && other.path.starts_with(&checkout.path));
    if let Some(nested) = nested {
        let evidence = vec![format!(
            "the checkout {} lies beneath it",
            nested.path.display()
        )];
        return (Some(Kept::HoldsCheckout), evidence);
    }
    let newest = checkout
        .build_state
        .iter()
        .map(|build| &build.measure)
        .chain([&checkout.own])
        .filter_map(|measure| measure.newest_modified.as_deref())
        .max();
    judge.checkout(&checkout.path, newest, &Survey::default())
}

/// The facts that every judgment shares. The planner and the executor judge
/// through the same functions, so a removal holds to the plan's own rules.
pub(crate) struct Judge<'a> {
    pub(crate) liveness: &'a Result<Liveness, String>,
    pub(crate) registry: &'a Standings,
    pub(crate) git: &'a Git,
    pub(crate) idle_since: String,
    pub(crate) tolerate_unreadable_processes: bool,
}

impl Judge<'_> {
    /// Judges one directory of build state from its measure and its survey.
    pub(crate) fn build_state(
        &self,
        checkout: &Path,
        path: &Path,
        ecosystem: Ecosystem,
        measure: &Measure,
        survey: &Survey,
    ) -> (Option<Kept>, Vec<String>) {
        let mut evidence = vec![format!(
            "the layout of {} proves that a build regenerates the directory",
            ecosystem.label()
        )];
        let kept = surveyed(survey, &mut evidence)
            .or_else(|| self.in_use(checkout, &mut evidence))
            .or_else(|| self.recent(measure.newest_modified.as_deref(), &mut evidence))
            .or_else(|| self.tracked(checkout, path, &mut evidence));
        (kept, evidence)
    }

    /// Judges a linked checkout: no process works in it, nothing in it
    /// changed within the idle time, and Git finds its tree clean and its
    /// head on a remote.
    pub(crate) fn checkout(
        &self,
        path: &Path,
        newest: Option<&str>,
        survey: &Survey,
    ) -> (Option<Kept>, Vec<String>) {
        let mut evidence = Vec::new();
        let kept = surveyed(survey, &mut evidence)
            .or_else(|| self.in_use(path, &mut evidence))
            .or_else(|| self.recent(newest, &mut evidence))
            .or_else(|| unsaved_work(self.git, path, &mut evidence));
        (kept, evidence)
    }

    /// Keeps a candidate when a process works in its checkout, or when Suds
    /// cannot tell where each of this user's processes works.
    fn in_use(&self, checkout: &Path, evidence: &mut Vec<String>) -> Option<Kept> {
        match self.registry.get(checkout) {
            Some(standing) if standing.event == registry::Event::Registered => {
                evidence.push(format!(
                    "the client {} registered the checkout at {} and has not released it",
                    standing.client, standing.at
                ));
                return Some(Kept::Registered);
            }
            Some(standing) => evidence.push(format!(
                "the client {} released the checkout at {}",
                standing.client, standing.at
            )),
            None => {}
        }
        let liveness = match self.liveness {
            Ok(liveness) => liveness,
            Err(reason) => {
                evidence.push(format!(
                    "Suds could not read the working directories of the processes: {reason}"
                ));
                return Some(Kept::Unverified);
            }
        };
        if let Some(directory) = liveness.within(checkout) {
            evidence.push(format!("a process works in {}", directory.display()));
            return Some(Kept::InUse);
        }
        let hidden = liveness.unreadable();
        if !hidden.is_empty() {
            let listed = hidden.join(", ");
            if !self.tolerate_unreadable_processes {
                evidence.push(format!(
                    "the system hides the working directory of these processes of this user: {listed}"
                ));
                return Some(Kept::Unverified);
            }
            evidence.push(format!(
                "the caller tolerates the hidden working directory of these processes: {listed}"
            ));
        }
        evidence.push(format!("no process works in {}", checkout.display()));
        None
    }

    /// Keeps a candidate whose newest entry changed within the idle time.
    fn recent(&self, newest: Option<&str>, evidence: &mut Vec<String>) -> Option<Kept> {
        match newest {
            Some(newest) if newest > self.idle_since.as_str() => {
                evidence.push(format!(
                    "an entry changed at {newest}, after {}",
                    self.idle_since
                ));
                Some(Kept::RecentlyModified)
            }
            Some(newest) => {
                evidence.push(format!("its newest entry dates from {newest}"));
                None
            }
            None => None,
        }
    }

    /// Keeps build state in which Git tracks a file or would track one. Such
    /// a file is no derived state, whatever directory holds it.
    fn tracked(&self, checkout: &Path, path: &Path, evidence: &mut Vec<String>) -> Option<Kept> {
        match self.git.ignores_all(checkout, path) {
            Ok(true) => {
                evidence.push("Git ignores every file beneath it".to_owned());
                None
            }
            Ok(false) => {
                evidence.push(
                    "Git tracks a file beneath it, or would track one, so a build does not regenerate the whole directory"
                        .to_owned(),
                );
                Some(Kept::TrackedFiles)
            }
            Err(reason) => {
                evidence.push(reason);
                Some(Kept::Unverified)
            }
        }
    }
}

/// Keeps a directory that holds more than derived state, or whose
/// measurement could not read an entry.
fn surveyed(survey: &Survey, evidence: &mut Vec<String>) -> Option<Kept> {
    if survey.checkouts_beneath > 0 {
        evidence.push(format!(
            "{} directories beneath it hold a Git repository",
            survey.checkouts_beneath
        ));
        return Some(Kept::HoldsCheckout);
    }
    if survey.skipped_mount_count > 0 {
        evidence.push(format!(
            "{} mount points lie beneath it, and Suds measured nothing behind them",
            survey.skipped_mount_count
        ));
        return Some(Kept::HoldsMountPoint);
    }
    if survey.error_count > 0 {
        evidence.push(format!(
            "Suds could not read {} entries beneath it",
            survey.error_count
        ));
        return Some(Kept::Unverified);
    }
    None
}

/// Keeps a checkout that holds work that no remote holds, by Git's account.
/// A failed query keeps the checkout too.
fn unsaved_work(git: &Git, checkout: &Path, evidence: &mut Vec<String>) -> Option<Kept> {
    match git.is_clean(checkout) {
        Ok(true) => evidence.push("Git reports no changed, staged, or untracked file".to_owned()),
        Ok(false) => {
            evidence.push("Git reports a changed, staged, or untracked file".to_owned());
            return Some(Kept::UncommittedChanges);
        }
        Err(reason) => {
            evidence.push(reason);
            return Some(Kept::Unverified);
        }
    }
    match git.unpushed(checkout) {
        Ok(0) => {
            evidence.push("a remote-tracking branch holds every commit of its head".to_owned());
            None
        }
        Ok(count) => {
            evidence.push(format!(
                "its head holds {count} commits that no remote holds"
            ));
            Some(Kept::UnpushedCommits)
        }
        Err(reason) => {
            evidence.push(reason);
            Some(Kept::Unverified)
        }
    }
}

/// Builds one candidate, or none when the directory vanished after the
/// inspection.
fn candidate(
    path: &Path,
    subject: Subject,
    checkout: &Checkout,
    measure: &Measure,
    kept: Option<Kept>,
    evidence: Vec<String>,
) -> Option<Candidate> {
    let meta = fs::symlink_metadata(path).ok()?;
    Some(Candidate {
        path: NativePath::new(path.to_path_buf()),
        subject,
        checkout: NativePath::new(checkout.path.clone()),
        repository: checkout.repository.clone().map(NativePath::new),
        expected: EntryRevision::from_metadata(&meta),
        allocated_bytes: measure.allocated_bytes,
        logical_bytes: measure.logical_bytes,
        decision: if kept.is_some() {
            Decision::Keep
        } else {
            Decision::Remove
        },
        kept,
        evidence,
    })
}

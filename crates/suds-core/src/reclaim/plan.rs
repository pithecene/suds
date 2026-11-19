//! The `suds.reclaim/v1` plan and its planner.
//!
//! Planning reads a development inspection, the processes' working
//! directories, and Git's own account of each checkout. It changes nothing.
//! Each candidate carries its decision and the evidence for it, so a person
//! or a journal reader can see why Suds removed or kept each directory.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::development::{Checkout, CheckoutKind, DevelopmentScan, Ecosystem};
use crate::inspection::EntryRevision;
use crate::model::Measure;
use crate::organize::native::NativePath;
use crate::organize::plan::new_id;
use crate::reclaim::git::Git;
use crate::reclaim::liveness::Liveness;
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
}

/// A plan to remove derived development state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReclaimPlan {
    pub schema: String,
    pub id: String,
    /// The time at which Suds built the plan, in RFC 3339 form and UTC.
    pub created_at: String,
    pub idle_seconds: u64,
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
    /// Another checkout lies beneath this one.
    HoldsCheckout,
    /// Suds could not establish a fact that a removal needs.
    Unverified,
}

/// Plans the removal of the derived state that `scan` found. The `liveness`
/// argument holds the processes' working directories, or the reason that
/// Suds could not read them, and that failure keeps every candidate.
pub fn plan_reclaim(
    scan: &DevelopmentScan,
    liveness: &Result<Liveness, String>,
    git: &Git,
    policy: Policy,
    now: i64,
) -> ReclaimPlan {
    let idle = i64::try_from(policy.idle_seconds).unwrap_or(i64::MAX);
    let planner = Planner {
        liveness,
        git,
        // Timestamps in this form sort in the order of their times.
        idle_since: timestamp::format_utc(now.saturating_sub(idle)),
    };
    let mut candidates = Vec::new();
    for checkout in &scan.checkouts {
        for build in &checkout.build_state {
            let subject = Subject::BuildState {
                ecosystem: build.ecosystem,
            };
            let mut evidence = vec![format!(
                "the layout of {} proves that a build regenerates the directory",
                build.ecosystem.label()
            )];
            let kept = planner
                .in_use(checkout, &mut evidence)
                .or_else(|| planner.recent(&[&build.measure], &mut evidence));
            candidates.extend(candidate(
                &build.path,
                subject,
                checkout,
                &build.measure,
                kept,
                evidence,
            ));
        }
        if policy.checkouts && checkout.kind == CheckoutKind::Linked {
            let (kept, evidence) = planner.judge_checkout(checkout, scan);
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
        candidates,
    }
}

/// The facts that every judgment of one plan shares.
struct Planner<'a> {
    liveness: &'a Result<Liveness, String>,
    git: &'a Git,
    /// The time before which a directory counts as idle.
    idle_since: String,
}

impl Planner<'_> {
    /// Keeps a candidate when a process works in its checkout, or when Suds
    /// could not read the processes' working directories.
    fn in_use(&self, checkout: &Checkout, evidence: &mut Vec<String>) -> Option<Kept> {
        match self.liveness {
            Err(reason) => {
                evidence.push(format!(
                    "Suds could not read the working directories of the processes: {reason}"
                ));
                Some(Kept::Unverified)
            }
            Ok(liveness) => match liveness.within(&checkout.path) {
                Some(directory) => {
                    evidence.push(format!("a process works in {}", directory.display()));
                    Some(Kept::InUse)
                }
                None => {
                    evidence.push(format!("no process works in {}", checkout.path.display()));
                    None
                }
            },
        }
    }

    /// Keeps a candidate when any of `measures` holds an entry that changed
    /// within the idle time.
    fn recent(&self, measures: &[&Measure], evidence: &mut Vec<String>) -> Option<Kept> {
        let newest = measures
            .iter()
            .filter_map(|measure| measure.newest_modified.as_deref())
            .max();
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

    /// Judges a linked checkout: no process works in it, nothing in it
    /// changed within the idle time, and Git finds its tree clean and its
    /// head on a remote.
    fn judge_checkout(
        &self,
        checkout: &Checkout,
        scan: &DevelopmentScan,
    ) -> (Option<Kept>, Vec<String>) {
        let mut evidence = Vec::new();
        let nested = scan
            .checkouts
            .iter()
            .find(|other| other.path != checkout.path && other.path.starts_with(&checkout.path));
        if let Some(nested) = nested {
            evidence.push(format!(
                "the checkout {} lies beneath it",
                nested.path.display()
            ));
            return (Some(Kept::HoldsCheckout), evidence);
        }
        let mut measures = vec![&checkout.own];
        measures.extend(checkout.build_state.iter().map(|build| &build.measure));
        let kept = self
            .in_use(checkout, &mut evidence)
            .or_else(|| self.recent(&measures, &mut evidence))
            .or_else(|| unsaved_work(self.git, &checkout.path, &mut evidence));
        (kept, evidence)
    }
}

/// Keeps a checkout that holds work that no remote holds, by Git's account.
/// A failed query keeps the checkout too.
pub(crate) fn unsaved_work(git: &Git, checkout: &Path, evidence: &mut Vec<String>) -> Option<Kept> {
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

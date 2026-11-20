//! The `suds.reclaim/v1` plan and its planner.
//!
//! Planning reads a development inspection, the processes' working
//! directories, and Git's own account of each checkout. It changes nothing.
//! Each candidate carries its decision and the evidence for it, so a person
//! or a journal reader can see why Suds removed or kept each directory.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::development::{self, Checkout, CheckoutKind, DevelopmentScan, Ecosystem, Survey};
use crate::inspection::EntryRevision;
use crate::model::Measure;
use crate::organize::native::NativePath;
use crate::organize::plan::new_id;
use crate::reclaim::git::Git;
use crate::reclaim::liveness::Liveness;
use crate::registry::{self, Reading};
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
    /// Which linked checkouts the plan considers alongside derived state.
    pub checkouts: CheckoutScope,
    /// Whether a removal may proceed while the system hides the working
    /// directory of one of this user's processes.
    pub tolerate_unreadable_processes: bool,
}

/// Which linked checkouts a reclaim plan can include.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckoutScope {
    /// Consider derived state only.
    DerivedOnly,
    /// Also consider linked checkouts their clients have released.
    Released,
    /// Also consider every linked checkout.
    AllLinked,
}

/// A plan to reclaim development storage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReclaimPlan {
    pub schema: String,
    pub id: String,
    /// The time at which Suds built the plan, in RFC 3339 form and UTC.
    pub created_at: String,
    pub idle_seconds: u64,
    /// A plan that an earlier release recorded names no value, and it
    /// tolerated no such process.
    #[serde(default)]
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
    BuildState {
        ecosystem: Ecosystem,
    },
    LinkedCheckout,
    /// A toolchain that a checkout fetched into its own tree as a
    /// repository, such as a vcpkg checkout. The checkout's build fetches
    /// it again.
    FetchedToolchain {
        ecosystem: Ecosystem,
    },
}

impl Subject {
    pub fn label(self) -> &'static str {
        match self {
            Subject::BuildState { ecosystem } => ecosystem.label(),
            Subject::LinkedCheckout => "checkout",
            Subject::FetchedToolchain { .. } => "toolchain",
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
    /// Retained for plans that recorded a mismatch with the released head.
    LandedHeadChanged,
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
    /// Another user owns a directory in it, so the user who runs Suds
    /// cannot remove it.
    OwnedByAnotherUser,
}

/// What the machine reports at planning: the processes' working
/// directories, or the reason that Suds could not read them, and the
/// registry's standing of each checkout.
pub struct Observed<'a> {
    pub liveness: &'a Result<Liveness, String>,
    pub registry: &'a Reading,
}

impl Kept {
    /// The reason as the plan's JSON names it.
    pub fn label(self) -> &'static str {
        match self {
            Kept::InUse => "in_use",
            Kept::RecentlyModified => "recently_modified",
            Kept::UncommittedChanges => "uncommitted_changes",
            Kept::UnpushedCommits => "unpushed_commits",
            Kept::LandedHeadChanged => "landed_head_changed",
            Kept::TrackedFiles => "tracked_files",
            Kept::HoldsCheckout => "holds_checkout",
            Kept::HoldsMountPoint => "holds_mount_point",
            Kept::Registered => "registered",
            Kept::OwnedByAnotherUser => "owned_by_another_user",
            Kept::Unverified => "unverified",
        }
    }
}

/// Plans reclamation from `scan` within the policy's scope. A failure to
/// read the processes' working directories keeps every candidate. Released
/// checkouts and their build state take priority within that scope.
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
        observed.registry.as_ref().is_ok_and(|standings| {
            standings
                .get(&checkout.path)
                .is_some_and(|standing| standing.event == registry::Event::Released)
        })
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
                &build.repositories,
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
        let linked = checkout.kind == CheckoutKind::Linked
            && match policy.checkouts {
                CheckoutScope::DerivedOnly => false,
                CheckoutScope::Released => released(checkout),
                CheckoutScope::AllLinked => true,
            };
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
    for store in &scan.stores {
        let owner = store.owner.as_ref().and_then(|owner| {
            scan.checkouts
                .iter()
                .find(|checkout| checkout.path == *owner)
        });
        let Some(owner) = owner else {
            continue;
        };
        if !named.insert(store.path.clone()) {
            continue;
        }
        let subject = Subject::FetchedToolchain {
            ecosystem: store.ecosystem,
        };
        let (_, survey, repositories) = development::survey_tree(&store.path);
        let (kept, evidence) = judge.fetched_toolchain(
            &owner.path,
            &store.path,
            store.ecosystem,
            store.measure.newest_modified.as_deref(),
            &survey,
            &repositories,
        );
        candidates.extend(candidate(
            &store.path,
            subject,
            owner,
            &store.measure,
            kept,
            evidence,
        ));
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
    // Git removes a checkout with everything that it ignores, so each
    // repository inside its build state must hold nothing of its own.
    let repositories: Vec<PathBuf> = checkout
        .build_state
        .iter()
        .flat_map(|build| build.repositories.iter().cloned())
        .collect();
    judge.checkout(&checkout.path, newest, &Survey::default(), &repositories)
}

/// The facts that every judgment shares. The planner and the executor judge
/// through the same functions, so a removal holds to the plan's own rules.
pub(crate) struct Judge<'a> {
    pub(crate) liveness: &'a Result<Liveness, String>,
    pub(crate) registry: &'a Reading,
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
        repositories: &[PathBuf],
    ) -> (Option<Kept>, Vec<String>) {
        let mut evidence = vec![format!(
            "the layout of {} proves that a build regenerates the directory",
            ecosystem.label()
        )];
        let kept = surveyed(survey, &mut evidence)
            .or_else(|| self.held_beneath(path, &mut evidence))
            .or_else(|| self.fetched(repositories, &mut evidence))
            .or_else(|| self.in_use(checkout, &mut evidence))
            .or_else(|| self.recent(measure.newest_modified.as_deref(), &mut evidence))
            .or_else(|| self.tracked(checkout, path, &mut evidence));
        (kept, evidence)
    }

    /// Judges a toolchain that a checkout fetched into its own tree. The
    /// checkout's build fetches it again, so it leaves when no process works
    /// in the checkout, nothing in it changed within the idle time, and its
    /// repository holds only what a remote holds.
    pub(crate) fn fetched_toolchain(
        &self,
        checkout: &Path,
        path: &Path,
        ecosystem: Ecosystem,
        newest: Option<&str>,
        survey: &Survey,
        repositories: &[PathBuf],
    ) -> (Option<Kept>, Vec<String>) {
        let mut evidence = vec![format!(
            "the layout of {} proves that the checkout's build fetches the directory",
            ecosystem.label()
        )];
        // No layout proves that the toolchain fetched a repository beneath
        // it, so such a repository may hold work.
        let nested = repositories.first().map(|nested| {
            evidence.push(format!(
                "the repository {} lies beneath it",
                nested.display()
            ));
            Kept::HoldsCheckout
        });
        let kept = surveyed(survey, &mut evidence)
            .or(nested)
            .or_else(|| self.held_beneath(path, &mut evidence))
            .or_else(|| self.in_use(checkout, &mut evidence))
            .or_else(|| self.recent(newest, &mut evidence))
            .or_else(|| self.fetched(&[path.to_path_buf()], &mut evidence))
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
        repositories: &[PathBuf],
    ) -> (Option<Kept>, Vec<String>) {
        let mut evidence = Vec::new();
        let kept = surveyed(survey, &mut evidence)
            .or_else(|| self.held_beneath(path, &mut evidence))
            .or_else(|| self.in_use(path, &mut evidence))
            .or_else(|| self.recent(newest, &mut evidence))
            .or_else(|| self.fetched(repositories, &mut evidence))
            .or_else(|| self.unsaved_work(path, &mut evidence));
        (kept, evidence)
    }

    /// The head that a client's release of `checkout` reported as landed.
    fn landed(&self, checkout: &Path) -> Option<&str> {
        let standing = self.registry.as_ref().ok()?.get(checkout)?;
        (standing.event == registry::Event::Released)
            .then_some(standing.landed.as_deref())
            .flatten()
    }

    fn unsaved_work(&self, checkout: &Path, evidence: &mut Vec<String>) -> Option<Kept> {
        match self.git.is_clean(checkout) {
            Ok(true) => {
                evidence.push("Git reports no changed, staged, or untracked file".to_owned())
            }
            Ok(false) => {
                evidence.push("Git reports a changed, staged, or untracked file".to_owned());
                return Some(Kept::UncommittedChanges);
            }
            Err(reason) => {
                evidence.push(reason);
                return Some(Kept::Unverified);
            }
        }
        if let Some(commit) = self.landed(checkout)
            && self.git.head(checkout).is_ok_and(|head| head == commit)
        {
            self.record_landed(checkout, commit, evidence);
            return None;
        }
        match self.git.unpushed(checkout) {
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

    fn record_landed(&self, checkout: &Path, commit: &str, evidence: &mut Vec<String>) {
        match self.git.remote_tracking_ref(checkout, commit) {
            Ok(Some(reference)) => {
                evidence.push(format!(
                    "the remote-tracking reference {reference} holds {commit}"
                ));
                return;
            }
            Ok(None) => {
                evidence.push(format!(
                    "no remote-tracking reference holds the released head {commit}"
                ));
            }
            Err(reason) => evidence.push(reason),
        }
        evidence.push(format!(
            "its client reported the head {commit} as landed on a remote"
        ));
    }

    /// Keeps a candidate when a process works in its checkout, or when Suds
    /// cannot tell where each of this user's processes works.
    fn in_use(&self, checkout: &Path, evidence: &mut Vec<String>) -> Option<Kept> {
        let standings = match self.registry {
            Ok(standings) => standings,
            Err(reason) => {
                evidence.push(format!(
                    "Suds could not read the registry, so a client may hold the checkout: {reason}"
                ));
                return Some(Kept::Unverified);
            }
        };
        if let Some((held, standing)) = registry::holder(standings, checkout) {
            evidence.push(format!(
                "the client {} registered {} at {} and has not released it",
                standing.client,
                held.display(),
                standing.at
            ));
            return Some(Kept::Registered);
        }
        if let Some(standing) = standings.get(checkout) {
            evidence.push(format!(
                "the client {} released the checkout at {}",
                standing.client, standing.at
            ));
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

    /// Keeps a candidate when a client registered it or a directory beneath
    /// it. [`Judge::in_use`] reports a registry that Suds cannot read, and
    /// it covers a registration above the candidate.
    fn held_beneath(&self, path: &Path, evidence: &mut Vec<String>) -> Option<Kept> {
        let (held, standing) = registry::held_beneath(self.registry.as_ref().ok()?, path)?;
        evidence.push(format!(
            "the client {} registered {} at {} and has not released it",
            standing.client,
            held.display(),
            standing.at
        ));
        Some(Kept::Registered)
    }

    /// Keeps build state that holds a repository with work of its own. A
    /// build fetches some dependencies as repositories, and such a
    /// repository holds only what a remote holds.
    fn fetched(&self, repositories: &[PathBuf], evidence: &mut Vec<String>) -> Option<Kept> {
        for repository in repositories {
            match self.git.holds_only_fetched(repository) {
                Ok(true) => {}
                Ok(false) => {
                    evidence.push(format!(
                        "the repository {} holds a change, a commit that no remote holds, or a second worktree",
                        repository.display()
                    ));
                    return Some(Kept::HoldsCheckout);
                }
                Err(reason) => {
                    evidence.push(reason);
                    return Some(Kept::Unverified);
                }
            }
        }
        if !repositories.is_empty() {
            evidence.push(format!(
                "each of the {} repositories beneath it holds only what a remote holds",
                repositories.len()
            ));
        }
        None
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
            "{} repositories lie beneath it, and no build's layout proves them fetched",
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
    if survey.foreign_directory_count > 0 {
        evidence.push(format!(
            "another user owns {} directories in it, and Suds removes nothing with an administrator's rights",
            survey.foreign_directory_count
        ));
        return Some(Kept::OwnedByAnotherUser);
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

#[cfg(test)]
mod tests {
    use super::{Kept, surveyed};
    use crate::development::Survey;

    #[test]
    fn keeps_a_directory_that_another_user_owns() {
        let survey = Survey {
            foreign_directory_count: 2,
            ..Survey::default()
        };
        let mut evidence = Vec::new();
        assert_eq!(
            surveyed(&survey, &mut evidence),
            Some(Kept::OwnedByAnotherUser)
        );
        assert!(evidence[0].contains("another user owns 2 directories"));
        assert_eq!(surveyed(&Survey::default(), &mut Vec::new()), None);
    }
}

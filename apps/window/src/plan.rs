//! The text of a reclaim plan and of its run, from the view model.

use std::collections::BTreeSet;
use std::path::Path;

use suds_core::reclaim::execute::ReclaimReport;
use suds_core::reclaim::plan::{Kept, Subject};
use suds_view::{Entry, KeptGroup, Proposal, bytes};

use crate::present::{count, short};
use crate::{KeptItem, OutcomeItem, PlanItem};

/// The last part of a path, which names a directory in a row.
fn last(path: &str) -> &str {
    path.rsplit('/')
        .find(|part| !part.is_empty())
        .unwrap_or(path)
}

/// An entry's name: a checkout's directory, or build state behind the directory of its checkout.
fn name(entry: &Entry) -> String {
    let inside = Path::new(&entry.path).strip_prefix(&entry.checkout).ok();
    match inside.map(|inside| inside.to_string_lossy()) {
        Some(inside) if !inside.is_empty() => format!("{}/{inside}", last(&entry.checkout)),
        _ => last(&entry.path).to_owned(),
    }
}

/// An entry's kind, in the words of the storage map's legend.
fn kind(entry: &Entry) -> String {
    match entry.subject {
        Subject::LinkedCheckout => "worktree".to_owned(),
        Subject::BuildState { ecosystem } => format!("{} build state", ecosystem.label()),
        Subject::FetchedToolchain { ecosystem } => format!("{} toolchain", ecosystem.label()),
    }
}

/// One row of the plan's removals. A run without `excluded` removes the entries that it marks.
pub fn row(entry: &Entry, excluded: &BTreeSet<usize>) -> PlanItem {
    let mut caption = format!("{} · {}", kind(entry), entry.project);
    if entry.within.is_some() {
        caption.push_str(" · leaves with its worktree");
    }
    PlanItem {
        id: i32::try_from(entry.candidate).unwrap_or(-1),
        name: name(entry).into(),
        caption: caption.into(),
        // The plan's last fact is the one that decided it, as the command prints it.
        reason: entry.evidence.last().map_or("", String::as_str).into(),
        size: bytes(entry.bytes).into(),
        included: !excluded.contains(&entry.candidate),
        checkout: entry.subject == Subject::LinkedCheckout,
    }
}

/// Why the plan keeps a candidate, as a person reads it.
const KEPT: [(Kept, &str); 13] = [
    (Kept::InUse, "A process is working in the checkout"),
    (Kept::RecentlyModified, "Changed within the last hour"),
    (Kept::UncommittedChanges, "Holds uncommitted changes"),
    (Kept::UnpushedCommits, "Holds commits that no remote has"),
    (
        Kept::LandedHeadChanged,
        "Changed after its tool finished with it",
    ),
    (Kept::NoLandedRelease, "Its work has not landed"),
    (
        Kept::UnaccountedContent,
        "Holds files outside Git and known build state",
    ),
    (Kept::TrackedFiles, "Git tracks files in the build state"),
    (Kept::HoldsCheckout, "Holds another checkout"),
    (Kept::HoldsMountPoint, "Holds a mount point"),
    (Kept::Registered, "In use by a tool"),
    (Kept::Unverified, "Suds could not verify it"),
    (Kept::OwnedByAnotherUser, "Another user owns files in it"),
];

fn kept_label(reason: Kept) -> &'static str {
    let found = KEPT.iter().find(|(kept, _)| *kept == reason);
    // A reason that a later core adds shows under the plan's own name for it.
    found.map_or(reason.label(), |(_, label)| label)
}

pub fn kept(group: &KeptGroup) -> KeptItem {
    KeptItem {
        label: kept_label(group.reason).into(),
        detail: format!(
            "{} · {}",
            count(group.count, "directory", "directories"),
            bytes(group.bytes)
        )
        .into(),
    }
}

/// The line that states what an approved run removes.
pub fn summary(removing: (usize, u64)) -> String {
    match removing {
        (0, _) => "Nothing to remove".to_owned(),
        (number, size) => format!(
            "{} · frees about {}",
            count(number, "directory", "directories"),
            bytes(size)
        ),
    }
}

/// The words on the button that approves a run.
pub fn action(removing: (usize, u64)) -> String {
    match removing.0 {
        0 => "Nothing to remove".to_owned(),
        number => format!("Remove {}…", count(number, "directory", "directories")),
    }
}

/// What a partial inspection left out of the plan, or nothing for a whole one.
fn partial(proposal: &Proposal) -> String {
    let mut missed = Vec::new();
    if proposal.unmeasured > 0 {
        missed.push(count(proposal.unmeasured, "root", "roots"));
    }
    if proposal.unread > 0 {
        let unread = usize::try_from(proposal.unread).unwrap_or(usize::MAX);
        missed.push(count(unread, "entry", "entries"));
    }
    if missed.is_empty() {
        return String::new();
    }
    format!(
        "Suds could not read {}, so the plan may leave out a directory beneath them.",
        missed.join(" and ")
    )
}

/// The lines above the plan's rows: what the inspection missed, and the hidden processes.
pub fn notice(proposal: &Proposal) -> String {
    let lines = [partial(proposal), hidden(proposal)];
    let lines: Vec<String> = lines.into_iter().filter(|line| !line.is_empty()).collect();
    lines.join("\n")
}

/// What the plan says of the processes whose working directory the system hides.
fn hidden(proposal: &Proposal) -> String {
    if proposal.hidden.is_empty() {
        return String::new();
    }
    let processes = count(proposal.hidden.len(), "process", "processes");
    if proposal.held_by_hidden() {
        format!(
            "The system hides the working directory of {processes} that you own, so Suds cannot tell whether one works in a checkout, and the plan keeps each directory: {}.",
            proposal.hidden.join(", ")
        )
    } else {
        format!(
            "The plan proceeds although the system hides the working directory of {processes} that you own."
        )
    }
}

/// Each fact of the entry at `candidate`, with the entry's full path first.
pub fn evidence(proposal: &Proposal, candidate: usize, home: Option<&str>) -> Vec<String> {
    let mut entries = proposal.removals.iter();
    let Some(entry) = entries.find(|entry| entry.candidate == candidate) else {
        return Vec::new();
    };
    let mut facts = vec![short(&entry.path, home)];
    facts.extend(entry.evidence.iter().cloned());
    facts
}

/// One row for each removal of a run, and the line that sums the run up.
pub fn outcome(report: &ReclaimReport, home: Option<&str>) -> (Vec<OutcomeItem>, String) {
    let rows = report.removals.iter().map(|removal| OutcomeItem {
        path: short(&removal.path.display(), home).into(),
        detail: match &removal.refusal {
            None => format!("removed · {}", bytes(removal.freed_bytes)),
            Some(reason) => format!("kept · {reason}"),
        }
        .into(),
        removed: removal.removed,
    });
    let rows: Vec<OutcomeItem> = rows.collect();
    let removed = rows.iter().filter(|row| row.removed).count();
    let mut line = format!(
        "Removed {} and freed about {}.",
        count(removed, "directory", "directories"),
        bytes(report.freed_bytes)
    );
    if removed < rows.len() {
        let refused = count(rows.len() - removed, "directory", "directories");
        line.push_str(&format!(
            " Suds kept {refused}, because a fact changed after planning."
        ));
    }
    line.push_str(&format!(" The journal records run {}.", report.run));
    (rows, line)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use suds_core::development::Ecosystem;
    use suds_core::reclaim::plan::{Kept, RECLAIM_SCHEMA, ReclaimPlan, Subject};
    use suds_view::{Entry, KeptGroup, Proposal};

    use super::{action, kept, notice, row, summary};

    fn proposal(hidden: &[&str], tolerate: bool) -> Proposal {
        let plan = ReclaimPlan {
            schema: RECLAIM_SCHEMA.into(),
            id: "plan-1".into(),
            created_at: "2026-11-21T00:00:00Z".into(),
            idle_seconds: 3600,
            tolerate_unreadable_processes: tolerate,
            candidates: Vec::new(),
        };
        Proposal::of(plan, hidden.iter().map(|name| (*name).to_owned()).collect())
    }

    #[test]
    fn the_notice_states_a_partial_inspection_and_the_hidden_processes() {
        assert_eq!(notice(&proposal(&[], false)), "");

        let held = notice(&proposal(&["7 (systemd)"], false));
        assert!(held.contains("1 process that you own"), "{held}");
        assert!(held.ends_with("the plan keeps each directory: 7 (systemd)."));
        let tolerated = notice(&proposal(&["7 (systemd)"], true));
        assert!(
            tolerated.starts_with("The plan proceeds although"),
            "{tolerated}"
        );

        let partial = Proposal {
            unread: 2,
            unmeasured: 1,
            ..proposal(&["7 (systemd)"], true)
        };
        let lines: Vec<String> = notice(&partial).lines().map(str::to_owned).collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[0],
            "Suds could not read 1 root and 2 entries, so the plan may leave out a directory beneath them."
        );
    }

    fn entry(path: &str, checkout: &str, subject: Subject, within: Option<usize>) -> Entry {
        Entry {
            candidate: 3,
            path: path.into(),
            checkout: checkout.into(),
            project: "pithecene/suds".into(),
            subject,
            bytes: 3 << 30,
            within,
            evidence: vec!["first".into(), "no process works in it".into()],
        }
    }

    #[test]
    fn a_row_names_build_state_behind_its_checkout() {
        let cargo = Subject::BuildState {
            ecosystem: Ecosystem::Cargo,
        };
        let build = entry(
            "/w/suds/.trees/a/target",
            "/w/suds/.trees/a",
            cargo,
            Some(1),
        );
        let item = row(&build, &BTreeSet::new());
        assert_eq!(item.name, "a/target");
        assert!(
            item.caption
                .starts_with("cargo build state · pithecene/suds")
        );
        assert!(item.caption.ends_with("leaves with its worktree"));
        assert_eq!(item.reason, "no process works in it");
        assert!(item.included && !item.checkout);

        let path = "/w/suds/.trees/a";
        let checkout = entry(path, path, Subject::LinkedCheckout, None);
        let item = row(&checkout, &BTreeSet::from([3]));
        assert_eq!(item.name, "a");
        assert_eq!(item.caption, "worktree · pithecene/suds");
        assert!(!item.included && item.checkout);
    }

    #[test]
    fn the_summary_and_a_kept_group_count_their_directories() {
        assert_eq!(summary((0, 0)), "Nothing to remove");
        assert_eq!(summary((1, 1 << 30)), "1 directory · frees about 1.0 GiB");
        assert_eq!(action((0, 0)), "Nothing to remove");
        assert_eq!(action((13, 9 << 30)), "Remove 13 directories…");
        let group = KeptGroup {
            reason: Kept::InUse,
            count: 131,
            bytes: 2 << 30,
        };
        let item = kept(&group);
        assert_eq!(item.label, "A process is working in the checkout");
        assert_eq!(item.detail, "131 directories · 2.0 GiB");
    }
}

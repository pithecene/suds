//! Applying a reclaim plan.
//!
//! The executor records the plan, then judges each candidate again
//! immediately before it acts, because a process can start or a file can
//! change after planning. For each removal it observes the processes,
//! measures the directory, and asks Git anew. The executor removes build
//! state itself, without following a symbolic link, and it has Git remove a
//! linked checkout.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

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
use crate::scan;
use crate::timestamp;

/// The schema tag of a reclaim run's result.
pub const RECLAIM_RUN_SCHEMA: &str = "suds.reclaim-run/v1";

/// Who approved a reclaim run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Authority {
    /// A person ran the command and asked it to apply the plan.
    Person,
    /// The owner's standing approval admitted the plan.
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
    // A person approved what the plan stated, so an entry that changed after
    // the plan keeps its candidate however long it then stood idle.
    let unchanged_since = idle_since(now, plan.idle_seconds).min(plan.created_at.clone());
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
                evidence: Vec::new(),
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
            idle_since: unchanged_since.clone(),
            tolerate_unreadable_processes: plan.tolerate_unreadable_processes,
        };
        let outcome = match check(
            journal.directory(),
            candidate,
            authority,
            &judge,
            &plan.created_at,
            &Granted::new(),
        ) {
            Err(reason) => Err(reason),
            Ok(checked) => {
                journal.append(Event::RemovalStarted {
                    run: run.clone(),
                    index,
                })?;
                // Git removes a checkout's Go caches with it, and Git cannot
                // enter a directory that no one may write. The change walks
                // each cache, so the checkout's check runs again after it.
                let mut granted = Granted::new();
                for cache in &checked.go_caches {
                    grant_owner_write(cache, &plan.created_at, &mut granted);
                }
                let checked = if checked.go_caches.is_empty() {
                    Ok(checked)
                } else {
                    let liveness = observe();
                    let standings = registry();
                    let judge = Judge {
                        liveness: &liveness,
                        registry: &standings,
                        git,
                        idle_since: unchanged_since.clone(),
                        tolerate_unreadable_processes: plan.tolerate_unreadable_processes,
                    };
                    check(
                        journal.directory(),
                        candidate,
                        authority,
                        &judge,
                        &plan.created_at,
                        &granted,
                    )
                };
                checked.and_then(|checked| remove(candidate, git).map(|()| checked.evidence))
            }
        };
        let refusal = match outcome {
            Ok(evidence) => {
                journal.append(Event::Removed {
                    run: run.clone(),
                    index,
                    freed_bytes: candidate.allocated_bytes,
                    evidence,
                })?;
                None
            }
            Err(reason) => {
                journal.append(Event::RemovalRefused {
                    run: run.clone(),
                    index,
                    reason: reason.clone(),
                })?;
                Some(reason)
            }
        };
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

#[derive(Default)]
struct CheckedRemoval {
    evidence: Vec<String>,
    go_caches: Vec<PathBuf>,
}

/// Verifies every precondition of one removal. The survey refuses a mount
/// point first, and the reading of each change comes last, so a change during
/// the survey keeps the candidate. The directories in `granted` hold the status
/// that a change of Suds itself left.
fn check(
    journal: &Path,
    candidate: &Candidate,
    authority: Authority,
    judge: &Judge<'_>,
    planned_at: &str,
    granted: &Granted,
) -> Result<CheckedRemoval, String> {
    let checked = judged(journal, candidate, authority, judge, planned_at)?;
    match changed(candidate.path.as_path(), planned_at, granted)? {
        None => Ok(checked),
        Some(entry) => Err(format!(
            "{} changed after Suds built the plan",
            entry.display()
        )),
    }
}

/// Judges one removal anew. The directory must be the entry that the plan
/// measured, and the plan's own rules must still decide to remove it.
fn judged(
    journal: &Path,
    candidate: &Candidate,
    authority: Authority,
    judge: &Judge<'_>,
    planned_at: &str,
) -> Result<CheckedRemoval, String> {
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
    same_owner(candidate, judge, planned_at)?;
    if candidate.subject == Subject::LinkedCheckout && authority == Authority::StandingApproval {
        return Err(format!(
            "{} is a checkout, which the standing approval does not cover",
            path.display()
        ));
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
            let repository = candidate.repository.as_ref().map(NativePath::as_path);
            if repository.is_none()
                || development::linked_repository(path, &path.join(".git")).as_deref() != repository
            {
                return Err(format!(
                    "{} is no longer a linked checkout of the repository in the plan",
                    path.display()
                ));
            }
            let (measure, survey, repositories) = development::survey_tree(path);
            let (kept, mut evidence) = judge.checkout(
                path,
                measure.newest_modified.as_deref(),
                &survey,
                &repositories,
            );
            if kept.is_none() {
                // The entries may have changed since the plan.
                let go_caches = judge
                    .unaccounted(path, &mut evidence)
                    .map_err(|_| evidence.last().cloned().unwrap_or_default())?;
                return Ok(CheckedRemoval {
                    evidence,
                    go_caches,
                });
            }
            (kept, evidence)
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
        None if candidate.subject == Subject::LinkedCheckout => Ok(CheckedRemoval {
            evidence,
            ..CheckedRemoval::default()
        }),
        None => Ok(CheckedRemoval::default()),
    }
}

/// Verifies that the owner of the candidate is the one of the plan, with the
/// same head for a linked checkout.
fn same_owner(candidate: &Candidate, judge: &Judge<'_>, planned_at: &str) -> Result<(), String> {
    let path = candidate.path.as_path();
    let checkout = candidate.checkout.as_path();
    if candidate.subject != Subject::LinkedCheckout {
        let mut owners = path.ancestors();
        // A toolchain that a build fetched is a repository of its own, and
        // build state that holds a repository at its root is a checkout.
        if matches!(candidate.subject, Subject::FetchedToolchain { .. }) {
            owners.next();
        }
        for directory in owners {
            let repository = directory.join(".git");
            match fs::symlink_metadata(&repository) {
                Ok(_) if directory == checkout => break,
                Ok(_) => {
                    return Err(format!(
                        "another checkout came to own {} after Suds built the plan",
                        path.display()
                    ));
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(format!("could not read {}: {err}", repository.display())),
            }
        }
    }
    let repository = checkout.join(".git");
    let owner = fs::symlink_metadata(&repository)
        .map_err(|err| format!("could not read {}: {err}", repository.display()))?;
    if !candidate
        .owner
        .as_ref()
        .is_some_and(|planned| EntryRevision::from_metadata(&owner).is_same_entry(planned))
    {
        return Err(format!(
            "another repository took the place of {} after Suds built the plan",
            repository.display()
        ));
    }
    // A linked checkout's pointer names its repository, and a rewrite of the
    // pointer keeps its inode.
    if owner.is_file() && status_changed(&owner, planned_at) {
        return Err(format!(
            "{} changed after Suds built the plan",
            repository.display()
        ));
    }
    if candidate.subject == Subject::LinkedCheckout {
        let head = judge.git.head(path)?;
        if candidate.head.as_deref() != Some(head.as_str()) {
            return Err(format!(
                "the head of {} moved after Suds built the plan",
                path.display()
            ));
        }
    }
    Ok(())
}

/// The first entry at or beneath `path` whose status changed at `since` or
/// later. No program sets the time of a status change, so a restore that keeps
/// a file's modification time shows here.
fn changed(path: &Path, since: &str, granted: &Granted) -> Result<Option<PathBuf>, String> {
    use std::os::unix::fs::MetadataExt;
    let mut device = None;
    let mut pending = vec![path.to_path_buf()];
    while let Some(entry) = pending.pop() {
        let unread = |err: std::io::Error| format!("could not read {}: {err}", entry.display());
        let meta = fs::symlink_metadata(&entry).map_err(unread)?;
        let crossed = *device.get_or_insert(meta.dev()) != meta.dev();
        if crossed || scan::mount_at(&entry).map_err(unread)? {
            return Err(format!("{} is a mount point", entry.display()));
        }
        let permitted = granted.get(&entry) == Some(&status_time(&meta));
        if !permitted && status_changed(&meta, since) {
            return Ok(Some(entry));
        }
        if meta.is_dir() {
            for child in fs::read_dir(&entry).map_err(unread)? {
                pending.push(child.map_err(unread)?.path());
            }
        }
    }
    Ok(None)
}

/// The time of each directory's status after Suds changed its permissions.
type Granted = HashMap<PathBuf, (i64, i64)>;

fn status_time(meta: &fs::Metadata) -> (i64, i64) {
    use std::os::unix::fs::MetadataExt;
    (meta.ctime(), meta.ctime_nsec())
}

fn status_changed(meta: &fs::Metadata, since: &str) -> bool {
    use std::os::unix::fs::MetadataExt;
    timestamp::format_utc(meta.ctime()).as_str() >= since
}

/// The plan `id` that `journal` records.
pub fn recorded(journal: &Journal, id: &str) -> Option<ReclaimPlan> {
    journal
        .records()
        .iter()
        .find_map(|record| match &record.event {
            Event::ReclaimPlanned { plan } if plan.id == id => Some(plan.clone()),
            _ => None,
        })
}

/// Lets the owner write each directory beneath `path`, without following a
/// symbolic link. A directory that refuses the change makes the removal
/// fail, and the removal reports it. `granted` takes each changed directory
/// whose status had stood since the plan.
fn grant_owner_write(path: &Path, since: &str, granted: &mut Granted) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(meta) = fs::symlink_metadata(path) else {
        return;
    };
    if !meta.is_dir() {
        return;
    }
    let mode = meta.permissions().mode();
    if mode & 0o700 != 0o700 {
        let changed = fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o700));
        if changed.is_ok()
            && !status_changed(&meta, since)
            && let Ok(meta) = fs::symlink_metadata(path)
        {
            granted.insert(path.to_path_buf(), status_time(&meta));
        }
    }
    let Ok(entries) = fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        grant_owner_write(&entry.path(), since, granted);
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
                grant_owner_write(path, "", &mut Granted::new());
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

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use super::{Granted, changed, grant_owner_write};
    use crate::timestamp;

    /// A grant admits the directory that Suds changed, and nothing that
    /// changed beside it or again after it.
    #[test]
    fn a_grant_admits_no_other_change() {
        let dir = tempfile::TempDir::new().unwrap();
        let cache = fs::canonicalize(dir.path()).unwrap().join("cache");
        let module = cache.join("module");
        fs::create_dir_all(&module).unwrap();
        fs::write(module.join("lib.go"), "package lib").unwrap();
        fs::set_permissions(&module, fs::Permissions::from_mode(0o555)).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();
        let since = timestamp::format_utc(i64::try_from(now.as_secs()).unwrap());
        let mut granted = Granted::new();
        grant_owner_write(&cache, &since, &mut granted);
        assert!(changed(&cache, &since, &Granted::new()).unwrap().is_some());
        assert_eq!(changed(&cache, &since, &granted).unwrap(), None);

        // An earlier directory that a restore moved in.
        let moved = cache.join("moved");
        fs::create_dir(&moved).unwrap();
        assert!(changed(&cache, &since, &granted).unwrap().is_some());
        fs::remove_dir(&moved).unwrap();
        fs::write(module.join("new.go"), "package lib").unwrap();
        fs::remove_file(module.join("new.go")).unwrap();
        assert_eq!(granted.len(), 1);
        assert!(changed(&module, &since, &granted).unwrap().is_some());
    }
}

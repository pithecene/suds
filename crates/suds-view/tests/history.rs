//! The history of a journal that holds a move run, a stores run, and a reclaim run.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use suds_core::development::{Ecosystem, Environment};
use suds_core::inspection::EntryRevision;
use suds_core::organize::execute;
use suds_core::organize::journal::{Event, Journal, RunKind, RunStatus};
use suds_core::organize::native::NativePath;
use suds_core::organize::plan::{MoveRequest, plan_move};
use suds_core::reclaim::execute::Authority;
use suds_core::reclaim::plan::{Candidate, Decision, RECLAIM_SCHEMA, ReclaimPlan, Subject};
use suds_core::state::Directories;
use suds_view::{History, history};
use tempfile::TempDir;

const GIB: u64 = 1 << 30;

fn home() -> (TempDir, Environment) {
    let temp = TempDir::new().unwrap();
    let environment = Environment {
        home: Some(temp.path().canonicalize().unwrap()),
        variables: HashMap::new(),
    };
    (temp, environment)
}

fn journal(environment: &Environment) -> Journal {
    Journal::open(&Directories::of(environment).unwrap().journal()).unwrap()
}

/// A candidate that a plan removes at `path`, which no run touches.
fn candidate(path: &Path) -> Candidate {
    let parent = path.parent().unwrap();
    Candidate {
        path: NativePath::new(path.to_owned()),
        subject: Subject::BuildState {
            ecosystem: Ecosystem::Cargo,
        },
        checkout: NativePath::new(parent.to_owned()),
        repository: None,
        expected: EntryRevision::from_metadata(&fs::metadata(parent).unwrap()),
        owner: None,
        head: None,
        allocated_bytes: 3 * GIB,
        logical_bytes: 3 * GIB,
        decision: Decision::Remove,
        kept: None,
        evidence: Vec::new(),
    }
}

/// Moves one file between two directories of `home` and records the run.
fn move_one(journal: &mut Journal, home: &Path) {
    let (source, destination) = (home.join("Downloads"), home.join("Archive"));
    fs::create_dir_all(&source).unwrap();
    fs::create_dir_all(&destination).unwrap();
    fs::write(source.join("notes.txt"), "notes").unwrap();
    let request = MoveRequest {
        name: "notes.txt".into(),
        expected: None,
    };
    let plan = plan_move(&source, &[request], &destination, 0).unwrap();
    execute::record_plan(journal, &plan).unwrap();
    execute::apply(journal, &plan, &mut |_| {}).unwrap();
}

fn trim_stores(journal: &mut Journal) {
    let run = || "run-stores".to_owned();
    let events = [
        Event::StoresStarted {
            run: run(),
            plan: "plan-stores".into(),
            authority: Authority::StandingApproval,
        },
        Event::StoreTrimStarted {
            run: run(),
            store: "cargo".into(),
            command: "cargo cache".into(),
        },
        Event::StoreTrimmed {
            run: run(),
            store: "cargo".into(),
            before_bytes: 10 * GIB,
            after_bytes: 4 * GIB,
        },
        Event::StoreTrimStarted {
            run: run(),
            store: "npm".into(),
            command: "npm cache verify".into(),
        },
        Event::StoreTrimFailed {
            run: run(),
            store: "npm".into(),
            reason: "the program left with status 1".into(),
        },
        Event::StoresCompleted {
            run: run(),
            status: RunStatus::Partial,
            freed_bytes: 6 * GIB,
        },
    ];
    events
        .into_iter()
        .for_each(|event| journal.append(event).unwrap());
}

/// Records a reclaim run that removed one directory, refused a second, and
/// stopped during a third.
fn reclaim(journal: &mut Journal, home: &Path) {
    let run = || "run-reclaim".to_owned();
    let names = ["a/target", "b/target", "c/target"];
    let plan = ReclaimPlan {
        schema: RECLAIM_SCHEMA.into(),
        id: "plan-reclaim".into(),
        created_at: "2026-11-21T00:00:00Z".into(),
        idle_seconds: 3600,
        tolerate_unreadable_processes: false,
        candidates: names.map(|name| candidate(&home.join(name))).to_vec(),
    };
    let events = [
        Event::ReclaimPlanned { plan },
        Event::ReclaimStarted {
            run: run(),
            plan: "plan-reclaim".into(),
            authority: Authority::Person,
        },
        Event::RemovalStarted {
            run: run(),
            index: 0,
        },
        Event::Removed {
            run: run(),
            index: 0,
            freed_bytes: 3 * GIB,
            evidence: Vec::new(),
        },
        Event::RemovalStarted {
            run: run(),
            index: 1,
        },
        Event::RemovalRefused {
            run: run(),
            index: 1,
            reason: "a process works in the checkout".into(),
        },
        Event::RemovalStarted {
            run: run(),
            index: 2,
        },
    ];
    events
        .into_iter()
        .for_each(|event| journal.append(event).unwrap());
}

#[test]
fn a_workstation_without_a_journal_has_an_empty_history() {
    let (_temp, environment) = home();
    assert_eq!(history(&environment).unwrap(), History::default());
    // The reading created no journal.
    let directories = Directories::of(&environment).unwrap();
    assert!(!directories.journal().exists());
}

#[test]
fn the_history_lists_each_run_with_its_steps_most_recent_first() {
    let (_temp, environment) = home();
    let home = environment.home.clone().unwrap();
    for directory in ["a", "b", "c"] {
        fs::create_dir_all(home.join(directory)).unwrap();
    }
    let mut journal = journal(&environment);
    move_one(&mut journal, &home);
    trim_stores(&mut journal);
    reclaim(&mut journal, &home);
    drop(journal);

    let history = history(&environment).unwrap();
    let kinds: Vec<RunKind> = history.runs.iter().map(|run| run.kind).collect();
    assert_eq!(kinds, [RunKind::Reclaim, RunKind::Stores, RunKind::Move]);
    assert_eq!(history.freed_bytes(), 9 * GIB);

    let reclaim = &history.runs[0];
    assert_eq!(reclaim.authority, Some(Authority::Person));
    // The run has no completion record, so the history states no outcome.
    assert_eq!(reclaim.status, None);
    assert_eq!((reclaim.succeeded(), reclaim.steps.len()), (1, 3));
    assert_eq!(reclaim.freed_bytes(), 3 * GIB);
    assert!(reclaim.steps[0].subject.ends_with("/a/target"));
    assert_eq!(reclaim.steps[0].note, None);
    let refused = reclaim.steps[1].note.as_deref();
    assert_eq!(refused, Some("a process works in the checkout"));
    assert_eq!(reclaim.steps[2].note.as_deref(), Some("started"));

    let stores = &history.runs[1];
    assert_eq!(stores.authority, Some(Authority::StandingApproval));
    assert_eq!(stores.status, Some(RunStatus::Partial));
    assert_eq!(stores.succeeded(), 1);
    assert_eq!(stores.freed_bytes(), 6 * GIB);
    assert_eq!(stores.steps[0].subject, "cargo");
    assert!(!stores.steps[1].succeeded);
    let failed = stores.steps[1].note.as_deref();
    assert_eq!(failed, Some("the program left with status 1"));

    let moved = &history.runs[2];
    assert_eq!(moved.authority, None);
    assert_eq!(moved.status, Some(RunStatus::Complete));
    assert_eq!(moved.freed_bytes(), 0);
    let step = &moved.steps[0];
    assert!(step.succeeded && step.note.is_none());
    assert!(step.subject.ends_with("/Downloads/notes.txt"));
    let destination = step.destination.as_deref().unwrap();
    assert!(destination.ends_with("/Archive/notes.txt"), "{destination}");
}

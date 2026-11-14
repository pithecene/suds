//! Tests of planning, applying, undoing, and reconciling moves against
//! temporary fixture trees and a temporary journal.

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::{MetadataExt, symlink};
use std::path::{Path, PathBuf};

use suds_core::organize::execute::{
    MoveOutcome, OperationError, RunReport, apply, reconcile, record_plan, undo,
};
use suds_core::organize::journal::{
    Event, Finding, Journal, JournalState, MoveStatus, RunStatus, read,
};
use suds_core::organize::plan::{MovePlan, MoveRequest, RefusalCode, plan_move};
use tempfile::TempDir;

/// A source directory, a destination directory, and a journal directory.
struct Fixture {
    _dir: TempDir,
    from: PathBuf,
    to: PathBuf,
    journal: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        // The planner records canonical paths, so the fixture uses them too.
        let root = fs::canonicalize(dir.path()).unwrap();
        let from = root.join("Downloads");
        let to = root.join("Documents");
        fs::create_dir(&from).unwrap();
        fs::create_dir(&to).unwrap();
        Fixture {
            journal: root.join("journal"),
            _dir: dir,
            from,
            to,
        }
    }

    fn plan(&self, names: &[&str]) -> MovePlan {
        let requests: Vec<MoveRequest> = names
            .iter()
            .map(|name| MoveRequest {
                name: OsString::from(name),
                expected: None,
            })
            .collect();
        let plan = plan_move(&self.from, &requests, &self.to, 0).unwrap();
        let mut journal = self.open();
        record_plan(&mut journal, &plan).unwrap();
        plan
    }

    fn open(&self) -> Journal {
        Journal::open(&self.journal).unwrap()
    }

    fn apply(&self, plan: &MovePlan) -> RunReport {
        apply(&mut self.open(), plan, &mut |_| {}).unwrap()
    }

    fn undo(&self, run: &str) -> RunReport {
        undo(&mut self.open(), run, &mut |_| {}).unwrap()
    }

    fn state(&self) -> JournalState {
        JournalState::from_records(&read(&self.journal).unwrap())
    }
}

fn write(path: &Path, contents: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn refusal_codes(report: &RunReport) -> Vec<Option<RefusalCode>> {
    report
        .moves
        .iter()
        .map(|outcome| outcome.refusal.as_ref().map(|refusal| refusal.code))
        .collect()
}

#[test]
fn moves_one_regular_file_and_journals_each_step() {
    let fixture = Fixture::new();
    write(&fixture.from.join("report.pdf"), b"report");
    let inode = fs::metadata(fixture.from.join("report.pdf")).unwrap().ino();
    let plan = fixture.plan(&["report.pdf"]);

    let mut seen = Vec::new();
    let report = apply(&mut fixture.open(), &plan, &mut |outcome: &MoveOutcome| {
        seen.push(outcome.status);
    })
    .unwrap();
    assert_eq!(report.schema, "suds.run/v1");
    assert_eq!(report.status, RunStatus::Complete);
    assert_eq!(seen, [MoveStatus::Applied]);
    assert!(!fixture.from.join("report.pdf").exists());
    let moved = fs::metadata(fixture.to.join("report.pdf")).unwrap();
    assert_eq!(moved.ino(), inode);
    assert_eq!(fs::read(fixture.to.join("report.pdf")).unwrap(), b"report");

    let events: Vec<&str> = read(&fixture.journal)
        .unwrap()
        .iter()
        .map(|record| match record.event {
            Event::PlanCreated { .. } => "plan_created",
            Event::RunStarted { .. } => "run_started",
            Event::MoveStarted { .. } => "move_started",
            Event::MoveApplied { .. } => "move_applied",
            Event::RunCompleted { .. } => "run_completed",
            _ => "other",
        })
        .collect();
    assert_eq!(
        events,
        [
            "plan_created",
            "run_started",
            "move_started",
            "move_applied",
            "run_completed"
        ]
    );
}

#[test]
fn moves_a_directory_with_all_of_its_contents() {
    let fixture = Fixture::new();
    write(&fixture.from.join("project/src/main.rs"), b"fn main() {}");
    write(&fixture.from.join("project/README"), b"read me");
    let plan = fixture.plan(&["project"]);
    assert_eq!(plan.moves[0].measure.files, 2);

    assert_eq!(fixture.apply(&plan).status, RunStatus::Complete);
    assert!(!fixture.from.join("project").exists());
    assert_eq!(
        fs::read(fixture.to.join("project/src/main.rs")).unwrap(),
        b"fn main() {}"
    );
    assert!(fixture.to.join("project/README").exists());
}

#[test]
fn moves_several_selected_entries_and_leaves_the_rest() {
    let fixture = Fixture::new();
    for name in ["a.zip", "b.dmg", "c.txt"] {
        write(&fixture.from.join(name), name.as_bytes());
    }
    let plan = fixture.plan(&["a.zip", "c.txt"]);
    let report = fixture.apply(&plan);
    assert_eq!(report.status, RunStatus::Complete);
    assert_eq!(report.moves.len(), 2);
    assert!(fixture.to.join("a.zip").exists());
    assert!(fixture.to.join("c.txt").exists());
    assert!(fixture.from.join("b.dmg").exists());
    assert!(!fixture.to.join("b.dmg").exists());
}

#[test]
fn refuses_a_destination_that_appeared_after_planning_and_keeps_both_entries() {
    let fixture = Fixture::new();
    write(&fixture.from.join("notes.txt"), b"mine");
    let plan = fixture.plan(&["notes.txt"]);
    write(&fixture.to.join("notes.txt"), b"theirs");

    let report = fixture.apply(&plan);
    assert_eq!(report.status, RunStatus::Refused);
    assert_eq!(
        refusal_codes(&report),
        [Some(RefusalCode::DestinationExists)]
    );
    assert_eq!(fs::read(fixture.from.join("notes.txt")).unwrap(), b"mine");
    assert_eq!(fs::read(fixture.to.join("notes.txt")).unwrap(), b"theirs");
}

#[test]
fn refuses_a_source_that_changed_or_disappeared_after_planning() {
    let fixture = Fixture::new();
    write(&fixture.from.join("changed"), b"one");
    write(&fixture.from.join("vanished"), b"two");
    write(&fixture.from.join("replaced"), b"three");
    let plan = fixture.plan(&["changed", "vanished", "replaced"]);
    write(&fixture.from.join("changed"), b"one and more");
    fs::remove_file(fixture.from.join("vanished")).unwrap();
    // A new file at the same path is a different entry, whatever its content.
    fs::remove_file(fixture.from.join("replaced")).unwrap();
    write(&fixture.from.join("other"), b"x");
    write(&fixture.from.join("replaced"), b"three");

    let report = fixture.apply(&plan);
    assert_eq!(report.status, RunStatus::Refused);
    assert_eq!(
        refusal_codes(&report),
        [
            Some(RefusalCode::SourceChanged),
            Some(RefusalCode::SourceMissing),
            Some(RefusalCode::SourceChanged),
        ]
    );
    assert!(fs::read_dir(&fixture.to).unwrap().next().is_none());
}

#[test]
fn records_a_partially_applied_batch() {
    let fixture = Fixture::new();
    write(&fixture.from.join("kept"), b"1");
    write(&fixture.from.join("moved"), b"2");
    let plan = fixture.plan(&["kept", "moved"]);
    write(&fixture.to.join("kept"), b"occupied");

    let report = fixture.apply(&plan);
    assert_eq!(report.status, RunStatus::Partial);
    assert_eq!(
        refusal_codes(&report),
        [Some(RefusalCode::DestinationExists), None]
    );
    let state = fixture.state();
    let statuses: Vec<MoveStatus> = state.runs[0].moves.iter().map(|m| m.status).collect();
    assert_eq!(statuses, [MoveStatus::Refused, MoveStatus::Applied]);
    assert_eq!(state.runs[0].completed, Some(RunStatus::Partial));
}

#[test]
fn refuses_a_directory_that_became_a_symbolic_link_after_planning() {
    let fixture = Fixture::new();
    write(&fixture.from.join("file"), b"data");
    let plan = fixture.plan(&["file"]);
    let elsewhere = fixture.to.with_file_name("Elsewhere");
    fs::rename(&fixture.to, &elsewhere).unwrap();
    symlink(&elsewhere, &fixture.to).unwrap();

    let report = fixture.apply(&plan);
    assert_eq!(
        refusal_codes(&report),
        [Some(RefusalCode::DirectoryChanged)]
    );
    assert!(fixture.from.join("file").exists());
    assert!(!elsewhere.join("file").exists());
}

#[test]
fn refuses_a_destination_inside_the_source_at_planning() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.from.join("outer/inner")).unwrap();
    let refused = plan_move(
        &fixture.from,
        &[MoveRequest {
            name: OsString::from("outer"),
            expected: None,
        }],
        &fixture.from.join("outer/inner"),
        0,
    )
    .unwrap_err();
    assert_eq!(
        refused.refusals[0].code,
        RefusalCode::DestinationInsideSource
    );
}

#[test]
fn refuses_a_symbolic_link_entry_at_planning() {
    let fixture = Fixture::new();
    symlink("/nonexistent", fixture.from.join("link")).unwrap();
    let refused = plan_move(
        &fixture.from,
        &[MoveRequest {
            name: OsString::from("link"),
            expected: None,
        }],
        &fixture.to,
        0,
    )
    .unwrap_err();
    assert_eq!(refused.refusals[0].code, RefusalCode::UnsupportedKind);
    assert!(fs::symlink_metadata(fixture.from.join("link")).is_ok());
}

/// The test needs a writable directory on another filesystem. Linux runners
/// usually mount `/dev/shm` as tmpfs. Elsewhere the test returns early.
#[test]
fn refuses_a_cross_filesystem_move() {
    let fixture = Fixture::new();
    write(&fixture.from.join("file"), b"data");
    let device = fs::metadata(&fixture.from).unwrap().dev();
    let Some(other) = ["/dev/shm", "/Volumes/RAMDisk"]
        .iter()
        .map(Path::new)
        .find(|path| fs::metadata(path).is_ok_and(|meta| meta.is_dir() && meta.dev() != device))
    else {
        eprintln!("skipped: no writable directory on another filesystem");
        return;
    };
    let Ok(target) = tempfile::TempDir::new_in(other) else {
        eprintln!("skipped: {} is not writable", other.display());
        return;
    };
    let refused = plan_move(
        &fixture.from,
        &[MoveRequest {
            name: OsString::from("file"),
            expected: None,
        }],
        target.path(),
        0,
    )
    .unwrap_err();
    assert_eq!(refused.refusals[0].code, RefusalCode::CrossFilesystem);
    assert!(fixture.from.join("file").exists());
}

#[test]
fn undoes_a_completed_run() {
    let fixture = Fixture::new();
    write(&fixture.from.join("a"), b"a");
    write(&fixture.from.join("dir/b"), b"b");
    let plan = fixture.plan(&["a", "dir"]);
    let run = fixture.apply(&plan).run;

    let report = fixture.undo(&run);
    assert_eq!(report.schema, "suds.undo/v1");
    assert_eq!(report.status, RunStatus::Complete);
    // The undo reverses the moves in the opposite order.
    let order: Vec<usize> = report.moves.iter().map(|outcome| outcome.index).collect();
    assert_eq!(order, [1, 0]);
    assert_eq!(fs::read(fixture.from.join("a")).unwrap(), b"a");
    assert_eq!(fs::read(fixture.from.join("dir/b")).unwrap(), b"b");
    assert!(fs::read_dir(&fixture.to).unwrap().next().is_none());
    assert!(matches!(
        undo(&mut fixture.open(), &run, &mut |_| {}),
        Err(OperationError::NothingToUndo(_))
    ));
    assert_eq!(
        fixture.state().summaries()[0].undo,
        Some(RunStatus::Complete)
    );
}

#[test]
fn refuses_undo_after_the_destination_changed() {
    let fixture = Fixture::new();
    write(&fixture.from.join("draft"), b"v1");
    let plan = fixture.plan(&["draft"]);
    let run = fixture.apply(&plan).run;
    write(&fixture.to.join("draft"), b"v2, edited after the move");

    let report = fixture.undo(&run);
    assert_eq!(report.status, RunStatus::Refused);
    assert_eq!(
        refusal_codes(&report),
        [Some(RefusalCode::DestinationChanged)]
    );
    assert!(!fixture.from.join("draft").exists());
    // The move remains applied, so a later undo may try again.
    assert!(fixture.state().summaries()[0].undoable);
}

#[test]
fn refuses_undo_when_the_original_path_became_occupied() {
    let fixture = Fixture::new();
    write(&fixture.from.join("setup.dmg"), b"first");
    let plan = fixture.plan(&["setup.dmg"]);
    let run = fixture.apply(&plan).run;
    write(&fixture.from.join("setup.dmg"), b"downloaded again");

    let report = fixture.undo(&run);
    assert_eq!(refusal_codes(&report), [Some(RefusalCode::SourceOccupied)]);
    assert_eq!(
        fs::read(fixture.from.join("setup.dmg")).unwrap(),
        b"downloaded again"
    );
    assert_eq!(fs::read(fixture.to.join("setup.dmg")).unwrap(), b"first");
}

#[test]
fn refuses_a_plan_that_suds_did_not_record_or_that_changed() {
    let fixture = Fixture::new();
    write(&fixture.from.join("file"), b"data");
    let unrecorded = plan_move(
        &fixture.from,
        &[MoveRequest {
            name: OsString::from("file"),
            expected: None,
        }],
        &fixture.to,
        0,
    )
    .unwrap();
    assert!(matches!(
        apply(&mut fixture.open(), &unrecorded, &mut |_| {}),
        Err(OperationError::UnknownPlan(_))
    ));

    let recorded = fixture.plan(&["file"]);
    let mut altered = recorded.clone();
    altered.moves[0].destination =
        suds_core::organize::native::NativePath::new(fixture.to.join("renamed"));
    assert!(matches!(
        apply(&mut fixture.open(), &altered, &mut |_| {}),
        Err(OperationError::AlteredPlan(_))
    ));
    assert!(fixture.from.join("file").exists());

    fixture.apply(&recorded);
    assert!(matches!(
        apply(&mut fixture.open(), &recorded, &mut |_| {}),
        Err(OperationError::AlreadyApplied { .. })
    ));
}

/// Writes the records that a run leaves when it stops after `move_started`,
/// and performs the rename when `renamed` holds, as if the process died
/// before it recorded the outcome.
fn interrupt(fixture: &Fixture, plan: &MovePlan, renamed: bool) -> String {
    let mut journal = fixture.open();
    let run = "run-interrupted".to_owned();
    journal
        .append(Event::RunStarted {
            run: run.clone(),
            plan: plan.id.clone(),
        })
        .unwrap();
    journal
        .append(Event::MoveStarted {
            run: run.clone(),
            index: 0,
        })
        .unwrap();
    if renamed {
        fs::rename(
            plan.moves[0].source.as_path(),
            plan.moves[0].destination.as_path(),
        )
        .unwrap();
    }
    run
}

#[test]
fn reconciles_an_interrupted_move_that_happened_and_can_undo_it() {
    let fixture = Fixture::new();
    write(&fixture.from.join("file"), b"data");
    let plan = fixture.plan(&["file"]);
    let run = interrupt(&fixture, &plan, true);

    let found = reconcile(&mut fixture.open()).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].finding, Finding::Applied);
    let summary = &fixture.state().summaries()[0];
    assert_eq!(summary.status, Some(RunStatus::Complete));
    assert!(summary.interrupted);

    assert_eq!(fixture.undo(&run).status, RunStatus::Complete);
    assert!(fixture.from.join("file").exists());
}

#[test]
fn reconciles_an_interrupted_move_that_did_not_happen() {
    let fixture = Fixture::new();
    write(&fixture.from.join("file"), b"data");
    let plan = fixture.plan(&["file"]);
    interrupt(&fixture, &plan, false);

    let found = reconcile(&mut fixture.open()).unwrap();
    assert_eq!(found[0].finding, Finding::NotApplied);
    let state = fixture.state();
    assert_eq!(state.runs[0].moves[0].status, MoveStatus::NotApplied);
    assert_eq!(state.runs[0].completed, Some(RunStatus::Refused));
    // A second invocation finds nothing more to reconcile.
    assert!(reconcile(&mut fixture.open()).unwrap().is_empty());
}

#[test]
fn leaves_an_ambiguous_interrupted_move_unresolved() {
    let fixture = Fixture::new();
    write(&fixture.from.join("file"), b"data");
    let plan = fixture.plan(&["file"]);
    interrupt(&fixture, &plan, false);
    // Both paths now hold an entry, so neither outcome holds.
    write(&fixture.to.join("file"), b"other");

    let found = reconcile(&mut fixture.open()).unwrap();
    assert_eq!(found[0].finding, Finding::Unresolved);
    let detail = found[0].detail.as_deref().unwrap();
    assert!(detail.contains("holds the planned entry"), "{detail}");
    assert!(detail.contains("holds another entry"), "{detail}");
    let state = fixture.state();
    assert_eq!(state.runs[0].moves[0].status, MoveStatus::Unresolved);
    let refusal = state.runs[0].moves[0].refusal.as_ref().unwrap();
    assert_eq!(refusal.code, RefusalCode::Unresolved);
    assert!(fixture.from.join("file").exists());
    assert_eq!(fs::read(fixture.to.join("file")).unwrap(), b"other");
}

#[test]
fn a_rejected_plan_file_cannot_apply_through_an_edited_path() {
    let fixture = Fixture::new();
    write(&fixture.from.join("file"), b"data");
    let plan = fixture.plan(&["file"]);
    let mut json = serde_json::to_value(&plan).unwrap();
    json["moves"][0]["destination"]["display"] = "/somewhere/else".into();
    assert!(serde_json::from_value::<MovePlan>(json).is_err());
}

//! The text of the journal's history, from the view model.

use suds_core::organize::journal::{RunKind, RunStatus};
use suds_core::reclaim::execute::Authority;
use suds_view::{History, Run, Step, bytes};

use crate::present::{count, day, observed, short};
use crate::{RunItem, StepItem};

/// The most characters of a path that a step's row shows.
const STEP_WIDTH: usize = 46;

/// The name of a run's kind, and the words that count its steps.
fn kind(kind: RunKind) -> (&'static str, [&'static str; 3]) {
    match kind {
        RunKind::Move => ("Move", ["entry", "entries", "moved"]),
        RunKind::Reclaim => ("Reclaim", ["directory", "directories", "removed"]),
        RunKind::Stores => ("Store trim", ["store", "stores", "trimmed"]),
        RunKind::Schedule => ("Schedule", ["step", "steps", "done"]),
        RunKind::Seed => ("Seed", ["directory", "directories", "cloned"]),
    }
}

/// A run's outcome in a word or two, with its rank. The rank is 0 for a whole run, 1 for a
/// partial or an open one, 2 for a refused one, and 3 for one without a step.
fn outcome(run: &Run) -> (&'static str, i32) {
    match run.status {
        _ if run.interrupted => ("interrupted", 1),
        // The journal marks a run without a step as refused, and such a run refused nothing.
        Some(RunStatus::Refused) if run.steps.is_empty() => ("nothing to do", 3),
        Some(RunStatus::Complete) => ("complete", 0),
        Some(RunStatus::Partial) => ("partial", 1),
        Some(RunStatus::Refused) => ("refused", 2),
        None => ("unfinished", 1),
    }
}

/// The line beneath a run's name: its steps, its approval, and its undo.
fn caption(run: &Run) -> String {
    let (_, [one, many, done]) = kind(run.kind);
    let mut caption = match run.steps.len() {
        0 => format!("no {one} {done}"),
        steps => format!("{} of {} {done}", run.succeeded(), count(steps, one, many)),
    };
    match run.authority {
        Some(Authority::Person) => caption.push_str(" · approved by a person"),
        Some(Authority::StandingApproval) => caption.push_str(" · under the standing approval"),
        None => {}
    }
    if let Some(undo) = run.undo {
        caption.push_str(&format!(" · undo {}", undo.label()));
    }
    caption
}

/// One row of the history. The row's `id` is the run's position.
pub fn row(position: usize, run: &Run) -> RunItem {
    let (state, rank) = outcome(run);
    let freed = run.freed_bytes();
    RunItem {
        id: i32::try_from(position).unwrap_or(-1),
        name: format!(
            "{} · {} {}",
            kind(run.kind).0,
            day(&run.started_at),
            observed(&run.started_at)
        )
        .into(),
        caption: caption(run).into(),
        // A move frees nothing, so its row states no size.
        size: if freed > 0 {
            bytes(freed).into()
        } else {
            "".into()
        },
        state: state.into(),
        rank,
    }
}

/// The line above the history's rows.
pub fn summary(history: &History) -> String {
    if history.runs.is_empty() {
        return String::new();
    }
    format!(
        "{} · freed about {}",
        count(history.runs.len(), "run", "runs"),
        bytes(history.freed_bytes())
    )
}

/// The end of `path` that fits in `width` characters, from a whole part on. The end of a
/// path tells two steps of one project apart.
fn tail(path: &str, width: usize) -> String {
    let count = path.chars().count();
    if count <= width {
        return path.to_owned();
    }
    let cut = path.char_indices().nth(count - width + 1);
    let rest = cut.map_or(path, |(at, _)| &path[at..]);
    // A cut inside a part leaves the part out, unless the path's last part alone is too long.
    let whole = rest.find('/').map_or(rest, |at| &rest[at..]);
    format!("…{whole}")
}

fn step(kind: RunKind, step: &Step, home: Option<&str>) -> StepItem {
    let note = step.note.as_deref().unwrap_or("no record states why");
    let detail = match (kind, step.succeeded) {
        (RunKind::Move, true) => {
            let destination = step.destination.as_deref().unwrap_or("");
            format!("moved to {}", short(destination, home))
        }
        (RunKind::Move, false) => format!("not moved · {note}"),
        (RunKind::Reclaim, true) => format!("removed · {}", bytes(step.freed_bytes)),
        (RunKind::Reclaim, false) => format!("kept · {note}"),
        (RunKind::Stores, true) => format!("trimmed · freed {}", bytes(step.freed_bytes)),
        (RunKind::Stores, false) => format!("not trimmed · {note}"),
        (RunKind::Schedule, true) => "done".to_owned(),
        (RunKind::Schedule, false) => format!("failed · {note}"),
        (RunKind::Seed, true) => "cloned".to_owned(),
        (RunKind::Seed, false) => format!("not cloned · {note}"),
    };
    let path = short(&step.subject, home);
    StepItem {
        name: tail(&path, STEP_WIDTH).into(),
        path: path.into(),
        detail: detail.into(),
        done: step.succeeded,
    }
}

/// One row for each step of `run`.
pub fn steps(run: &Run, home: Option<&str>) -> Vec<StepItem> {
    let steps = run.steps.iter();
    steps.map(|each| step(run.kind, each, home)).collect()
}

#[cfg(test)]
mod tests {
    use suds_core::organize::journal::{RunKind, RunStatus};
    use suds_core::reclaim::execute::Authority;
    use suds_view::{History, Run, Step};

    use super::{row, steps, summary, tail};

    const GIB: u64 = 1 << 30;

    fn step(subject: &str, freed_bytes: u64, note: Option<&str>) -> Step {
        Step {
            subject: subject.into(),
            destination: None,
            succeeded: note.is_none(),
            freed_bytes,
            note: note.map(str::to_owned),
        }
    }

    fn run(kind: RunKind, status: Option<RunStatus>, steps: Vec<Step>) -> Run {
        Run {
            run: "run-1".into(),
            kind,
            started_at: "2026-11-21T05:41:15Z".into(),
            status,
            interrupted: false,
            undo: None,
            authority: None,
            steps,
        }
    }

    #[test]
    fn a_row_states_a_runs_kind_its_time_and_its_steps() {
        let removed = step("/home/a/w/target", 3 * GIB, None);
        let kept = step("/home/a/v/target", 0, Some("a process works in it"));
        let reclaim = Run {
            authority: Some(Authority::Person),
            ..run(
                RunKind::Reclaim,
                Some(RunStatus::Partial),
                vec![removed, kept],
            )
        };
        let item = row(4, &reclaim);
        assert_eq!(item.id, 4);
        assert_eq!(item.name, "Reclaim · 2026-11-21 05:41");
        let caption = "1 of 2 directories removed · approved by a person";
        assert_eq!(item.caption, caption);
        assert_eq!(item.size, "3.0 GiB");
        assert_eq!((item.state.as_str(), item.rank), ("partial", 1));

        let details = steps(&reclaim, Some("/home/a"));
        assert_eq!(details[0].path, "~/w/target");
        assert_eq!(details[0].name, "~/w/target");
        assert_eq!(details[0].detail, "removed · 3.0 GiB");
        assert!(details[0].done && !details[1].done);
        assert_eq!(details[1].detail, "kept · a process works in it");
    }

    #[test]
    fn a_run_without_a_step_had_nothing_to_do() {
        let idle = run(RunKind::Stores, Some(RunStatus::Refused), Vec::new());
        let item = row(0, &idle);
        assert_eq!((item.state.as_str(), item.rank), ("nothing to do", 3));
        assert_eq!(item.caption, "no store trimmed");
    }

    #[test]
    fn a_long_path_shows_its_end_from_a_whole_part_on() {
        assert_eq!(tail("~/w/target", 46), "~/w/target");
        assert_eq!(tail("~/workspace/suds/target", 12), "…/target");
        assert_eq!(tail("~/workspace/suds/target", 13), "…/suds/target");
        // A last part beyond the width loses its start.
        assert_eq!(tail("~/a-very-long-name", 8), "…ng-name");
        assert_eq!(tail("~/wörk/späce/tärget", 9), "…/tärget");
    }

    #[test]
    fn a_move_frees_nothing_and_names_its_destination() {
        let moved = Step {
            destination: Some("/home/a/Archive/notes.txt".into()),
            ..step("/home/a/Downloads/notes.txt", 0, None)
        };
        let undone = Run {
            undo: Some(RunStatus::Complete),
            ..run(RunKind::Move, Some(RunStatus::Complete), vec![moved])
        };
        let item = row(0, &undone);
        assert_eq!(item.caption, "1 of 1 entry moved · undo complete");
        assert_eq!(item.size, "");
        assert_eq!((item.state.as_str(), item.rank), ("complete", 0));
        let details = steps(&undone, Some("/home/a"));
        assert_eq!(details[0].detail, "moved to ~/Archive/notes.txt");
    }

    #[test]
    fn an_open_run_and_a_failed_trim_state_what_the_journal_holds() {
        let failed = step("npm", 0, Some("the program left with status 1"));
        let trimmed = step("cargo", 6 * GIB, None);
        let stores = Run {
            authority: Some(Authority::StandingApproval),
            ..run(RunKind::Stores, None, vec![trimmed, failed])
        };
        let item = row(0, &stores);
        assert_eq!(item.name, "Store trim · 2026-11-21 05:41");
        let caption = "1 of 2 stores trimmed · under the standing approval";
        assert_eq!(item.caption, caption);
        assert_eq!((item.state.as_str(), item.rank), ("unfinished", 1));
        let details = steps(&stores, None);
        assert_eq!(details[0].detail, "trimmed · freed 6.0 GiB");
        assert_eq!(
            details[1].detail,
            "not trimmed · the program left with status 1"
        );

        let interrupted = Run {
            interrupted: true,
            ..run(RunKind::Move, Some(RunStatus::Partial), Vec::new())
        };
        assert_eq!(row(0, &interrupted).state, "interrupted");
        let kept = step("/w/target", 0, Some("a process works in it"));
        let refused = run(RunKind::Reclaim, Some(RunStatus::Refused), vec![kept]);
        assert_eq!(row(0, &refused).rank, 2);

        assert_eq!(summary(&History::default()), "");
        let history = History {
            runs: vec![stores, refused],
        };
        assert_eq!(summary(&history), "2 runs · freed about 6.0 GiB");
    }
}

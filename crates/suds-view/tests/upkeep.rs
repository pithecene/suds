//! The standing of the scheduled maintenance, read from a temporary home.

use std::collections::HashMap;
use std::fs;

use suds_core::development::Environment;
use suds_core::organize::journal::{Event, Journal, RunStatus};
use suds_core::reclaim::execute::Authority;
use suds_core::schedule::{self, Scheduler, Standing, Unit};
use suds_core::state::Directories;
use suds_view::{FailedTrim, OVERDUE_SECONDS, upkeep};
use tempfile::TempDir;

fn home() -> (TempDir, Environment) {
    let temp = TempDir::new().unwrap();
    let environment = Environment {
        home: Some(temp.path().canonicalize().unwrap()),
        variables: HashMap::new(),
    };
    (temp, environment)
}

/// Places the units of this system's scheduler, which run a program of the home directory.
fn schedule(environment: &Environment) {
    let program = environment.home.as_ref().unwrap().join("suds");
    fs::write(&program, "").unwrap();
    let unit = Unit {
        program,
        path: Some("/usr/bin".into()),
        minutes: 30,
    };
    let files = schedule::files(Scheduler::native(), &unit, environment).unwrap();
    schedule::place(&files).unwrap();
}

/// Writes a pressure reading now and returns its time in seconds.
fn read_pressure(environment: &Environment) -> i64 {
    let reading = Directories::of(environment).unwrap().pressure();
    fs::create_dir_all(reading.parent().unwrap()).unwrap();
    fs::write(&reading, "{}").unwrap();
    let written = reading.metadata().unwrap().modified().unwrap();
    let since = written.duration_since(std::time::UNIX_EPOCH).unwrap();
    i64::try_from(since.as_secs()).unwrap()
}

/// Records a stores run of `authority` in which pnpm fails when `fails` holds.
fn trim(environment: &Environment, run: &str, authority: Authority, fails: bool) {
    let directory = Directories::of(environment).unwrap().journal();
    let mut journal = Journal::open(&directory).unwrap();
    let (run, store) = (|| run.to_owned(), || "pnpm.store".to_owned());
    let outcome = if fails {
        Event::StoreTrimFailed {
            run: run(),
            store: store(),
            reason: "could not run pnpm".into(),
        }
    } else {
        Event::StoreTrimmed {
            run: run(),
            store: store(),
            before_bytes: 2,
            after_bytes: 1,
        }
    };
    let events = [
        Event::StoresStarted {
            run: run(),
            plan: "plan".into(),
            authority,
        },
        Event::StoreTrimStarted {
            run: run(),
            store: store(),
            command: "pnpm store prune".into(),
        },
        outcome,
        Event::StoresCompleted {
            run: run(),
            status: RunStatus::Complete,
            freed_bytes: 0,
        },
    ];
    events
        .into_iter()
        .for_each(|event| journal.append(event).unwrap());
}

#[test]
fn a_workstation_without_a_schedule_says_so_and_is_never_overdue() {
    let (_temp, environment) = home();
    let upkeep = upkeep(&environment, 1_800_000_000);
    assert_eq!(upkeep.schedule, Standing::Absent);
    assert_eq!(upkeep.last_run, None);
    assert!(!upkeep.overdue && !upkeep.sound());
    assert_eq!(upkeep.failed_trims, []);
}

#[test]
fn a_schedule_is_overdue_a_day_after_its_last_run() {
    let (_temp, environment) = home();
    schedule(&environment);
    // The units stand in place, and no run wrote a reading.
    assert!(upkeep(&environment, 0).overdue);

    let written = read_pressure(&environment);
    let fresh = upkeep(&environment, written + OVERDUE_SECONDS);
    assert_eq!(fresh.schedule, Standing::Current);
    assert!(fresh.sound(), "{fresh:?}");
    assert!(
        fresh
            .last_run
            .as_deref()
            .is_some_and(|at| at.ends_with('Z'))
    );
    let late = upkeep(&environment, written + OVERDUE_SECONDS + 1);
    assert!(late.overdue && !late.sound());
}

#[test]
fn the_last_scheduled_stores_run_states_each_trim_that_failed() {
    let (_temp, environment) = home();
    schedule(&environment);
    let now = read_pressure(&environment);
    trim(&environment, "run-1", Authority::StandingApproval, true);
    // A person's run after it tells nothing of the schedule.
    trim(&environment, "run-2", Authority::Person, false);

    let failed = upkeep(&environment, now);
    let expected = FailedTrim {
        store: "pnpm.store".into(),
        reason: "could not run pnpm".into(),
    };
    assert_eq!(failed.failed_trims, [expected]);
    assert!(!failed.sound());

    trim(&environment, "run-3", Authority::StandingApproval, false);
    assert!(upkeep(&environment, now).sound());
}

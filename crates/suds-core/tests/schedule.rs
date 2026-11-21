//! The units of each scheduler, their placing, and their standing in a temporary home.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use suds_core::development::Environment;
use suds_core::organize::journal::{Journal, JournalState, RunKind, RunStatus};
use suds_core::schedule::{
    Change, Scheduler, Standing, Step, Unit, UnitFile, activation, files, install, place,
    plan_install, standing,
};
use tempfile::TempDir;

const BOTH: [Scheduler; 2] = [Scheduler::Systemd, Scheduler::Launchd];

/// A home directory that holds a file in the place of the Suds program.
fn home() -> (TempDir, Environment, PathBuf) {
    let temp = TempDir::new().unwrap();
    let home = temp.path().canonicalize().unwrap();
    let program = home.join("bin & co/suds");
    fs::create_dir_all(program.parent().unwrap()).unwrap();
    fs::write(&program, "").unwrap();
    let environment = Environment {
        home: Some(home),
        variables: HashMap::new(),
    };
    (temp, environment, program)
}

fn unit(program: &Path) -> Unit {
    Unit {
        program: program.to_owned(),
        path: Some("/home/a/.local/share/mise/shims:/opt/50% $x & co/bin:/usr/bin".into()),
        minutes: 30,
    }
}

fn written(scheduler: Scheduler, unit: &Unit, environment: &Environment) -> Vec<UnitFile> {
    files(scheduler, unit, environment).unwrap()
}

fn stale(standing: Standing) -> String {
    match standing {
        Standing::Stale(reason) => reason,
        other => panic!("the units are stale, and the standing is {other:?}"),
    }
}

#[test]
fn the_systemd_units_run_the_maintenance_with_the_search_path() {
    let (_temp, environment, _program) = home();
    let odd = unit(Path::new("/opt/100% \"q\"/$x/suds"));
    let units = written(Scheduler::Systemd, &odd, &environment);
    let home = environment.home.as_ref().unwrap();
    let directory = home.join(".config/systemd/user");
    assert_eq!(units[0].path, directory.join("suds-maintain.service"));
    assert_eq!(units[1].path, directory.join("suds-maintain.timer"));

    let service = &units[0].contents;
    assert!(service.contains("ExecStart=\"/opt/100%% \\\"q\\\"/$$x/suds\" maintain\n"));
    assert!(service.contains("SuccessExitStatus=1\n"));
    // A variable's value doubles `%`, and the manager leaves `$` alone there.
    let line =
        "Environment=\"PATH=/home/a/.local/share/mise/shims:/opt/50%% $x & co/bin:/usr/bin\"\n";
    assert!(service.contains(line), "{service}");
    assert!(service.find(line).unwrap() < service.find("ExecStart=").unwrap());
    assert!(units[1].contents.contains("OnUnitActiveSec=30min\n"));

    let bare = Unit { path: None, ..odd };
    let units = written(Scheduler::Systemd, &bare, &environment);
    assert!(!units[0].contents.contains("Environment="));
}

#[test]
fn the_units_follow_the_configuration_directory_and_need_a_home() {
    let (_temp, mut environment, program) = home();
    let config = environment.home.as_ref().unwrap().join("xdg");
    let named = ("XDG_CONFIG_HOME".to_owned(), config.clone().into());
    environment.variables.extend([named]);
    let units = written(Scheduler::Systemd, &unit(&program), &environment);
    assert!(units[0].path.starts_with(config.join("systemd/user")));

    let homeless = Environment::default();
    assert_eq!(files(Scheduler::Systemd, &unit(&program), &homeless), None);
    assert_eq!(
        standing(Scheduler::Systemd, &homeless, None),
        Standing::Absent
    );
}

#[test]
fn the_launchd_agent_runs_the_maintenance_with_the_search_path() {
    let (_temp, environment, program) = home();
    let agent = written(Scheduler::Launchd, &unit(&program), &environment);
    let home = environment.home.as_ref().unwrap();
    let place = home.join("Library/LaunchAgents/sh.pithecene.suds.maintain.plist");
    assert_eq!(agent[0].path, place);
    let text = &agent[0].contents;
    assert!(text.contains("bin &amp; co/suds</string>\n\t\t<string>maintain</string>"));
    assert!(text.contains("<integer>1800</integer>"));
    let value = "\t\t<key>PATH</key>\n\t\t<string>/home/a/.local/share/mise/shims:/opt/50% $x &amp; co/bin:/usr/bin</string>\n";
    assert!(text.contains(value), "{text}");

    let steps = activation(Scheduler::Launchd, &agent);
    assert!(!steps[0].required && steps[1].required);
    assert_eq!(steps[1].command[..2], ["launchctl", "bootstrap"]);
    assert_eq!(steps[1].command[3], place.to_string_lossy());
    let steps = activation(Scheduler::Systemd, &[]);
    let enable = [
        "systemctl",
        "--user",
        "enable",
        "--now",
        "suds-maintain.timer",
    ];
    assert_eq!(steps[1].command, enable);
}

#[test]
fn placed_units_are_current_whatever_minutes_and_path_a_person_chose() {
    for scheduler in BOTH {
        let (_temp, environment, program) = home();
        assert_eq!(standing(scheduler, &environment, None), Standing::Absent);

        let chosen = Unit {
            minutes: 15,
            ..unit(&program)
        };
        let units = written(scheduler, &chosen, &environment);
        place(&units).unwrap();
        assert_eq!(standing(scheduler, &environment, None), Standing::Current);
        let named = standing(scheduler, &environment, Some(&program));
        assert_eq!(named, Standing::Current);

        // A second placing replaces the files and leaves no staged file.
        place(&units).unwrap();
        let directory = units[0].path.parent().unwrap();
        assert_eq!(fs::read_dir(directory).unwrap().count(), units.len());
    }
}

#[test]
fn stale_units_state_their_reason() {
    for scheduler in BOTH {
        let (_temp, environment, program) = home();
        let now = unit(&program);
        let check = || standing(scheduler, &environment, None);

        // An earlier release stated no search path.
        let earlier = Unit {
            path: None,
            ..now.clone()
        };
        place(&written(scheduler, &earlier, &environment)).unwrap();
        assert!(stale(check()).contains("no search path"));

        let units = written(scheduler, &now, &environment);
        place(&units).unwrap();
        let other = standing(scheduler, &environment, Some(Path::new("/opt/suds")));
        assert!(stale(other).contains("another Suds program"));

        let first = &units[0];
        fs::write(&first.path, format!("{}# by hand\n", first.contents)).unwrap();
        assert!(stale(check()).contains("an earlier release"));
        fs::write(&first.path, "nothing of a unit").unwrap();
        assert!(stale(check()).contains("cannot read the unit"));

        place(&units).unwrap();
        fs::remove_file(&program).unwrap();
        assert!(stale(check()).contains("no longer exists"));
    }
}

#[test]
fn a_unit_with_one_file_of_two_is_stale() {
    let (_temp, environment, program) = home();
    let units = written(Scheduler::Systemd, &unit(&program), &environment);
    place(&units).unwrap();
    fs::remove_file(&units[1].path).unwrap();
    let reason = stale(standing(Scheduler::Systemd, &environment, None));
    assert!(reason.contains("is missing"), "{reason}");
}

fn journal(environment: &Environment) -> Journal {
    let directory = environment.home.as_ref().unwrap().join("journal");
    Journal::open(&directory).unwrap()
}

#[test]
fn an_installation_plans_each_file_and_changes_nothing_before_its_run() {
    let (_temp, environment, program) = home();
    let scheduler = Scheduler::Systemd;
    let plan = plan_install(scheduler, &unit(&program), &environment).unwrap();
    let changes: Vec<Change> = plan.files.iter().map(|file| file.change).collect();
    assert_eq!(changes, [Change::Create, Change::Create]);
    assert_eq!(plan.steps.len(), 2);
    assert_eq!(standing(scheduler, &environment, None), Standing::Absent);

    let mut ran = Vec::new();
    let mut journal = journal(&environment);
    let mut stub = |step: &Step| {
        ran.push(step.command.join(" "));
        Ok(())
    };
    let report = install(&mut journal, &plan, &mut stub).unwrap();
    assert_eq!(report.status, RunStatus::Complete);
    assert_eq!(report.steps.len(), 4);
    assert_eq!(ran.len(), 2);
    assert_eq!(standing(scheduler, &environment, None), Standing::Current);

    // The journal records the run with each file and each command.
    let summaries = JournalState::from_records(journal.records()).summaries();
    assert_eq!(summaries[0].kind, RunKind::Schedule);
    assert_eq!(summaries[0].status, Some(RunStatus::Complete));
    let steps: Vec<&str> = summaries[0]
        .units
        .iter()
        .map(|unit| unit.step.as_str())
        .collect();
    assert!(steps[0].ends_with("suds-maintain.service"));
    assert_eq!(steps[2], "systemctl --user daemon-reload");

    // A second plan keeps each file, and its run still runs each command.
    let again = plan_install(scheduler, &unit(&program), &environment).unwrap();
    assert!(again.files.iter().all(|file| file.change == Change::Keep));
    let report = install(&mut journal, &again, &mut |_| Ok(())).unwrap();
    assert_eq!(
        (report.status, report.steps.len()),
        (RunStatus::Complete, 2)
    );
    let other = Unit {
        minutes: 15,
        ..unit(&program)
    };
    let replaced = plan_install(scheduler, &other, &environment).unwrap();
    assert_eq!(replaced.files[0].change, Change::Keep);
    assert_eq!(replaced.files[1].change, Change::Replace);
}

#[test]
fn an_installation_stops_at_a_changed_file_and_at_a_failed_command() {
    let (_temp, environment, program) = home();
    let scheduler = Scheduler::Systemd;
    let plan = plan_install(scheduler, &unit(&program), &environment).unwrap();
    let mut journal = journal(&environment);

    // The scheduler's own program fails, and the run states its reason.
    let mut failing = |_: &Step| Err("exit status: 1".to_owned());
    let report = install(&mut journal, &plan, &mut failing).unwrap();
    assert_eq!(report.status, RunStatus::Partial);
    let last = report.steps.last().unwrap();
    assert_eq!(last.step, "systemctl --user daemon-reload");
    assert_eq!(last.reason.as_deref(), Some("exit status: 1"));
    assert_eq!(report.steps.len(), 3);

    // A person edited a file after the plan, so the run leaves it and runs no command.
    let stale = plan_install(scheduler, &unit(&program), &environment).unwrap();
    let other = Unit {
        minutes: 15,
        ..unit(&program)
    };
    let plan = plan_install(scheduler, &other, &environment).unwrap();
    let timer = &stale.files[1].file.path;
    fs::write(timer, "by hand").unwrap();
    let mut ran = 0;
    let mut count = |_: &Step| {
        ran += 1;
        Ok(())
    };
    let report = install(&mut journal, &plan, &mut count).unwrap();
    assert_eq!(report.status, RunStatus::Refused);
    let reason = report.steps[0].reason.as_deref();
    assert_eq!(reason, Some("the file changed after planning"));
    assert_eq!(ran, 0);
    assert_eq!(fs::read_to_string(timer).unwrap(), "by hand");
}

#[cfg(unix)]
#[test]
fn a_link_and_an_unreadable_file_fail_the_reading_and_follow_nowhere() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let (_temp, environment, program) = home();
    let scheduler = Scheduler::Systemd;
    let units = written(scheduler, &unit(&program), &environment);
    place(&units).unwrap();
    let service = &units[0].path;

    // A text of another encoding is no unit of this release.
    fs::write(service, [0xff, 0xfe]).unwrap();
    let reason = stale(standing(scheduler, &environment, None));
    assert!(reason.contains("cannot read the unit"), "{reason}");

    let target = environment.home.as_ref().unwrap().join("elsewhere");
    fs::write(&target, "kept").unwrap();
    fs::remove_file(service).unwrap();
    symlink(&target, service).unwrap();
    let Standing::Unreadable(reason) = standing(scheduler, &environment, None) else {
        panic!("a link is no unit's file");
    };
    assert!(reason.contains("is a symbolic link"), "{reason}");
    let refused = plan_install(scheduler, &unit(&program), &environment);
    assert!(refused.unwrap_err().contains("is a symbolic link"));
    assert_eq!(fs::read_to_string(&target).unwrap(), "kept");

    fs::remove_file(service).unwrap();
    fs::write(service, "closed").unwrap();
    fs::set_permissions(service, fs::Permissions::from_mode(0o000)).unwrap();
    let denied = fs::read(service).is_err();
    let standing = standing(scheduler, &environment, None);
    fs::set_permissions(service, fs::Permissions::from_mode(0o644)).unwrap();
    // A superuser reads the closed file, which then holds no unit.
    assert_eq!(matches!(standing, Standing::Unreadable(_)), denied);
    assert_ne!(standing, Standing::Absent);
}

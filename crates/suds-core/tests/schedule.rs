//! The units of each scheduler, their placing, and their standing in a temporary home.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use suds_core::development::Environment;
use suds_core::schedule::{
    Scheduler, Standing, Unit, UnitFile, activation, files, place, standing,
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

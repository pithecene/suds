//! The saved reading, in a temporary home.

use std::collections::HashMap;
use std::fs;

use suds_core::development::Environment;
use suds_core::pressure::Level;
use suds_core::state::Directories;
use suds_view::{
    Checkout, Class, PressureCard, Store, StoreStanding, Workstation, last_reading, save_reading,
};
use tempfile::TempDir;

fn home() -> (TempDir, Environment) {
    let temp = TempDir::new().unwrap();
    let environment = Environment {
        home: Some(temp.path().canonicalize().unwrap()),
        variables: HashMap::new(),
    };
    (temp, environment)
}

fn view() -> Workstation {
    Workstation {
        observed_at: "2026-11-21T16:01:00Z".into(),
        pressure: Some(PressureCard {
            level: Level::Soft,
            usage_bytes: 9 << 30,
            ceiling_bytes: 10 << 30,
            share: 0.9,
            filesystem_available_bytes: Some(1 << 30),
            enforced: false,
            complete: true,
        }),
        classes: vec![Class {
            label: "Repositories".into(),
            bytes: 1 << 30,
            count: 1,
            share: 1.0,
        }],
        checkouts: vec![Checkout {
            path: "/home/a/w/suds".into(),
            name: "suds".into(),
            parent: "/home/a/w".into(),
            linked: false,
            repository: None,
            origin: Some("github.com/pithecene/suds".into()),
            own_bytes: 1 << 30,
            build_bytes: 0,
            build_state: Vec::new(),
            newest_modified: "2026-11-21T15:00:00Z".into(),
            hold: None,
            share: 1.0,
        }],
        stores: vec![Store {
            id: "pnpm.store".into(),
            ecosystem: "pnpm".into(),
            class: "Shared stores".into(),
            path: "/home/a/.local/share/pnpm/store".into(),
            bytes: 12 << 30,
            budget_bytes: Some(10 << 30),
            standing: StoreStanding::Over,
            share: 1.0,
        }],
        error_count: 0,
        registry_error: None,
    }
}

#[test]
fn a_saved_reading_comes_back_whole() {
    let (_temp, environment) = home();
    assert_eq!(last_reading(&environment), Ok(None));
    save_reading(&environment, &view()).unwrap();
    assert_eq!(last_reading(&environment), Ok(Some(view())));

    // A second reading replaces the first and leaves no staged file.
    let later = Workstation {
        observed_at: "2026-11-21T17:00:00Z".into(),
        ..view()
    };
    save_reading(&environment, &later).unwrap();
    assert_eq!(last_reading(&environment), Ok(Some(later)));
    let state = Directories::of(&environment).unwrap().state;
    let names: Vec<_> = fs::read_dir(state).unwrap().flatten().collect();
    assert_eq!(names.len(), 1);
}

#[test]
fn a_reading_of_another_schema_or_another_form_counts_as_none() {
    let (_temp, environment) = home();
    save_reading(&environment, &view()).unwrap();
    let path = Directories::of(&environment).unwrap().reading();
    let text = fs::read_to_string(&path).unwrap();
    fs::write(&path, text.replace("suds.reading/v1", "suds.reading/v0")).unwrap();
    assert_eq!(last_reading(&environment), Ok(None));
    fs::write(&path, "{").unwrap();
    assert_eq!(last_reading(&environment), Ok(None));
}

#[test]
fn a_home_that_takes_no_file_states_the_failure() {
    let (_temp, environment) = home();
    // A file stands where the state directory belongs, so no reading has a place.
    let state = Directories::of(&environment).unwrap().state;
    fs::create_dir_all(state.parent().unwrap()).unwrap();
    fs::write(&state, "").unwrap();
    let failure = save_reading(&environment, &view()).unwrap_err();
    assert!(failure.starts_with("could not write "), "{failure}");
}

#[test]
fn a_reading_that_suds_cannot_read_is_a_failure_and_not_an_absence() {
    let (_temp, environment) = home();
    // A directory stands at the reading's path, so the read fails for a reason other than absence.
    let path = Directories::of(&environment).unwrap().reading();
    fs::create_dir_all(&path).unwrap();
    let failure = last_reading(&environment).unwrap_err();
    assert!(failure.starts_with("could not read "), "{failure}");
    assert!(failure.contains("reading.json"), "{failure}");
}

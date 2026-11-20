//! The first view, read from a workstation in a temporary directory and from a written report.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use suds_core::development::{DevelopmentReport, Environment};
use suds_core::registry::{Event, Standing, Standings};
use suds_core::state::Directories;
use suds_core::stores::Budgets;
use suds_view::{Observation, Sort, StoreStanding, arrange, observe, workstation};
use tempfile::TempDir;

const GIB: u64 = 1 << 30;

/// A home directory whose settings name one root, and the root.
fn home() -> (TempDir, Environment, PathBuf) {
    let temp = TempDir::new().unwrap();
    let home = temp.path().canonicalize().unwrap();
    let environment = Environment {
        home: Some(home.clone()),
        variables: HashMap::new(),
    };
    let root = home.join("workspace");
    fs::create_dir_all(&root).unwrap();
    (temp, environment, root)
}

fn configure(environment: &Environment, root: &Path) {
    let settings = Directories::of(environment).unwrap().settings();
    fs::create_dir_all(settings.parent().unwrap()).unwrap();
    let text = serde_json::json!({ "roots": [root] }).to_string();
    fs::write(settings, text).unwrap();
}

#[test]
fn a_workstation_without_a_root_is_unconfigured() {
    let (_temp, environment, _root) = home();
    let Observation::Unconfigured(settings) = observe(&environment, 0).unwrap() else {
        panic!("the settings name no root");
    };
    assert!(settings.ends_with("settings.json"), "{settings}");
}

#[test]
fn a_reading_shows_each_checkout_with_its_build_state() {
    let (_temp, environment, root) = home();
    configure(&environment, &root);
    let app = root.join("app");
    fs::create_dir_all(app.join(".git")).unwrap();
    fs::write(app.join("Cargo.toml"), "[package]\n").unwrap();
    fs::create_dir_all(app.join("target")).unwrap();
    fs::write(
        app.join("target/CACHEDIR.TAG"),
        "Signature: 8a477f597d28d172789f06886806bc55\n",
    )
    .unwrap();
    fs::write(app.join("target/object"), vec![0u8; 64 * 1024]).unwrap();

    let Observation::Measured(view) = observe(&environment, 1_800_000_000).unwrap() else {
        panic!("the settings name a root");
    };
    assert_eq!(view.checkouts.len(), 1);
    let checkout = &view.checkouts[0];
    assert_eq!(checkout.name, "app");
    assert_eq!(checkout.parent, root.to_string_lossy());
    assert!(!checkout.linked);
    assert_eq!(checkout.build_state.len(), 1);
    assert_eq!(checkout.build_state[0].ecosystem, "cargo");
    assert!(checkout.build_bytes >= 64 * 1024);
    assert_eq!(checkout.share, 1.0);
    assert_eq!(checkout.hold, None);
    let pressure = view.pressure.as_ref().unwrap();
    assert!(pressure.usage_bytes >= checkout.total_bytes());
    assert!(pressure.ceiling_bytes > 0);
    // The reading wrote nothing beside the settings.
    assert!(!Directories::of(&environment).unwrap().pressure().exists());
}

fn measure(bytes: u64, modified: &str) -> serde_json::Value {
    serde_json::json!({
        "logical_bytes": bytes, "allocated_bytes": bytes, "files": 1, "directories": 1,
        "symlinks": 0, "others": 0, "oldest_modified": modified, "newest_modified": modified,
    })
}

/// A report of three checkouts and two stores, as `suds inspect --profile development` writes one.
fn report() -> DevelopmentReport {
    let checkout = |path: &str, kind: &str, bytes: u64, modified: &str| {
        serde_json::json!({
            "path": path, "kind": kind, "repository": null,
            "own": measure(bytes, modified), "build_state": [],
        })
    };
    let store = |id: &str, bytes: u64, status: &str| {
        let mut store = measure(bytes, "2026-11-01T00:00:00Z");
        let fields = serde_json::json!({
            "id": id, "class": "shared_store", "ecosystem": "go", "path": format!("/stores/{id}"),
            "status": status, "owner": null,
        });
        store
            .as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        store
    };
    let total = |count: u64, bytes: u64| serde_json::json!({ "count": count, "allocated_bytes": bytes, "logical_bytes": bytes });
    serde_json::from_value(serde_json::json!({
        "schema": "suds.development/v1",
        "platform": { "os": "linux", "arch": "x86_64" },
        "observed_at": "2026-11-20T20:16:42Z",
        "status": "complete",
        "totals": {
            "main_checkouts": total(1, 3 * GIB), "linked_checkouts": total(2, GIB),
            "build_state": total(0, 0), "shared_stores": total(2, 12 * GIB),
            "toolchains": total(0, 0), "machines": total(0, 0),
        },
        "workspaces": [],
        "checkouts": [
            checkout("/work/Zeta", "main", 3 * GIB, "2026-11-01T00:00:00Z"),
            checkout("/work/zeta/.claude/worktrees/alpha", "linked", GIB / 2, "2026-11-20T00:00:00Z"),
            checkout("/work/other/beta", "linked", GIB / 2, "2026-11-10T00:00:00Z"),
        ],
        "stores": [store("go.module-cache", 11 * GIB, "present"), store("go.build-cache", GIB, "present")],
        "skipped_mount_count": 0,
        "error_count": 0,
        "errors": [],
    }))
    .unwrap()
}

#[test]
fn the_view_orders_by_size_and_marks_each_hold_and_budget() {
    let mut standings = Standings::new();
    standings.insert(
        PathBuf::from("/work/other/beta"),
        Standing {
            event: Event::Released,
            client: "bosun".into(),
            attempt: None,
            at: "2026-11-19T00:00:00Z".into(),
            landed: Some("abc123".into()),
        },
    );
    let view = workstation(&report(), None, &standings, &Budgets::default());

    assert_eq!(view.pressure, None);
    let names: Vec<&str> = view.checkouts.iter().map(|c| c.name.as_str()).collect();
    // Two checkouts of one size keep the order of their paths.
    assert_eq!(names, ["Zeta", "beta", "alpha"]);
    assert_eq!(view.checkouts[0].share, 1.0);
    let hold = view.checkouts[1].hold.as_ref().unwrap();
    assert!(hold.released);
    assert_eq!(hold.client, "bosun");
    assert_eq!(hold.landed.as_deref(), Some("abc123"));
    assert_eq!(view.checkouts[2].hold, None);

    // The module cache lies over its budget of 10 GiB, and the build cache within its own.
    assert_eq!(view.stores[0].id, "go.module-cache");
    assert_eq!(view.stores[0].standing, StoreStanding::Over);
    assert_eq!(view.stores[0].share, 1.0);
    assert_eq!(view.stores[1].standing, StoreStanding::Within);
    assert!((view.stores[1].share - 0.1).abs() < 0.001);

    let shares: f32 = view.classes.iter().map(|class| class.share).sum();
    assert!((shares - 1.0).abs() < 0.001, "{shares}");
}

#[test]
fn an_arrangement_filters_by_path_and_sorts() {
    let view = workstation(&report(), None, &Standings::new(), &Budgets::default());
    let names = |query: &str, sort: Sort| -> Vec<&str> {
        arrange(&view.checkouts, query, sort)
            .into_iter()
            .map(|position| view.checkouts[position].name.as_str())
            .collect()
    };
    assert_eq!(names("", Sort::Size), ["Zeta", "beta", "alpha"]);
    assert_eq!(names("", Sort::Name), ["alpha", "beta", "Zeta"]);
    assert_eq!(names("", Sort::Modified), ["alpha", "beta", "Zeta"]);
    // The query matches a path's parent as well as its name, in any case.
    assert_eq!(names(" ZETA ", Sort::Name), ["alpha", "Zeta"]);
    assert_eq!(names("nothing", Sort::Size), Vec::<&str>::new());
}

//! Tests of the development profile against a temporary fixture tree.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use suds_core::development::{
    CheckoutKind, DEVELOPMENT_SCHEMA, DevelopmentReport, DevelopmentScan, Ecosystem, Environment,
    StoreClass, StoreStatus, build_report, inspect,
};
use suds_core::model::ReportStatus;
use tempfile::TempDir;

fn write(path: &Path, bytes: usize) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, vec![b'x'; bytes]).unwrap();
}

/// A workspace that holds one repository with a linked checkout, a
/// submodule, and directories whose names alone suggest build state.
struct Fixture {
    _dir: TempDir,
    workspace: PathBuf,
    home: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let workspace = root.join("workspace");
        let repo = workspace.join("repo");
        let linked = repo.join(".claude/worktrees/feature");

        // The main checkout, with Cargo's and a package manager's output.
        write(&repo.join(".git/HEAD"), 10);
        write(&repo.join(".git/worktrees/feature/HEAD"), 10);
        write(&repo.join("Cargo.toml"), 20);
        write(&repo.join("src/main.rs"), 100);
        write(&repo.join("target/CACHEDIR.TAG"), 43);
        write(&repo.join("target/debug/app"), 5000);
        write(&repo.join("web/package.json"), 30);
        write(&repo.join("web/node_modules/pkg/index.js"), 700);
        // A package manager's directory at a root without a manifest, with
        // a nested directory that belongs to it.
        write(&repo.join("tools/node_modules/.modules.yaml"), 8);
        write(&repo.join("tools/node_modules/dep/package.json"), 12);
        write(
            &repo.join("tools/node_modules/dep/node_modules/inner/index.js"),
            50,
        );
        // A directory named `target` that Cargo did not write.
        write(&repo.join("docs/target/notes.md"), 60);
        // A submodule, whose `.git` file names a `modules` directory.
        write(&repo.join("vendor/lib/code.c"), 40);
        fs::write(
            repo.join("vendor/lib/.git"),
            "gitdir: ../../.git/modules/lib\n",
        )
        .unwrap();

        // The linked checkout, with its own output and a CMake build.
        fs::create_dir_all(&linked).unwrap();
        fs::write(
            linked.join(".git"),
            format!(
                "gitdir: {}\n",
                repo.join(".git/worktrees/feature").display()
            ),
        )
        .unwrap();
        write(&linked.join("Cargo.toml"), 20);
        write(&linked.join("src/main.rs"), 120);
        write(&linked.join("target/CACHEDIR.TAG"), 43);
        write(&linked.join("target/debug/app"), 9000);
        write(&linked.join("out/CMakeCache.txt"), 15);
        write(&linked.join("out/lib.a"), 300);

        // Build output that no checkout owns.
        write(&workspace.join("scratch/Cargo.toml"), 20);
        write(&workspace.join("scratch/target/CACHEDIR.TAG"), 43);
        write(&workspace.join("scratch/target/debug/app"), 800);

        let home = root.join("home");
        write(&home.join(".cache/go-build/README"), 50);
        write(&home.join(".cache/go-build/00/entry"), 400);
        // A module cache without the entry that Go writes there.
        write(&home.join("go/pkg/mod/stray"), 25);
        write(&home.join(".local/share/mise/installs/go/1.0/bin/go"), 2000);
        Fixture {
            _dir: dir,
            workspace,
            home,
        }
    }

    fn environment(&self) -> Environment {
        // The explicit cache directory keeps the fixture the same on macOS,
        // whose default cache directory differs from Linux's.
        let cache = self.home.join(".cache").into_os_string();
        Environment {
            home: Some(self.home.clone()),
            variables: HashMap::from([("XDG_CACHE_HOME".to_owned(), cache)]),
        }
    }

    fn scan(&self) -> DevelopmentScan {
        inspect(std::slice::from_ref(&self.workspace), &self.environment())
    }
}

#[test]
fn classifies_checkouts_and_their_build_state_by_layout() {
    let fixture = Fixture::new();
    let scan = fixture.scan();
    let repo = fixture.workspace.join("repo");
    let linked = repo.join(".claude/worktrees/feature");

    assert_eq!(scan.checkouts.len(), 2);
    let main = &scan.checkouts[0];
    assert_eq!(main.path, repo);
    assert_eq!(main.kind, CheckoutKind::Main);
    assert_eq!(main.repository, None);
    let built: Vec<(PathBuf, Ecosystem, u64)> = main
        .build_state
        .iter()
        .map(|state| {
            (
                state.path.clone(),
                state.ecosystem,
                state.measure.logical_bytes,
            )
        })
        .collect();
    assert_eq!(
        built,
        [
            (repo.join("target"), Ecosystem::Cargo, 5043),
            (repo.join("tools/node_modules"), Ecosystem::Node, 70),
            (repo.join("web/node_modules"), Ecosystem::Node, 700),
        ]
    );
    // The checkout's own bytes hold its repository, its sources, the
    // submodule, and the directory that only shares a name with Cargo's.
    assert_eq!(
        main.own.logical_bytes,
        10 + 10 + 20 + 100 + 30 + 60 + 40 + 31
    );

    let feature = &scan.checkouts[1];
    assert_eq!(feature.path, linked);
    assert_eq!(feature.kind, CheckoutKind::Linked);
    assert_eq!(feature.repository.as_deref(), Some(repo.as_path()));
    let ecosystems: Vec<Ecosystem> = feature
        .build_state
        .iter()
        .map(|state| state.ecosystem)
        .collect();
    assert_eq!(ecosystems, [Ecosystem::Cmake, Ecosystem::Cargo]);
    assert_eq!(scan.error_count, 0);
}

#[test]
fn leaves_build_output_without_a_checkout_unclassified() {
    let fixture = Fixture::new();
    let scan = fixture.scan();
    assert!(scan.workspaces[0].measured);
    assert_eq!(scan.workspaces[0].unclassified.logical_bytes, 20 + 43 + 800);
    assert!(
        scan.checkouts
            .iter()
            .all(|checkout| !checkout.path.starts_with(fixture.workspace.join("scratch")))
    );
}

#[test]
fn reports_each_store_with_the_proof_of_its_owner() {
    let fixture = Fixture::new();
    let scan = fixture.scan();
    let status = |id: &str| {
        let store = scan.stores.iter().find(|store| store.id == id).unwrap();
        (store.status, store.class, store.measure.logical_bytes)
    };
    assert_eq!(
        status("go.build-cache"),
        (StoreStatus::Present, StoreClass::SharedStore, 450)
    );
    assert_eq!(
        status("go.module-cache"),
        (StoreStatus::Unrecognized, StoreClass::SharedStore, 25)
    );
    assert_eq!(
        status("mise.installs"),
        (StoreStatus::Present, StoreClass::Toolchain, 2000)
    );
    assert_eq!(
        status("pnpm.store"),
        (StoreStatus::Absent, StoreClass::SharedStore, 0)
    );
}

#[test]
fn a_variable_moves_a_store_and_a_relative_value_does_not() {
    let fixture = Fixture::new();
    let elsewhere = fixture.home.join("elsewhere");
    write(&elsewhere.join("README"), 5);
    let mut environment = fixture.environment();
    environment
        .variables
        .insert("GOCACHE".to_owned(), elsewhere.clone().into_os_string());
    environment
        .variables
        .insert("SCCACHE_DIR".to_owned(), "relative/cache".into());
    let scan = inspect(&[], &environment);
    let path = |id: &str| {
        let store = scan.stores.iter().find(|store| store.id == id).unwrap();
        store.path.clone()
    };
    assert_eq!(path("go.build-cache"), elsewhere);
    assert!(path("sccache.cache").is_absolute());
    assert!(scan.checkouts.is_empty());
}

#[test]
fn the_report_sums_each_class_and_round_trips() {
    let fixture = Fixture::new();
    let report = build_report(&fixture.scan(), 0);
    assert_eq!(report.schema, DEVELOPMENT_SCHEMA);
    assert_eq!(report.status, ReportStatus::Complete);
    assert_eq!(report.totals.main_checkouts.count, 1);
    assert_eq!(report.totals.linked_checkouts.count, 1);
    assert_eq!(report.totals.build_state.count, 5);
    assert_eq!(
        report.totals.build_state.logical_bytes,
        5043 + 70 + 700 + 315 + 9043
    );
    assert_eq!(report.totals.shared_stores.count, 2);
    assert_eq!(report.totals.toolchains.logical_bytes, 2000);
    let text = serde_json::to_string(&report).unwrap();
    let parsed: DevelopmentReport = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed, report);
}

#[test]
fn a_workspace_that_does_not_exist_makes_the_report_partial() {
    let fixture = Fixture::new();
    let missing = fixture.workspace.join("missing");
    let scan = inspect(&[missing], &fixture.environment());
    assert!(!scan.workspaces[0].measured);
    assert_eq!(build_report(&scan, 0).status, ReportStatus::Partial);
}

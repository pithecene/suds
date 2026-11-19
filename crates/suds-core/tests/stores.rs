//! Tests of the stores plan and its adapters against stub programs.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use suds_core::development::{DevelopmentScan, Ecosystem, Store, StoreClass, StoreStatus};
use suds_core::model::Measure;
use suds_core::organize::journal::{Journal, JournalState, RunKind, RunStatus, read};
use suds_core::reclaim::execute::Authority;
use suds_core::stores::{
    Budgets, DOCKER_ANONYMOUS_VOLUMES, DOCKER_BUILD_CACHE, DOCKER_IMAGES, DOCKER_NAMED_VOLUMES,
    Decision, EngineUsage, Inputs, Programs, StoresPlan, apply, plan_stores,
};
use tempfile::TempDir;

const GIB: u64 = 1024 * 1024 * 1024;
/// A time long after every `newest_modified` of the fixture.
const NOW: i64 = 2_000_000_000;

/// A directory of stub programs. Each stub appends its name and its
/// arguments to one log, and the stub of pnpm fails.
struct Stubs {
    dir: TempDir,
}

impl Stubs {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        for name in ["go", "pnpm", "ccache", "mise", "docker"] {
            let status = if name == "pnpm" { 1 } else { 0 };
            let script = format!(
                "#!/bin/sh\necho \"{name} $*\" >> \"{}\"\necho refused >&2\nexit {status}\n",
                dir.path().join("log").display()
            );
            let path = dir.path().join(name);
            fs::write(&path, script).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        Stubs { dir }
    }

    fn programs(&self) -> Programs {
        let stub = |name: &str| self.dir.path().join(name);
        Programs {
            go: stub("go"),
            pnpm: stub("pnpm"),
            ccache: stub("ccache"),
            mise: stub("mise"),
            docker: stub("docker"),
        }
    }

    fn log(&self) -> Vec<String> {
        fs::read_to_string(self.dir.path().join("log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

fn store(id: &str, ecosystem: Ecosystem, gib: u64, status: StoreStatus) -> Store {
    Store {
        id: id.to_owned(),
        class: StoreClass::SharedStore,
        ecosystem,
        path: PathBuf::from("/stores").join(id),
        status,
        measure: Measure {
            allocated_bytes: gib * GIB,
            newest_modified: Some("2026-01-01T00:00:00Z".to_owned()),
            ..Measure::default()
        },
    }
}

fn scan(stores: Vec<Store>) -> DevelopmentScan {
    DevelopmentScan {
        workspaces: Vec::new(),
        checkouts: Vec::new(),
        stores,
        skipped_mount_count: 0,
        error_count: 0,
        errors: Vec::new(),
    }
}

fn every_store() -> DevelopmentScan {
    let present = StoreStatus::Present;
    scan(vec![
        store("go.build-cache", Ecosystem::Go, 82, present),
        store("go.module-cache", Ecosystem::Go, 11, present),
        store("pnpm.store", Ecosystem::Pnpm, 14, present),
        store("ccache.cache", Ecosystem::Ccache, 6, present),
        store("mise.installs", Ecosystem::Mise, 33, present),
        store("cargo.registry", Ecosystem::Cargo, 50, present),
        store("sccache.cache", Ecosystem::Sccache, 1, StoreStatus::Absent),
        store(
            "vcpkg.binary-cache",
            Ecosystem::Vcpkg,
            9,
            StoreStatus::Unrecognized,
        ),
    ])
}

fn engine() -> Result<EngineUsage, String> {
    Ok(EngineUsage {
        images: 134 * GIB,
        build_cache: 13 * GIB,
        unused_anonymous_volumes: 40 * GIB,
        unused_named_volumes: vec![("database".to_owned(), 2 * GIB)],
    })
}

fn plan(scan: &DevelopmentScan, programs: &Programs, now: i64) -> StoresPlan {
    let budgets = Budgets::default();
    let engine = engine();
    plan_stores(
        scan,
        &Inputs {
            budgets: &budgets,
            programs,
            engine: &engine,
            idle_seconds: 3600,
            now,
        },
    )
}

fn decision<'a>(plan: &'a StoresPlan, id: &str) -> &'a Decision {
    &plan
        .entries
        .iter()
        .find(|entry| entry.id == id)
        .unwrap_or_else(|| panic!("no entry {id}"))
        .decision
}

#[test]
fn decides_each_store_by_its_budget_and_its_program() {
    let stubs = Stubs::new();
    let plan = plan(&every_store(), &stubs.programs(), NOW);

    for id in [
        "go.build-cache",
        "go.module-cache",
        "pnpm.store",
        "ccache.cache",
        "mise.installs",
        DOCKER_IMAGES,
        DOCKER_BUILD_CACHE,
        DOCKER_ANONYMOUS_VOLUMES,
    ] {
        assert!(matches!(decision(&plan, id), Decision::Trim { .. }), "{id}");
    }
    // Cargo bounds its own registry, so the store has no budget here.
    assert!(matches!(
        decision(&plan, "cargo.registry"),
        Decision::Audit { .. }
    ));
    assert!(matches!(
        decision(&plan, "vcpkg.binary-cache"),
        Decision::Audit { .. }
    ));
    assert!(plan.entries.iter().all(|entry| entry.id != "sccache.cache"));
    assert_eq!(
        decision(&plan, DOCKER_NAMED_VOLUMES),
        &Decision::AwaitsPerson {
            names: vec!["database".to_owned()]
        }
    );
}

#[test]
fn leaves_a_store_within_its_budget_and_one_that_a_build_may_write() {
    let stubs = Stubs::new();
    let small = scan(vec![store(
        "go.build-cache",
        Ecosystem::Go,
        3,
        StoreStatus::Present,
    )]);
    assert_eq!(
        decision(&plan(&small, &stubs.programs(), NOW), "go.build-cache"),
        &Decision::Within
    );

    // The plan's time lies minutes after the store's newest entry.
    let just_after = 1_767_225_600 + 600;
    let busy = plan(&every_store(), &stubs.programs(), just_after);
    assert!(matches!(
        decision(&busy, "go.build-cache"),
        Decision::Keep { .. }
    ));
}

#[test]
fn runs_each_program_with_its_exact_arguments_and_journals_each_trim() {
    let stubs = Stubs::new();
    let plan = plan(&every_store(), &stubs.programs(), NOW);
    let journal_dir = stubs.dir.path().join("journal");
    let mut journal = Journal::open(&journal_dir).unwrap();
    let report = apply(&mut journal, &plan, Authority::Person, &mut |entry| {
        Ok(entry.budget_bytes.unwrap_or(0))
    })
    .unwrap();
    drop(journal);

    let ccache_budget = (5 * GIB).to_string();
    let cache_budget = (10 * GIB).to_string();
    assert_eq!(
        stubs.log(),
        [
            "go clean -cache".to_owned(),
            "go clean -modcache".to_owned(),
            "pnpm store prune".to_owned(),
            format!("ccache --trim-dir /stores/ccache.cache --trim-max-size {ccache_budget}"),
            "mise prune --yes".to_owned(),
            "docker image prune --all --force".to_owned(),
            format!("docker builder prune --force --max-used-space {cache_budget}"),
            "docker volume prune --force".to_owned(),
        ]
    );

    // The stub of pnpm fails, and the other trims proceed.
    assert_eq!(report.status, RunStatus::Partial);
    let failed: Vec<&str> = report
        .trims
        .iter()
        .filter(|trim| trim.failure.is_some())
        .map(|trim| trim.store.as_str())
        .collect();
    assert_eq!(failed, ["pnpm.store"]);
    assert_eq!(report.freed_bytes, (72 + 1 + 1 + 13 + 114 + 3 + 39) * GIB);

    let summaries = JournalState::from_records(&read(&journal_dir).unwrap()).summaries();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].kind, RunKind::Stores);
    assert_eq!(summaries[0].trims.len(), 8);
    assert_eq!(summaries[0].trims[0].command, {
        let go: &Path = &stubs.programs().go;
        format!("{} clean -cache", go.display())
    });
    assert!(
        summaries[0].trims[2]
            .reason
            .as_ref()
            .unwrap()
            .contains("refused")
    );
}

#[test]
fn audits_the_engine_when_it_does_not_answer() {
    let stubs = Stubs::new();
    let budgets = Budgets::default();
    let engine = Err("the engine does not run".to_owned());
    let plan = plan_stores(
        &scan(Vec::new()),
        &Inputs {
            budgets: &budgets,
            programs: &stubs.programs(),
            engine: &engine,
            idle_seconds: 3600,
            now: NOW,
        },
    );
    assert_eq!(plan.entries.len(), 1);
    assert!(matches!(plan.entries[0].decision, Decision::Audit { .. }));
}

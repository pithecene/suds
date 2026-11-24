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
    Decision, EngineUsage, Inputs, Programs, StoresPlan, apply, builder_budget_option,
    measure_after, plan_stores,
};
use tempfile::TempDir;

mod support;
use support::program;

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
            // Each line holds the stub's arguments, the variables that bind
            // it to a fixture store, and its working directory.
            let script = format!(
                "#!/bin/sh\nvars=$(env | grep -E '^(GOCACHE|GOMODCACHE|MISE_DATA_DIR)=/stores' | sort | tr '\\n' ' ')\necho \"{name} $* | $vars| $(pwd)\" >> \"{}\"\necho refused >&2\nexit {status}\n",
                dir.path().join("log").display()
            );
            program(&dir.path().join(name), &script, "755");
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
            search: None,
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
        // An eviction names a directory that exists. This one is no cache of
        // Go, so a run refuses it.
        path: if id == "go.build-cache" {
            PathBuf::from("/usr/share")
        } else {
            PathBuf::from("/stores").join(id)
        },
        status,
        measure: Measure {
            allocated_bytes: gib * GIB,
            newest_modified: Some("2026-01-01T00:00:00Z".to_owned()),
            ..Measure::default()
        },
        owner: None,
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
        store("ccache.cache", Ecosystem::Ccache, 12, present),
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
            builder_budget_option: "--keep-storage",
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

    let Decision::Evict { eviction } = decision(&plan, "go.build-cache") else {
        panic!("the cache exceeds its budget");
    };
    assert_eq!(eviction.budget_bytes, 10 * GIB);
    // Go states a use to the hour, so an hour joins the idle time.
    assert_eq!(eviction.used_since, NOW - 2 * 3600);
    for id in [
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
        decision(&busy, "go.module-cache"),
        Decision::Keep { .. }
    ));
    // Each entry of Go's build cache states its own last use, so a build
    // elsewhere in the cache keeps only the entries that it uses.
    assert!(matches!(
        decision(&busy, "go.build-cache"),
        Decision::Evict { .. }
    ));
}

#[test]
fn keeps_a_build_cache_inside_a_checkout_and_audits_one_that_it_cannot_read() {
    let stubs = Stubs::new();
    let present = StoreStatus::Present;
    let mut inside = scan(vec![store("go.build-cache", Ecosystem::Go, 82, present)]);
    let checkout = TempDir::new().unwrap();
    fs::create_dir_all(checkout.path().join("cache")).unwrap();
    fs::create_dir(checkout.path().join(".git")).unwrap();
    inside.stores[0].path = checkout.path().join("cache");
    assert!(matches!(
        decision(&plan(&inside, &stubs.programs(), NOW), "go.build-cache"),
        Decision::Keep { .. }
    ));

    let mut absent = scan(vec![store("go.build-cache", Ecosystem::Go, 82, present)]);
    absent.stores[0].path = PathBuf::from("/stores/absent");
    assert!(matches!(
        decision(&plan(&absent, &stubs.programs(), NOW), "go.build-cache"),
        Decision::Audit { .. }
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

    // Each command names the store that the plan measured, through an
    // argument or a variable, and runs in the root directory.
    let cache_budget = 10 * GIB;
    assert_eq!(
        stubs.log(),
        [
            "go clean -modcache | GOMODCACHE=/stores/go.module-cache | /".to_owned(),
            "pnpm store prune --store-dir /stores/pnpm.store | | /".to_owned(),
            "ccache --trim-dir /stores/ccache.cache --trim-max-size 10485760Ki | | /".to_owned(),
            "mise prune --yes | MISE_DATA_DIR=/stores | /".to_owned(),
            "docker image prune --all --force | | /".to_owned(),
            format!("docker builder prune --force --keep-storage {cache_budget} | | /"),
            "docker volume prune --force | | /".to_owned(),
        ]
    );

    // The stub of pnpm fails and the fixture's directory is no cache,
    // and the other trims proceed.
    assert_eq!(report.status, RunStatus::Partial);
    let failed: Vec<&str> = report
        .trims
        .iter()
        .filter(|trim| trim.failure.is_some())
        .map(|trim| trim.store.as_str())
        .collect();
    assert_eq!(failed, ["go.build-cache", "pnpm.store"]);
    assert_eq!(report.freed_bytes, (1 + 2 + 13 + 114 + 3 + 39) * GIB);

    let summaries = JournalState::from_records(&read(&journal_dir).unwrap()).summaries();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].kind, RunKind::Stores);
    assert_eq!(summaries[0].trims.len(), 8);
    assert!(
        summaries[0].trims[0]
            .command
            .starts_with("evict from /usr/share")
    );
    assert_eq!(summaries[0].trims[1].command, {
        let go: &Path = &stubs.programs().go;
        format!(
            "GOMODCACHE=/stores/go.module-cache {} clean -modcache",
            go.display()
        )
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
            builder_budget_option: "--keep-storage",
            idle_seconds: 3600,
            now: NOW,
        },
    );
    assert_eq!(plan.entries.len(), 1);
    assert!(matches!(plan.entries[0].decision, Decision::Audit { .. }));
    // A store that Suds could not measure states no size.
    assert_eq!(plan.entries[0].measured_bytes, None);
}

#[test]
fn reads_the_build_cache_option_from_the_engine_help() {
    let dir = TempDir::new().unwrap();
    let stub = |name: &str, help: &str| {
        let path = dir.path().join(name);
        program(&path, &format!("#!/bin/sh\necho '{help}'\n"), "755");
        path
    };
    let buildx = stub("buildx", "      --max-used-space bytes   Maximum amount");
    let classic = stub(
        "classic",
        "      --keep-storage bytes   Amount of disk space",
    );
    assert_eq!(builder_budget_option(&buildx), "--max-used-space");
    assert_eq!(builder_budget_option(&classic), "--keep-storage");
    assert_eq!(
        builder_budget_option(&dir.path().join("missing")),
        "--keep-storage"
    );
}

#[test]
fn a_measurement_that_cannot_read_the_store_states_no_size() {
    use std::os::unix::fs::MetadataExt;

    let dir = TempDir::new().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    let cache = root.join("cache");
    fs::create_dir_all(cache.join("hidden")).unwrap();
    fs::write(cache.join("entry"), b"data").unwrap();
    let mut scanned = scan(vec![store(
        "go.build-cache",
        Ecosystem::Go,
        82,
        StoreStatus::Present,
    )]);
    scanned.stores[0].path.clone_from(&cache);
    let stubs = Stubs::new();
    let plan = plan(&scanned, &stubs.programs(), NOW);
    let entry = &plan.entries[0];
    assert!(measure_after(entry, &stubs.programs().docker).unwrap() > 0);

    // The superuser reads every directory, so the rest holds only for an
    // ordinary user.
    if fs::symlink_metadata(&cache).unwrap().uid() == 0 {
        return;
    }
    fs::set_permissions(cache.join("hidden"), fs::Permissions::from_mode(0o000)).unwrap();
    let measured = measure_after(entry, &stubs.programs().docker);
    fs::set_permissions(cache.join("hidden"), fs::Permissions::from_mode(0o700)).unwrap();
    assert!(measured.unwrap_err().contains("could not measure"));
}

/// macOS rejects a file name that is not valid UTF-8, so this test runs on
/// Linux alone.
#[cfg(target_os = "linux")]
#[test]
fn a_command_receives_the_bytes_of_a_path_that_is_not_utf8() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let dir = TempDir::new().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    let store_path = root.join(OsStr::from_bytes(b"caf\xe9"));
    fs::create_dir(&store_path).unwrap();
    let log = root.join("received");
    let ccache = root.join("ccache");
    let script = format!(
        "#!/bin/sh\ntest -d \"$2\" && echo found > \"{}\"\n",
        log.display()
    );
    program(&ccache, &script, "755");
    let mut scanned = scan(vec![store(
        "ccache.cache",
        Ecosystem::Ccache,
        12,
        StoreStatus::Present,
    )]);
    scanned.stores[0].path.clone_from(&store_path);
    let programs = Programs {
        ccache,
        ..Stubs::new().programs()
    };
    let plan = plan(&scanned, &programs, NOW);
    assert_eq!(plan.entries[0].path.as_ref().unwrap().as_path(), store_path);
    let mut journal = Journal::open(&root.join("journal")).unwrap();
    apply(&mut journal, &plan, Authority::Person, &mut |_| Ok(0)).unwrap();
    assert_eq!(fs::read_to_string(&log).unwrap(), "found\n");
}

#[test]
fn reports_a_machine_and_trims_none() {
    let stubs = Stubs::new();
    let machine = |id: &str, gib: u64, owner: Option<&str>| Store {
        class: StoreClass::Machine,
        owner: owner.map(PathBuf::from),
        ..store(id, Ecosystem::Lima, gib, StoreStatus::Present)
    };
    let stores = scan(vec![
        machine("colima.machines", 160, None),
        machine("lima.machines", 20, None),
        machine("lima.machine", 1, Some("/workspace/repo")),
        machine("other.machine", 160, None),
    ]);
    let first = plan(&stores, &stubs.programs(), NOW);
    // A disk beyond its budget and a machine inside a checkout both reach
    // the person, and no program trims either.
    let Decision::Audit { reason } = decision(&first, "colima.machines") else {
        panic!("a large machine is an audit");
    };
    assert!(reason.contains("exceeds its budget"));
    assert_eq!(decision(&first, "lima.machines"), &Decision::Within);
    let Decision::Audit { reason } = decision(&first, "lima.machine") else {
        panic!("a machine inside a checkout is an audit");
    };
    assert!(reason.contains("a checkout holds a virtual machine"));
    // A machine without a budget is no machine within one.
    assert!(matches!(
        decision(&first, "other.machine"),
        Decision::Audit { .. }
    ));
    // A machine that the walk found outside a checkout has the budget too.
    let stray = scan(vec![machine("lima.machine", 160, None)]);
    let second = plan(&stray, &stubs.programs(), NOW);
    assert!(matches!(
        decision(&second, "lima.machine"),
        Decision::Audit { .. }
    ));
}

#[test]
fn a_program_outside_the_search_path_resolves_in_a_version_managers_directory() {
    use suds_core::development::Environment;

    let temp = TempDir::new().unwrap();
    let home = temp.path().canonicalize().unwrap();
    let place = |directory: &str, name: &str, mode: &str| {
        let path = home.join(directory).join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        program(&path, "#!/bin/sh\n", mode);
        path
    };
    let shim = place(".local/share/mise/shims", "pnpm", "755");
    // A later place holds the program too, and the first place wins.
    place(".local/bin", "pnpm", "755");
    // A file that no user may run is no program.
    place(".local/share/mise/shims", "ccache", "644");
    let later = place(".cargo/bin", "ccache", "755");
    place("searched", "docker", "755");
    place(".local/bin", "docker", "755");

    let searched = ("PATH".to_owned(), home.join("searched").into());
    let environment = Environment {
        home: Some(home.clone()),
        variables: [searched].into(),
    };
    let resolution = Programs::resolved(&environment);
    let programs = &resolution.programs;
    assert_eq!(programs.pnpm, shim);
    assert_eq!(programs.ccache, later);
    // The search path resolves the name, so the name stays as it stood.
    assert_eq!(programs.docker, Path::new("docker"));
    assert_eq!(resolution.unread, Vec::<String>::new());

    // A found program starts its own programs from the searched and the known places.
    let search = programs
        .search
        .clone()
        .expect("a program stands beyond the search path");
    let places: Vec<PathBuf> = std::env::split_paths(&search).collect();
    assert_eq!(places[0], home.join("searched"));
    assert!(places.contains(&home.join(".local/share/mise/shims")));
    assert!(!places.contains(&home.join(".asdf/shims")));

    // A directory that refuses a reading stands in the result.
    let closed = home.join(".local/share/mise/shims");
    fs::set_permissions(&closed, fs::Permissions::from_mode(0o000)).unwrap();
    let denied = fs::metadata(closed.join("pnpm")).is_err();
    let resolution = Programs::resolved(&environment);
    fs::set_permissions(&closed, fs::Permissions::from_mode(0o755)).unwrap();
    // A superuser reads the closed directory, and the search then misses nothing.
    assert_eq!(!resolution.unread.is_empty(), denied);
    if denied {
        assert!(
            resolution.unread[0].contains("mise/shims/"),
            "{:?}",
            resolution.unread
        );
        assert_eq!(resolution.programs.pnpm, home.join(".local/bin/pnpm"));
    }
}

#[test]
fn a_program_beyond_the_search_path_runs_with_the_known_places_on_its_path() {
    let stubs = Stubs::new();
    let found = Programs {
        search: Some(PathBuf::from("/known/bin:/usr/bin")),
        ..stubs.programs()
    };
    let named = Programs {
        search: Some(PathBuf::from("/known/bin")),
        ..Programs::default()
    };
    for (programs, carries) in [(found, true), (named, false)] {
        let present = StoreStatus::Present;
        let scan = scan(vec![store("pnpm.store", Ecosystem::Pnpm, 30, present)]);
        let plan = plan(&scan, &programs, NOW);
        let Decision::Trim { trim } = &plan.entries[0].decision else {
            panic!("the store exceeds its budget");
        };
        let path = trim.environment.iter().find(|(name, _)| name == "PATH");
        assert_eq!(path.is_some(), carries, "{}", trim.display());
    }
}

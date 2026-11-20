//! The development profile: checkouts, their build state, and shared stores.
//!
//! Each class rests on proof from its ecosystem's own layout, and a name
//! alone proves none. The profile reads metadata, with one exception: the
//! `.git` file of a linked checkout, whose first line names its repository.
//! The profile changes nothing.

use std::collections::{HashMap, HashSet, VecDeque};
use std::env;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use rustix::fs::OFlags;
use serde::{Deserialize, Serialize};

use crate::inspection::ScanError;
use crate::model::{Measure, Platform, ReportStatus, TraversalError};
use crate::roots::normalize;
use crate::scan::SharedScan;
use crate::timestamp;

/// The schema tag of every development report.
pub const DEVELOPMENT_SCHEMA: &str = "suds.development/v1";

/// The longest `.git` file that the profile reads. A linked checkout's file
/// holds one line with a path.
const POINTER_LIMIT: u64 = 4096;

/// The program whose layout proves a class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ecosystem {
    Cargo,
    Ccache,
    Cmake,
    Go,
    Mise,
    Node,
    Pnpm,
    Sccache,
    Vcpkg,
}

impl Ecosystem {
    pub fn label(self) -> &'static str {
        match self {
            Ecosystem::Cargo => "cargo",
            Ecosystem::Ccache => "ccache",
            Ecosystem::Cmake => "cmake",
            Ecosystem::Go => "go",
            Ecosystem::Mise => "mise",
            Ecosystem::Node => "node",
            Ecosystem::Pnpm => "pnpm",
            Ecosystem::Sccache => "sccache",
            Ecosystem::Vcpkg => "vcpkg",
        }
    }
}

/// Whether a checkout holds its repository or borrows another checkout's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckoutKind {
    /// The checkout's `.git` is a directory, so it holds the repository.
    Main,
    /// The checkout's `.git` is a file that names a main checkout's
    /// `worktrees` directory.
    Linked,
}

/// One directory of derived state beneath a checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildState {
    pub path: PathBuf,
    pub ecosystem: Ecosystem,
    /// The directory without any checkout beneath it.
    pub measure: Measure,
    /// What the directory holds besides derived state.
    pub survey: Survey,
    /// Each repository beneath it that the build's own layout proves
    /// fetched. A removal asks Git whether each one holds work that no
    /// remote holds.
    pub repositories: Vec<PathBuf>,
}

/// What a directory of build state holds that a build does not regenerate,
/// and what the measurement could not see. A directory counts as derived
/// state as a whole only when every count is zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Survey {
    /// The directories beneath it that hold a `.git` entry and that no
    /// layout proves fetched. Each one is a checkout.
    pub checkouts_beneath: u64,
    /// The mount points beneath it, which the measurement did not enter.
    pub skipped_mount_count: u64,
    /// The entries beneath it that the measurement could not read.
    pub error_count: u64,
}

impl Survey {
    pub fn is_clear(&self) -> bool {
        *self == Survey::default()
    }
}

/// One checkout with the build state beneath it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkout {
    pub path: PathBuf,
    pub kind: CheckoutKind,
    /// For a linked checkout, the main checkout that holds its repository.
    pub repository: Option<PathBuf>,
    /// The checkout without its build state and without any checkout
    /// beneath it.
    pub own: Measure,
    pub build_state: Vec<BuildState>,
}

/// The class of a store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoreClass {
    /// Content that every checkout of an ecosystem reads from one copy.
    SharedStore,
    /// Installed tools, each at the version that a pin names.
    Toolchain,
}

/// What the profile found at a store's location.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoreStatus {
    /// The directory exists and holds its program's marker.
    Present,
    /// No directory exists at the location.
    Absent,
    /// A directory exists without its program's marker, so nothing proves
    /// that the program owns it.
    Unrecognized,
    /// Suds could not examine the location, or a symbolic link stands on
    /// its path beneath the home directory. The report's errors name the
    /// path.
    Unreadable,
}

/// One shared store or toolchain directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Store {
    pub id: String,
    pub class: StoreClass,
    pub ecosystem: Ecosystem,
    pub path: PathBuf,
    pub status: StoreStatus,
    pub measure: Measure,
    /// For a toolchain that a checkout fetched into its own tree, that
    /// checkout. A store at a program's own location has no owner.
    pub owner: Option<PathBuf>,
}

/// One directory that the caller named as a place to look for checkouts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    pub path: PathBuf,
    /// Whether the scan could measure the directory.
    pub measured: bool,
    /// Everything beneath the directory that belongs to no class.
    pub unclassified: Measure,
}

/// The lossless result of a development inspection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevelopmentScan {
    pub workspaces: Vec<Workspace>,
    pub checkouts: Vec<Checkout>,
    pub stores: Vec<Store>,
    pub skipped_mount_count: u64,
    pub error_count: u64,
    /// The first traversal errors, in traversal order.
    pub errors: Vec<ScanError>,
}

/// The process's home directory and the variables that locate each store.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Environment {
    pub home: Option<PathBuf>,
    pub variables: HashMap<String, OsString>,
}

impl Environment {
    /// The environment of this process.
    pub fn of_process() -> Self {
        Environment {
            home: env::home_dir().filter(|home| home.is_absolute()),
            variables: env::vars_os()
                .filter_map(|(name, value)| Some((name.into_string().ok()?, value)))
                .collect(),
        }
    }

    /// The absolute path that `name` holds. A program ignores a relative
    /// path in these variables, and so does the profile.
    fn path(&self, name: &str) -> Option<PathBuf> {
        self.variables
            .get(name)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
    }

    fn home(&self, relative: &str) -> Option<PathBuf> {
        self.home.as_ref().map(|home| home.join(relative))
    }

    fn cache_home(&self) -> Option<PathBuf> {
        self.path("XDG_CACHE_HOME")
            .or_else(|| self.home(default_cache_home()))
    }

    fn data_home(&self) -> Option<PathBuf> {
        self.path("XDG_DATA_HOME")
            .or_else(|| self.home(".local/share"))
    }
}

#[cfg(target_os = "linux")]
fn default_cache_home() -> &'static str {
    ".cache"
}

#[cfg(target_os = "macos")]
fn default_cache_home() -> &'static str {
    "Library/Caches"
}

/// A store's identifier, its location, and the entry that proves its owner.
struct StoreSpec {
    id: &'static str,
    class: StoreClass,
    ecosystem: Ecosystem,
    path: Option<PathBuf>,
    /// An entry beneath the store that only its program writes.
    marker: Option<&'static str>,
}

/// Each store at the location that its program documents: the program's own
/// variable first, and its default beneath the home directory otherwise.
fn store_specs(environment: &Environment) -> Vec<StoreSpec> {
    let shared = StoreClass::SharedStore;
    let go_path = environment
        .variables
        .get("GOPATH")
        .and_then(|value| env::split_paths(value).next())
        .filter(|path| path.is_absolute())
        .or_else(|| environment.home("go"));
    let cargo_home = environment
        .path("CARGO_HOME")
        .or_else(|| environment.home(".cargo"));
    // The store of pnpm lies beneath the program's home directory, which
    // `PNPM_HOME` names when the variable holds a path.
    let pnpm_data = environment.path("PNPM_HOME").or_else(|| {
        if cfg!(target_os = "macos") && environment.path("XDG_DATA_HOME").is_none() {
            environment.home("Library/pnpm")
        } else {
            environment.data_home().map(|data| data.join("pnpm"))
        }
    });
    let sccache = if cfg!(target_os = "macos") {
        environment.home("Library/Caches/Mozilla.sccache")
    } else {
        environment.cache_home().map(|cache| cache.join("sccache"))
    };
    // On every platform but Windows, vcpkg keeps its archives beneath
    // `~/.cache` unless a variable names another directory.
    let vcpkg_cache = environment
        .path("XDG_CACHE_HOME")
        .or_else(|| environment.home(".cache"))
        .map(|cache| cache.join("vcpkg/archives"));
    vec![
        StoreSpec {
            id: "go.build-cache",
            class: shared,
            ecosystem: Ecosystem::Go,
            path: environment
                .path("GOCACHE")
                .or_else(|| environment.cache_home().map(|cache| cache.join("go-build"))),
            marker: Some("README"),
        },
        StoreSpec {
            id: "go.module-cache",
            class: shared,
            ecosystem: Ecosystem::Go,
            path: environment
                .path("GOMODCACHE")
                .or_else(|| go_path.map(|path| path.join("pkg/mod"))),
            marker: Some("cache/download"),
        },
        StoreSpec {
            id: "cargo.registry",
            class: shared,
            ecosystem: Ecosystem::Cargo,
            path: cargo_home.map(|home| home.join("registry")),
            marker: None,
        },
        StoreSpec {
            id: "pnpm.store",
            class: shared,
            ecosystem: Ecosystem::Pnpm,
            path: pnpm_data.map(|data| data.join("store")),
            marker: None,
        },
        StoreSpec {
            id: "sccache.cache",
            class: shared,
            ecosystem: Ecosystem::Sccache,
            path: environment.path("SCCACHE_DIR").or(sccache),
            marker: None,
        },
        StoreSpec {
            id: "ccache.cache",
            class: shared,
            ecosystem: Ecosystem::Ccache,
            path: environment
                .path("CCACHE_DIR")
                .or_else(|| environment.cache_home().map(|cache| cache.join("ccache"))),
            marker: None,
        },
        StoreSpec {
            id: "vcpkg.binary-cache",
            class: shared,
            ecosystem: Ecosystem::Vcpkg,
            path: environment
                .path("VCPKG_DEFAULT_BINARY_CACHE")
                .or(vcpkg_cache),
            marker: None,
        },
        StoreSpec {
            id: "mise.installs",
            class: StoreClass::Toolchain,
            ecosystem: Ecosystem::Mise,
            path: environment
                .path("MISE_DATA_DIR")
                .or_else(|| environment.data_home().map(|data| data.join("mise")))
                .map(|data| data.join("installs")),
            marker: None,
        },
    ]
}

/// The directory of each store that exists, without a measurement. An
/// audit of the arena needs each location and no size.
pub fn store_locations(environment: &Environment) -> Vec<PathBuf> {
    store_specs(environment)
        .into_iter()
        .filter_map(|spec| spec.path)
        .filter(|path| fs::symlink_metadata(path).is_ok())
        .collect()
}

/// What a directory's own layout proves about it.
enum Claim {
    Checkout(CheckoutKind, Option<PathBuf>),
    Build(Ecosystem),
    Toolchain(Ecosystem),
}

fn is_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file())
}

fn is_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir())
}

/// Whether `dir` holds the chain of directories `chain`, and whether the
/// last one holds each of `files`. The check reads metadata alone, and a
/// symbolic link at any step fails it.
fn holds(dir: &Path, chain: &[&str], files: &[&str]) -> bool {
    let mut at = dir.to_path_buf();
    for name in chain {
        at.push(name);
        if !is_dir(&at) {
            return false;
        }
    }
    files.iter().all(|name| is_file(&at.join(name)))
}

/// Whether `dir` has the layout of Go's build cache: its notice, its trim
/// record, and the first and the last of its 256 directories.
fn go_build_cache(dir: &Path) -> bool {
    holds(dir, &[], &["README", "trim.txt"]) && is_dir(&dir.join("00")) && is_dir(&dir.join("ff"))
}

/// Classifies `dir` from the entries that its ecosystem writes. Build state
/// has an owner, so the profile claims it only beneath a checkout.
fn claim(dir: &Path, in_checkout: bool) -> Option<Claim> {
    // A vcpkg checkout holds a repository of its own, so its marker decides
    // before the `.git` entry.
    if is_file(&dir.join(".vcpkg-root")) {
        return Some(Claim::Toolchain(Ecosystem::Vcpkg));
    }
    let git = dir.join(".git");
    match fs::symlink_metadata(&git) {
        Ok(meta) if meta.is_dir() => return Some(Claim::Checkout(CheckoutKind::Main, None)),
        Ok(meta) if meta.is_file() => {
            // A submodule's file names a `modules` directory, so the
            // submodule stays a part of its parent checkout.
            if let Some(repository) = linked_repository(dir, &git) {
                return Some(Claim::Checkout(CheckoutKind::Linked, Some(repository)));
            }
        }
        _ => {}
    }
    if !in_checkout {
        return None;
    }
    let parent = dir.parent()?;
    let beside = |name: &str| is_file(&parent.join(name));
    let ecosystem = match dir.file_name()?.to_str() {
        Some("target") if is_file(&dir.join("CACHEDIR.TAG")) && beside("Cargo.toml") => {
            Ecosystem::Cargo
        }
        // A workspace's root may hold no manifest of its own, and each
        // package manager writes a state file into the directory.
        Some("node_modules")
            if beside("package.json")
                || beside("pnpm-workspace.yaml")
                || is_file(&dir.join(".modules.yaml"))
                || is_file(&dir.join(".package-lock.json")) =>
        {
            Ecosystem::Node
        }
        Some("vcpkg_installed") if beside("vcpkg.json") => Ecosystem::Vcpkg,
        _ if is_file(&dir.join("CMakeCache.txt")) => Ecosystem::Cmake,
        // A project may point a cache into its own checkout, where each
        // checkout then fills a copy. The cache's program names it there.
        _ if go_build_cache(dir) || holds(dir, &["cache", "download"], &[]) => Ecosystem::Go,
        _ if holds(dir, &["0"], &["CACHEDIR.TAG", "stats"]) => Ecosystem::Ccache,
        _ => return None,
    };
    Some(Claim::Build(ecosystem))
}

/// The ecosystem whose layout proves that `dir` is build state beneath a
/// checkout.
pub(crate) fn build_state_at(dir: &Path) -> Option<Ecosystem> {
    match claim(dir, true) {
        Some(Claim::Build(ecosystem)) => Some(ecosystem),
        _ => None,
    }
}

/// The ecosystem whose layout proves that `dir` is a toolchain that a
/// checkout fetched into its own tree.
pub(crate) fn toolchain_at(dir: &Path) -> Option<Ecosystem> {
    match claim(dir, true) {
        Some(Claim::Toolchain(ecosystem)) => Some(ecosystem),
        _ => None,
    }
}

/// The main checkout that the `.git` file of `dir` names, when the file
/// names an entry of that checkout's `worktrees` directory.
fn linked_repository(dir: &Path, git: &Path) -> Option<PathBuf> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(OFlags::NOFOLLOW.bits().cast_signed())
        .open(git)
        .ok()?;
    let mut text = String::new();
    file.take(POINTER_LIMIT).read_to_string(&mut text).ok()?;
    let pointer = text.lines().next()?.strip_prefix("gitdir:")?.trim();
    let entry = normalize(&dir.join(pointer));
    let worktrees = entry.parent()?;
    let repository = worktrees.parent()?;
    if worktrees.file_name()? != "worktrees" || repository.file_name()? != ".git" {
        return None;
    }
    repository.parent().map(Path::to_path_buf)
}

/// Inspects each workspace for checkouts and then each store. The checkouts
/// come first, so a file that a checkout shares with a store through a hard
/// link counts toward the checkout, as `du` counts it there.
pub fn inspect(workspaces: &[PathBuf], environment: &Environment) -> DevelopmentScan {
    let mut scan = SharedScan::default();
    let mut found = Found::default();
    let mut results = Vec::new();
    for path in outermost(workspaces) {
        let mut workspace = Workspace {
            path: path.clone(),
            measured: true,
            unclassified: Measure::default(),
        };
        if let Some(claimed) = claim(path, false) {
            found.take(&mut scan, path.clone(), claimed, None);
        } else if let Some(bounded) = scan.measure(path, &mut |dir| claim(dir, false).is_some()) {
            workspace.unclassified = bounded.measure;
            for dir in bounded.set_aside {
                if let Some(claimed) = claim(&dir, false) {
                    found.take(&mut scan, dir, claimed, None);
                }
            }
        } else {
            workspace.measured = false;
        }
        results.push(workspace);
    }
    while let Some(mut checkout) = found.pending.pop_front() {
        if let Some(bounded) = scan.measure(&checkout.path, &mut |dir| claim(dir, true).is_some()) {
            checkout.own = bounded.measure;
            for dir in bounded.set_aside {
                match claim(&dir, true) {
                    Some(Claim::Build(ecosystem)) => {
                        // A repository that the build fetched belongs to
                        // the build. Any other one is a checkout, and the
                        // report lists it as one.
                        let build = measure_build(&mut scan, &dir, ecosystem);
                        for path in build.unproven {
                            if let Some(claimed) = claim(&path, false) {
                                found.take(&mut scan, path, claimed, Some(&checkout.path));
                            }
                        }
                        checkout.build_state.push(BuildState {
                            path: dir,
                            ecosystem,
                            measure: build.measure,
                            survey: build.survey,
                            repositories: build.fetched,
                        });
                    }
                    Some(claimed) => {
                        found.take(&mut scan, dir, claimed, Some(&checkout.path));
                    }
                    None => {}
                }
            }
        }
        found.checkouts.push(checkout);
    }
    for spec in store_specs(environment) {
        let Some(path) = spec.path else {
            continue;
        };
        let examined = examine_store(&path, environment.home.as_deref());
        if let Err(err) = &examined {
            scan.record_error(&path, err);
        }
        let present = matches!(examined, Ok(true));
        let status = if examined.is_err() {
            StoreStatus::Unreadable
        } else if !present {
            StoreStatus::Absent
        } else if spec
            .marker
            .is_some_and(|marker| fs::symlink_metadata(path.join(marker)).is_err())
        {
            StoreStatus::Unrecognized
        } else {
            StoreStatus::Present
        };
        let measure = if present {
            measure_whole(&mut scan, &path)
        } else {
            Measure::default()
        };
        found.stores.push(Store {
            id: spec.id.to_owned(),
            class: spec.class,
            ecosystem: spec.ecosystem,
            path,
            status,
            measure,
            owner: None,
        });
    }
    DevelopmentScan {
        workspaces: results,
        checkouts: found.checkouts,
        stores: found.stores,
        skipped_mount_count: scan.skipped_mount_count(),
        error_count: scan.error_count(),
        errors: scan.errors().to_vec(),
    }
}

/// The workspaces that lie beneath no other workspace, in their given
/// order and without a repeat. An outer workspace covers each one beneath
/// it, so a second traversal would count the same entries again.
fn outermost(workspaces: &[PathBuf]) -> Vec<&PathBuf> {
    let mut kept: Vec<&PathBuf> = Vec::new();
    for path in workspaces {
        let covered = workspaces
            .iter()
            .any(|other| other != path && path.starts_with(other));
        if !covered && !kept.contains(&path) {
            kept.push(path);
        }
    }
    kept
}

/// Whether a directory exists at a store's location. The function examines
/// each component beneath the home directory without following a link, as
/// the storage profile examines its default roots. A link there would lead
/// the measurement outside the location that the program documents.
fn examine_store(path: &Path, home: Option<&Path>) -> io::Result<bool> {
    let mut examined = match home {
        Some(home) if path.starts_with(home) => home.to_path_buf(),
        _ => PathBuf::new(),
    };
    let beneath = path.strip_prefix(&examined).unwrap_or(path);
    for component in beneath.components() {
        examined.push(component);
        let meta = match fs::symlink_metadata(&examined) {
            Ok(meta) => meta,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(err) => return Err(err),
        };
        if meta.file_type().is_symlink() && home.is_some_and(|home| path.starts_with(home)) {
            return Err(io::Error::other(format!(
                "{} is a symbolic link on the store's path, and the inspection follows no symbolic link",
                examined.display()
            )));
        }
    }
    fs::symlink_metadata(path).map(|meta| meta.is_dir())
}

/// What a measurement of build state found. The `fetched` repositories are
/// those that the build's own layout proves fetched. The measurement set
/// aside each `unproven` one, which is a checkout.
struct BuildMeasure {
    measure: Measure,
    survey: Survey,
    fetched: Vec<PathBuf>,
    unproven: Vec<PathBuf>,
}

/// Whether the layout of `ecosystem` proves that its build fetched the
/// repository at `dir` beneath the build state at `build`. CMake clones a
/// dependency into `_deps/<name>-src` and keeps its own bookkeeping beside
/// it in `_deps/<name>-subbuild`. No other layout proves a fetch.
fn fetched_by(ecosystem: Ecosystem, build: &Path, dir: &Path) -> bool {
    if ecosystem != Ecosystem::Cmake {
        return false;
    }
    let name = dir.file_name().and_then(|name| name.to_str());
    let (Some(parent), Some(name)) = (dir.parent(), name) else {
        return false;
    };
    let Some(dependency) = name.strip_suffix("-src") else {
        return false;
    };
    parent == build.join("_deps")
        && fs::symlink_metadata(parent.join(format!("{dependency}-subbuild")))
            .is_ok_and(|meta| meta.is_dir())
}

/// Measures one directory of build state and surveys it. A repository that
/// the build fetched counts toward the measure. Any other directory with a
/// `.git` entry is a checkout, so the measurement sets it aside.
fn measure_build(scan: &mut SharedScan, path: &Path, ecosystem: Ecosystem) -> BuildMeasure {
    let (mounts, errors) = (scan.skipped_mount_count(), scan.error_count());
    let mut fetched = Vec::new();
    let bounded = scan.measure(path, &mut |dir| {
        if fs::symlink_metadata(dir.join(".git")).is_err() {
            return false;
        }
        if fetched_by(ecosystem, path, dir) {
            fetched.push(dir.to_path_buf());
            return false;
        }
        true
    });
    let (measure, unproven) = bounded.map_or_else(Default::default, |bounded| {
        (bounded.measure, bounded.set_aside)
    });
    let survey = Survey {
        checkouts_beneath: u64::try_from(unproven.len()).unwrap_or(u64::MAX),
        skipped_mount_count: scan.skipped_mount_count().saturating_sub(mounts),
        error_count: scan.error_count().saturating_sub(errors),
    };
    BuildMeasure {
        measure,
        survey,
        fetched,
        unproven,
    }
}

/// Measures and surveys the build state at `path` in a scan of its own, so
/// that a removal can verify the directory as it stands. The result names
/// each repository that the build fetched.
pub(crate) fn survey_build_state(
    path: &Path,
    ecosystem: Ecosystem,
) -> (Measure, Survey, Vec<PathBuf>) {
    let found = measure_build(&mut SharedScan::default(), path, ecosystem);
    (found.measure, found.survey, found.fetched)
}

/// Measures the tree at `path` in a scan of its own. The result counts the
/// mount points and the unreadable entries beneath it, and it names each
/// directory beneath it that holds a `.git` entry.
pub(crate) fn survey_tree(path: &Path) -> (Measure, Survey, Vec<PathBuf>) {
    let mut scan = SharedScan::default();
    let mut repositories = Vec::new();
    let bounded = scan.measure(path, &mut |dir| {
        if fs::symlink_metadata(dir.join(".git")).is_ok() {
            repositories.push(dir.to_path_buf());
        }
        false
    });
    let survey = Survey {
        checkouts_beneath: 0,
        skipped_mount_count: scan.skipped_mount_count(),
        error_count: scan.error_count(),
    };
    let measure = bounded.map_or_else(Measure::default, |bounded| bounded.measure);
    (measure, survey, repositories)
}

fn measure_whole(scan: &mut SharedScan, path: &Path) -> Measure {
    scan.measure(path, &mut |_| false)
        .map(|bounded| bounded.measure)
        .unwrap_or_default()
}

/// The classified directories that an inspection has found so far.
#[derive(Default)]
struct Found {
    /// The checkouts that await their measurement.
    pending: VecDeque<Checkout>,
    /// The path of each checkout that the inspection has queued, so that no
    /// second route to a checkout measures it again.
    seen: HashSet<PathBuf>,
    checkouts: Vec<Checkout>,
    stores: Vec<Store>,
}

impl Found {
    /// Records what `claimed` found at `path`. The `owner` is the checkout
    /// whose tree holds the path, or none beneath a workspace alone.
    fn take(&mut self, scan: &mut SharedScan, path: PathBuf, claimed: Claim, owner: Option<&Path>) {
        match claimed {
            Claim::Checkout(kind, repository) => {
                if self.seen.insert(path.clone()) {
                    self.pending.push_back(Checkout {
                        path,
                        kind,
                        repository,
                        own: Measure::default(),
                        build_state: Vec::new(),
                    });
                }
            }
            Claim::Toolchain(ecosystem) => {
                let measure = measure_whole(scan, &path);
                self.stores.push(Store {
                    id: format!("{}.checkout", ecosystem.label()),
                    class: StoreClass::Toolchain,
                    ecosystem,
                    path,
                    status: StoreStatus::Present,
                    measure,
                    owner: owner.map(Path::to_path_buf),
                });
            }
            // Build state outside a checkout has no owner, so the profile
            // leaves it unclassified.
            Claim::Build(_) => {}
        }
    }
}

/// One run of `suds inspect --profile development`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DevelopmentReport {
    pub schema: String,
    pub platform: Platform,
    /// The time at which the run started, in RFC 3339 form and UTC.
    pub observed_at: String,
    pub status: ReportStatus,
    pub totals: Totals,
    pub workspaces: Vec<WorkspaceResult>,
    pub checkouts: Vec<CheckoutResult>,
    pub stores: Vec<StoreResult>,
    pub skipped_mount_count: u64,
    pub error_count: u64,
    /// The first traversal errors, in traversal order.
    pub errors: Vec<TraversalError>,
}

/// The allocated bytes of each class.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Totals {
    pub main_checkouts: ClassTotal,
    pub linked_checkouts: ClassTotal,
    /// The build state beneath every checkout. The checkout totals leave it
    /// out.
    pub build_state: ClassTotal,
    pub shared_stores: ClassTotal,
    pub toolchains: ClassTotal,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassTotal {
    pub count: u64,
    pub allocated_bytes: u64,
    pub logical_bytes: u64,
}

impl ClassTotal {
    fn add(&mut self, measure: &Measure) {
        self.count = self.count.saturating_add(1);
        self.allocated_bytes = self.allocated_bytes.saturating_add(measure.allocated_bytes);
        self.logical_bytes = self.logical_bytes.saturating_add(measure.logical_bytes);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceResult {
    pub path: String,
    pub measured: bool,
    pub unclassified: Measure,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutResult {
    pub path: String,
    pub kind: CheckoutKind,
    pub repository: Option<String>,
    pub own: Measure,
    pub build_state: Vec<BuildStateResult>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildStateResult {
    pub path: String,
    pub ecosystem: Ecosystem,
    #[serde(flatten)]
    pub measure: Measure,
    #[serde(flatten)]
    pub survey: Survey,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreResult {
    pub id: String,
    pub class: StoreClass,
    pub ecosystem: Ecosystem,
    pub path: String,
    pub status: StoreStatus,
    /// For a toolchain that a checkout fetched into its own tree, that
    /// checkout.
    pub owner: Option<String>,
    #[serde(flatten)]
    pub measure: Measure,
}

fn display(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Converts a scan to its report. Only this conversion replaces a path's
/// invalid sequences, so a displayed path never names a file.
pub fn build_report(scan: &DevelopmentScan, observed_at: i64) -> DevelopmentReport {
    let mut totals = Totals::default();
    for checkout in &scan.checkouts {
        match checkout.kind {
            CheckoutKind::Main => totals.main_checkouts.add(&checkout.own),
            CheckoutKind::Linked => totals.linked_checkouts.add(&checkout.own),
        }
        for build in &checkout.build_state {
            totals.build_state.add(&build.measure);
        }
    }
    for store in &scan.stores {
        if matches!(store.status, StoreStatus::Absent | StoreStatus::Unreadable) {
            continue;
        }
        match store.class {
            StoreClass::SharedStore => totals.shared_stores.add(&store.measure),
            StoreClass::Toolchain => totals.toolchains.add(&store.measure),
        }
    }
    let complete = scan.error_count == 0 && scan.workspaces.iter().all(|w| w.measured);
    DevelopmentReport {
        schema: DEVELOPMENT_SCHEMA.to_owned(),
        platform: Platform {
            os: env::consts::OS.to_owned(),
            arch: env::consts::ARCH.to_owned(),
        },
        observed_at: timestamp::format_utc(observed_at),
        status: if complete {
            ReportStatus::Complete
        } else {
            ReportStatus::Partial
        },
        totals,
        workspaces: scan
            .workspaces
            .iter()
            .map(|workspace| WorkspaceResult {
                path: display(&workspace.path),
                measured: workspace.measured,
                unclassified: workspace.unclassified.clone(),
            })
            .collect(),
        checkouts: scan
            .checkouts
            .iter()
            .map(|checkout| CheckoutResult {
                path: display(&checkout.path),
                kind: checkout.kind,
                repository: checkout.repository.as_deref().map(display),
                own: checkout.own.clone(),
                build_state: checkout
                    .build_state
                    .iter()
                    .map(|build| BuildStateResult {
                        path: display(&build.path),
                        ecosystem: build.ecosystem,
                        measure: build.measure.clone(),
                        survey: build.survey,
                    })
                    .collect(),
            })
            .collect(),
        stores: scan
            .stores
            .iter()
            .map(|store| StoreResult {
                id: store.id.clone(),
                class: store.class,
                ecosystem: store.ecosystem,
                path: display(&store.path),
                status: store.status,
                owner: store.owner.as_deref().map(display),
                measure: store.measure.clone(),
            })
            .collect(),
        skipped_mount_count: scan.skipped_mount_count,
        error_count: scan.error_count,
        errors: scan
            .errors
            .iter()
            .map(|error| TraversalError {
                path: display(&error.path),
                kind: error.kind,
                message: error.message.clone(),
            })
            .collect(),
    }
}

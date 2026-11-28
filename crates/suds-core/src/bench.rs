//! Safe operation counters, process resource sampling, multi-scale development
//! storage fixtures, and benchmark report comparison for Suds.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use rustix::fs::statvfs;
use serde::{Deserialize, Serialize};

use crate::clones::FLOOR;
use crate::reclaim::git::Git;
use crate::{registry, seed, timestamp};

/// Schema version tag for machine-readable Suds benchmark reports.
pub const BENCHMARK_SCHEMA: &str = "suds.benchmark/v2";

static BULK_DIR_CALLS: AtomicU64 = AtomicU64::new(0);
static ENTRY_PROBE_CALLS: AtomicU64 = AtomicU64::new(0);
static HELPER_LAUNCHES: AtomicU64 = AtomicU64::new(0);
static FORCE_REFERENCE_SCAN: AtomicBool = AtomicBool::new(false);
static FIXTURE_SEQ: AtomicU64 = AtomicU64::new(1);

/// Cumulative Suds operation counters captured at one instant.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counts {
    pub bulk_dir_calls: u64,
    pub entry_probe_calls: u64,
    pub helper_launches: u64,
}

impl Counts {
    /// Captures current cumulative operation counters.
    pub fn now() -> Self {
        Self {
            bulk_dir_calls: BULK_DIR_CALLS.load(Ordering::Relaxed),
            entry_probe_calls: ENTRY_PROBE_CALLS.load(Ordering::Relaxed),
            helper_launches: HELPER_LAUNCHES.load(Ordering::Relaxed),
        }
    }

    /// Computes operations recorded since `earlier`.
    pub fn since(self, earlier: Self) -> Self {
        Self {
            bulk_dir_calls: self.bulk_dir_calls.saturating_sub(earlier.bulk_dir_calls),
            entry_probe_calls: self
                .entry_probe_calls
                .saturating_sub(earlier.entry_probe_calls),
            helper_launches: self.helper_launches.saturating_sub(earlier.helper_launches),
        }
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn record_bulk_dir_call() {
    BULK_DIR_CALLS.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn record_entry_probe() {
    ENTRY_PROBE_CALLS.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn record_helper_launch() {
    HELPER_LAUNCHES.fetch_add(1, Ordering::Relaxed);
}

#[cfg(target_os = "macos")]
pub(crate) fn reference_scan_enabled() -> bool {
    FORCE_REFERENCE_SCAN.load(Ordering::Relaxed)
}

/// Runs `f` with batched directory scanning disabled so traversal uses portable per-entry metadata probes.
pub fn with_reference_scan<T>(f: impl FnOnce() -> T) -> T {
    struct Guard(bool);
    impl Drop for Guard {
        fn drop(&mut self) {
            FORCE_REFERENCE_SCAN.store(self.0, Ordering::SeqCst);
        }
    }
    let prev = FORCE_REFERENCE_SCAN.swap(true, Ordering::SeqCst);
    let _guard = Guard(prev);
    f()
}

/// The CPU time of this process and of the children it waited for, with the process's peak resident size.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessSample {
    pub self_user_us: u64,
    pub self_sys_us: u64,
    pub child_user_us: u64,
    pub child_sys_us: u64,
    /// The largest resident size since the process started, or `None` where the system gave none. No case
    /// lowers it, so a case's own share is the growth between two samples.
    pub peak_rss_bytes: Option<u64>,
}

impl ProcessSample {
    pub fn capture() -> Option<Self> {
        capture_os()
    }

    pub fn delta_since(self, earlier: Self) -> Self {
        Self {
            self_user_us: self.self_user_us.saturating_sub(earlier.self_user_us),
            self_sys_us: self.self_sys_us.saturating_sub(earlier.self_sys_us),
            child_user_us: self.child_user_us.saturating_sub(earlier.child_user_us),
            child_sys_us: self.child_sys_us.saturating_sub(earlier.child_sys_us),
            peak_rss_bytes: self.peak_rss_bytes,
        }
    }

    /// How far the process's peak resident size rose after `earlier`, where both samples hold one.
    pub fn peak_rss_growth_since(self, earlier: Self) -> Option<u64> {
        Some(self.peak_rss_bytes?.saturating_sub(earlier.peak_rss_bytes?))
    }

    pub fn total_cpu_us(self) -> u64 {
        self.self_user_us
            .saturating_add(self.self_sys_us)
            .saturating_add(self.child_user_us)
            .saturating_add(self.child_sys_us)
    }
}

#[cfg(target_os = "linux")]
fn capture_os() -> Option<ProcessSample> {
    let stat = fs::read_to_string("/proc/self/stat").ok()?;
    let status = fs::read_to_string("/proc/self/status").ok();
    parse_proc_sample(&stat, status.as_deref())
}

/// Reads the CPU time of this process and of the children it waited for, and this process's peak resident size.
#[cfg(target_os = "macos")]
fn capture_os() -> Option<ProcessSample> {
    use nix::sys::resource::{UsageWho, getrusage};
    use nix::sys::time::TimeValLike;

    let micros = |time: nix::sys::time::TimeVal| u64::try_from(time.num_microseconds()).ok();
    let own = getrusage(UsageWho::RUSAGE_SELF).ok()?;
    let children = getrusage(UsageWho::RUSAGE_CHILDREN).ok()?;
    Some(ProcessSample {
        self_user_us: micros(own.user_time())?,
        self_sys_us: micros(own.system_time())?,
        child_user_us: micros(children.user_time())?,
        child_sys_us: micros(children.system_time())?,
        // On macOS the peak resident size is in bytes.
        peak_rss_bytes: u64::try_from(own.max_rss()).ok(),
    })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn capture_os() -> Option<ProcessSample> {
    None
}

#[cfg(any(target_os = "linux", test))]
fn parse_proc_sample(stat: &str, status: Option<&str>) -> Option<ProcessSample> {
    // Linux `/proc/[pid]/stat` reports jiffies in `USER_HZ` (100 Hz = 10,000 microseconds per tick).
    const TICK_US: u64 = 10_000;
    let after_comm = stat.rfind(')')?.checked_add(1)?;
    let fields: Vec<&str> = stat.get(after_comm..)?.split_whitespace().collect();
    let utime = fields.get(11)?.parse::<u64>().ok()?.saturating_mul(TICK_US);
    let stime = fields.get(12)?.parse::<u64>().ok()?.saturating_mul(TICK_US);
    let cutime = fields.get(13)?.parse::<u64>().ok()?.saturating_mul(TICK_US);
    let cstime = fields.get(14)?.parse::<u64>().ok()?.saturating_mul(TICK_US);
    // Only `VmHWM` is the peak. A status that lacks it gives no peak, never the current size in its place.
    let peak_rss_bytes = status
        .and_then(|s| parse_status_kib(s, "VmHWM:"))
        .and_then(|kib| kib.checked_mul(1024));
    Some(ProcessSample {
        self_user_us: utime,
        self_sys_us: stime,
        child_user_us: cutime,
        child_sys_us: cstime,
        peak_rss_bytes,
    })
}

#[cfg(any(target_os = "linux", test))]
fn parse_status_kib(status: &str, prefix: &str) -> Option<u64> {
    status.lines().find_map(|line| {
        line.strip_prefix(prefix)?
            .split_whitespace()
            .next()?
            .parse::<u64>()
            .ok()
    })
}

/// Host, filesystem, Git revision, and Rust toolchain metadata recorded in benchmark reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentInfo {
    pub git_revision: String,
    pub git_dirty: bool,
    pub os: String,
    pub arch: String,
    pub logical_cpus: usize,
    /// The type of the filesystem that holds the measured root, or `None` where the system named none.
    pub filesystem: Option<String>,
    pub filesystem_block_size: Option<u64>,
    pub rustc_version: String,
    /// Whether the benchmark binary carries debug assertions, which an unoptimized build does.
    pub debug_assertions: bool,
    /// The limit on this process's open files, or `None` where the limit is infinite or unread.
    pub open_files_limit: Option<u64>,
    pub observed_at: String,
}

impl EnvironmentInfo {
    /// Reads the revision from `repo_root` and the filesystem from `measured_root`, the tree that the cases read.
    pub fn collect(repo_root: &Path, measured_root: &Path) -> Self {
        let git_revision = run_git_trimmed(repo_root, &["rev-parse", "HEAD"])
            .unwrap_or_else(|| "unknown".to_owned());
        let git_dirty =
            run_git_trimmed(repo_root, &["status", "--porcelain"]).is_some_and(|s| !s.is_empty());
        let logical_cpus = std::thread::available_parallelism().map_or(1, usize::from);
        let filesystem = filesystem_type(measured_root);
        let filesystem_block_size = statvfs(measured_root).ok().map(|st| st.f_frsize);
        let open_files_limit =
            rustix::process::getrlimit(rustix::process::Resource::Nofile).current;
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
        Self {
            git_revision,
            git_dirty,
            os: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
            logical_cpus,
            filesystem,
            filesystem_block_size,
            rustc_version: env!("CARGO_PKG_RUST_VERSION").to_owned(),
            debug_assertions: cfg!(debug_assertions),
            open_files_limit,
            observed_at: timestamp::format_utc(now_secs),
        }
    }
}

/// Names the type of the filesystem that holds `path`: its name on macOS, and its magic number on Linux. The
/// reading comes from the filesystem itself, so Suds runs no program for it.
fn filesystem_type(path: &Path) -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let stat = rustix::fs::statfs(path).ok()?;
        let name: Vec<u8> = stat
            .f_fstypename
            .iter()
            .take_while(|byte| **byte != 0)
            .map(|byte| byte.to_ne_bytes()[0])
            .collect();
        String::from_utf8(name).ok().filter(|name| !name.is_empty())
    }
    #[cfg(target_os = "linux")]
    {
        let stat = rustix::fs::statfs(path).ok()?;
        Some(format!("{:#x}", stat.f_type))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = path;
        None
    }
}

fn run_git_trimmed(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Domain metrics returned by an individual Suds benchmark step.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepOutcome {
    pub entries_visited: u64,
    pub allocated_bytes: u64,
    pub reclaimed_bytes: u64,
    pub cursor_bytes: u64,
    pub slices: u64,
    /// Whether the step measured all of its work. A step that states nothing is incomplete.
    pub complete: bool,
}

/// Measurements captured for a single timed benchmark sample.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SampleReport {
    pub wall_us: u64,
    pub cpu_us: Option<u64>,
    /// The process's peak resident size after the sample, which earlier cases also raised.
    pub process_peak_rss_bytes: Option<u64>,
    /// How far this sample raised the process's peak resident size: the sample's own share of it.
    pub peak_rss_growth_bytes: Option<u64>,
    pub counts: Counts,
    pub outcome: StepOutcome,
}

/// Aggregated measurement of one Suds benchmark case.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaseReport {
    pub name: String,
    pub parameters: BTreeMap<String, String>,
    pub samples: usize,
    pub completed_samples: usize,
    pub incomplete_samples: usize,
    pub complete: bool,
    pub min_us: u64,
    pub median_us: u64,
    pub p95_us: u64,
    pub max_us: u64,
    pub median_cpu_us: Option<u64>,
    pub entries_per_sec: u64,
    /// The process's peak resident size after the case's last sample, which earlier cases also raised.
    pub process_peak_rss_bytes: Option<u64>,
    /// The largest rise of the process's peak resident size in one sample of this case.
    pub peak_rss_growth_bytes: Option<u64>,
    pub counts: Counts,
    pub outcome: StepOutcome,
    pub sample_records: Vec<SampleReport>,
}

/// Machine-readable report produced by a Suds benchmark suite run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BenchReport {
    pub schema: String,
    pub suite: String,
    pub environment: EnvironmentInfo,
    pub cases: Vec<CaseReport>,
}

/// Runs `samples` timed measurements and aggregates medians, throughput, and operation counts.
pub fn measure_case<S>(
    name: impl Into<String>,
    parameters: BTreeMap<String, String>,
    samples: usize,
    mut setup: impl FnMut() -> S,
    mut step: impl FnMut(&mut S) -> StepOutcome,
) -> CaseReport {
    let runs = samples.max(1);
    let mut elapsed = Vec::with_capacity(runs);
    let mut cpus = Vec::with_capacity(runs);
    let mut process_peak: Option<u64> = None;
    let mut peak_growth: Option<u64> = None;
    let mut sample_records = Vec::with_capacity(runs);
    for _ in 0..runs {
        let mut state = setup();
        let before_counts = Counts::now();
        let before_proc = ProcessSample::capture();
        let started = Instant::now();
        let outcome = step(&mut state);
        let took_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
        let after_proc = ProcessSample::capture();
        let after_counts = Counts::now();
        elapsed.push(took_us);
        let cpu_us = match (before_proc, after_proc) {
            (Some(before), Some(after)) => Some(after.delta_since(before).total_cpu_us()),
            _ => None,
        };
        cpus.extend(cpu_us);
        let (peak, growth) = sample_memory(before_proc, after_proc);
        // The case's peak is the one after its last sample, so a sample that read none leaves none.
        process_peak = peak;
        peak_growth = peak_growth.max(growth);
        let counts = after_counts.since(before_counts);
        sample_records.push(SampleReport {
            wall_us: took_us,
            cpu_us,
            process_peak_rss_bytes: peak,
            peak_rss_growth_bytes: growth,
            counts,
            outcome,
        });
    }
    elapsed.sort_unstable();
    cpus.sort_unstable();
    let mid = elapsed.len() / 2;
    let p95 = nearest_rank_index(elapsed.len(), 95);
    let median_us = elapsed[mid];
    let median_cpu_us = (!cpus.is_empty()).then(|| cpus[cpus.len() / 2]);
    let completed_samples = sample_records.iter().filter(|s| s.outcome.complete).count();
    let incomplete_samples = runs.saturating_sub(completed_samples);
    let counts = Counts {
        bulk_dir_calls: median_u64(sample_records.iter().map(|s| s.counts.bulk_dir_calls)),
        entry_probe_calls: median_u64(sample_records.iter().map(|s| s.counts.entry_probe_calls)),
        helper_launches: median_u64(sample_records.iter().map(|s| s.counts.helper_launches)),
    };
    let outcome = StepOutcome {
        entries_visited: median_u64(sample_records.iter().map(|s| s.outcome.entries_visited)),
        allocated_bytes: median_u64(sample_records.iter().map(|s| s.outcome.allocated_bytes)),
        reclaimed_bytes: median_u64(sample_records.iter().map(|s| s.outcome.reclaimed_bytes)),
        cursor_bytes: median_u64(sample_records.iter().map(|s| s.outcome.cursor_bytes)),
        slices: median_u64(sample_records.iter().map(|s| s.outcome.slices)),
        complete: incomplete_samples == 0,
    };
    let entries_per_sec = if median_us > 0 {
        outcome
            .entries_visited
            .saturating_mul(1_000_000)
            .checked_div(median_us)
            .unwrap_or(0)
    } else {
        0
    };
    CaseReport {
        name: name.into(),
        parameters,
        samples: runs,
        completed_samples,
        incomplete_samples,
        complete: incomplete_samples == 0,
        min_us: elapsed[0],
        median_us,
        p95_us: elapsed[p95],
        max_us: *elapsed.last().unwrap_or(&0),
        median_cpu_us,
        entries_per_sec,
        process_peak_rss_bytes: process_peak,
        peak_rss_growth_bytes: peak_growth,
        counts,
        outcome,
        sample_records,
    }
}

/// The process's peak resident size after a sample, and how far the sample raised it. The peak needs the reading
/// after the sample alone, and the rise needs both readings.
fn sample_memory(
    before: Option<ProcessSample>,
    after: Option<ProcessSample>,
) -> (Option<u64>, Option<u64>) {
    let peak = after.and_then(|after| after.peak_rss_bytes);
    let growth = match (before, after) {
        (Some(before), Some(after)) => after.peak_rss_growth_since(before),
        _ => None,
    };
    (peak, growth)
}

fn median_u64(values: impl Iterator<Item = u64>) -> u64 {
    let mut sorted: Vec<u64> = values.collect();
    if sorted.is_empty() {
        return 0;
    }
    sorted.sort_unstable();
    sorted[sorted.len() / 2]
}

fn nearest_rank_index(len: usize, pct: usize) -> usize {
    if len == 0 {
        return 0;
    }
    len.saturating_mul(pct)
        .div_ceil(100)
        .saturating_sub(1)
        .min(len - 1)
}

/// Multi-scale fixture profile modeling Cargo output, hard-linked stores, clones, and claims.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScaleProfile {
    pub name: String,
    pub repositories: usize,
    pub worktrees: usize,
    pub target_files_per_checkout: usize,
    pub hardlinked_packages: usize,
}

impl ScaleProfile {
    /// Each dimension of the profile as a case's parameters, so two reports of different fixtures do not compare.
    pub fn parameters(&self) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("profile".to_owned(), self.name.clone()),
            ("repositories".to_owned(), self.repositories.to_string()),
            ("worktrees".to_owned(), self.worktrees.to_string()),
            (
                "target_files_per_checkout".to_owned(),
                self.target_files_per_checkout.to_string(),
            ),
            (
                "hardlinked_packages".to_owned(),
                self.hardlinked_packages.to_string(),
            ),
        ])
    }

    pub fn small() -> Self {
        Self {
            name: "small".to_owned(),
            repositories: 3,
            worktrees: 9,
            target_files_per_checkout: 24,
            hardlinked_packages: 16,
        }
    }

    pub fn medium() -> Self {
        Self {
            name: "medium".to_owned(),
            repositories: 6,
            worktrees: 24,
            target_files_per_checkout: 48,
            hardlinked_packages: 32,
        }
    }

    pub fn large() -> Self {
        Self {
            name: "large".to_owned(),
            repositories: 10,
            worktrees: 60,
            target_files_per_checkout: 80,
            hardlinked_packages: 64,
        }
    }
}

/// Generated development workspace with Git repositories, linked worktrees, Cargo build state,
/// hard-linked pnpm packages, APFS-seeded clones, and active/ended claims.
pub struct BenchWorkspace {
    base: PathBuf,
    pub workspace_root: PathBuf,
    pub store_root: PathBuf,
    pub state_dir: PathBuf,
    pub config_dir: PathBuf,
    pub main_repos: Vec<PathBuf>,
    pub active_worktrees: Vec<PathBuf>,
    pub eligible_worktrees: Vec<PathBuf>,
    pub unseeded_worktrees: Vec<PathBuf>,
}

impl BenchWorkspace {
    /// Generates a reproducible multi-repository workspace from `profile`.
    pub fn generate(profile: &ScaleProfile) -> Result<Self, String> {
        let seq = FIXTURE_SEQ.fetch_add(1, Ordering::Relaxed);
        let raw_base = std::env::temp_dir().join(format!(
            "suds-bench-{}-{}-{seq}",
            profile.name,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&raw_base);
        fs::create_dir_all(&raw_base).map_err(|e| e.to_string())?;
        let base = raw_base.canonicalize().map_err(|e| e.to_string())?;
        let workspace_root = base.join("workspace");
        let store_root = base.join("pnpm-store/v3");
        let state_dir = base.join("state");
        let config_dir = base.join("config");
        fs::create_dir_all(&workspace_root).map_err(|e| e.to_string())?;
        fs::create_dir_all(&store_root).map_err(|e| e.to_string())?;
        fs::create_dir_all(&state_dir).map_err(|e| e.to_string())?;
        fs::create_dir_all(&config_dir).map_err(|e| e.to_string())?;
        fs::write(store_root.join("files"), "").map_err(|e| e.to_string())?;

        // Populate shared store files above the clone floor for hard-link deduplication.
        let payload = vec![b'a'; usize::try_from(FLOOR).unwrap_or(16_384) + 4_096];
        let mut store_files = Vec::with_capacity(profile.hardlinked_packages);
        let store_pkg_dir = store_root.join("pkg-blobs");
        fs::create_dir_all(&store_pkg_dir).map_err(|e| e.to_string())?;
        for idx in 0..profile.hardlinked_packages {
            let blob = store_pkg_dir.join(format!("pkg-{idx:04}.js"));
            fs::write(&blob, &payload).map_err(|e| e.to_string())?;
            store_files.push(blob);
        }

        let repos_count = profile.repositories.max(1);
        let wts_per_repo = (profile.worktrees / repos_count).max(1);
        let registry_path = state_dir.join("registry.jsonl");
        let mut journal = crate::organize::journal::Journal::open(&state_dir.join("journal"))
            .map_err(|e| e.to_string())?;
        let git = Git::default();

        let mut main_repos = Vec::with_capacity(repos_count);
        let mut active_worktrees = Vec::new();
        let mut eligible_worktrees = Vec::new();
        let mut unseeded_worktrees = Vec::new();

        for r in 0..repos_count {
            let remote = base.join(format!("remote-{r:02}.git"));
            run_git(
                &base,
                [
                    OsStr::new("init"),
                    OsStr::new("-q"),
                    OsStr::new("--bare"),
                    remote.as_os_str(),
                ],
            )?;
            let repo = workspace_root.join(format!("repo-{r:02}"));
            fs::create_dir_all(repo.join("src")).map_err(|e| e.to_string())?;
            run_git(&repo, ["init", "-q", "-b", "main"])?;
            run_git(&repo, ["config", "user.name", "Bench"])?;
            run_git(&repo, ["config", "user.email", "bench@example.com"])?;
            run_git(&repo, ["config", "commit.gpgsign", "false"])?;
            fs::write(
                repo.join("Cargo.toml"),
                format!("[package]\nname = \"repo-{r:02}\"\nversion = \"0.1.0\"\n"),
            )
            .map_err(|e| e.to_string())?;
            fs::write(repo.join("Cargo.lock"), "# lockfile v4\n").map_err(|e| e.to_string())?;
            fs::write(
                repo.join(".gitignore"),
                "target/\nnode_modules/\n.claude/\n",
            )
            .map_err(|e| e.to_string())?;
            fs::write(repo.join("src/lib.rs"), "pub fn item() -> u64 { 1 }\n")
                .map_err(|e| e.to_string())?;
            run_git(&repo, ["add", "."])?;
            run_git(&repo, ["commit", "-q", "-m", "chore: initial"])?;
            run_git(
                &repo,
                [
                    OsStr::new("remote"),
                    OsStr::new("add"),
                    OsStr::new("origin"),
                    remote.as_os_str(),
                ],
            )?;
            run_git(&repo, ["push", "-q", "origin", "main"])?;

            populate_cargo_target(&repo, profile.target_files_per_checkout, &payload)?;
            populate_hardlinked_node_modules(&repo, &store_files)?;

            for w in 0..wts_per_repo {
                let branch = format!("feat/wt-{w:03}");
                let wt = repo.join(format!(".claude/worktrees/wt-{w:03}"));
                if let Some(parent) = wt.parent() {
                    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                run_git(
                    &repo,
                    [
                        OsStr::new("worktree"),
                        OsStr::new("add"),
                        OsStr::new("-q"),
                        OsStr::new("-b"),
                        OsStr::new(&branch),
                        wt.as_os_str(),
                    ],
                )?;
                run_git(&wt, ["push", "-q", "origin", &branch])?;
                populate_hardlinked_node_modules(&wt, &store_files)?;

                match w % 3 {
                    0 => {
                        // Active claimed checkout with APFS-cloned (or populated) build state.
                        seed_or_populate(
                            &mut journal,
                            &repo,
                            &wt,
                            profile.target_files_per_checkout,
                            &payload,
                            &git,
                        )?;
                        registry::register(
                            &registry_path,
                            &wt,
                            "bosun",
                            Some(&format!("wf-active-{r}-{w}")),
                            None,
                            1_000,
                        )?;
                        active_worktrees.push(wt);
                    }
                    1 => {
                        // Ended claim checkout whose derived build state is eligible for reclamation.
                        populate_cargo_target(&wt, profile.target_files_per_checkout, &payload)?;
                        registry::append_ended(
                            &registry_path,
                            &wt,
                            "bosun",
                            Some(&format!("wf-ended-{r}-{w}")),
                            None,
                            1_000,
                        )?;
                        eligible_worktrees.push(wt);
                    }
                    _ => {
                        // Fresh unseeded checkout ready for seed benchmarking.
                        registry::append_ended(
                            &registry_path,
                            &wt,
                            "bosun",
                            Some(&format!("wf-fresh-{r}-{w}")),
                            None,
                            1_000,
                        )?;
                        unseeded_worktrees.push(wt);
                    }
                }
            }
            main_repos.push(repo);
        }

        Ok(Self {
            base,
            workspace_root,
            store_root,
            state_dir,
            config_dir,
            main_repos,
            active_worktrees,
            eligible_worktrees,
            unseeded_worktrees,
        })
    }
}

impl Drop for BenchWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

fn populate_cargo_target(checkout: &Path, files: usize, payload: &[u8]) -> Result<(), String> {
    let deps = checkout.join("target/debug/deps");
    fs::create_dir_all(&deps).map_err(|e| e.to_string())?;
    fs::write(
        checkout.join("target/CACHEDIR.TAG"),
        "Signature: 8a477f597d28d172789f06886806bc55\n",
    )
    .map_err(|e| e.to_string())?;
    for idx in 0..files {
        fs::write(deps.join(format!("libdep_{idx:04}.rlib")), payload)
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn populate_hardlinked_node_modules(
    checkout: &Path,
    store_files: &[PathBuf],
) -> Result<(), String> {
    if store_files.is_empty() {
        return Ok(());
    }
    let nm = checkout.join("node_modules/.pnpm/pkg/node_modules/pkg");
    fs::create_dir_all(&nm).map_err(|e| e.to_string())?;
    fs::write(
        checkout.join("node_modules/.modules.yaml"),
        "storeDir: pnpm\n",
    )
    .map_err(|e| e.to_string())?;
    for (idx, src) in store_files.iter().enumerate() {
        let dst = nm.join(format!("mod-{idx:04}.js"));
        fs::hard_link(src, dst).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn seed_or_populate(
    journal: &mut crate::organize::journal::Journal,
    source: &Path,
    dest: &Path,
    files: usize,
    payload: &[u8],
    git: &Git,
) -> Result<(), String> {
    let plan = seed::plan_seed(dest, Some(source), git)?;
    let report = seed::apply(journal, &plan, Some("bench")).map_err(|e| e.to_string())?;
    if !report.clones.iter().any(|c| c.cloned) {
        populate_cargo_target(dest, files, payload)?;
    }
    Ok(())
}

fn run_git<I, S>(dir: &Path, args: I) -> Result<(), String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let arg_vec: Vec<OsString> = args
        .into_iter()
        .map(|a| a.as_ref().to_os_string())
        .collect();
    let out = Command::new("git")
        .current_dir(dir)
        .args(&arg_vec)
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!(
            "git {arg_vec:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(())
}

/// Per-case comparison between a baseline report and a candidate report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaseDelta {
    pub name: String,
    pub baseline_median_us: u64,
    pub candidate_median_us: u64,
    pub median_change_pct: f64,
    pub baseline_entries_per_sec: u64,
    pub candidate_entries_per_sec: u64,
    pub bulk_dir_calls_delta: i64,
    pub entry_probe_calls_delta: i64,
    pub helper_launches_delta: i64,
    pub parameter_mismatches: Vec<String>,
    pub incomplete: bool,
}

/// Structured comparison of two Suds benchmark reports.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComparisonReport {
    pub suite: String,
    pub baseline_revision: String,
    pub candidate_revision: String,
    pub compatible: bool,
    /// Whether both reports are of this suite's schema. Where one is not, no option compares them, and the
    /// comparison holds no case.
    pub schemas_match: bool,
    pub incompatibilities: Vec<String>,
    pub cases: Vec<CaseDelta>,
}

impl ComparisonReport {
    /// The refusal of a comparison that the reports do not support, which names each reason, or none where they
    /// compare or `allow_incompatible` holds. A caller prints it in place of the deltas.
    pub fn refusal(&self, allow_incompatible: bool) -> Option<String> {
        if self.schemas_match && (self.compatible || allow_incompatible) {
            return None;
        }
        let mut text = if self.schemas_match {
            "the reports do not compare, and `--allow-incompatible` compares them anyway:"
                .to_owned()
        } else {
            "the reports are not both of this suite's schema, and no option compares them:"
                .to_owned()
        };
        for reason in &self.incompatibilities {
            text.push_str("\n  - ");
            text.push_str(reason);
        }
        Some(text)
    }
}

/// Reads the report of `side` from `text`. A report of another schema lacks fields that this suite compares, so its
/// schema answers before any other field, and no option compares it.
pub fn read_report(side: &str, text: &str) -> Result<BenchReport, String> {
    // A typed reading refuses a field that the text names twice, which a reading into a generic value would not.
    #[derive(Deserialize)]
    struct Header {
        schema: String,
    }
    let header: Header = serde_json::from_str(text)
        .map_err(|err| format!("{side} holds no report with one schema: {err}"))?;
    if header.schema != BENCHMARK_SCHEMA {
        return Err(format!(
            "{side} has schema {}, and this suite compares {BENCHMARK_SCHEMA}",
            header.schema
        ));
    }
    serde_json::from_str(text)
        .map_err(|err| format!("{side} holds no report that Suds reads: {err}"))
}

/// Reads the two reports of a comparison from their files, one after the other, so a baseline that the suite
/// refuses opens no candidate.
pub fn read_reports(
    baseline: &Path,
    candidate: &Path,
) -> Result<(BenchReport, BenchReport), String> {
    read_reports_with(baseline, candidate, |path| {
        fs::read_to_string(path).map_err(|err| format!("{}: {err}", path.display()))
    })
}

/// Like [`read_reports`], taking each file's text from `text`, which a test watches.
fn read_reports_with(
    baseline: &Path,
    candidate: &Path,
    mut text: impl FnMut(&Path) -> Result<String, String>,
) -> Result<(BenchReport, BenchReport), String> {
    let base = read_report("baseline", &text(baseline)?)?;
    let cand = read_report("candidate", &text(candidate)?)?;
    Ok((base, cand))
}

/// The form of a comparison's output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComparisonFormat {
    Text,
    Json,
}

/// Writes the comparison of two reports to `out`. A refusal comes before any output, so a reader of `out` never
/// takes a delta from reports that do not compare.
pub fn write_comparison(
    out: &mut impl std::io::Write,
    baseline: &BenchReport,
    candidate: &BenchReport,
    format: ComparisonFormat,
    allow_incompatible: bool,
) -> Result<(), String> {
    let comp = compare_reports(baseline, candidate);
    if let Some(refusal) = comp.refusal(allow_incompatible) {
        return Err(refusal);
    }
    let text = match format {
        ComparisonFormat::Json => {
            let body = serde_json::to_string_pretty(&comp).map_err(|err| err.to_string())?;
            format!("{body}\n")
        }
        ComparisonFormat::Text => format_comparison(&comp),
    };
    out.write_all(text.as_bytes())
        .map_err(|err| err.to_string())
}

/// Compares matching benchmark cases between `baseline` and `candidate`, checking environment and workload compatibility.
pub fn compare_reports(baseline: &BenchReport, candidate: &BenchReport) -> ComparisonReport {
    let mut incompatibilities = Vec::new();
    let mut schemas_match = true;
    // A report of another schema names its fields differently, so two such reports that agree still do not compare.
    for (side, report) in [("baseline", baseline), ("candidate", candidate)] {
        if report.schema != BENCHMARK_SCHEMA {
            schemas_match = false;
            incompatibilities.push(format!(
                "{side} has schema {}, and this suite compares {BENCHMARK_SCHEMA}",
                report.schema
            ));
        }
    }
    // The cases of a report of another schema mean something else, so the comparison reads none of them.
    if !schemas_match {
        return ComparisonReport {
            suite: candidate.suite.clone(),
            baseline_revision: baseline.environment.git_revision.clone(),
            candidate_revision: candidate.environment.git_revision.clone(),
            compatible: false,
            schemas_match,
            incompatibilities,
            cases: Vec::new(),
        };
    }
    for (side, report) in [("baseline", baseline), ("candidate", candidate)] {
        let mut seen = std::collections::BTreeSet::new();
        for case in &report.cases {
            if !seen.insert(case.name.as_str()) {
                incompatibilities.push(format!("{side} names case {} twice", case.name));
            }
        }
    }
    for case in &baseline.cases {
        if !candidate.cases.iter().any(|c| c.name == case.name) {
            incompatibilities.push(format!("candidate lacks case {}", case.name));
        }
    }
    for case in &candidate.cases {
        if !baseline.cases.iter().any(|c| c.name == case.name) {
            incompatibilities.push(format!("baseline lacks case {}", case.name));
        }
    }
    // A case's memory is its rise above the peak that earlier cases left, so the same work reports a smaller
    // rise, or none, after a case with a higher peak.
    let order = |report: &BenchReport| -> Vec<String> {
        report.cases.iter().map(|c| c.name.clone()).collect()
    };
    let (mut base_names, mut cand_names) = (order(baseline), order(candidate));
    if base_names != cand_names {
        base_names.sort();
        cand_names.sort();
        if base_names == cand_names {
            incompatibilities.push("the reports run their cases in a different order".to_owned());
        }
    }
    if baseline.suite != candidate.suite {
        incompatibilities.push(format!(
            "suite mismatch: {} vs {}",
            baseline.suite, candidate.suite
        ));
    }
    if baseline.environment.os != candidate.environment.os {
        incompatibilities.push(format!(
            "os mismatch: {} vs {}",
            baseline.environment.os, candidate.environment.os
        ));
    }
    if baseline.environment.arch != candidate.environment.arch {
        incompatibilities.push(format!(
            "arch mismatch: {} vs {}",
            baseline.environment.arch, candidate.environment.arch
        ));
    }
    // An unread filesystem equals no other, so two reports that both lack one do not compare either.
    match (
        &baseline.environment.filesystem,
        &candidate.environment.filesystem,
    ) {
        (Some(base), Some(cand)) if base == cand => {}
        (base, cand) => incompatibilities.push(format!(
            "filesystem mismatch: {} vs {}",
            base.as_deref().unwrap_or("<unread>"),
            cand.as_deref().unwrap_or("<unread>")
        )),
    }
    if baseline.environment.filesystem_block_size != candidate.environment.filesystem_block_size {
        incompatibilities.push(format!(
            "filesystem_block_size mismatch: {:?} vs {:?}",
            baseline.environment.filesystem_block_size, candidate.environment.filesystem_block_size
        ));
    }
    if baseline.environment.debug_assertions != candidate.environment.debug_assertions {
        incompatibilities.push(format!(
            "debug_assertions mismatch: {} vs {}",
            baseline.environment.debug_assertions, candidate.environment.debug_assertions
        ));
    }
    if baseline.environment.open_files_limit != candidate.environment.open_files_limit {
        incompatibilities.push(format!(
            "open_files_limit mismatch: {:?} vs {:?}",
            baseline.environment.open_files_limit, candidate.environment.open_files_limit
        ));
    }
    if baseline.environment.logical_cpus != candidate.environment.logical_cpus {
        incompatibilities.push(format!(
            "logical_cpus mismatch: {} vs {}",
            baseline.environment.logical_cpus, candidate.environment.logical_cpus
        ));
    }

    let base_map: BTreeMap<&str, &CaseReport> = baseline
        .cases
        .iter()
        .map(|c| (c.name.as_str(), c))
        .collect();
    let mut cases = Vec::new();
    for cand in &candidate.cases {
        let Some(base) = base_map.get(cand.name.as_str()) else {
            continue;
        };
        let mut parameter_mismatches = Vec::new();
        for (key, bval) in &base.parameters {
            match cand.parameters.get(key) {
                Some(cval) if cval == bval => {}
                Some(cval) => parameter_mismatches.push(format!("{key}: {bval} vs {cval}")),
                None => parameter_mismatches.push(format!("{key}: {bval} vs <missing>")),
            }
        }
        for (key, cval) in &cand.parameters {
            if !base.parameters.contains_key(key) {
                parameter_mismatches.push(format!("{key}: <missing> vs {cval}"));
            }
        }
        if !parameter_mismatches.is_empty() {
            incompatibilities.push(format!(
                "case {} parameter mismatch ({})",
                cand.name,
                parameter_mismatches.join(", ")
            ));
        }
        let incomplete = !base.complete || !cand.complete;
        if incomplete {
            incompatibilities.push(format!(
                "case {} has incomplete samples (baseline complete={}, candidate complete={})",
                cand.name, base.complete, cand.complete
            ));
        }
        cases.push(CaseDelta {
            name: cand.name.clone(),
            baseline_median_us: base.median_us,
            candidate_median_us: cand.median_us,
            median_change_pct: pct_change(base.median_us, cand.median_us),
            baseline_entries_per_sec: base.entries_per_sec,
            candidate_entries_per_sec: cand.entries_per_sec,
            bulk_dir_calls_delta: signed_delta(
                base.counts.bulk_dir_calls,
                cand.counts.bulk_dir_calls,
            ),
            entry_probe_calls_delta: signed_delta(
                base.counts.entry_probe_calls,
                cand.counts.entry_probe_calls,
            ),
            helper_launches_delta: signed_delta(
                base.counts.helper_launches,
                cand.counts.helper_launches,
            ),
            parameter_mismatches,
            incomplete,
        });
    }
    ComparisonReport {
        suite: candidate.suite.clone(),
        baseline_revision: baseline.environment.git_revision.clone(),
        candidate_revision: candidate.environment.git_revision.clone(),
        compatible: incompatibilities.is_empty(),
        schemas_match,
        incompatibilities,
        cases,
    }
}

/// Formats a human-readable comparison table across matching Suds benchmark cases.
pub fn format_comparison(comp: &ComparisonReport) -> String {
    let mut out = format!(
        "Suite: {} ({} vs {})\n",
        comp.suite,
        short_rev(&comp.baseline_revision),
        short_rev(&comp.candidate_revision),
    );
    if !comp.compatible {
        out.push_str("WARNING: reports are not directly comparable:\n");
        for issue in &comp.incompatibilities {
            out.push_str(&format!("  - {issue}\n"));
        }
    }
    out.push_str(&format!(
        "{:<42} {:>11} {:>11} {:>9} {:>9} {:>9} {:>8}\n",
        "CASE", "BASE (us)", "CAND (us)", "TIME %", "dBULK", "dPROBE", "dGIT",
    ));
    for c in &comp.cases {
        let mut flags = Vec::new();
        if c.incomplete {
            flags.push("INCOMPLETE");
        }
        if !c.parameter_mismatches.is_empty() {
            flags.push("PARAM_MISMATCH");
        }
        let suffix = if flags.is_empty() {
            String::new()
        } else {
            format!(" [{}]", flags.join(","))
        };
        out.push_str(&format!(
            "{:<42} {:>11} {:>11} {:>+8.1}% {:>+9} {:>+9} {:>+8}{suffix}\n",
            c.name,
            c.baseline_median_us,
            c.candidate_median_us,
            c.median_change_pct,
            c.bulk_dir_calls_delta,
            c.entry_probe_calls_delta,
            c.helper_launches_delta,
        ));
    }
    out
}

fn short_rev(rev: &str) -> &str {
    rev.get(..10.min(rev.len())).unwrap_or(rev)
}

fn pct_change(base: u64, cand: u64) -> f64 {
    if base == 0 {
        return 0.0;
    }
    ((cand as f64) - (base as f64)) * 100.0 / (base as f64)
}

fn signed_delta(base: u64, cand: u64) -> i64 {
    i128::from(cand)
        .saturating_sub(i128::from(base))
        .clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;

    use super::{
        BENCHMARK_SCHEMA, BenchReport, CaseReport, EnvironmentInfo, ProcessSample, ScaleProfile,
        StepOutcome, compare_reports, measure_case, nearest_rank_index, parse_proc_sample,
        read_report, sample_memory,
    };

    #[test]
    fn nearest_rank_selects_upper_tail_percentile() {
        assert_eq!(nearest_rank_index(1, 95), 0);
        assert_eq!(nearest_rank_index(2, 95), 1);
        assert_eq!(nearest_rank_index(3, 95), 2);
        assert_eq!(nearest_rank_index(20, 95), 18);
        assert_eq!(nearest_rank_index(100, 95), 94);
    }

    #[test]
    fn proc_sample_parses_ticks_and_high_water_mark() {
        let stat = "42 (suds bench) R 1 42 42 0 -1 4194304 10 20 0 0 15 6 4 2 20 0 1 0 100 0 0";
        let status = "Name:\tsuds\nVmHWM:\t   2048 kB\nVmRSS:\t   1024 kB\n";
        assert_eq!(
            parse_proc_sample(stat, Some(status)),
            Some(ProcessSample {
                self_user_us: 150_000,
                self_sys_us: 60_000,
                child_user_us: 40_000,
                child_sys_us: 20_000,
                peak_rss_bytes: Some(2_097_152),
            })
        );
    }

    #[test]
    fn a_status_without_a_peak_gives_no_peak_and_never_zero_or_the_current_size() {
        let stat = "42 (suds bench) R 1 42 42 0 -1 4194304 10 20 0 0 15 6 4 2 20 0 1 0 100 0 0";
        let current_only = "Name:\tsuds\nVmRSS:\t   1024 kB\n";
        assert_eq!(
            parse_proc_sample(stat, Some(current_only)).map(|sample| sample.peak_rss_bytes),
            Some(None)
        );
        assert_eq!(
            parse_proc_sample(stat, None).map(|sample| sample.peak_rss_bytes),
            Some(None)
        );
    }

    #[test]
    fn a_case_s_memory_is_the_growth_of_the_peak_and_not_the_process_s_peak() {
        let sample = |peak: Option<u64>| ProcessSample {
            peak_rss_bytes: peak,
            ..ProcessSample::default()
        };
        // A case that runs after a larger one leaves the process's peak where it was, so its own share is zero.
        assert_eq!(
            sample(Some(900)).peak_rss_growth_since(sample(Some(900))),
            Some(0)
        );
        assert_eq!(
            sample(Some(900)).peak_rss_growth_since(sample(Some(600))),
            Some(300)
        );
        assert_eq!(sample(None).peak_rss_growth_since(sample(Some(600))), None);
        assert_eq!(sample(Some(900)).peak_rss_growth_since(sample(None)), None);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn this_system_gives_a_process_sample_with_a_peak_and_a_filesystem_type() {
        let sample = ProcessSample::capture().expect("a process sample on this system");
        assert!(sample.peak_rss_bytes.is_some_and(|bytes| bytes > 0));
        let directory = std::env::temp_dir();
        assert!(super::filesystem_type(&directory).is_some());
    }

    #[test]
    fn measure_case_retains_samples_and_tracks_incomplete_runs() {
        let mut iter = 0_u64;
        let report = measure_case(
            "test/partial",
            BTreeMap::new(),
            3,
            || {
                iter += 1;
                iter
            },
            |n| StepOutcome {
                entries_visited: *n * 100,
                complete: *n != 2,
                ..StepOutcome::default()
            },
        );
        assert_eq!(report.sample_records.len(), 3);
        assert_eq!(report.completed_samples, 2);
        assert_eq!(report.incomplete_samples, 1);
        assert!(!report.complete);
        assert_eq!(report.outcome.entries_visited, 200);
    }

    fn environment(filesystem: Option<&str>) -> EnvironmentInfo {
        EnvironmentInfo {
            git_revision: "aaaa1111".to_owned(),
            git_dirty: false,
            os: "linux".to_owned(),
            arch: "x86_64".to_owned(),
            logical_cpus: 8,
            filesystem: filesystem.map(str::to_owned),
            filesystem_block_size: Some(4096),
            rustc_version: "1.98".to_owned(),
            debug_assertions: false,
            open_files_limit: Some(1024),
            observed_at: "2026-11-27T00:00:00Z".to_owned(),
        }
    }

    fn case(name: &str, profile: &str) -> CaseReport {
        let mut parameters = BTreeMap::new();
        parameters.insert("profile".to_owned(), profile.to_owned());
        measure_case(
            name,
            parameters,
            1,
            || (),
            |()| StepOutcome {
                complete: true,
                ..StepOutcome::default()
            },
        )
    }

    fn report(environment: EnvironmentInfo, cases: Vec<CaseReport>) -> BenchReport {
        BenchReport {
            schema: BENCHMARK_SCHEMA.to_owned(),
            suite: "system".to_owned(),
            environment,
            cases,
        }
    }

    #[test]
    fn compare_reports_flags_filesystem_and_parameter_mismatches() {
        let base = report(
            environment(Some("ext4")),
            vec![case("system/scan", "small")],
        );
        let cand = report(
            environment(Some("tmpfs")),
            vec![case("system/scan", "large")],
        );
        let comp = compare_reports(&base, &cand);
        assert!(!comp.compatible);
        assert!(
            comp.incompatibilities
                .iter()
                .any(|m| m.contains("filesystem mismatch: ext4 vs tmpfs"))
        );
        assert!(
            comp.incompatibilities
                .iter()
                .any(|m| m.contains("parameter mismatch"))
        );
    }

    #[test]
    fn compare_reports_accepts_two_reports_of_one_environment_and_workload() {
        let one = || {
            report(
                environment(Some("ext4")),
                vec![case("system/scan", "small")],
            )
        };
        let comp = compare_reports(&one(), &one());
        assert_eq!(comp.incompatibilities, Vec::<String>::new());
        assert!(comp.compatible);
    }

    #[test]
    fn compare_reports_refuses_an_unread_filesystem_on_both_sides() {
        let one = || report(environment(None), vec![case("system/scan", "small")]);
        assert_eq!(
            compare_reports(&one(), &one()).incompatibilities,
            ["filesystem mismatch: <unread> vs <unread>"]
        );
    }

    #[test]
    fn compare_reports_refuses_a_case_that_one_report_lacks_or_repeats() {
        let both = || {
            report(
                environment(Some("ext4")),
                vec![
                    case("system/scan", "small"),
                    case("system/reclaim", "small"),
                ],
            )
        };
        let one = || {
            report(
                environment(Some("ext4")),
                vec![case("system/scan", "small")],
            )
        };
        assert_eq!(
            compare_reports(&both(), &one()).incompatibilities,
            ["candidate lacks case system/reclaim"]
        );
        assert_eq!(
            compare_reports(&one(), &both()).incompatibilities,
            ["baseline lacks case system/reclaim"]
        );
        let twice = report(
            environment(Some("ext4")),
            vec![case("system/scan", "small"), case("system/scan", "small")],
        );
        assert_eq!(
            compare_reports(&twice, &one()).incompatibilities,
            ["baseline names case system/scan twice"]
        );
    }

    #[test]
    fn compare_reports_refuses_a_report_of_an_earlier_schema() {
        let current = || {
            report(
                environment(Some("ext4")),
                vec![case("system/scan", "small")],
            )
        };
        let mut earlier = current();
        earlier.schema = "suds.benchmark/v1".to_owned();
        assert_eq!(
            compare_reports(&earlier, &current()).incompatibilities,
            ["baseline has schema suds.benchmark/v1, and this suite compares suds.benchmark/v2"]
        );
    }

    #[test]
    fn compare_reports_refuses_each_difference_of_the_build_and_its_limits() {
        let base = report(
            environment(Some("ext4")),
            vec![case("system/scan", "small")],
        );
        let mut debug = environment(Some("ext4"));
        debug.debug_assertions = true;
        let mut limit = environment(Some("ext4"));
        limit.open_files_limit = None;
        let mut block = environment(Some("ext4"));
        block.filesystem_block_size = None;
        for (changed, refusal) in [
            (debug, "debug_assertions mismatch: false vs true"),
            (limit, "open_files_limit mismatch: Some(1024) vs None"),
            (block, "filesystem_block_size mismatch: Some(4096) vs None"),
        ] {
            let cand = report(changed, vec![case("system/scan", "small")]);
            assert_eq!(compare_reports(&base, &cand).incompatibilities, [refusal]);
        }
    }
    #[test]
    fn a_step_that_states_no_completion_is_incomplete() {
        let silent = measure_case(
            "test/silent",
            BTreeMap::new(),
            1,
            || (),
            |()| StepOutcome::default(),
        );
        assert!(!silent.complete);
        assert_eq!(silent.incomplete_samples, 1);
    }

    #[test]
    fn a_sample_s_peak_needs_the_reading_after_it_alone() {
        let sample = |peak: u64| ProcessSample {
            peak_rss_bytes: Some(peak),
            ..ProcessSample::default()
        };
        assert_eq!(sample_memory(None, Some(sample(900))), (Some(900), None));
        assert_eq!(sample_memory(Some(sample(600)), None), (None, None));
        assert_eq!(
            sample_memory(Some(sample(600)), Some(sample(900))),
            (Some(900), Some(300))
        );
    }

    #[test]
    fn compare_reports_refuses_the_same_cases_in_another_order() {
        let forward = report(
            environment(Some("ext4")),
            vec![
                case("system/scan", "small"),
                case("system/reclaim", "small"),
            ],
        );
        let reversed = report(
            environment(Some("ext4")),
            vec![
                case("system/reclaim", "small"),
                case("system/scan", "small"),
            ],
        );
        assert_eq!(
            compare_reports(&forward, &reversed).incompatibilities,
            ["the reports run their cases in a different order"]
        );
    }

    #[test]
    fn a_refusal_names_each_reason_and_the_option_lifts_it() {
        let base = report(
            environment(Some("ext4")),
            vec![case("system/scan", "small")],
        );
        let cand = report(
            environment(Some("tmpfs")),
            vec![case("system/scan", "small")],
        );
        let comp = compare_reports(&base, &cand);
        assert_eq!(
            comp.refusal(false).as_deref(),
            Some(
                "the reports do not compare, and `--allow-incompatible` compares them anyway:\n  - filesystem mismatch: ext4 vs tmpfs"
            )
        );
        assert_eq!(comp.refusal(true), None);
        assert_eq!(compare_reports(&base, &base).refusal(false), None);
    }

    #[test]
    fn a_report_of_an_earlier_schema_is_refused_before_its_fields_are_read() {
        // The earlier schema's report holds none of the fields that this one added.
        let earlier =
            r#"{"schema":"suds.benchmark/v1","suite":"system","environment":{},"cases":[]}"#;
        assert_eq!(
            read_report("baseline", earlier).unwrap_err(),
            "baseline has schema suds.benchmark/v1, and this suite compares suds.benchmark/v2"
        );
        let current = report(
            environment(Some("ext4")),
            vec![case("system/scan", "small")],
        );
        let text = serde_json::to_string(&current).unwrap();
        assert_eq!(read_report("candidate", &text).unwrap(), current);
    }

    #[test]
    fn a_profile_s_parameters_name_each_of_its_dimensions() {
        let mut larger = ScaleProfile::small();
        larger.target_files_per_checkout *= 2;
        assert_ne!(larger.parameters(), ScaleProfile::small().parameters());
        assert_eq!(ScaleProfile::small().parameters().len(), 5);
    }
    #[test]
    fn no_option_compares_reports_of_different_schemas() {
        let current = || {
            report(
                environment(Some("ext4")),
                vec![case("system/scan", "small")],
            )
        };
        let mut earlier = current();
        earlier.schema = "suds.benchmark/v1".to_owned();
        let comp = compare_reports(&earlier, &current());
        assert!(!comp.schemas_match);
        assert!(comp.cases.is_empty());
        assert!(comp.refusal(true).is_some());
    }

    #[test]
    fn a_report_that_names_its_schema_twice_is_refused() {
        let current = report(
            environment(Some("ext4")),
            vec![case("system/scan", "small")],
        );
        let text = serde_json::to_string(&current).unwrap();
        let twice = text.replacen('{', r#"{"schema":"suds.benchmark/v1","#, 1);
        let refusal = read_report("baseline", &twice).unwrap_err();
        assert!(refusal.contains("duplicate field"), "{refusal}");
    }

    #[test]
    fn a_refused_comparison_writes_nothing() {
        use super::{ComparisonFormat, write_comparison};

        let one = |filesystem: &str| {
            report(
                environment(Some(filesystem)),
                vec![case("system/scan", "small")],
            )
        };
        for format in [ComparisonFormat::Text, ComparisonFormat::Json] {
            let mut out = Vec::new();
            let refusal =
                write_comparison(&mut out, &one("ext4"), &one("tmpfs"), format, false).unwrap_err();
            assert!(
                refusal.contains("filesystem mismatch: ext4 vs tmpfs"),
                "{refusal}"
            );
            assert!(out.is_empty());
            write_comparison(&mut out, &one("ext4"), &one("tmpfs"), format, true).unwrap();
            assert!(!out.is_empty());
        }
    }

    #[test]
    fn a_schema_mismatch_refuses_whatever_else_a_comparison_states() {
        let one = || {
            report(
                environment(Some("ext4")),
                vec![case("system/scan", "small")],
            )
        };
        let mut comp = compare_reports(&one(), &one());
        comp.schemas_match = false;
        assert!(comp.refusal(false).is_some());
        assert!(comp.refusal(true).is_some());
    }
    #[test]
    fn a_refused_baseline_opens_no_candidate() {
        let mut opened = Vec::new();
        let refusal = super::read_reports_with(
            Path::new("baseline.json"),
            Path::new("candidate.json"),
            |path| {
                opened.push(path.to_path_buf());
                Ok(r#"{"schema":"suds.benchmark/v1"}"#.to_owned())
            },
        )
        .unwrap_err();
        assert_eq!(
            refusal,
            "baseline has schema suds.benchmark/v1, and this suite compares suds.benchmark/v2"
        );
        assert_eq!(opened, [Path::new("baseline.json")]);
    }

    #[test]
    fn a_foreign_schema_is_the_only_diagnosis() {
        let mut earlier = report(
            environment(Some("tmpfs")),
            vec![case("system/other", "large")],
        );
        earlier.schema = "suds.benchmark/v1".to_owned();
        let current = report(
            environment(Some("ext4")),
            vec![case("system/scan", "small")],
        );
        assert_eq!(
            compare_reports(&earlier, &current).incompatibilities,
            ["baseline has schema suds.benchmark/v1, and this suite compares suds.benchmark/v2"]
        );
    }
}

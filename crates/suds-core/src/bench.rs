//! Safe operation counters, process resource sampling, multi-scale development
//! storage fixtures, and benchmark report comparison for Suds.

use std::collections::BTreeMap;
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
pub const BENCHMARK_SCHEMA: &str = "suds.benchmark/v1";

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

pub(crate) fn record_bulk_dir_call() {
    BULK_DIR_CALLS.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn record_entry_probe() {
    ENTRY_PROBE_CALLS.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn record_helper_launch() {
    HELPER_LAUNCHES.fetch_add(1, Ordering::Relaxed);
}

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

/// Process CPU and resident-memory figures captured through safe `rustix` `getrusage`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessSample {
    pub self_user_us: u64,
    pub self_sys_us: u64,
    pub child_user_us: u64,
    pub child_sys_us: u64,
    pub max_rss_bytes: u64,
}

impl ProcessSample {
    pub fn capture() -> Self {
        let (self_user_us, self_sys_us, max_rss_bytes) = rusage(libc::RUSAGE_SELF);
        let (child_user_us, child_sys_us, _) = rusage(libc::RUSAGE_CHILDREN);
        Self {
            self_user_us,
            self_sys_us,
            child_user_us,
            child_sys_us,
            max_rss_bytes,
        }
    }

    pub fn delta_since(self, earlier: Self) -> Self {
        Self {
            self_user_us: self.self_user_us.saturating_sub(earlier.self_user_us),
            self_sys_us: self.self_sys_us.saturating_sub(earlier.self_sys_us),
            child_user_us: self.child_user_us.saturating_sub(earlier.child_user_us),
            child_sys_us: self.child_sys_us.saturating_sub(earlier.child_sys_us),
            max_rss_bytes: self.max_rss_bytes,
        }
    }

    pub fn total_cpu_us(self) -> u64 {
        self.self_user_us
            .saturating_add(self.self_sys_us)
            .saturating_add(self.child_user_us)
            .saturating_add(self.child_sys_us)
    }
}

#[expect(
    unsafe_code,
    reason = "rustix exposes no safe getrusage wrapper in its enabled features"
)]
fn rusage(who: libc::c_int) -> (u64, u64, u64) {
    // SAFETY: `usage` starts zeroed and `getrusage` populates it for `who`.
    unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        if libc::getrusage(who, &mut usage) != 0 {
            return (0, 0, 0);
        }
        let user = timeval_us(usage.ru_utime.tv_sec, i64::from(usage.ru_utime.tv_usec));
        let sys = timeval_us(usage.ru_stime.tv_sec, i64::from(usage.ru_stime.tv_usec));
        #[cfg(target_os = "macos")]
        let rss = u64::try_from(usage.ru_maxrss).unwrap_or(0);
        #[cfg(not(target_os = "macos"))]
        let rss = u64::try_from(usage.ru_maxrss)
            .unwrap_or(0)
            .saturating_mul(1024);
        (user, sys, rss)
    }
}

fn timeval_us(secs: i64, usecs: i64) -> u64 {
    u64::try_from(secs)
        .unwrap_or(0)
        .saturating_mul(1_000_000)
        .saturating_add(u64::try_from(usecs).unwrap_or(0))
}

/// Host, filesystem, Git revision, and Rust toolchain metadata recorded in benchmark reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentInfo {
    pub git_revision: String,
    pub git_dirty: bool,
    pub os: String,
    pub arch: String,
    pub logical_cpus: usize,
    pub filesystem_block_size: u64,
    pub rustc_version: String,
    pub observed_at: String,
}

impl EnvironmentInfo {
    pub fn collect(repo_root: &Path) -> Self {
        let root_str = repo_root.display().to_string();
        let git_revision = run_trimmed("git", &["-C", &root_str, "rev-parse", "HEAD"])
            .unwrap_or_else(|| "unknown".to_owned());
        let git_dirty = run_trimmed("git", &["-C", &root_str, "status", "--porcelain"])
            .is_some_and(|s| !s.is_empty());
        let logical_cpus = std::thread::available_parallelism().map_or(1, usize::from);
        let filesystem_block_size = statvfs(repo_root).map_or(0, |st| st.f_frsize);
        let rustc_version =
            run_trimmed("rustc", &["--version"]).unwrap_or_else(|| "unknown".to_owned());
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
        Self {
            git_revision,
            git_dirty,
            os: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
            logical_cpus,
            filesystem_block_size,
            rustc_version,
            observed_at: timestamp::format_utc(now_secs),
        }
    }
}

fn run_trimmed(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).output().ok()?;
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
}

/// Aggregated measurement of one Suds benchmark case.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaseReport {
    pub name: String,
    pub parameters: BTreeMap<String, String>,
    pub samples: usize,
    pub min_us: u64,
    pub median_us: u64,
    pub p95_us: u64,
    pub max_us: u64,
    pub median_cpu_us: u64,
    pub entries_per_sec: u64,
    pub max_rss_bytes: u64,
    pub counts: Counts,
    pub outcome: StepOutcome,
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
    let mut last_proc = ProcessSample::default();
    let mut last_counts = Counts::default();
    let mut last_outcome = StepOutcome::default();
    for _ in 0..runs {
        let mut state = setup();
        let before_counts = Counts::now();
        let before_proc = ProcessSample::capture();
        let started = Instant::now();
        let outcome = step(&mut state);
        let took_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
        let after_proc = ProcessSample::capture();
        let after_counts = Counts::now();
        let proc_delta = after_proc.delta_since(before_proc);
        elapsed.push(took_us);
        cpus.push(proc_delta.total_cpu_us());
        last_proc = proc_delta;
        last_counts = after_counts.since(before_counts);
        last_outcome = outcome;
    }
    elapsed.sort_unstable();
    cpus.sort_unstable();
    let mid = elapsed.len() / 2;
    let p95 = ((elapsed.len().saturating_sub(1)) * 95) / 100;
    let median_us = elapsed[mid];
    let entries_per_sec = if median_us > 0 {
        last_outcome
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
        min_us: elapsed[0],
        median_us,
        p95_us: elapsed[p95],
        max_us: *elapsed.last().unwrap_or(&0),
        median_cpu_us: cpus[cpus.len() / 2],
        entries_per_sec,
        max_rss_bytes: last_proc.max_rss_bytes,
        counts: last_counts,
        outcome: last_outcome,
    }
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
                &["init", "-q", "--bare", &format!("remote-{r:02}.git")],
            )?;
            let repo = workspace_root.join(format!("repo-{r:02}"));
            fs::create_dir_all(repo.join("src")).map_err(|e| e.to_string())?;
            run_git(&repo, &["init", "-q", "-b", "main"])?;
            run_git(&repo, &["config", "user.name", "Bench"])?;
            run_git(&repo, &["config", "user.email", "bench@example.com"])?;
            run_git(&repo, &["config", "commit.gpgsign", "false"])?;
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
            run_git(&repo, &["add", "."])?;
            run_git(&repo, &["commit", "-q", "-m", "chore: initial"])?;
            run_git(
                &repo,
                &["remote", "add", "origin", &remote.display().to_string()],
            )?;
            run_git(&repo, &["push", "-q", "origin", "main"])?;

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
                    &[
                        "worktree",
                        "add",
                        "-q",
                        "-b",
                        &branch,
                        &wt.display().to_string(),
                    ],
                )?;
                run_git(&wt, &["push", "-q", "origin", &branch])?;
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

fn run_git(dir: &Path, args: &[&str]) -> Result<(), String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!(
            "git {args:?} failed: {}",
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
}

/// Structured comparison of two Suds benchmark reports.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComparisonReport {
    pub suite: String,
    pub baseline_revision: String,
    pub candidate_revision: String,
    pub cases: Vec<CaseDelta>,
}

/// Compares matching benchmark cases between `baseline` and `candidate`.
pub fn compare_reports(baseline: &BenchReport, candidate: &BenchReport) -> ComparisonReport {
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
        });
    }
    ComparisonReport {
        suite: candidate.suite.clone(),
        baseline_revision: baseline.environment.git_revision.clone(),
        candidate_revision: candidate.environment.git_revision.clone(),
        cases,
    }
}

/// Formats a human-readable comparison table across matching Suds benchmark cases.
pub fn format_comparison(comp: &ComparisonReport) -> String {
    let mut out = format!(
        "Suite: {} ({} vs {})\n{:<42} {:>11} {:>11} {:>9} {:>9} {:>9} {:>8}\n",
        comp.suite,
        short_rev(&comp.baseline_revision),
        short_rev(&comp.candidate_revision),
        "CASE",
        "BASE (us)",
        "CAND (us)",
        "TIME %",
        "dBULK",
        "dPROBE",
        "dGIT",
    );
    for c in &comp.cases {
        out.push_str(&format!(
            "{:<42} {:>11} {:>11} {:>+8.1}% {:>+9} {:>+9} {:>+8}\n",
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

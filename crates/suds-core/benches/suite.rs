//! Multi-scale benchmark suite and revision comparison harness for Suds.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use suds_core::bench::{
    BENCHMARK_SCHEMA, BenchReport, BenchWorkspace, CaseReport, EnvironmentInfo, ScaleProfile,
    StepOutcome, compare_reports, format_comparison, measure_case, with_reference_scan,
};
use suds_core::budget::Budget;
use suds_core::development::{Environment, inspect};
use suds_core::maintain::{Context, maintain_observe_with_budget, maintain_with_budget};
use suds_core::organize::journal::Journal;
use suds_core::reclaim::execute::{Authority, Conditions, apply};
use suds_core::reclaim::git::Git;
use suds_core::reclaim::liveness::Liveness;
use suds_core::reclaim::plan::{CheckoutScope, Observed, Policy, plan_reclaim};
use suds_core::state::{Directories, Settings};
use suds_core::stores::Programs;
use suds_core::{registry, seed};

const BENCH_NOW: i64 = 2_000_000_000;

enum BenchCommand {
    Component {
        samples: usize,
        output: Option<PathBuf>,
    },
    System {
        profile: ProfileSelection,
        samples: usize,
        output: Option<PathBuf>,
    },
    Live {
        root: PathBuf,
        samples: usize,
        output: Option<PathBuf>,
    },
    Compare {
        baseline: PathBuf,
        candidate: PathBuf,
        json: bool,
    },
}

#[derive(Clone, Copy)]
enum ProfileSelection {
    Small,
    Medium,
    Large,
    All,
}

impl ProfileSelection {
    fn parse(raw: &str) -> Result<Self, String> {
        match raw {
            "small" => Ok(Self::Small),
            "medium" => Ok(Self::Medium),
            "large" => Ok(Self::Large),
            "all" => Ok(Self::All),
            other => Err(format!("unknown profile `{other}`")),
        }
    }

    fn profiles(self) -> Vec<ScaleProfile> {
        match self {
            Self::Small => vec![ScaleProfile::small()],
            Self::Medium => vec![ScaleProfile::medium()],
            Self::Large => vec![ScaleProfile::large()],
            Self::All => vec![
                ScaleProfile::small(),
                ScaleProfile::medium(),
                ScaleProfile::large(),
            ],
        }
    }
}

fn parse_cli() -> Result<BenchCommand, String> {
    let args: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| a != "--bench")
        .collect();
    let Some(sub) = args.first().map(String::as_str) else {
        return Ok(BenchCommand::Component {
            samples: 5,
            output: None,
        });
    };
    let rest = &args[1..];
    match sub {
        "component" => {
            let flags = parse_run_flags(rest, 5, FlagScope::Component)?;
            Ok(BenchCommand::Component {
                samples: flags.samples,
                output: flags.output,
            })
        }
        "system" => {
            let flags = parse_run_flags(rest, 3, FlagScope::System)?;
            Ok(BenchCommand::System {
                profile: flags.profile,
                samples: flags.samples,
                output: flags.output,
            })
        }
        "live" => {
            let flags = parse_run_flags(rest, 2, FlagScope::Live)?;
            Ok(BenchCommand::Live {
                root: flags.root,
                samples: flags.samples,
                output: flags.output,
            })
        }
        "compare" => {
            let mut json = false;
            let mut positional = Vec::new();
            for item in rest {
                if item == "--json" {
                    json = true;
                } else {
                    positional.push(PathBuf::from(item));
                }
            }
            if positional.len() != 2 {
                return Err(
                    "usage: suite compare <baseline.json> <candidate.json> [--json]".to_owned(),
                );
            }
            Ok(BenchCommand::Compare {
                baseline: positional[0].clone(),
                candidate: positional[1].clone(),
                json,
            })
        }
        other => Err(format!("unknown subcommand `{other}`")),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FlagScope {
    Component,
    System,
    Live,
}

struct RunFlags {
    samples: usize,
    output: Option<PathBuf>,
    profile: ProfileSelection,
    root: PathBuf,
}

fn parse_run_flags(
    args: &[String],
    default_samples: usize,
    scope: FlagScope,
) -> Result<RunFlags, String> {
    let mut flags = RunFlags {
        samples: default_samples,
        output: None,
        profile: ProfileSelection::Small,
        root: PathBuf::from("."),
    };
    let mut i = 0;
    while i < args.len() {
        match (args[i].as_str(), scope) {
            ("--samples", _) => {
                i += 1;
                flags.samples = args
                    .get(i)
                    .ok_or("missing --samples value")?
                    .parse()
                    .map_err(|_| "invalid --samples integer")?;
            }
            ("--output", _) => {
                i += 1;
                flags.output = Some(PathBuf::from(args.get(i).ok_or("missing --output path")?));
            }
            ("--profile", FlagScope::System) => {
                i += 1;
                flags.profile =
                    ProfileSelection::parse(args.get(i).ok_or("missing --profile value")?)?;
            }
            ("--root", FlagScope::Live) => {
                i += 1;
                flags.root = PathBuf::from(args.get(i).ok_or("missing --root path")?);
            }
            (flag, _) => return Err(format!("unknown option `{flag}`")),
        }
        i += 1;
    }
    Ok(flags)
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("suds-bench: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let command = parse_cli()?;
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    match command {
        BenchCommand::Component { samples, output } => {
            let report = BenchReport {
                schema: BENCHMARK_SCHEMA.to_owned(),
                suite: "component".to_owned(),
                environment: EnvironmentInfo::collect(&cwd),
                cases: run_component(samples)?,
            };
            emit_report(&report, output.as_deref())
        }
        BenchCommand::System {
            profile,
            samples,
            output,
        } => {
            let mut cases = Vec::new();
            for p in profile.profiles() {
                cases.extend(run_system_profile(&p, samples)?);
            }
            let report = BenchReport {
                schema: BENCHMARK_SCHEMA.to_owned(),
                suite: "system".to_owned(),
                environment: EnvironmentInfo::collect(&cwd),
                cases,
            };
            emit_report(&report, output.as_deref())
        }
        BenchCommand::Live {
            root,
            samples,
            output,
        } => {
            let root = root.canonicalize().map_err(|e| e.to_string())?;
            let report = BenchReport {
                schema: BENCHMARK_SCHEMA.to_owned(),
                suite: "live".to_owned(),
                environment: EnvironmentInfo::collect(&root),
                cases: run_live(&root, samples)?,
            };
            emit_report(&report, output.as_deref())
        }
        BenchCommand::Compare {
            baseline,
            candidate,
            json,
        } => {
            let base: BenchReport = serde_json::from_str(
                &fs::read_to_string(&baseline)
                    .map_err(|e| format!("{}: {e}", baseline.display()))?,
            )
            .map_err(|e| e.to_string())?;
            let cand: BenchReport = serde_json::from_str(
                &fs::read_to_string(&candidate)
                    .map_err(|e| format!("{}: {e}", candidate.display()))?,
            )
            .map_err(|e| e.to_string())?;
            let comp = compare_reports(&base, &cand);
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&comp).map_err(|e| e.to_string())?
                );
            } else {
                print!("{}", format_comparison(&comp));
            }
            Ok(())
        }
    }
}

fn run_component(samples: usize) -> Result<Vec<CaseReport>, String> {
    let ws = BenchWorkspace::generate(&ScaleProfile::small())?;
    let roots = vec![ws.workspace_root.clone(), ws.store_root.clone()];
    let env = Environment::default();

    let mut params = BTreeMap::new();
    params.insert("profile".to_owned(), "small".to_owned());

    let mut cases = Vec::new();
    cases.push(measure_case(
        "component/traversal_batched",
        params.clone(),
        samples,
        || (),
        |()| {
            let scan = inspect(&roots, &env);
            StepOutcome {
                entries_visited: scan.entries_visited,
                allocated_bytes: scan.checkouts.iter().map(|c| c.own.allocated_bytes).sum(),
                ..StepOutcome::default()
            }
        },
    ));

    cases.push(measure_case(
        "component/traversal_reference",
        params,
        samples,
        || (),
        |()| {
            let scan = with_reference_scan(|| inspect(&roots, &env));
            StepOutcome {
                entries_visited: scan.entries_visited,
                allocated_bytes: scan.checkouts.iter().map(|c| c.own.allocated_bytes).sum(),
                ..StepOutcome::default()
            }
        },
    ));

    Ok(cases)
}

fn run_system_profile(profile: &ScaleProfile, samples: usize) -> Result<Vec<CaseReport>, String> {
    let ws = BenchWorkspace::generate(profile)?;
    let roots = vec![ws.workspace_root.clone(), ws.store_root.clone()];
    let env = Environment::default();
    let programs = Programs::default();

    let mut base_params = BTreeMap::new();
    base_params.insert("profile".to_owned(), profile.name.clone());
    base_params.insert("repositories".to_owned(), profile.repositories.to_string());
    base_params.insert("worktrees".to_owned(), profile.worktrees.to_string());

    let mut cases = Vec::new();

    // 1. Filesystem traversal and clone/hard-link deduplication.
    cases.push(measure_case(
        format!("system/{}/filesystem_traversal", profile.name),
        base_params.clone(),
        samples,
        || (),
        |()| {
            let scan = inspect(&roots, &env);
            let allocated: u64 = scan
                .checkouts
                .iter()
                .map(|c| {
                    c.own.allocated_bytes
                        + c.build_state
                            .iter()
                            .map(|b| b.measure.allocated_bytes)
                            .sum::<u64>()
                })
                .sum();
            StepOutcome {
                entries_visited: scan.entries_visited,
                allocated_bytes: allocated,
                ..StepOutcome::default()
            }
        },
    ));

    // 2. Multi-slice bounded observation to completion.
    let full_scan = inspect(std::slice::from_ref(&ws.workspace_root), &env);
    let slice_cap = (full_scan.entries_visited * 2 / 5).max(150);
    let dirs = Directories {
        state: ws.state_dir.clone(),
        config: ws.config_dir.clone(),
    };
    let obs_settings = Settings {
        roots: vec![ws.workspace_root.clone()],
        ceiling_bytes: Some(10 * 1024 * 1024 * 1024),
        idle_minutes: 1,
        standing_approval: false,
        tolerate_unreadable_processes: true,
        budgets: Default::default(),
    };
    cases.push(measure_case(
        format!("system/{}/multi_slice_observation", profile.name),
        base_params.clone(),
        samples,
        || {
            let _ = fs::remove_file(dirs.pressure());
            let _ = fs::remove_file(dirs.maintain_cursor());
        },
        |()| {
            let budget = Budget {
                max_entries: Some(slice_cap),
                pace_every: 0,
                background_priority: false,
                ..Budget::for_maintenance()
            };
            let git = Git::default();
            let mut ts = BENCH_NOW;
            let mut visited = 0_u64;
            let mut peak_cursor = 0_u64;
            let mut usage = 0_u64;
            for _ in 0..16 {
                ts += 5;
                let report = maintain_observe_with_budget(
                    &obs_settings,
                    &Context {
                        directories: &dirs,
                        environment: &env,
                        programs: &programs,
                        git: &git,
                        now: ts,
                    },
                    &budget,
                    &mut || ts,
                )
                .unwrap();
                visited += report.entries_visited;
                if let Ok(meta) = fs::metadata(dirs.maintain_cursor()) {
                    peak_cursor = peak_cursor.max(meta.len());
                }
                if report.pressure.complete && !dirs.maintain_cursor().exists() {
                    usage = report.pressure.usage_bytes;
                    break;
                }
            }
            StepOutcome {
                entries_visited: visited,
                allocated_bytes: usage,
                cursor_bytes: peak_cursor,
                ..StepOutcome::default()
            }
        },
    ));

    // 3. Reclamation planning across active and ended claims.
    let reg = registry::reading(&ws.state_dir.join("registry.jsonl"));
    cases.push(measure_case(
        format!("system/{}/reclaim_planning", profile.name),
        base_params.clone(),
        samples,
        || (),
        |()| {
            let git = Git::default();
            let observed = Observed {
                liveness: &Ok(Liveness::default()),
                registry: &reg,
            };
            let plan = plan_reclaim(
                &full_scan,
                &observed,
                &git,
                Policy {
                    idle_seconds: 60,
                    checkouts: CheckoutScope::Released,
                    tolerate_unreadable_processes: true,
                },
                BENCH_NOW,
            );
            StepOutcome {
                entries_visited: plan.candidates.len() as u64,
                reclaimed_bytes: plan.removable_bytes(),
                ..StepOutcome::default()
            }
        },
    ));

    // 4. Build-state seeding followed by bounded reclamation execution.
    if let (Some(src_repo), Some(dest_wt)) = (ws.main_repos.first(), ws.unseeded_worktrees.first())
    {
        let seeded_target = dest_wt.join("target");
        cases.push(measure_case(
            format!("system/{}/seed_and_reclaim_cycle", profile.name),
            base_params.clone(),
            samples,
            || {
                let _ = fs::remove_dir_all(&seeded_target);
            },
            |()| {
                let git = Git::default();
                let mut journal = Journal::open(&ws.state_dir.join("journal")).unwrap();
                let splan = seed::plan_seed(dest_wt, Some(src_repo), &git).unwrap();
                let shared = splan.shared_bytes();
                let _ = seed::apply(&mut journal, &splan, Some("bench")).unwrap();

                let target_scan = inspect(std::slice::from_ref(dest_wt), &env);
                let r_reg = registry::reading(&ws.state_dir.join("registry.jsonl"));
                let mut rplan = plan_reclaim(
                    &target_scan,
                    &Observed {
                        liveness: &Ok(Liveness::default()),
                        registry: &r_reg,
                    },
                    &git,
                    Policy {
                        idle_seconds: 0,
                        checkouts: CheckoutScope::DerivedOnly,
                        tolerate_unreadable_processes: true,
                    },
                    BENCH_NOW,
                );
                // Restrict reclamation to the seeded target so `node_modules` stays intact across samples.
                rplan
                    .candidates
                    .retain(|c| c.path.as_path() == seeded_target);
                let reg_path = ws.state_dir.join("registry.jsonl");
                let rreport = apply(
                    &mut journal,
                    &rplan,
                    Conditions {
                        authority: Authority::StandingApproval,
                        observe: &mut || Ok(Liveness::default()),
                        registry: &mut || registry::reading(&reg_path),
                        registry_path: Some(&reg_path),
                        git: &git,
                        now: BENCH_NOW,
                    },
                )
                .unwrap();
                StepOutcome {
                    entries_visited: target_scan.entries_visited,
                    allocated_bytes: shared,
                    reclaimed_bytes: rreport.freed_bytes,
                    ..StepOutcome::default()
                }
            },
        ));
    }

    // 5. Unattended background maintenance step.
    cases.push(measure_case(
        format!("system/{}/background_maintain_step", profile.name),
        base_params,
        1,
        || (),
        |()| {
            let maint_settings = Settings {
                roots: vec![ws.workspace_root.clone()],
                ceiling_bytes: Some(1024),
                idle_minutes: 1,
                standing_approval: true,
                tolerate_unreadable_processes: true,
                budgets: Default::default(),
            };
            let budget = Budget {
                pace_every: 0,
                background_priority: false,
                ..Budget::for_maintenance()
            };
            let git = Git::default();
            let mut ts = BENCH_NOW;
            let report = maintain_with_budget(
                &maint_settings,
                &Context {
                    directories: &dirs,
                    environment: &env,
                    programs: &programs,
                    git: &git,
                    now: ts,
                },
                &budget,
                &mut || {
                    ts += 1;
                    ts
                },
            )
            .unwrap();
            let freed = report.reclaim.as_ref().map_or(0, |r| r.freed_bytes);
            StepOutcome {
                entries_visited: report.entries_visited,
                allocated_bytes: report.pressure.usage_bytes,
                reclaimed_bytes: freed,
                ..StepOutcome::default()
            }
        },
    ));

    Ok(cases)
}

fn run_live(root: &Path, samples: usize) -> Result<Vec<CaseReport>, String> {
    let scratch = std::env::temp_dir().join(format!("suds-live-bench-{}", std::process::id()));
    let _ = fs::remove_dir_all(&scratch);
    fs::create_dir_all(&scratch).map_err(|e| e.to_string())?;
    let dirs = Directories {
        state: scratch.join("state"),
        config: scratch.join("config"),
    };
    let settings = Settings {
        roots: vec![root.to_path_buf()],
        ceiling_bytes: Some(100 * 1024 * 1024 * 1024),
        idle_minutes: 60,
        standing_approval: false,
        tolerate_unreadable_processes: true,
        budgets: Default::default(),
    };
    let env = Environment::default();
    let programs = Programs::default();

    let mut params = BTreeMap::new();
    params.insert("root".to_owned(), root.display().to_string());

    let mut cases = Vec::new();
    cases.push(measure_case(
        "live/observe_root",
        params,
        samples,
        || {
            let _ = fs::remove_file(dirs.maintain_cursor());
        },
        |()| {
            let git = Git::default();
            let budget = Budget {
                pace_every: 0,
                background_priority: false,
                ..Budget::for_maintenance()
            };
            let mut ts = BENCH_NOW;
            let report = maintain_observe_with_budget(
                &settings,
                &Context {
                    directories: &dirs,
                    environment: &env,
                    programs: &programs,
                    git: &git,
                    now: ts,
                },
                &budget,
                &mut || {
                    ts += 1;
                    ts
                },
            )
            .unwrap();
            StepOutcome {
                entries_visited: report.entries_visited,
                allocated_bytes: report.pressure.usage_bytes,
                ..StepOutcome::default()
            }
        },
    ));

    let _ = fs::remove_dir_all(&scratch);
    Ok(cases)
}

fn emit_report(report: &BenchReport, output: Option<&Path>) -> Result<(), String> {
    println!(
        "Suite: {} | rev: {} | os: {}/{} | block: {}B",
        report.suite,
        &report.environment.git_revision[..10.min(report.environment.git_revision.len())],
        report.environment.os,
        report.environment.arch,
        report.environment.filesystem_block_size
    );
    println!(
        "{:<42} {:>10} {:>10} {:>10} {:>8} {:>8} {:>6}",
        "CASE", "MED (us)", "CPU (us)", "ENTRIES/s", "BULK", "PROBES", "GIT"
    );
    for c in &report.cases {
        let cpu_col = c
            .median_cpu_us
            .map_or_else(|| "n/a".to_owned(), |v| v.to_string());
        println!(
            "{:<42} {:>10} {:>10} {:>10} {:>8} {:>8} {:>6}",
            c.name,
            c.median_us,
            cpu_col,
            c.entries_per_sec,
            c.counts.bulk_dir_calls,
            c.counts.entry_probe_calls,
            c.counts.helper_launches,
        );
    }
    if let Some(path) = output {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let body = serde_json::to_string_pretty(report).map_err(|e| e.to_string())?;
        fs::write(path, body).map_err(|e| e.to_string())?;
        println!("Saved benchmark report to {}", path.display());
    }
    Ok(())
}

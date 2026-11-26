//! The commands that a client and a schedule run: `register`, `release`,
//! `pressure`, `maintain`, and `schedule`.

use std::env;
use std::io::{self, Write};
use std::process::ExitCode;

use suds_core::arena::{self, Arena};
use suds_core::development::{self, Environment};
use suds_core::maintain::{self, Context, MaintenanceReport};
use suds_core::organize::journal::{Journal, RunStatus};
use suds_core::pressure::{Level, Pressure};
use suds_core::reclaim::git::Git;
use suds_core::registry::{self, Event, Record};
use suds_core::roots;
use suds_core::schedule::{self, Change, InstallPlan, Standing, Unit, UnitFile};
use suds_core::state::{
    self, Directories, PAUSE_SCHEMA, PauseState, clear_pause, load_settings, write_pause,
};
use suds_core::stores::Programs;
use suds_core::timestamp;

use crate::cli::{
    ArenaArgs, Format, HoldArgs, MaintainArgs, PressureArgs, ScheduleArgs, Scheduler,
};
use crate::now_seconds;
use crate::output::{emit, finish, planned, ran};
use crate::table::{escape, format_bytes};

fn directories() -> Result<Directories, String> {
    Directories::of(&Environment::of_process())
        .ok_or_else(|| "no home directory exists for this process".to_owned())
}

/// The program of each store as this run finds it. The search states on the error stream
/// each directory that it could not read, since a program may stand there unseen.
pub(crate) fn resolved_programs() -> Programs {
    let resolution = Programs::resolved(&Environment::of_process());
    for failure in &resolution.unread {
        eprintln!("suds: {}", escape(failure));
    }
    resolution.programs
}

pub fn register_command(args: &HoldArgs) -> ExitCode {
    finish(hold(args, Event::Registered))
}

pub fn release_command(args: &HoldArgs) -> ExitCode {
    finish(hold(args, Event::Released))
}

/// Appends one record to the registry. The checkout's path resolves against
/// the working directory, without following a link.
fn hold(args: &HoldArgs, event: Event) -> Result<ExitCode, String> {
    let cwd =
        env::current_dir().map_err(|err| format!("could not read the working directory: {err}"))?;
    let checkout = roots::normalize(&cwd.join(&args.checkout));
    if args.landed.is_some() && event != Event::Released {
        return Err("only a release reports a landed commit".to_owned());
    }
    if args.claim_ended && event != Event::Released {
        return Err("only a release states an ended claim".to_owned());
    }
    if args.claim.is_some() && event != Event::Registered {
        return Err("only a registration states a claim's text".to_owned());
    }
    let path = directories()?.registry();
    let attempt = args.attempt.as_deref();
    let now = now_seconds();
    let landed = args.landed.as_deref();
    let record = match event {
        Event::Registered => registry::register(
            &path,
            &checkout,
            &args.client,
            attempt,
            args.claim.as_deref(),
            now,
        )?,
        Event::Released => match landed {
            _ if args.claim_ended => {
                registry::append_ended(&path, &checkout, &args.client, attempt, landed, now)?
            }
            Some(landed) => {
                registry::append_landed(&path, &checkout, &args.client, attempt, landed, now)?
            }
            None => registry::append(&path, &checkout, &args.client, attempt, event, now)?,
        },
    };
    planned(emit(Format::Json, &record, |_, _: &Record| Ok(())))
}

pub fn pressure_command(args: &PressureArgs) -> ExitCode {
    finish(pressure(args))
}

/// Prints the last reading that a maintenance run recorded. The command
/// measures nothing, so a client can read the level before each start.
fn pressure(args: &PressureArgs) -> Result<ExitCode, String> {
    let path = directories()?.pressure();
    let reading = maintain::read_pressure(&path).ok_or_else(|| {
        format!(
            "{} holds no reading. Run suds maintain to measure.",
            path.display()
        )
    })?;
    planned(emit(args.format, &reading, write_pressure))
}

fn write_pressure(out: &mut impl Write, reading: &Pressure) -> io::Result<()> {
    let level = match reading.level {
        Level::Normal => "normal",
        Level::Soft => "soft",
        Level::Hard => "hard",
    };
    let limit = if reading.enforced {
        "a filesystem limit enforces the ceiling"
    } else {
        "no filesystem limit enforces the ceiling"
    };
    writeln!(
        out,
        "{level}: {} of {} at {}, and {limit}",
        format_bytes(reading.usage_bytes),
        format_bytes(reading.ceiling_bytes),
        reading.observed_at
    )
}

pub fn maintain_command(args: &MaintainArgs) -> ExitCode {
    finish(run_maintenance(args))
}

fn run_maintenance(args: &MaintainArgs) -> Result<ExitCode, String> {
    let directories = directories()?;
    let pause_path = directories.pause();
    if args.pause {
        let state = PauseState {
            schema: PAUSE_SCHEMA.to_owned(),
            paused: true,
            reason: args.reason.clone(),
            updated_at: timestamp::format_utc(now_seconds()),
        };
        write_pause(&pause_path, &state)?;
        return planned(emit(args.format, &state, |out, state| {
            writeln!(out, "paused: {} ({})", state.paused, state.updated_at)
        }));
    }
    if args.resume {
        clear_pause(&pause_path)?;
        let state = PauseState {
            schema: PAUSE_SCHEMA.to_owned(),
            paused: false,
            reason: None,
            updated_at: timestamp::format_utc(now_seconds()),
        };
        return planned(emit(args.format, &state, |out, state| {
            writeln!(out, "paused: {} ({})", state.paused, state.updated_at)
        }));
    }
    if args.status {
        let pause = state::read_pause(&pause_path)?;
        let pressure = maintain::read_pressure(&directories.pressure());
        let snapshot = serde_json::json!({
            "paused": pause.as_ref().is_some_and(|p| p.paused),
            "pause": pause,
            "pressure": pressure,
        });
        return planned(emit(args.format, &snapshot, |out, snap| {
            let paused = snap["paused"].as_bool().unwrap_or(false);
            writeln!(
                out,
                "maintenance: {}",
                if paused { "paused" } else { "active" }
            )?;
            if let Some(p) = &pressure {
                write_pressure(out, p)?;
            }
            Ok(())
        }));
    }

    let settings = load_settings(&directories.settings())?;
    let context = Context {
        directories: &directories,
        environment: &Environment::of_process(),
        programs: &resolved_programs(),
        git: &Git::default(),
        now: now_seconds(),
    };
    let report = if args.observe {
        maintain::maintain_observe(&settings, &context)?
    } else {
        maintain::maintain(&settings, &context)?
    };
    let partial = !report.paused
        && (!report.outcome.is_complete()
            || report.error_count > 0
            || report
                .reclaim
                .iter()
                .flat_map(|reclaim| &reclaim.removals)
                .any(|removal| !removal.removed)
            || report
                .stores
                .iter()
                .flat_map(|stores| &stores.trims)
                .any(|trim| trim.failure.is_some()));
    ran(emit(args.format, &report, write_maintenance), partial)
}

fn write_maintenance(out: &mut impl Write, report: &MaintenanceReport) -> io::Result<()> {
    if report.paused {
        match &report.pause_reason {
            Some(reason) => writeln!(out, "Suds paused maintenance: {reason}")?,
            None => writeln!(out, "Suds paused maintenance.")?,
        }
        return write_pressure(out, &report.pressure);
    }
    write_pressure(out, &report.pressure)?;
    if !report.standing_approval {
        return writeln!(
            out,
            "The settings hold no standing approval, so Suds measured and changed nothing."
        );
    }
    if report.reclaim.is_none() && report.stores.is_none() {
        return writeln!(
            out,
            "Suds observed development storage and changed nothing."
        );
    }
    let reclaimed = report.reclaim.as_ref().map_or(0, |run| run.freed_bytes);
    let trimmed = report.stores.as_ref().map_or(0, |run| run.freed_bytes);
    writeln!(
        out,
        "Suds reclaimed about {} from development directories and trimmed about {} from the stores. The journal records each change.",
        format_bytes(reclaimed),
        format_bytes(trimmed)
    )
}

pub fn arena_command(args: &ArenaArgs) -> ExitCode {
    finish(audit_arena(args))
}

/// Audits the arena and prints its provisioning plan. Suds runs no command
/// of the plan.
fn audit_arena(args: &ArenaArgs) -> Result<ExitCode, String> {
    let settings = load_settings(&directories()?.settings())?;
    let root = match &args.root {
        Some(root) => {
            let cwd = env::current_dir()
                .map_err(|err| format!("could not read the working directory: {err}"))?;
            roots::normalize(&cwd.join(root))
        }
        None => settings
            .roots
            .first()
            .cloned()
            .ok_or("the settings name no root. Name the development tree with --root DIR.")?,
    };
    let others: Vec<_> = settings
        .roots
        .iter()
        .filter(|other| **other != root)
        .cloned()
        .chain(development::store_locations(&Environment::of_process()))
        .collect();
    let audited = arena::audit(&root, &others, settings.ceiling_bytes)?;
    planned(emit(args.format, &audited, write_arena))
}

fn write_arena(out: &mut impl Write, audited: &Arena) -> io::Result<()> {
    writeln!(
        out,
        "The ceiling is {}, and the filesystem holds {} with {} free.",
        format_bytes(audited.ceiling_bytes),
        format_bytes(audited.capacity_bytes),
        format_bytes(audited.available_bytes)
    )?;
    let verdict = if audited.enforced {
        "A filesystem enforces the ceiling"
    } else {
        "No filesystem enforces the ceiling"
    };
    writeln!(out, "{verdict}: {}.", audited.finding)?;
    for location in &audited.uncovered {
        writeln!(out, "  outside the root's filesystem: {}", escape(location))?;
    }
    writeln!(
        out,
        "The audit measures no usage. Run suds maintain, and suds pressure prints the usage against the ceiling."
    )?;
    if audited.provisioning.is_empty() {
        return Ok(());
    }
    writeln!(out)?;
    writeln!(
        out,
        "These commands give the ceiling a filesystem limit. Suds runs none of them."
    )?;
    for step in &audited.provisioning {
        let who = if step.privileged {
            "administrator"
        } else {
            "user"
        };
        writeln!(out, "  [{who}] {}", step.command)?;
        writeln!(out, "      {}", step.purpose)?;
    }
    Ok(())
}

pub fn schedule_command(args: &ScheduleArgs) -> ExitCode {
    finish(schedule(args))
}

/// Prints the units that run `suds maintain` on a schedule, places them, or
/// checks the placed ones.
fn schedule(args: &ScheduleArgs) -> Result<ExitCode, String> {
    let scheduler = match args.scheduler {
        Some(Scheduler::Systemd) => schedule::Scheduler::Systemd,
        Some(Scheduler::Launchd) => schedule::Scheduler::Launchd,
        None => schedule::Scheduler::native(),
    };
    let environment = Environment::of_process();
    let program = env::current_exe()
        .map_err(|err| format!("could not find the path of this program: {err}"))?;
    let mut out = io::stdout().lock();
    if args.check {
        let standing = schedule::standing(scheduler, &environment, Some(&program));
        let line = match &standing {
            Standing::Current => "The placed units are the units of this release.".to_owned(),
            Standing::Absent => {
                "No unit stands in place, so no scheduler runs the maintenance.".to_owned()
            }
            Standing::Stale(reason) => format!("The placed units are stale: {reason}."),
            Standing::Unreadable(reason) => {
                format!("Suds could not read the placed units: {reason}.")
            }
        };
        writeln!(out, "{line}").map_err(|err| format!("failed to write the standing: {err}"))?;
        return Ok(match standing {
            Standing::Current => ExitCode::SUCCESS,
            _ => ExitCode::from(crate::EXIT_PARTIAL),
        });
    }
    // The units carry this process's search path, so a scheduled run finds each store's
    // program as this shell finds it.
    let unit = Unit {
        program,
        path: env::var("PATH").ok().filter(|path| !path.is_empty()),
        minutes: args.minutes,
    };
    if !args.install {
        let files = schedule::files(scheduler, &unit, &environment)
            .ok_or("no home directory exists for this process, so the units have no place")?;
        return planned(print_units(&mut out, &files));
    }
    let plan = schedule::plan_install(scheduler, &unit, &environment)?;
    if !args.apply {
        return planned(print_install(&mut out, &plan));
    }
    let mut journal = Journal::open(&directories()?.journal()).map_err(|err| err.to_string())?;
    let report = schedule::install(&mut journal, &plan, &mut schedule::run_step)
        .map_err(|err| err.to_string())?;
    let mut written = Ok(());
    for step in &report.steps {
        let line = match &step.reason {
            None => format!("done    {}", escape(&step.step)),
            Some(reason) => format!("failed  {}: {}", escape(&step.step), escape(reason)),
        };
        written = written.and_then(|()| writeln!(out, "{line}"));
    }
    let line = format!("The journal records run {}.", report.run);
    written = written.and_then(|()| writeln!(out, "{line}"));
    ran(written, report.status != RunStatus::Complete)
}

/// Prints what an installation writes and runs, for a person to read before `--apply`.
fn print_install(out: &mut impl Write, plan: &InstallPlan) -> io::Result<()> {
    for planned in &plan.files {
        let change = match planned.change {
            Change::Create => "create ",
            Change::Replace => "replace",
            Change::Keep => "keep   ",
        };
        writeln!(
            out,
            "{change}  {}",
            escape(&planned.file.path.to_string_lossy())
        )?;
    }
    for step in &plan.steps {
        writeln!(out, "run      {}", escape(&step.command.join(" ")))?;
    }
    writeln!(out)?;
    let files: Vec<UnitFile> = plan.files.iter().map(|file| file.file.clone()).collect();
    print_units(out, &files)?;
    writeln!(out)?;
    writeln!(
        out,
        "Suds changed nothing. Run the command with --apply to place the units."
    )?;
    out.flush()
}

/// Prints each file beneath a comment that names its place.
fn print_units(out: &mut impl Write, files: &[UnitFile]) -> io::Result<()> {
    for (position, file) in files.iter().enumerate() {
        if position > 0 {
            writeln!(out)?;
        }
        // A property list takes no comment before its declaration.
        if !file.contents.starts_with("<?xml") {
            writeln!(out, "# {}", file.path.display())?;
        }
        out.write_all(file.contents.as_bytes())?;
    }
    out.flush()
}

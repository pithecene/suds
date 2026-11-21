//! The commands that a client and a schedule run: `register`, `release`,
//! `pressure`, `maintain`, and `schedule`.

use std::env;
use std::io::{self, Write};
use std::process::{Command, ExitCode};

use suds_core::arena::{self, Arena};
use suds_core::development::{self, Environment};
use suds_core::maintain::{self, Context, MaintenanceReport};
use suds_core::pressure::{Level, Pressure};
use suds_core::reclaim::git::Git;
use suds_core::registry::{self, Event, Record};
use suds_core::roots;
use suds_core::schedule::{self, Standing, Unit, UnitFile};
use suds_core::state::{Directories, load_settings};
use suds_core::stores::Programs;

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
    let path = directories()?.registry();
    let attempt = args.attempt.as_deref();
    let now = now_seconds();
    let record = match &args.landed {
        Some(landed) => {
            registry::append_landed(&path, &checkout, &args.client, attempt, landed, now)?
        }
        None => registry::append(&path, &checkout, &args.client, attempt, event, now)?,
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
    let settings = load_settings(&directories.settings())?;
    let context = Context {
        directories: &directories,
        environment: &Environment::of_process(),
        programs: &Programs::resolved(&Environment::of_process()),
        git: &Git::default(),
        now: now_seconds(),
    };
    let report = maintain::maintain(&settings, &context)?;
    let partial = report.error_count > 0
        || report
            .reclaim
            .iter()
            .flat_map(|reclaim| &reclaim.removals)
            .any(|removal| !removal.removed)
        || report
            .stores
            .iter()
            .flat_map(|stores| &stores.trims)
            .any(|trim| trim.failure.is_some());
    ran(emit(args.format, &report, write_maintenance), partial)
}

fn write_maintenance(out: &mut impl Write, report: &MaintenanceReport) -> io::Result<()> {
    write_pressure(out, &report.pressure)?;
    if !report.standing_approval {
        return writeln!(
            out,
            "The settings hold no standing approval, so Suds measured and changed nothing."
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
    let files = schedule::files(scheduler, &unit, &environment)
        .ok_or("no home directory exists for this process, so the units have no place")?;
    if !args.install {
        return planned(print_units(&mut out, &files));
    }
    schedule::place(&files)?;
    for file in &files {
        let _ = writeln!(out, "wrote {}", escape(&file.path.to_string_lossy()));
    }
    for step in schedule::activation(scheduler, &files) {
        let line = step.command.join(" ");
        let (program, arguments) = step.command.split_first().ok_or("an empty command")?;
        let status = Command::new(program).args(arguments).status();
        let succeeded = status.as_ref().is_ok_and(|status| status.success());
        if succeeded {
            let _ = writeln!(out, "ran {}", escape(&line));
        } else if step.required {
            let reason = match status {
                Ok(status) => status.to_string(),
                Err(err) => err.to_string(),
            };
            return Err(format!(
                "Suds placed the units, and `{line}` failed: {reason}"
            ));
        }
    }
    planned(out.flush())
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

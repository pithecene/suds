//! The `plan`, `apply`, `journal`, and `undo` commands.
//!
//! Each command calls the shared planner, executor, and journal in
//! `suds-core`. The plan file is the only input that `apply` accepts, so a
//! run moves exactly the entries that the reviewed plan names.

use std::env;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use suds_core::organize::execute::{self, RunReport};
use suds_core::organize::journal::{self, Journal, JournalState, RunStatus, RunSummary};
use suds_core::organize::plan::{MovePlan, MoveRequest, PRECONDITIONS, plan_move};
use suds_core::roots;

use crate::cli::{ApplyArgs, Format, JournalArgs, JournalOption, PlanMoveArgs, UndoArgs};
use crate::table::{entry_label, escape, format_bytes};

/// Suds refused one or more moves, or it refused to build the plan.
const EXIT_REFUSED: u8 = 1;
/// The invocation failed before Suds moved anything.
const EXIT_INVOCATION: u8 = 2;

pub fn plan_move_command(args: &PlanMoveArgs) -> ExitCode {
    match plan_and_write(args) {
        Ok(code) => code,
        Err(message) => fail(&message),
    }
}

fn plan_and_write(args: &PlanMoveArgs) -> Result<ExitCode, String> {
    let journal_dir = journal_directory(&args.journal)?;
    if fs::symlink_metadata(&args.output).is_ok() {
        return Err(format!(
            "{} already exists, and Suds replaces no file",
            args.output.display()
        ));
    }
    let cwd = working_directory(args.sources.iter().chain([&args.destination]))?;
    let (source_directory, requests) = split_sources(&args.sources, &cwd)?;
    let destination = roots::normalize(&cwd.join(&args.destination));

    let plan = match plan_move(&source_directory, &requests, &destination, now_seconds()) {
        Ok(plan) => plan,
        Err(refused) => {
            for refusal in &refused.refusals {
                eprintln!("suds: refused: {}", escape(&refusal.message));
            }
            return Ok(ExitCode::from(EXIT_REFUSED));
        }
    };
    // The journal opens first, so that a busy or unwritable journal stops
    // the command before it writes a file. The plan file precedes the plan's
    // record, so the journal records only a plan that a file holds.
    let mut journal = Journal::open(&journal_dir).map_err(|err| err.to_string())?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&args.output)
        .map_err(|err| format!("could not create {}: {err}", args.output.display()))?;
    let written = serde_json::to_writer_pretty(&mut file, &plan)
        .map_err(io::Error::from)
        .and_then(|()| writeln!(file))
        .and_then(|()| file.sync_all())
        .map_err(|err| format!("could not write {}: {err}", args.output.display()))
        .and_then(|()| execute::record_plan(&mut journal, &plan).map_err(|err| err.to_string()));
    if let Err(message) = written {
        // This invocation created the file, and no journal record makes it
        // usable, so removing it frees the path for a retry.
        drop(file);
        return Err(match fs::remove_file(&args.output) {
            Ok(()) => message,
            Err(err) => format!(
                "{message}. Suds also could not remove the unusable file {}: {err}",
                args.output.display()
            ),
        });
    }
    drop(journal);

    let mut out = io::stdout().lock();
    write_preview(&mut out, &plan, &args.output)
        .and_then(|()| out.flush())
        .map_err(|err| format!("failed to write the preview: {err}"))?;
    Ok(ExitCode::SUCCESS)
}

/// Splits each source into its parent directory and its name, and requires
/// one shared parent.
fn split_sources(sources: &[PathBuf], cwd: &Path) -> Result<(PathBuf, Vec<MoveRequest>), String> {
    let mut parent: Option<PathBuf> = None;
    let mut requests = Vec::new();
    for source in sources {
        let path = roots::normalize(&cwd.join(source));
        let (Some(directory), Some(name)) = (path.parent(), path.file_name()) else {
            return Err(format!("{} names no entry of a directory", path.display()));
        };
        match &parent {
            Some(existing) if existing != directory => {
                return Err(format!(
                    "every --from entry must lie in one directory, but {} and {} differ",
                    existing.display(),
                    directory.display()
                ));
            }
            Some(_) => {}
            None => parent = Some(directory.to_path_buf()),
        }
        requests.push(MoveRequest {
            name: OsString::from(name),
            expected: None,
        });
    }
    let parent = parent.ok_or_else(|| "name at least one entry with --from".to_owned())?;
    Ok((parent, requests))
}

fn write_preview(out: &mut impl Write, plan: &MovePlan, output: &Path) -> io::Result<()> {
    writeln!(
        out,
        "Plan {} written to {}",
        plan.id,
        escape(&output.to_string_lossy())
    )?;
    writeln!(
        out,
        "From: {}",
        escape(&plan.source_directory.path.display())
    )?;
    writeln!(
        out,
        "To:   {}",
        escape(&plan.destination_directory.path.display())
    )?;
    writeln!(out)?;
    for planned in &plan.moves {
        let name = planned
            .name()
            .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
        writeln!(
            out,
            "  {:<9}  {:>10} allocated  {:>10} logical  modified {}  {}",
            entry_label(planned.expected.kind),
            format_bytes(planned.measure.allocated_bytes),
            format_bytes(planned.measure.logical_bytes),
            planned
                .measure
                .newest_modified
                .as_deref()
                .map_or("-", |time| &time[..10.min(time.len())]),
            escape(&name)
        )?;
        if planned.measure_error_count > 0 {
            writeln!(
                out,
                "             Suds could not read {} entries beneath it, so its sizes are lower bounds",
                planned.measure_error_count
            )?;
        }
    }
    writeln!(out)?;
    writeln!(
        out,
        "Each move is one atomic rename on one filesystem. Suds overwrites nothing, and suds undo can reverse every move."
    )?;
    writeln!(
        out,
        "Immediately before each move, Suds verifies these conditions:"
    )?;
    for condition in PRECONDITIONS {
        writeln!(out, "  - {condition}")?;
    }
    writeln!(out)?;
    writeln!(
        out,
        "Apply the plan with: suds apply {}",
        escape(&output.to_string_lossy())
    )
}

pub fn apply_command(args: &ApplyArgs) -> ExitCode {
    let result = journal_directory(&args.journal).and_then(|dir| {
        let text = fs::read(&args.plan)
            .map_err(|err| format!("could not read {}: {err}", args.plan.display()))?;
        let plan: MovePlan = serde_json::from_slice(&text)
            .map_err(|err| format!("{} is not a suds.plan/v1 plan: {err}", args.plan.display()))?;
        let mut journal = Journal::open(&dir).map_err(|err| err.to_string())?;
        execute::apply(&mut journal, &plan, &mut |_| {}).map_err(|err| err.to_string())
    });
    finish_run(result)
}

pub fn undo_command(args: &UndoArgs) -> ExitCode {
    let result = journal_directory(&args.journal).and_then(|dir| {
        let mut journal = Journal::open(&dir).map_err(|err| err.to_string())?;
        execute::undo(&mut journal, &args.run, &mut |_| {}).map_err(|err| err.to_string())
    });
    finish_run(result)
}

/// Writes a run's result as one JSON document, and exits with status 0 only
/// when every move succeeded.
fn finish_run(result: Result<RunReport, String>) -> ExitCode {
    let report = match result {
        Ok(report) => report,
        Err(message) => return fail(&message),
    };
    let mut out = io::stdout().lock();
    let written = serde_json::to_writer_pretty(&mut out, &report)
        .map_err(io::Error::from)
        .and_then(|()| writeln!(out))
        .and_then(|()| out.flush());
    if let Err(err) = written {
        // The moves already happened, and the journal records them.
        eprintln!("suds: failed to write the result, which the journal records: {err}");
        return ExitCode::from(EXIT_INVOCATION);
    }
    match report.status {
        RunStatus::Complete => ExitCode::SUCCESS,
        RunStatus::Partial | RunStatus::Refused => ExitCode::from(EXIT_REFUSED),
    }
}

pub fn journal_command(args: &JournalArgs) -> ExitCode {
    let summaries = match journal_directory(&args.journal)
        .and_then(|dir| journal::read(&dir).map_err(|err| err.to_string()))
    {
        Ok(records) => JournalState::from_records(&records).summaries(),
        Err(message) => return fail(&message),
    };
    let mut out = io::stdout().lock();
    let written = match args.format {
        Format::Table => write_summaries(&mut out, &summaries),
        Format::Json => serde_json::to_writer_pretty(&mut out, &summaries)
            .map_err(io::Error::from)
            .and_then(|()| writeln!(out)),
    };
    match written.and_then(|()| out.flush()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => fail(&format!("failed to write the journal: {err}")),
    }
}

fn write_summaries(out: &mut impl Write, summaries: &[RunSummary]) -> io::Result<()> {
    if summaries.is_empty() {
        return writeln!(out, "The journal records no run.");
    }
    for summary in summaries {
        let status = summary.status.map_or("in progress", RunStatus::label);
        let interrupted = if summary.interrupted {
            ", interrupted"
        } else {
            ""
        };
        let undo = summary
            .undo
            .map_or_else(String::new, |undo| format!(", undo {}", undo.label()));
        writeln!(
            out,
            "{}  {}  {status}{interrupted}{undo}",
            summary.run, summary.started_at
        )?;
        for planned in &summary.moves {
            writeln!(
                out,
                "  {:<11}  {} -> {}",
                planned.status.label(),
                escape(&planned.source.display()),
                escape(&planned.destination.display())
            )?;
            if let Some(refusal) = &planned.refusal {
                writeln!(out, "               {}", escape(&refusal.message))?;
            }
        }
    }
    Ok(())
}

fn journal_directory(option: &JournalOption) -> Result<PathBuf, String> {
    match &option.directory {
        Some(dir) => {
            let cwd = working_directory([dir])?;
            Ok(roots::normalize(&cwd.join(dir)))
        }
        None => journal::default_directory().map_err(|err| err.to_string()),
    }
}

/// The working directory when any path is relative, and an empty path, to
/// which an absolute path joins unchanged, otherwise.
fn working_directory<'a>(paths: impl IntoIterator<Item = &'a PathBuf>) -> Result<PathBuf, String> {
    if paths.into_iter().all(|path| path.is_absolute()) {
        return Ok(PathBuf::new());
    }
    env::current_dir().map_err(|err| {
        format!("could not read the working directory to resolve a relative path: {err}")
    })
}

fn fail(message: &str) -> ExitCode {
    eprintln!("suds: {}", escape(message));
    ExitCode::from(EXIT_INVOCATION)
}

fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
        })
}

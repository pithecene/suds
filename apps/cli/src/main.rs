//! The Suds command-line interface.
//!
//! The `inspect` command reads filesystem metadata and modifies no file. The
//! `apply` and `undo` commands move only the entries that a recorded plan
//! names.

mod cli;
mod organize;
mod table;

use std::env;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::Parser;
use suds_core::inspection::{RootScan, build_report};
use suds_core::model::{Report, ReportStatus};
use suds_core::platform::{self, RootSpec};
use suds_core::{roots, scan};

use crate::cli::{Cli, Command, Format, InspectArgs, PlanCommand};

/// An existing root yielded a partial measurement or no measurement.
const EXIT_PARTIAL: u8 = 1;
/// The invocation failed before any report existed. Clap uses the same status
/// for a usage error.
const EXIT_INVOCATION: u8 = 2;

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Inspect(args) => inspect(&args),
        Command::Plan(PlanCommand::Move(args)) => organize::plan_move_command(&args),
        Command::Apply(args) => organize::apply_command(&args),
        Command::Journal(args) => organize::journal_command(&args),
        Command::Undo(args) => organize::undo_command(&args),
    }
}

fn inspect(args: &InspectArgs) -> ExitCode {
    let roots = match resolve_roots(&args.roots) {
        Ok(roots) => roots,
        Err(message) => {
            eprintln!("suds: {message}");
            return ExitCode::from(EXIT_INVOCATION);
        }
    };
    let observed_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
        });
    // The `suds` command accepts no cancellation request, so this
    // cancellation flag stays false.
    let never = AtomicBool::new(false);
    let scans: Vec<RootScan> = roots
        .iter()
        .map(|root| scan::scan_root(root, &never))
        .collect();
    let report = build_report(&scans, observed_at);

    let mut out = io::stdout().lock();
    let written = match args.format {
        Format::Table => table::write(&mut out, &report, args.top),
        Format::Json => write_json(&mut out, &report),
    };
    if let Err(err) = written.and_then(|()| out.flush()) {
        eprintln!("suds: failed to write the report: {err}");
        return ExitCode::from(EXIT_INVOCATION);
    }
    match report.status {
        ReportStatus::Complete => ExitCode::SUCCESS,
        ReportStatus::Partial => ExitCode::from(EXIT_PARTIAL),
    }
}

fn resolve_roots(paths: &[PathBuf]) -> Result<Vec<RootSpec>, String> {
    if paths.is_empty() {
        return platform::default_roots().map_err(|err| err.to_string());
    }
    // An absolute path joined to an empty base stays unchanged, so only a
    // relative root requires the working directory.
    let cwd = if paths.iter().all(|path| path.is_absolute()) {
        PathBuf::new()
    } else {
        env::current_dir().map_err(|err| {
            format!("could not read the working directory to resolve a relative root: {err}")
        })?
    };
    Ok(roots::explicit_roots(paths, &cwd))
}

fn write_json(out: &mut impl Write, report: &Report) -> io::Result<()> {
    serde_json::to_writer_pretty(&mut *out, report)?;
    writeln!(out)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::AtomicBool;

    use suds_core::inspection::build_report;
    use suds_core::model::{Report, SCHEMA};
    use suds_core::scan::scan_root;

    use super::{resolve_roots, write_json};

    #[test]
    fn json_parses_back_into_the_report_type() {
        let dir = tempfile::TempDir::new().unwrap();
        fs::write(dir.path().join("file"), b"data").unwrap();
        let roots = resolve_roots(&[dir.path().to_path_buf()]).unwrap();
        let never = AtomicBool::new(false);
        let scans: Vec<_> = roots.iter().map(|root| scan_root(root, &never)).collect();
        let report = build_report(&scans, 0);

        let mut json = Vec::new();
        write_json(&mut json, &report).unwrap();
        let parsed: Report = serde_json::from_slice(&json).unwrap();
        assert_eq!(parsed, report);
        assert_eq!(parsed.schema, SCHEMA);
    }
}

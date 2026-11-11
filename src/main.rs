//! Suds, a local-first maintenance and reconciliation utility for developer
//! workstations.
//!
//! This release observes filesystem metadata and modifies nothing.

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
compile_error!("Suds supports macOS and Linux only.");

mod cli;
mod model;
mod platform;
mod scan;
mod table;
mod timestamp;

use std::env;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::Parser;

use crate::cli::{Cli, Command, Format, InspectArgs};
use crate::model::{Platform, Report, ReportStatus, RootKind, RootStatus, SCHEMA};
use crate::platform::RootSpec;

/// An existing root yielded a partial measurement or no measurement.
const EXIT_PARTIAL: u8 = 1;
/// The invocation failed before any report existed. Clap uses the same status
/// for a usage error.
const EXIT_INVOCATION: u8 = 2;

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Inspect(args) => inspect(&args),
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
    let report = build_report(&roots);

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
    let mut roots = Vec::with_capacity(paths.len());
    for (index, path) in paths.iter().enumerate() {
        let absolute = if path.is_absolute() {
            path.clone()
        } else {
            let cwd = env::current_dir().map_err(|err| {
                format!("could not read the working directory to resolve a relative root: {err}")
            })?;
            cwd.join(path)
        };
        roots.push(RootSpec {
            id: format!("explicit.{}", index + 1),
            kind: RootKind::Unclassified,
            path: normalize(&absolute),
        });
    }
    Ok(roots)
}

/// Removes `.` components and resolves `..` components lexically.
///
/// Like the scan, lexical resolution follows no symbolic link.
fn normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn build_report(roots: &[RootSpec]) -> Report {
    let observed_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
        });
    let results: Vec<_> = roots.iter().map(scan::scan_root).collect();
    let status = if results
        .iter()
        .all(|root| matches!(root.status, RootStatus::Complete | RootStatus::Absent))
    {
        ReportStatus::Complete
    } else {
        ReportStatus::Partial
    };
    Report {
        schema: SCHEMA.to_owned(),
        platform: Platform {
            os: env::consts::OS.to_owned(),
            arch: env::consts::ARCH.to_owned(),
        },
        observed_at: timestamp::format_utc(observed_at),
        status,
        roots: results,
    }
}

fn write_json(out: &mut impl Write, report: &Report) -> io::Result<()> {
    serde_json::to_writer_pretty(&mut *out, report)?;
    writeln!(out)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::{build_report, normalize, resolve_roots, write_json};
    use crate::model::{Report, SCHEMA};

    #[test]
    fn json_parses_back_into_the_report_type() {
        let dir = tempfile::TempDir::new().unwrap();
        fs::write(dir.path().join("file"), b"data").unwrap();
        let roots = resolve_roots(&[dir.path().to_path_buf()]).unwrap();
        let report = build_report(&roots);

        let mut json = Vec::new();
        write_json(&mut json, &report).unwrap();
        let parsed: Report = serde_json::from_slice(&json).unwrap();
        assert_eq!(parsed, report);
        assert_eq!(parsed.schema, SCHEMA);
    }

    #[test]
    fn normalizes_lexically() {
        assert_eq!(normalize(Path::new("/a/./b/../c/")), Path::new("/a/c"));
        assert_eq!(normalize(Path::new("/../a")), Path::new("/a"));
        assert_eq!(
            normalize(Path::new("/tmp/with space/x")),
            Path::new("/tmp/with space/x")
        );
    }
}

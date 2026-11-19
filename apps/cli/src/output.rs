//! The output of a plan or of a run's result.

use std::io::{self, StdoutLock, Write};
use std::process::ExitCode;

use serde::Serialize;

use crate::cli::Format;
use crate::{EXIT_INVOCATION, EXIT_PARTIAL};

/// Writes `document` to standard output as a table or as one JSON document.
pub fn emit<T: Serialize>(
    format: Format,
    document: &T,
    table: impl FnOnce(&mut StdoutLock<'static>, &T) -> io::Result<()>,
) -> io::Result<()> {
    let mut out = io::stdout().lock();
    match format {
        Format::Table => table(&mut out, document)?,
        Format::Json => {
            serde_json::to_writer_pretty(&mut out, document)?;
            writeln!(out)?;
        }
    }
    out.flush()
}

/// The exit status of a command that printed a plan and changed nothing.
pub fn planned(written: io::Result<()>) -> Result<ExitCode, String> {
    written
        .map(|()| ExitCode::SUCCESS)
        .map_err(|err| format!("failed to write the plan: {err}"))
}

/// The exit status of a run whose changes the journal already records.
/// The status is 1 when the run left a part of its plan undone.
pub fn ran(written: io::Result<()>, partial: bool) -> Result<ExitCode, String> {
    written
        .map_err(|err| format!("failed to write the result, which the journal records: {err}"))?;
    Ok(if partial {
        ExitCode::from(EXIT_PARTIAL)
    } else {
        ExitCode::SUCCESS
    })
}

/// Prints a command's failure and returns the status of a failed invocation.
pub fn finish(result: Result<ExitCode, String>) -> ExitCode {
    result.unwrap_or_else(|message| {
        eprintln!("suds: {message}");
        ExitCode::from(EXIT_INVOCATION)
    })
}

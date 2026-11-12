//! The command-line interface.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

/// Local-first environment manager for developer workstations.
///
/// This release measures storage and changes nothing.
#[derive(Debug, Parser)]
#[command(name = "suds", version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Measure the storage of user-owned roots without modifying them.
    ///
    /// Without `--root`, Suds inspects the macOS profile: the user's caches,
    /// logs, Xcode derived data, downloads, and trash.
    Inspect(InspectArgs),
}

#[derive(Debug, Args)]
pub struct InspectArgs {
    /// The output format.
    #[arg(long, value_enum, default_value_t = Format::Table)]
    pub format: Format,

    /// A root to inspect in place of the default profile. Repeat the option
    /// for several roots.
    #[arg(long = "root", value_name = "PATH")]
    pub roots: Vec<PathBuf>,

    /// The number of direct children that the table lists for each root. The
    /// JSON output lists every child.
    #[arg(long, value_name = "N", default_value_t = 10)]
    pub top: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Format {
    Table,
    Json,
}

//! The command-line interface.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

/// Local-first environment manager for developer workstations.
///
/// Suds measures storage and moves entries only through a reviewed plan,
/// which the journal records and `suds undo` reverses.
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

    /// Build a plan and write it to a file, without moving anything.
    #[command(subcommand)]
    Plan(PlanCommand),

    /// Apply a plan that `suds plan` built, and write the run's result as JSON.
    Apply(ApplyArgs),

    /// List the runs that the journal records, most recent first.
    Journal(JournalArgs),

    /// Return each entry that a run moved to its original path.
    Undo(UndoArgs),

    /// Plan the removal of build state that no process uses, and remove it
    /// with `--apply`. A build regenerates build state, so its removal is
    /// permanent.
    Reclaim(ReclaimArgs),
}

#[derive(Debug, Args)]
pub struct ReclaimArgs {
    /// A directory beneath which Suds looks for checkouts. Repeat the option
    /// for several directories.
    #[arg(long = "root", value_name = "DIR", required = true)]
    pub roots: Vec<PathBuf>,

    /// The time for which a directory must have stood unchanged.
    #[arg(long, value_name = "MINUTES", default_value_t = 60)]
    pub idle_minutes: u64,

    /// Consider each linked checkout too. Suds has Git remove one only when
    /// its tree is clean and a remote holds every commit of its head.
    #[arg(long)]
    pub checkouts: bool,

    /// Remove what the plan names. Without the option, Suds prints the plan
    /// and changes nothing.
    #[arg(long)]
    pub apply: bool,

    /// Proceed while the system hides the working directory of a process of
    /// this user. Without the option, such a process keeps every directory.
    #[arg(long)]
    pub tolerate_unreadable_processes: bool,

    /// The output format.
    #[arg(long, value_enum, default_value_t = Format::Table)]
    pub format: Format,

    #[command(flatten)]
    pub journal: JournalOption,
}

#[derive(Debug, Subcommand)]
pub enum PlanCommand {
    /// Plan to move direct children of one directory into another directory
    /// on the same filesystem.
    Move(PlanMoveArgs),
}

#[derive(Debug, Args)]
pub struct PlanMoveArgs {
    /// An entry to move. Repeat the option for several entries, which must
    /// share one parent directory.
    #[arg(long = "from", value_name = "PATH", required = true)]
    pub sources: Vec<PathBuf>,

    /// The directory that receives the entries.
    #[arg(long = "to", value_name = "DIR")]
    pub destination: PathBuf,

    /// The file to which Suds writes the plan. Suds refuses to replace an
    /// existing file.
    #[arg(long, value_name = "FILE")]
    pub output: PathBuf,

    #[command(flatten)]
    pub journal: JournalOption,
}

#[derive(Debug, Args)]
pub struct ApplyArgs {
    /// The plan file that `suds plan` wrote.
    #[arg(value_name = "PLAN")]
    pub plan: PathBuf,

    #[command(flatten)]
    pub journal: JournalOption,
}

#[derive(Debug, Args)]
pub struct JournalArgs {
    /// The output format.
    #[arg(long, value_enum, default_value_t = Format::Table)]
    pub format: Format,

    #[command(flatten)]
    pub journal: JournalOption,
}

#[derive(Debug, Args)]
pub struct UndoArgs {
    /// The run identifier that `suds apply` and `suds journal` show.
    #[arg(value_name = "RUN")]
    pub run: String,

    #[command(flatten)]
    pub journal: JournalOption,
}

#[derive(Debug, Args)]
pub struct JournalOption {
    /// The journal directory. On macOS, the default is
    /// ~/Library/Application Support/Suds/journal. Linux has no default.
    #[arg(long = "journal", value_name = "DIR")]
    pub directory: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct InspectArgs {
    /// The output format.
    #[arg(long, value_enum, default_value_t = Format::Table)]
    pub format: Format,

    /// The profile. The storage profile measures each root. The development
    /// profile finds the checkouts beneath each root and measures them with
    /// their build state, and it measures each shared store and toolchain.
    #[arg(long, value_enum, default_value_t = Profile::Storage)]
    pub profile: Profile,

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
pub enum Profile {
    Storage,
    Development,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Format {
    Table,
    Json,
}

//! The command-line interface.

use std::ffi::OsString;
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

    /// Plan each shared store's trim to its budget, and run the trims with
    /// `--apply`. Each trim is a command of the store's own program.
    Stores(StoresArgs),

    /// Record that a client holds a checkout. Suds removes nothing beneath
    /// a checkout that a client registered and has not released.
    Register(HoldArgs),

    /// Record that a client released a checkout that it registered.
    Release(HoldArgs),

    /// Print the last pressure reading that `suds maintain` recorded.
    Pressure(PressureArgs),

    /// Measure the development storage and record its pressure. Under the
    /// settings' standing approval, also reclaim and trim the stores.
    Maintain(MaintainArgs),

    /// Print the units that run `suds maintain` on a schedule.
    Schedule(ScheduleArgs),

    /// Report the ceiling of development storage, whether a filesystem
    /// enforces it, and the commands that would make one enforce it.
    Arena(ArenaArgs),
}

#[derive(Debug, Args)]
pub struct ArenaArgs {
    /// The directory of the development tree. Without the option, Suds
    /// audits the first root of the settings.
    #[arg(long = "root", value_name = "DIR")]
    pub root: Option<PathBuf>,

    /// The output format.
    #[arg(long, value_enum, default_value_t = Format::Table)]
    pub format: Format,
}

#[derive(Debug, Args)]
pub struct HoldArgs {
    /// The checkout's directory.
    #[arg(long, value_name = "DIR")]
    pub checkout: PathBuf,

    /// The client's name, such as `bosun`.
    #[arg(long, value_name = "NAME")]
    pub client: String,

    /// The client's own name for the work, such as a workflow's identifier.
    #[arg(long, value_name = "ID")]
    pub attempt: Option<String>,
}

#[derive(Debug, Args)]
pub struct PressureArgs {
    /// The output format.
    #[arg(long, value_enum, default_value_t = Format::Table)]
    pub format: Format,
}

#[derive(Debug, Args)]
pub struct MaintainArgs {
    /// The output format.
    #[arg(long, value_enum, default_value_t = Format::Table)]
    pub format: Format,
}

#[derive(Debug, Args)]
pub struct ScheduleArgs {
    /// The scheduler whose units Suds prints.
    #[arg(long, value_enum)]
    pub scheduler: Scheduler,

    /// The time between two runs.
    #[arg(long, value_name = "MINUTES", default_value_t = 30)]
    pub minutes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Scheduler {
    Systemd,
    Launchd,
}

#[derive(Debug, Args)]
pub struct StoresArgs {
    /// A store's budget, as in `go.build-cache=10G`. Repeat the option for
    /// several stores.
    #[arg(long = "budget", value_name = "STORE=SIZE")]
    pub budgets: Vec<OsString>,

    /// The time for which a store must have stood unchanged.
    #[arg(long, value_name = "MINUTES", default_value_t = 60)]
    pub idle_minutes: u64,

    /// Run the trims that the plan names. Without the option, Suds prints
    /// the plan and changes nothing.
    #[arg(long)]
    pub apply: bool,

    /// The output format.
    #[arg(long, value_enum, default_value_t = Format::Table)]
    pub format: Format,

    #[command(flatten)]
    pub journal: JournalOption,
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
    /// The journal directory. The default lies in the user's state
    /// directory: ~/Library/Application Support/Suds/journal on macOS, and
    /// ~/.local/state/suds/journal on Linux.
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

//! The `seed` command.

use std::io::{self, Write};
use std::process::ExitCode;

use suds_core::organize::journal::Journal;
use suds_core::reclaim::git::Git;
use suds_core::seed::{self, SeedPlan, SeedReport};

use crate::cli::SeedArgs;
use crate::organize::journal_directory;
use crate::output::{emit, finish, planned, ran};
use crate::table::{escape, format_bytes};
use crate::workspaces_of;

pub fn seed_command(args: &SeedArgs) -> ExitCode {
    finish(run(args))
}

fn run(args: &SeedArgs) -> Result<ExitCode, String> {
    let named: Vec<_> = std::iter::once(args.checkout.clone())
        .chain(args.from.clone())
        .collect();
    let resolved = workspaces_of(&named)?;
    let destination = &resolved[0];
    let plan = seed::plan_seed(
        destination,
        resolved.get(1).map(|from| from.as_path()),
        &Git::default(),
    )?;
    if !args.apply {
        return planned(emit(args.format, &plan, write_plan));
    }
    let directory = journal_directory(&args.journal)?;
    let mut journal = Journal::open(&directory).map_err(|err| err.to_string())?;
    let report =
        seed::apply(&mut journal, &plan, args.client.as_deref()).map_err(|err| err.to_string())?;
    let refused = report.clones.iter().any(|clone| !clone.cloned);
    ran(emit(args.format, &report, write_report), refused)
}

fn write_plan(out: &mut impl Write, plan: &SeedPlan) -> io::Result<()> {
    for clone in &plan.clones {
        writeln!(
            out,
            "clone  {:>10}  {}",
            format_bytes(clone.shared_bytes),
            escape(&clone.to.display())
        )?;
        writeln!(
            out,
            "       {:>10}  from {}",
            "",
            escape(&clone.from.display())
        )?;
    }
    for passed in &plan.passed {
        writeln!(
            out,
            "pass   {:>10}  {}: {}",
            "-",
            escape(&passed.path.display()),
            escape(&passed.reason)
        )?;
    }
    if plan.clones.is_empty() {
        return writeln!(
            out,
            "No checkout of the repository offers build state that {} can take. Suds changed nothing.",
            escape(&plan.destination.display())
        );
    }
    writeln!(
        out,
        "The plan clones {} directories, which share about {} with their sources. Suds changed nothing. Run the command with --apply to clone them.",
        plan.clones.len(),
        format_bytes(plan.shared_bytes())
    )
}

fn write_report(out: &mut impl Write, report: &SeedReport) -> io::Result<()> {
    for clone in &report.clones {
        let path = escape(&clone.to.display());
        let line = match &clone.refusal {
            None => format!("cloned  {:>10}  {path}", format_bytes(clone.shared_bytes)),
            Some(reason) => format!("passed  {:>10}  {path}: {}", "-", escape(reason)),
        };
        writeln!(out, "{line}")?;
    }
    writeln!(
        out,
        "Run {} cloned build state that shares about {} with its source. The journal records each clone.",
        report.run,
        format_bytes(report.shared_bytes)
    )
}

//! The `stores` command.

use std::io::{self, Write};
use std::process::ExitCode;

use suds_core::development::{self, Environment};
use suds_core::organize::journal::Journal;
use suds_core::reclaim::execute::Authority;
use suds_core::stores::{
    self, Budgets, Decision, Entry, Inputs, StoresPlan, StoresReport, builder_budget_option,
    engine_usage, measure_after, parse_budget, plan_stores,
};

use crate::cli::StoresArgs;
use crate::now_seconds;
use crate::organize::journal_directory;
use crate::output::{emit, finish, planned, ran};
use crate::table::{escape, format_bytes, write_rows};

pub fn stores_command(args: &StoresArgs) -> ExitCode {
    finish(run(args))
}

fn run(args: &StoresArgs) -> Result<ExitCode, String> {
    let mut budgets = Budgets::default();
    for word in &args.budgets {
        let (id, bytes) = parse_budget(word)?;
        budgets.set(&id, bytes);
    }
    let programs = crate::clients::resolved_programs();
    // An inspection without a workspace measures the stores alone, and each
    // root adds what its checkouts hold.
    let workspaces = crate::workspaces_of(&args.roots)?;
    let scan = development::inspect(&workspaces, &Environment::of_process());
    let engine = engine_usage(&programs.docker);
    let inputs = Inputs {
        budgets: &budgets,
        programs: &programs,
        engine: &engine,
        builder_budget_option: builder_budget_option(&programs.docker),
        idle_seconds: args.idle_minutes.saturating_mul(60),
        now: now_seconds(),
    };
    let plan = plan_stores(&scan, &inputs);

    if !args.apply {
        return planned(emit(args.format, &plan, write_plan));
    }

    let directory = journal_directory(&args.journal)?;
    let mut journal = Journal::open(&directory).map_err(|err| err.to_string())?;
    let report = stores::apply(&mut journal, &plan, Authority::Person, &mut |entry| {
        measure_after(entry, &programs.docker)
    })
    .map_err(|err| err.to_string())?;
    let failed = report.trims.iter().any(|trim| trim.failure.is_some());
    let written = emit(args.format, &report, write_report);
    ran(written, failed)
}

/// One store's row of the plan's table.
fn plan_row(entry: &Entry) -> Vec<String> {
    let (decision, detail) = match &entry.decision {
        Decision::Within => ("within", String::new()),
        Decision::Trim { trim } => ("trim", trim.display()),
        Decision::Evict { eviction } => ("evict", eviction.display()),
        Decision::Audit { reason } => ("audit", reason.clone()),
        Decision::Keep { reason } => ("keep", reason.clone()),
        Decision::AwaitsPerson { names } => (
            "awaits a person",
            format!("{} named volumes that no container uses", names.len()),
        ),
    };
    let budget = entry
        .budget_bytes
        .map_or_else(|| "-".to_owned(), format_bytes);
    vec![
        entry.id.clone(),
        entry
            .measured_bytes
            .map_or_else(|| "-".to_owned(), format_bytes),
        budget,
        decision.to_owned(),
        escape(&detail),
    ]
}

fn write_plan(out: &mut impl Write, plan: &StoresPlan) -> io::Result<()> {
    let header = ["STORE", "MEASURED", "BUDGET", "DECISION", "DETAIL"].map(str::to_owned);
    let rows: Vec<Vec<String>> = std::iter::once(header.to_vec())
        .chain(plan.entries.iter().map(plan_row))
        .collect();
    write_rows(out, "", &rows, &[false, true, true, false, false])?;
    let trims = plan
        .entries
        .iter()
        .filter(|entry| {
            matches!(
                entry.decision,
                Decision::Trim { .. } | Decision::Evict { .. }
            )
        })
        .count();
    writeln!(out)?;
    writeln!(
        out,
        "The plan runs {trims} trims, each through the store's own program, except that Suds evicts from Go's build cache itself. Suds changed nothing. Run the command with --apply to run them."
    )
}

fn write_report(out: &mut impl Write, report: &StoresReport) -> io::Result<()> {
    for trim in &report.trims {
        match (&trim.failure, trim.after_bytes) {
            (None, Some(after)) => writeln!(
                out,
                "trimmed  {}  {} -> {}  {}",
                trim.store,
                format_bytes(trim.before_bytes),
                format_bytes(after),
                escape(&trim.command)
            )?,
            (failure, _) => writeln!(
                out,
                "failed   {}  {}: {}",
                trim.store,
                escape(&trim.command),
                escape(failure.as_deref().unwrap_or("the store has no measure"))
            )?,
        }
    }
    writeln!(
        out,
        "Run {} freed about {}. The journal records each trim.",
        report.run,
        format_bytes(report.freed_bytes)
    )
}

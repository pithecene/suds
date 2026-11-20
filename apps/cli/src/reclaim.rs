//! The `reclaim` command.

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use suds_core::development::{self, Environment};
use suds_core::organize::journal::Journal;
use suds_core::reclaim::execute::{self, Authority, Conditions, ReclaimReport};
use suds_core::reclaim::git::Git;
use suds_core::reclaim::liveness::Liveness;
use suds_core::reclaim::plan::{
    Candidate, CheckoutScope, Decision, Observed, Policy, ReclaimPlan, plan_reclaim,
};
use suds_core::registry::{self, Reading, Standings};
use suds_core::state::Directories;

use crate::cli::ReclaimArgs;
use crate::organize::journal_directory;
use crate::output::{emit, finish, planned, ran};
use crate::table::{escape, format_bytes, write_rows};
use crate::{now_seconds, resolve_roots};

pub fn reclaim_command(args: &ReclaimArgs) -> ExitCode {
    finish(run(args))
}

fn run(args: &ReclaimArgs) -> Result<ExitCode, String> {
    let workspaces: Vec<PathBuf> = resolve_roots(&args.roots)?
        .into_iter()
        .map(|root| root.path)
        .collect();
    // An environment without a home directory names no store, so the
    // inspection measures the checkouts alone.
    let scan = development::inspect(&workspaces, &Environment::default());
    let git = Git::default();
    let policy = Policy {
        idle_seconds: args.idle_minutes.saturating_mul(60),
        checkouts: if args.checkouts {
            CheckoutScope::AllLinked
        } else {
            CheckoutScope::Released
        },
        tolerate_unreadable_processes: args.tolerate_unreadable_processes,
    };
    let observed = Observed {
        liveness: &Liveness::observe(),
        registry: &standings(),
    };
    let plan = plan_reclaim(&scan, &observed, &git, policy, now_seconds());

    if !args.apply {
        return planned(emit(args.format, &plan, write_plan));
    }

    let directory = journal_directory(&args.journal)?;
    let mut journal = Journal::open(&directory).map_err(|err| err.to_string())?;
    let conditions = Conditions {
        authority: Authority::Person,
        observe: &mut Liveness::observe,
        registry: &mut standings,
        git: &git,
        now: now_seconds(),
    };
    let report = execute::apply(&mut journal, &plan, conditions).map_err(|err| err.to_string())?;
    let refused = report.removals.iter().any(|removal| !removal.removed);
    let written = emit(args.format, &report, write_report);
    ran(written, refused)
}

/// The registry's standing of each checkout. A process without a home
/// directory has no registry, and a registry that Suds cannot read is an
/// error, which keeps every candidate.
fn standings() -> Reading {
    match Directories::of(&Environment::of_process()) {
        Some(directories) => registry::reading(&directories.registry()),
        None => Ok(Standings::new()),
    }
}

fn write_plan(out: &mut impl Write, plan: &ReclaimPlan) -> io::Result<()> {
    let mut rows = vec![
        ["DECISION", "KIND", "ALLOCATED", "PATH", "REASON"]
            .map(str::to_owned)
            .to_vec(),
    ];
    let mut removable = 0_usize;
    for candidate in &plan.candidates {
        let decision = match candidate.decision {
            Decision::Remove => {
                removable += 1;
                "remove"
            }
            Decision::Keep => "keep",
        };
        rows.push(vec![
            decision.to_owned(),
            candidate.subject.label().to_owned(),
            format_bytes(candidate.allocated_bytes),
            escape(&candidate.path.display()),
            escape(&reason(candidate)),
        ]);
    }
    write_rows(out, "", &rows, &[false, false, true, false, false])?;
    writeln!(out)?;
    writeln!(
        out,
        "The plan removes {removable} directories and frees about {}. Suds changed nothing. Run the command with --apply to remove them.",
        format_bytes(plan.removable_bytes())
    )
}

/// Why the plan decided as it did: the last fact of its evidence, behind
/// the reason's name for a kept candidate. A kept candidate's last fact is
/// the keeping one.
fn reason(candidate: &Candidate) -> String {
    let fact = candidate.evidence.last().map_or("", String::as_str);
    match candidate.kept {
        Some(kept) => format!("{}: {fact}", kept.label()),
        None => fact.to_owned(),
    }
}

fn write_report(out: &mut impl Write, report: &ReclaimReport) -> io::Result<()> {
    for removal in &report.removals {
        match &removal.refusal {
            None => writeln!(
                out,
                "removed  {:>10}  {}",
                format_bytes(removal.freed_bytes),
                escape(&removal.path.display())
            )?,
            Some(reason) => writeln!(
                out,
                "kept     {:>10}  {}: {}",
                "-",
                escape(&removal.path.display()),
                escape(reason)
            )?,
        }
    }
    writeln!(
        out,
        "Run {} freed about {}. The journal records each removal.",
        report.run,
        format_bytes(report.freed_bytes)
    )
}

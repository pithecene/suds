//! One unattended run: a measurement, and a reclamation where the owner's
//! settings hold the standing approval.
//!
//! A schedule starts this run with no person present. The run always
//! measures the development storage and records the pressure. It removes
//! derived state and trims stores only under the standing approval of
//! decision 0002, and the journal records that authority with each run.

use std::fs;
use std::path::Path;

use serde::Serialize;

use crate::arena;
use crate::development::{self, Environment};
use crate::organize::journal::Journal;
use crate::pressure::{self, Measurement, Pressure, Thresholds};
use crate::reclaim::execute::{self, Authority, Conditions, ReclaimReport};
use crate::reclaim::git::Git;
use crate::reclaim::liveness::Liveness;
use crate::reclaim::plan::{Observed, Policy, plan_reclaim};
use crate::registry;
use crate::state::{Directories, Settings};
use crate::stores::{self, Budgets, Inputs, Programs, StoresReport};

/// The schema tag of a maintenance run's result.
pub const MAINTENANCE_SCHEMA: &str = "suds.maintenance/v1";

/// The result of one maintenance run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MaintenanceReport {
    pub schema: String,
    pub pressure: Pressure,
    /// Whether the settings hold the owner's standing approval.
    pub standing_approval: bool,
    pub reclaim: Option<ReclaimReport>,
    pub stores: Option<StoresReport>,
}

/// What a maintenance run reads beside the settings.
pub struct Context<'a> {
    pub directories: &'a Directories,
    pub environment: &'a Environment,
    pub programs: &'a Programs,
    pub git: &'a Git,
    /// The time of the run, in seconds since the Unix epoch.
    pub now: i64,
}

/// Measures the development storage, records the pressure, and reclaims
/// under the standing approval.
pub fn maintain(settings: &Settings, context: &Context<'_>) -> Result<MaintenanceReport, String> {
    let first = settings
        .roots
        .first()
        .ok_or("the settings name no root, so Suds has no development storage to measure")?;
    let scan = development::inspect(&settings.roots, context.environment);
    let audited = arena::audit(first, settings.ceiling_bytes)?;
    let measurement = Measurement {
        usage_bytes: pressure::usage(&scan),
        ceiling_bytes: audited.ceiling_bytes,
        enforced: audited.enforced,
        filesystem_available_bytes: Some(audited.available_bytes),
        now: context.now,
    };
    let path = context.directories.pressure();
    let reading = pressure::next(
        read_pressure(&path).as_ref(),
        measurement,
        Thresholds::default(),
    );
    write_pressure(&path, &reading)?;
    let mut report = MaintenanceReport {
        schema: MAINTENANCE_SCHEMA.to_owned(),
        pressure: reading,
        standing_approval: settings.standing_approval,
        reclaim: None,
        stores: None,
    };
    if !settings.standing_approval {
        return Ok(report);
    }

    let idle_seconds = settings.idle_minutes.saturating_mul(60);
    let registry_path = context.directories.registry();
    let standings = registry::standings(&registry::read(&registry_path)?);
    let policy = Policy {
        idle_seconds,
        checkouts: false,
        tolerate_unreadable_processes: settings.tolerate_unreadable_processes,
    };
    let observed = Observed {
        liveness: &Liveness::observe(),
        registry: &standings,
    };
    let plan = plan_reclaim(&scan, &observed, context.git, policy, context.now);
    let mut journal =
        Journal::open(&context.directories.journal()).map_err(|err| err.to_string())?;
    let conditions = Conditions {
        authority: Authority::StandingApproval,
        observe: &mut Liveness::observe,
        // A registry that Suds cannot read keeps nothing by itself, and the
        // other rules still judge each removal.
        registry: &mut || {
            registry::read(&registry_path)
                .map(|records| registry::standings(&records))
                .unwrap_or_default()
        },
        git: context.git,
        now: context.now,
    };
    report.reclaim =
        Some(execute::apply(&mut journal, &plan, conditions).map_err(|err| err.to_string())?);

    let mut budgets = Budgets::default();
    for (id, bytes) in &settings.budgets {
        budgets.set(id, *bytes);
    }
    let engine = stores::engine_usage(&context.programs.docker);
    let inputs = Inputs {
        budgets: &budgets,
        programs: context.programs,
        engine: &engine,
        builder_budget_option: stores::builder_budget_option(&context.programs.docker),
        idle_seconds,
        now: context.now,
    };
    let stores_plan = stores::plan_stores(&scan, &inputs);
    let docker = &context.programs.docker;
    report.stores = Some(
        stores::apply(
            &mut journal,
            &stores_plan,
            Authority::StandingApproval,
            &mut |entry| stores::measure_after(entry, docker),
        )
        .map_err(|err| err.to_string())?,
    );
    Ok(report)
}

/// The last reading that a maintenance run recorded, or none.
pub fn read_pressure(path: &Path) -> Option<Pressure> {
    let text = fs::read(path).ok()?;
    serde_json::from_slice(&text).ok()
}

/// Records a reading. The new file replaces the old one in one rename, so a
/// reader never sees a part of a reading.
fn write_pressure(path: &Path, reading: &Pressure) -> Result<(), String> {
    let failed = |err: std::io::Error| format!("could not write {}: {err}", path.display());
    if let Some(directory) = path.parent() {
        fs::create_dir_all(directory).map_err(failed)?;
    }
    let text = serde_json::to_vec_pretty(reading).map_err(|err| failed(err.into()))?;
    let staged = path.with_extension("json.new");
    fs::write(&staged, text).map_err(failed)?;
    fs::rename(&staged, path).map_err(failed)
}

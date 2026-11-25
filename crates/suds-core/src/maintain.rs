//! One unattended run: a measurement, and a reclamation where the owner's
//! settings hold the standing approval.
//!
//! A schedule starts this run with no person present. The run always
//! measures the development storage and records the pressure. It removes
//! derived state and eligible landed checkouts and trims stores under the
//! standing approval of decision 0002. The journal records that authority.

use std::fs;
use std::path::Path;
use std::time::Instant;

use serde::Serialize;

use crate::arena;
use crate::development::{self, Environment};
use crate::model::{ReportStatus, TraversalError};
use crate::organize::journal::Journal;
use crate::pressure::{self, Measurement, Pressure, Thresholds};
use crate::reclaim::execute::{self, Authority, Conditions, ReclaimReport};
use crate::reclaim::git::Git;
use crate::reclaim::liveness::Liveness;
use crate::reclaim::plan::{CheckoutScope, Observed, Policy, plan_reclaim};
use crate::registry;
use crate::state::{Directories, Settings, write_staged};
use crate::stores::{self, Budgets, Inputs, Programs, StoresReport};

/// The schema tag of a maintenance run's result.
pub const MAINTENANCE_SCHEMA: &str = "suds.maintenance/v1";

/// The result of one maintenance run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MaintenanceReport {
    pub schema: String,
    pub pressure: Pressure,
    /// The entries that the inspection could not read. A count above zero
    /// makes the pressure's usage a lower bound.
    pub error_count: u64,
    /// The first of those entries, in traversal order.
    pub errors: Vec<TraversalError>,
    /// Whether the settings hold the owner's standing approval.
    pub standing_approval: bool,
    /// Observed free bytes on the primary filesystem before maintenance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filesystem_available_before_bytes: Option<u64>,
    /// Observed free bytes on the primary filesystem after maintenance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filesystem_available_after_bytes: Option<u64>,
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
    let started = Instant::now();
    let mut clock = || {
        context
            .now
            .saturating_add(i64::try_from(started.elapsed().as_secs()).unwrap_or(0))
    };
    maintain_with_clock(settings, context, &mut clock)
}

/// Like [`maintain`], reading observation timestamps from `clock` after
/// acquiring the maintenance lock and after cleanup.
pub fn maintain_with_clock(
    settings: &Settings,
    context: &Context<'_>,
    clock: &mut dyn FnMut() -> i64,
) -> Result<MaintenanceReport, String> {
    let first = settings
        .roots
        .first()
        .ok_or("the settings name no root, so Suds has no development storage to measure")?;
    let path = context.directories.pressure();
    // Coordinate concurrent maintenance callers before scanning the workspace.
    let _maintain_lock = registry::lock(&path.with_extension("maintain.lock"))?;
    let scan = development::inspect(&settings.roots, context.environment);
    let initial_now = clock();
    let inspection = development::build_report(&scan, initial_now);
    // The pressure counts every root and every store, so the audit covers
    // each of them.
    let others: Vec<_> = settings
        .roots
        .iter()
        .skip(1)
        .cloned()
        .chain(development::store_locations(context.environment))
        .collect();
    let audited = arena::audit(first, &others, settings.ceiling_bytes)?;
    let available_before = Some(audited.available_bytes);
    let measurement = Measurement {
        usage_bytes: pressure::usage(&scan),
        ceiling_bytes: audited.ceiling_bytes,
        enforced: audited.enforced,
        filesystem_available_bytes: available_before,
        complete: inspection.status == ReportStatus::Complete,
        now: initial_now,
    };
    let reading = pressure::next(
        read_pressure(&path).as_ref(),
        measurement,
        Thresholds::default(),
    );
    write_pressure(&path, &reading)?;
    let mut report = MaintenanceReport {
        schema: MAINTENANCE_SCHEMA.to_owned(),
        pressure: reading.clone(),
        error_count: inspection.error_count,
        errors: inspection.errors,
        standing_approval: settings.standing_approval,
        filesystem_available_before_bytes: available_before,
        filesystem_available_after_bytes: available_before,
        reclaim: None,
        stores: None,
    };
    if !settings.standing_approval {
        return Ok(report);
    }

    let idle_seconds = settings.idle_minutes.saturating_mul(60);
    let registry_path = context.directories.registry();
    let standings = registry::reading(&registry_path);
    let policy = Policy {
        idle_seconds,
        // A checkout leaves only through a plan that a person approved.
        checkouts: CheckoutScope::DerivedOnly,
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
        registry: &mut || registry::reading(&registry_path),
        registry_path: Some(&registry_path),
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

    // Publish a post-maintenance observation from the same advancing clock so
    // recovery starts after cleanup without backdating the transition window.
    let post_scan = development::inspect(&settings.roots, context.environment);
    let post_now = clock();
    let post_inspection = development::build_report(&post_scan, post_now);
    let post_audited = arena::audit(first, &others, settings.ceiling_bytes)?;
    let available_after = Some(post_audited.available_bytes);
    let post_measurement = Measurement {
        usage_bytes: pressure::usage(&post_scan),
        ceiling_bytes: post_audited.ceiling_bytes,
        enforced: post_audited.enforced,
        filesystem_available_bytes: available_after,
        complete: post_inspection.status == ReportStatus::Complete,
        now: post_now,
    };
    let post_reading = pressure::next(Some(&reading), post_measurement, Thresholds::default());
    write_pressure(&path, &post_reading)?;
    report.pressure = post_reading;
    report.filesystem_available_after_bytes = available_after;
    if post_inspection.error_count > 0 {
        report.error_count = report
            .error_count
            .saturating_add(post_inspection.error_count);
        report.errors.extend(post_inspection.errors);
    }
    Ok(report)
}

/// The last reading that a maintenance run recorded, or none.
pub fn read_pressure(path: &Path) -> Option<Pressure> {
    let text = fs::read(path).ok()?;
    serde_json::from_slice(&text).ok()
}

/// Records a reading, as a file of Suds' own state.
fn write_pressure(path: &Path, reading: &Pressure) -> Result<(), String> {
    let failed = |err: std::io::Error| format!("could not write {}: {err}", path.display());
    let text = serde_json::to_vec_pretty(reading).map_err(|err| failed(err.into()))?;
    write_staged(path, &text).map_err(failed)
}

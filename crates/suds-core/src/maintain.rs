//! One unattended run: a measurement, and a reclamation where the owner's
//! settings hold the standing approval.
//!
//! A schedule starts this run with no person present. The run always
//! measures the development storage and records the pressure. It removes
//! derived state and eligible landed checkouts and trims stores under the
//! standing approval of decision 0002. The journal records that authority.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;

use crate::arena;
use crate::budget::{self, Budget, BudgetTracker, ExecutionOutcome};
use crate::development::{self, DevelopmentScan, Environment, StoreClass, StoreStatus};
use crate::model::{ReportStatus, TraversalError};
use crate::organize::journal::Journal;
use crate::pressure::{self, Level, Measurement, Pressure, Thresholds};
use crate::reclaim::execute::{self, Authority, Conditions, ReclaimReport};
use crate::reclaim::git::Git;
use crate::reclaim::liveness::Liveness;
use crate::reclaim::plan::{CheckoutScope, Observed, Policy, plan_reclaim};
use crate::registry;
use crate::state::{
    self, Directories, MAINTAIN_CURSOR_SCHEMA, MaintainCursor, Settings, write_staged,
};
use crate::stores::{self, Budgets, Inputs, Programs, StoresReport};
use crate::timestamp;

/// The schema tag of a maintenance run's result.
pub const MAINTENANCE_SCHEMA: &str = "suds.maintenance/v1";

/// The result of one maintenance run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MaintenanceReport {
    pub schema: String,
    pub pressure: Pressure,
    /// Execution outcome of this run (`complete`, `partial`, `cancelled`,
    /// `exhausted`, or `errored`).
    pub outcome: ExecutionOutcome,
    /// Whether durable pause state prevented unattended work from starting.
    pub paused: bool,
    /// Reason recorded in durable pause state, when paused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause_reason: Option<String>,
    /// Filesystem entries visited within this run's execution budget.
    pub entries_visited: u64,
    /// Oversized directories deferred rather than allocated in full.
    pub directories_deferred: u64,
    /// Background workers still draining when the run ended.
    pub draining_workers: usize,
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
    maintain_with_budget(settings, context, &Budget::for_maintenance(), clock)
}

fn paused_report(
    settings: &Settings,
    context: &Context<'_>,
    pressure_path: &Path,
) -> Result<Option<MaintenanceReport>, String> {
    let Some(pause) = state::read_pause(&context.directories.pause())? else {
        return Ok(None);
    };
    if !pause.paused {
        return Ok(None);
    }
    let cached = read_pressure(pressure_path).unwrap_or_else(|| Pressure {
        schema: pressure::PRESSURE_SCHEMA.to_owned(),
        observed_at: timestamp::format_utc(context.now),
        level: Level::Normal,
        lower_since: None,
        usage_bytes: 0,
        ceiling_bytes: settings.ceiling_bytes.unwrap_or(0),
        enforced: false,
        filesystem_available_bytes: None,
        complete: false,
    });
    let available = cached.filesystem_available_bytes;
    Ok(Some(MaintenanceReport {
        schema: MAINTENANCE_SCHEMA.to_owned(),
        pressure: cached,
        outcome: ExecutionOutcome::Cancelled,
        paused: true,
        pause_reason: pause.reason,
        entries_visited: 0,
        directories_deferred: 0,
        draining_workers: 0,
        error_count: 0,
        errors: Vec::new(),
        standing_approval: settings.standing_approval,
        filesystem_available_before_bytes: available,
        filesystem_available_after_bytes: available,
        reclaim: None,
        stores: None,
    }))
}

/// Like [`maintain_with_clock`], governed by an explicit execution `budget`.
/// Checks durable pause state before taking any lock or traversing storage.
pub fn maintain_with_budget(
    settings: &Settings,
    context: &Context<'_>,
    budget: &Budget,
    clock: &mut dyn FnMut() -> i64,
) -> Result<MaintenanceReport, String> {
    maintain_inner(settings, context, budget, clock, false)
}

/// Measures development storage without reclaiming or modifying durable pause
/// state, advancing the bounded maintenance cursor across slices until a full
/// reading completes.
pub fn maintain_observe(
    settings: &Settings,
    context: &Context<'_>,
) -> Result<MaintenanceReport, String> {
    let started = Instant::now();
    let mut clock = || {
        context
            .now
            .saturating_add(i64::try_from(started.elapsed().as_secs()).unwrap_or(0))
    };
    maintain_observe_with_budget(settings, context, &Budget::for_maintenance(), &mut clock)
}

/// Like [`maintain_observe`], governed by an explicit execution `budget`.
pub fn maintain_observe_with_budget(
    settings: &Settings,
    context: &Context<'_>,
    budget: &Budget,
    clock: &mut dyn FnMut() -> i64,
) -> Result<MaintenanceReport, String> {
    maintain_inner(settings, context, budget, clock, true)
}

fn completed_slice_usage(
    scan: &DevelopmentScan,
    completed_checkouts: &HashSet<PathBuf>,
    completed_stores: &HashSet<String>,
    count_workspaces: bool,
) -> u64 {
    let workspaces: u64 = if count_workspaces {
        let unclassified: u64 = scan
            .workspaces
            .iter()
            .map(|workspace| workspace.unclassified.allocated_bytes)
            .sum();
        let workspace_machines: u64 = scan
            .stores
            .iter()
            .filter(|store| {
                store.class == StoreClass::Machine
                    && !matches!(store.status, StoreStatus::Absent | StoreStatus::Unreadable)
                    && !completed_stores.contains(&store.id)
            })
            .map(|store| store.measure.allocated_bytes)
            .sum();
        unclassified.saturating_add(workspace_machines)
    } else {
        0
    };
    let checkouts: u64 = scan
        .checkouts
        .iter()
        .filter(|checkout| completed_checkouts.contains(&checkout.path))
        .map(|checkout| {
            checkout.own.allocated_bytes
                + checkout
                    .build_state
                    .iter()
                    .map(|build| build.measure.allocated_bytes)
                    .sum::<u64>()
        })
        .sum();
    let stores: u64 = scan
        .stores
        .iter()
        .filter(|store| {
            !matches!(store.status, StoreStatus::Absent | StoreStatus::Unreadable)
                && completed_stores.contains(&store.id)
        })
        .map(|store| store.measure.allocated_bytes)
        .sum();
    workspaces.saturating_add(checkouts).saturating_add(stores)
}

fn incomplete_slice_usage(
    scan: &DevelopmentScan,
    completed_checkouts: &HashSet<PathBuf>,
    completed_stores: &HashSet<String>,
    include_workspaces: bool,
) -> u64 {
    let workspaces: u64 = if include_workspaces {
        scan.workspaces
            .iter()
            .map(|workspace| workspace.unclassified.allocated_bytes)
            .sum()
    } else {
        0
    };
    let checkouts: u64 = scan
        .checkouts
        .iter()
        .filter(|checkout| !completed_checkouts.contains(&checkout.path))
        .map(|checkout| {
            checkout.own.allocated_bytes
                + checkout
                    .build_state
                    .iter()
                    .map(|build| build.measure.allocated_bytes)
                    .sum::<u64>()
        })
        .sum();
    let stores: u64 = scan
        .stores
        .iter()
        .filter(|store| {
            !matches!(store.status, StoreStatus::Absent | StoreStatus::Unreadable)
                && !completed_stores.contains(&store.id)
                && !(store.class == StoreClass::Machine && !include_workspaces)
        })
        .map(|store| store.measure.allocated_bytes)
        .sum();
    workspaces.saturating_add(checkouts).saturating_add(stores)
}

fn maintain_inner(
    settings: &Settings,
    context: &Context<'_>,
    budget: &Budget,
    clock: &mut dyn FnMut() -> i64,
    observe_only: bool,
) -> Result<MaintenanceReport, String> {
    let path = context.directories.pressure();
    // Check durable pause before acquiring any lock or scanning storage.
    // An unreadable or malformed pause record fails closed.
    if !observe_only && let Some(report) = paused_report(settings, context, &path)? {
        return Ok(report);
    }

    let first = settings
        .roots
        .first()
        .ok_or("the settings name no root, so Suds has no development storage to measure")?;
    // Coordinate concurrent maintenance callers before scanning the workspace.
    let _maintain_lock = registry::lock_bounded(
        &path.with_extension("maintain.lock"),
        &budget.cancel,
        std::time::Duration::from_secs(5),
        "maintain",
    )?;
    // Recheck durable pause after acquiring the lock so a pause written during
    // contention stops queued maintenance from scanning or reclaiming.
    if !observe_only && let Some(report) = paused_report(settings, context, &path)? {
        return Ok(report);
    }
    if budget.background_priority {
        budget::apply_background_thread_policy();
    }
    let tracker = BudgetTracker::new(budget.clone());
    let cursor_path = context.directories.maintain_cursor();
    let existing_cursor =
        state::read_maintain_cursor(&cursor_path).filter(|cursor| cursor.roots == settings.roots);
    let skip_set: HashSet<PathBuf> = existing_cursor
        .as_ref()
        .map(|cursor| cursor.completed_checkouts.iter().cloned().collect())
        .unwrap_or_default();
    let skip_stores: HashSet<String> = existing_cursor
        .as_ref()
        .map(|cursor| cursor.completed_stores.iter().cloned().collect())
        .unwrap_or_default();
    let prior_usage = existing_cursor
        .as_ref()
        .map_or(0, |cursor| cursor.accumulated_usage_bytes);
    let prior_workspaces_counted = existing_cursor
        .as_ref()
        .is_some_and(|cursor| cursor.workspaces_counted);
    let prior_had_errors = existing_cursor
        .as_ref()
        .is_some_and(|cursor| cursor.had_errors);
    let seed_pending: Vec<PathBuf> = existing_cursor
        .map(|cursor| cursor.pending_checkouts)
        .unwrap_or_default();
    // Reserve 40% of the unified entry budget for verification and bounded
    // reclamation under standing approval (survey and status checks visit each
    // candidate twice) without resetting the run's entry budget.
    let scan_entry_cap = if !observe_only && settings.standing_approval {
        budget
            .max_entries
            .filter(|limit| *limit >= 5)
            .map(|limit| (limit.saturating_mul(3) / 5).max(1))
    } else {
        None
    };
    let slice = development::inspect_with_tracker(
        &settings.roots,
        context.environment,
        Arc::clone(&tracker),
        &skip_set,
        &seed_pending,
        &skip_stores,
        scan_entry_cap,
    );
    let scan = slice.scan;
    let initial_now = clock();
    let inspection = development::build_report(&scan, initial_now);
    let mut completed_this_run = slice.completed_checkouts;
    let mut remaining_pending = slice.remaining_pending;
    let completed_stores_this_run = slice.completed_stores;
    let remaining_stores = slice.remaining_stores;
    let has_remaining = !remaining_pending.is_empty();
    let had_errors_any = prior_had_errors || inspection.error_count > 0;
    let slice_incomplete =
        slice.slice_capped || !scan.outcome.is_complete() || has_remaining || remaining_stores;
    let full_scan_complete =
        !slice_incomplete && !had_errors_any && inspection.status == ReportStatus::Complete;
    let single_pass_complete = full_scan_complete
        && skip_set.is_empty()
        && seed_pending.is_empty()
        && skip_stores.is_empty();
    let completed_checkout_set: HashSet<PathBuf> = completed_this_run.iter().cloned().collect();
    let completed_store_set: HashSet<String> = completed_stores_this_run.iter().cloned().collect();
    let count_workspaces = !prior_workspaces_counted && slice.workspaces_complete;
    let slice_completed_bytes = completed_slice_usage(
        &scan,
        &completed_checkout_set,
        &completed_store_set,
        count_workspaces,
    );
    let accumulated_usage = prior_usage.saturating_add(slice_completed_bytes);
    let measured_usage = if full_scan_complete {
        accumulated_usage
    } else {
        accumulated_usage.saturating_add(incomplete_slice_usage(
            &scan,
            &completed_checkout_set,
            &completed_store_set,
            !prior_workspaces_counted && !count_workspaces,
        ))
    };
    let workspaces_counted_after = prior_workspaces_counted || count_workspaces;
    let persist_cursor = |completed_now: &[PathBuf],
                          pending_now: &[PathBuf],
                          incomplete: bool|
     -> Result<(), String> {
        if !incomplete && pending_now.is_empty() && !remaining_stores {
            state::clear_maintain_cursor(&cursor_path)?;
        } else if !completed_now.is_empty()
            || !pending_now.is_empty()
            || !completed_stores_this_run.is_empty()
            || count_workspaces
        {
            let mut completed: Vec<PathBuf> = skip_set
                .iter()
                .cloned()
                .chain(completed_now.iter().cloned())
                .collect();
            completed.sort();
            completed.dedup();
            let mut pending: Vec<PathBuf> = pending_now.to_vec();
            pending.sort();
            pending.dedup();
            pending.retain(|p| !completed.contains(p));
            let mut stores: Vec<String> = skip_stores
                .iter()
                .cloned()
                .chain(completed_stores_this_run.iter().cloned())
                .collect();
            stores.sort();
            stores.dedup();
            let completed_now_set: HashSet<PathBuf> = completed_now.iter().cloned().collect();
            let usage_after = prior_usage.saturating_add(completed_slice_usage(
                &scan,
                &completed_now_set,
                &completed_store_set,
                count_workspaces,
            ));
            state::write_maintain_cursor(
                &cursor_path,
                &MaintainCursor {
                    schema: MAINTAIN_CURSOR_SCHEMA.to_owned(),
                    roots: settings.roots.clone(),
                    completed_checkouts: completed,
                    pending_checkouts: pending,
                    completed_stores: stores,
                    accumulated_usage_bytes: usage_after,
                    workspaces_counted: workspaces_counted_after,
                    had_errors: had_errors_any,
                    updated_at: timestamp::format_utc(initial_now),
                },
            )?;
        }
        Ok(())
    };
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
        usage_bytes: measured_usage,
        ceiling_bytes: audited.ceiling_bytes,
        enforced: audited.enforced,
        filesystem_available_bytes: available_before,
        complete: full_scan_complete,
        now: initial_now,
    };
    let reading = pressure::next(
        read_pressure(&path).as_ref(),
        measurement,
        Thresholds::default(),
    );
    write_pressure(&path, &reading)?;
    let initial_outcome = if full_scan_complete {
        ExecutionOutcome::Complete
    } else if tracker.is_stopped() {
        tracker.outcome(had_errors_any)
    } else if slice.slice_capped {
        ExecutionOutcome::Exhausted
    } else if slice_incomplete {
        ExecutionOutcome::Partial
    } else if had_errors_any {
        ExecutionOutcome::Errored
    } else {
        scan.outcome
    };
    let mut report = MaintenanceReport {
        schema: MAINTENANCE_SCHEMA.to_owned(),
        pressure: reading.clone(),
        outcome: initial_outcome,
        paused: false,
        pause_reason: None,
        entries_visited: tracker.entries_visited(),
        directories_deferred: tracker.directories_deferred(),
        draining_workers: tracker.draining_workers(),
        error_count: inspection.error_count,
        errors: inspection.errors,
        standing_approval: settings.standing_approval,
        filesystem_available_before_bytes: available_before,
        filesystem_available_after_bytes: available_before,
        reclaim: None,
        stores: None,
    };
    if observe_only || !settings.standing_approval {
        persist_cursor(&completed_this_run, &remaining_pending, slice_incomplete)?;
        return Ok(report);
    }
    let completed_set: HashSet<PathBuf> = completed_this_run.iter().cloned().collect();
    let reclaim_scan: DevelopmentScan = if full_scan_complete {
        scan.clone()
    } else {
        // On a partial or sliced run, act only on checkouts whose survey
        // completed cleanly in this slice, keeping derived-only scope.
        let mut partial = scan.clone();
        partial
            .checkouts
            .retain(|checkout| completed_set.contains(&checkout.path));
        partial
    };
    if reclaim_scan.checkouts.is_empty() && !full_scan_complete {
        persist_cursor(&completed_this_run, &remaining_pending, slice_incomplete)?;
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
    let helper_runner = crate::helper::HelperRunner::new(
        crate::helper::HelperPolicy::for_maintenance(Arc::clone(&budget.cancel), budget.deadline),
    );
    let bounded_git = context.git.clone().with_runner(Arc::clone(&helper_runner));
    let observed = Observed {
        liveness: &Liveness::observe_with_runner(&helper_runner),
        registry: &standings,
    };
    let plan = plan_reclaim(&reclaim_scan, &observed, &bounded_git, policy, context.now);
    let mut journal =
        Journal::open(&context.directories.journal()).map_err(|err| err.to_string())?;
    let mut observe_bounded = || Liveness::observe_with_runner(&helper_runner);
    let conditions = Conditions {
        authority: Authority::StandingApproval,
        observe: &mut observe_bounded,
        registry: &mut || registry::reading(&registry_path),
        registry_path: Some(&registry_path),
        git: &bounded_git,
        now: context.now,
    };
    let exec_bounds = execute::ExecutionBounds {
        tracker: Arc::clone(&tracker),
        lock_timeout: std::time::Duration::from_secs(5),
        max_entries_per_candidate: Some(500_000),
    };
    let reclaim_report = execute::apply_with_bounds(
        &mut journal,
        &plan,
        conditions,
        execute::ApplyHooks::default(),
        Some(&exec_bounds),
    )
    .map_err(|err| err.to_string())?;
    // Move any checkout whose verification stopped on budget back to pending so
    // the next bounded run retries its reclamation instead of skipping it.
    let mut budget_deferred: HashSet<PathBuf> = HashSet::new();
    for removal in &reclaim_report.removals {
        if !removal.removed
            && removal
                .refusal
                .as_deref()
                .is_some_and(|r| r.contains("budget"))
            && let Some(candidate) = plan.candidates.get(removal.index)
        {
            budget_deferred.insert(candidate.checkout.as_path().to_path_buf());
        }
    }
    if !budget_deferred.is_empty() {
        completed_this_run.retain(|path| !budget_deferred.contains(path));
        remaining_pending.extend(budget_deferred);
    }
    let run_incomplete = slice_incomplete || !remaining_pending.is_empty() || tracker.is_stopped();
    persist_cursor(&completed_this_run, &remaining_pending, run_incomplete)?;
    report.reclaim = Some(reclaim_report);
    report.entries_visited = tracker.entries_visited();
    report.directories_deferred = tracker.directories_deferred();
    report.draining_workers = tracker.draining_workers();

    // Automatic maintenance skips unbounded external store adapters (such as
    // `docker system df` and blocking CLI trims) and runs only in-process
    // eviction on a complete initial pass with remaining budget.
    if single_pass_complete && !tracker.is_stopped() {
        let mut budgets = Budgets::default();
        for (id, bytes) in &settings.budgets {
            budgets.set(id, *bytes);
        }
        let engine = Err(
            "external container engine telemetry skipped during automatic maintenance".to_owned(),
        );
        let inputs = Inputs {
            budgets: &budgets,
            programs: context.programs,
            engine: &engine,
            builder_budget_option: "",
            idle_seconds,
            now: context.now,
        };
        let stores_plan = stores::plan_stores(&scan, &inputs);
        let docker = &context.programs.docker;
        report.stores = Some(
            stores::apply_with_tracker(
                &mut journal,
                &stores_plan,
                Authority::StandingApproval,
                &tracker,
                false,
                &mut |entry| stores::measure_after_with_tracker(entry, docker, &tracker),
            )
            .map_err(|err| err.to_string())?,
        );
    }

    // Publish a post-maintenance observation sharing the same tracker when the
    // pass finished all checkouts and remaining budget permits. An incomplete
    // post-observation never erases an active `lower_since` window established
    // by a complete initial observation unless its lower bound raises pressure.
    if !run_incomplete && !tracker.is_stopped() {
        let post_slice = development::inspect_with_tracker(
            &settings.roots,
            context.environment,
            Arc::clone(&tracker),
            &HashSet::new(),
            &[],
            &HashSet::new(),
            None,
        );
        let post_scan = post_slice.scan;
        let post_now = clock();
        let post_inspection = development::build_report(&post_scan, post_now);
        let post_audited = arena::audit(first, &others, settings.ceiling_bytes)?;
        let available_after = Some(post_audited.available_bytes);
        let post_complete =
            post_inspection.status == ReportStatus::Complete && post_scan.outcome.is_complete();
        let post_measurement = Measurement {
            usage_bytes: pressure::usage(&post_scan),
            ceiling_bytes: post_audited.ceiling_bytes,
            enforced: post_audited.enforced,
            filesystem_available_bytes: available_after,
            complete: post_complete,
            now: post_now,
        };
        let post_reading = pressure::next(Some(&reading), post_measurement, Thresholds::default());
        if post_complete || post_reading.level > reading.level {
            write_pressure(&path, &post_reading)?;
            report.pressure = post_reading;
        }
        report.outcome = if post_complete {
            ExecutionOutcome::Complete
        } else {
            tracker.outcome(post_inspection.error_count > 0 || report.error_count > 0)
        };
        report.filesystem_available_after_bytes = available_after;
        if post_inspection.error_count > 0 {
            report.error_count = report
                .error_count
                .saturating_add(post_inspection.error_count);
            report.errors.extend(post_inspection.errors);
        }
    } else {
        let post_audited = arena::audit(first, &others, settings.ceiling_bytes)?;
        report.filesystem_available_after_bytes = Some(post_audited.available_bytes);
        if tracker.is_stopped() {
            report.outcome = tracker.outcome(report.error_count > 0);
        }
    }
    report.entries_visited = tracker.entries_visited();
    report.directories_deferred = tracker.directories_deferred();
    report.draining_workers = tracker.draining_workers();
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

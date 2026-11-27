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
use crate::clones::Counted;
use crate::development::{self, DevelopmentScan, Environment, SliceResume, Store, StoreStatus};
use crate::model::{ReportStatus, TraversalError};
use crate::organize::journal::Journal;
use crate::pressure::{self, Measurement, Pressure, Thresholds};
use crate::reclaim::execute::{self, Authority, Conditions, ReclaimReport};
use crate::reclaim::git::Git;
use crate::reclaim::liveness::Liveness;
use crate::reclaim::plan::{CheckoutScope, Observed, Policy, plan_reclaim};
use crate::registry;
use crate::scan::ScanDedup;
use crate::state::{
    self, CursorDedup, Directories, MAINTAIN_CURSOR_SCHEMA, MaintainCursor, MaintainCursorMode,
    Settings, write_staged,
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
    /// Whether durable pause state held this run to a measurement that reclaims nothing.
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

/// The durable pause, where one is in force. An unreadable record is an error, so a broken pause never lets a run
/// reclaim.
fn active_pause(context: &Context<'_>) -> Result<Option<state::PauseState>, String> {
    Ok(state::read_pause(&context.directories.pause())?.filter(|pause| pause.paused))
}

/// Like [`maintain_with_clock`], governed by an explicit execution `budget`.
/// Reads durable pause state before taking any lock. A paused run measures within the budget and reclaims nothing.
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

fn store_counted_in_completed(
    store: &Store,
    completed_checkouts: &HashSet<PathBuf>,
    completed_stores: &HashSet<String>,
    count_workspaces: bool,
) -> bool {
    if matches!(store.status, StoreStatus::Absent | StoreStatus::Unreadable) {
        return false;
    }
    if let Some(owner) = &store.owner {
        completed_checkouts.contains(owner)
    } else if development::is_configured_store(&store.id) {
        completed_stores.contains(&store.id)
    } else {
        count_workspaces
    }
}

fn store_counted_in_incomplete(
    store: &Store,
    completed_checkouts: &HashSet<PathBuf>,
    completed_stores: &HashSet<String>,
    include_workspaces: bool,
) -> bool {
    if matches!(store.status, StoreStatus::Absent | StoreStatus::Unreadable) {
        return false;
    }
    if let Some(owner) = &store.owner {
        !completed_checkouts.contains(owner)
    } else if development::is_configured_store(&store.id) {
        !completed_stores.contains(&store.id)
    } else {
        include_workspaces
    }
}

fn completed_slice_usage(
    scan: &DevelopmentScan,
    completed_checkouts: &HashSet<PathBuf>,
    completed_stores: &HashSet<String>,
    reclaimed_paths: &HashSet<PathBuf>,
    count_workspaces: bool,
) -> u64 {
    let workspaces: u64 = if count_workspaces {
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
        .filter(|checkout| {
            completed_checkouts.contains(&checkout.path)
                && !reclaimed_paths.contains(&checkout.path)
        })
        .map(|checkout| {
            checkout.own.allocated_bytes
                + checkout
                    .build_state
                    .iter()
                    .filter(|build| !reclaimed_paths.contains(&build.path))
                    .map(|build| build.measure.allocated_bytes)
                    .sum::<u64>()
        })
        .sum();
    let stores: u64 = scan
        .stores
        .iter()
        .filter(|store| {
            !reclaimed_paths.contains(&store.path)
                && store_counted_in_completed(
                    store,
                    completed_checkouts,
                    completed_stores,
                    count_workspaces,
                )
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
            store_counted_in_incomplete(
                store,
                completed_checkouts,
                completed_stores,
                include_workspaces,
            )
        })
        .map(|store| store.measure.allocated_bytes)
        .sum();
    workspaces.saturating_add(checkouts).saturating_add(stores)
}

fn dedup_to_cursor(dedup: &ScanDedup) -> CursorDedup {
    let mut linked: Vec<(u64, u64)> = dedup.linked.iter().copied().collect();
    linked.sort_unstable();
    CursorDedup {
        linked,
        cloned: dedup.cloned.to_device_extents(),
    }
}

fn dedup_from_cursor(cursor: &CursorDedup) -> ScanDedup {
    ScanDedup {
        linked: cursor.linked.iter().copied().collect(),
        cloned: Counted::from_device_extents(&cursor.cloned),
    }
}

fn maintain_inner(
    settings: &Settings,
    context: &Context<'_>,
    budget: &Budget,
    clock: &mut dyn FnMut() -> i64,
    observe_only: bool,
) -> Result<MaintenanceReport, String> {
    let path = context.directories.pressure();
    // An unreadable or malformed pause record fails closed, before any lock or scan.
    if !observe_only {
        active_pause(context)?;
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
    // Read the pause after acquiring the lock, so a pause written during contention holds a queued run too. A
    // pause stops reclamation and never the reading: the paused run takes the bounded observation slice, so the
    // pressure that a client waits on stays current while recovery is held.
    let pause = if observe_only {
        None
    } else {
        active_pause(context)?
    };
    let observe_only = observe_only || pause.is_some();
    if budget.background_priority {
        budget::apply_background_thread_policy();
    }
    let tracker = BudgetTracker::new(budget.clone());
    let cursor_path = context.directories.maintain_cursor();
    let cursor_mode = if observe_only || !settings.standing_approval {
        MaintainCursorMode::Observe
    } else {
        MaintainCursorMode::Reclaim
    };
    let existing_cursor = state::read_maintain_cursor(&cursor_path)
        .filter(|cursor| cursor.roots == settings.roots && cursor.mode == cursor_mode);
    let skip_set: HashSet<PathBuf> = existing_cursor
        .as_ref()
        .map(|cursor| cursor.completed_checkouts.iter().cloned().collect())
        .unwrap_or_default();
    let skip_stores: HashSet<String> = existing_cursor
        .as_ref()
        .map(|cursor| cursor.completed_stores.iter().cloned().collect())
        .unwrap_or_default();
    let prior_machine_paths: Vec<PathBuf> = existing_cursor
        .as_ref()
        .map(|cursor| cursor.discovered_machine_paths.clone())
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
    let prior_invalidated = existing_cursor
        .as_ref()
        .is_some_and(|cursor| cursor.measurement_invalidated);
    let prior_dedup = existing_cursor
        .as_ref()
        .map(|cursor| dedup_from_cursor(&cursor.dedup));
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
        SliceResume {
            skip_checkouts: &skip_set,
            seed_pending: &seed_pending,
            skip_stores: &skip_stores,
            skip_workspaces: prior_workspaces_counted,
            prior_machine_paths: &prior_machine_paths,
            prior_dedup,
            scan_entry_cap,
        },
    );
    let scan = slice.scan;
    let initial_now = clock();
    let inspection = development::build_report(&scan, initial_now);
    let mut completed_this_run = slice.completed_checkouts;
    let mut remaining_pending = slice.remaining_pending;
    let completed_stores_this_run = slice.completed_stores;
    let remaining_stores = slice.remaining_stores;
    let committed_dedup = slice.committed_dedup;
    let shared_checkouts = slice.shared_checkouts;
    let shared_builds = slice.shared_builds;
    let has_remaining = !remaining_pending.is_empty();
    let had_errors_any = prior_had_errors || inspection.error_count > 0;
    let slice_incomplete =
        slice.slice_capped || !scan.outcome.is_complete() || has_remaining || remaining_stores;
    let workspaces_ok = prior_workspaces_counted || slice.workspaces_complete;
    let full_scan_complete = !slice_incomplete
        && workspaces_ok
        && !had_errors_any
        && !prior_invalidated
        && inspection.status == ReportStatus::Complete;
    let single_pass_complete = full_scan_complete
        && skip_set.is_empty()
        && seed_pending.is_empty()
        && skip_stores.is_empty()
        && !prior_workspaces_counted;
    let completed_checkout_set: HashSet<PathBuf> = completed_this_run.iter().cloned().collect();
    let completed_store_set: HashSet<String> = completed_stores_this_run.iter().cloned().collect();
    let count_workspaces = !prior_workspaces_counted && slice.workspaces_complete;
    let empty_reclaimed = HashSet::new();
    let slice_completed_bytes = completed_slice_usage(
        &scan,
        &completed_checkout_set,
        &completed_store_set,
        &empty_reclaimed,
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
                          reclaimed_paths: &HashSet<PathBuf>,
                          invalidated_now: bool,
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
            let mut machine_paths: Vec<PathBuf> = prior_machine_paths
                .iter()
                .cloned()
                .chain(
                    scan.stores
                        .iter()
                        .filter(|store| {
                            store.class == development::StoreClass::Machine
                                && !development::is_configured_store(&store.id)
                                && store_counted_in_completed(
                                    store,
                                    &completed_now_set,
                                    &completed_store_set,
                                    count_workspaces,
                                )
                        })
                        .map(|store| store.path.clone()),
                )
                .collect();
            machine_paths.sort();
            machine_paths.dedup();
            let usage_after = prior_usage.saturating_add(completed_slice_usage(
                &scan,
                &completed_now_set,
                &completed_store_set,
                reclaimed_paths,
                count_workspaces,
            ));
            state::write_maintain_cursor(
                &cursor_path,
                &MaintainCursor {
                    schema: MAINTAIN_CURSOR_SCHEMA.to_owned(),
                    mode: cursor_mode,
                    roots: settings.roots.clone(),
                    completed_checkouts: completed,
                    pending_checkouts: pending,
                    completed_stores: stores,
                    discovered_machine_paths: machine_paths,
                    accumulated_usage_bytes: usage_after,
                    workspaces_counted: workspaces_counted_after,
                    had_errors: had_errors_any,
                    measurement_invalidated: prior_invalidated || invalidated_now,
                    dedup: dedup_to_cursor(&committed_dedup),
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
        paused: pause.is_some(),
        pause_reason: pause.and_then(|pause| pause.reason),
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
        persist_cursor(
            &completed_this_run,
            &remaining_pending,
            &empty_reclaimed,
            false,
            slice_incomplete,
        )?;
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
        persist_cursor(
            &completed_this_run,
            &remaining_pending,
            &empty_reclaimed,
            false,
            slice_incomplete,
        )?;
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
    let mut reclaimed_paths: HashSet<PathBuf> = HashSet::new();
    for removal in &reclaim_report.removals {
        if removal.removed {
            if let Some(candidate) = plan.candidates.get(removal.index) {
                reclaimed_paths.insert(candidate.path.as_path().to_path_buf());
            }
        } else if removal
            .refusal
            .as_deref()
            .is_some_and(|r| r.contains("budget"))
            && let Some(candidate) = plan.candidates.get(removal.index)
        {
            budget_deferred.insert(candidate.checkout.as_path().to_path_buf());
        }
    }
    let invalidated_now = reclaimed_paths.iter().any(|p| shared_builds.contains(p))
        || budget_deferred.iter().any(|p| shared_checkouts.contains(p));
    if !budget_deferred.is_empty() {
        completed_this_run.retain(|path| !budget_deferred.contains(path));
        remaining_pending.extend(budget_deferred);
    }
    let run_incomplete = slice_incomplete || !remaining_pending.is_empty() || tracker.is_stopped();
    persist_cursor(
        &completed_this_run,
        &remaining_pending,
        &reclaimed_paths,
        invalidated_now,
        run_incomplete,
    )?;
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
        let empty_checkouts = HashSet::new();
        let empty_stores = HashSet::new();
        let post_slice = development::inspect_with_tracker(
            &settings.roots,
            context.environment,
            Arc::clone(&tracker),
            SliceResume {
                skip_checkouts: &empty_checkouts,
                seed_pending: &[],
                skip_stores: &empty_stores,
                skip_workspaces: false,
                prior_machine_paths: &[],
                prior_dedup: None,
                scan_entry_cap: None,
            },
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

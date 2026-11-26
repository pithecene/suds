//! Shared execution budgets, cancellation, and background thread policy.
//!
//! Observation, seeding, and maintenance share one budget so no single
//! directory, oversized workspace, or background schedule can saturate CPU or
//! filesystem metadata operations.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// The default number of entries a single directory may hold before a bounded
/// traversal defers it rather than allocating and probing all of its entries.
pub const DEFAULT_MAX_DIR_ENTRIES: usize = 250_000;

/// The default shortlist of sibling checkouts inspected during automatic seed
/// source selection.
pub const DEFAULT_MAX_SEED_CANDIDATES: usize = 16;

/// The outcome of a bounded execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionOutcome {
    Complete,
    Partial,
    Cancelled,
    Exhausted,
    Errored,
}

impl ExecutionOutcome {
    /// Whether the outcome represents a whole, uninterrupted traversal.
    pub fn is_complete(self) -> bool {
        self == ExecutionOutcome::Complete
    }
}

/// The limits that govern one observation, seed-selection, or maintenance run.
#[derive(Debug, Clone)]
pub struct Budget {
    /// Shared cancellation flag. When `true`, the tracker admits no additional work.
    pub cancel: Arc<AtomicBool>,
    /// Monotonic deadline after which the tracker admits no additional work.
    pub deadline: Option<Instant>,
    /// Maximum total metadata readers, including the traversal caller itself.
    /// A value of `0` or `1` runs solely on the caller with no read-ahead pool
    /// workers. Values of `2` or more allow at most `N - 1` background read-ahead workers.
    pub max_total_readers: usize,
    /// Maximum filesystem entries visited across the run.
    pub max_entries: Option<u64>,
    /// Maximum entries listed in any single directory before deferring it.
    pub max_dir_entries: Option<usize>,
    /// Maximum sibling checkouts inspected when selecting a seed source.
    pub max_seed_candidates: usize,
    /// Optional pacing interval: after every `pace_every` entries, sleep for
    /// `pace_delay` when both are non-zero.
    pub pace_every: u64,
    pub pace_delay: Duration,
    /// Apply OS background CPU QoS and throttled disk I/O policy to worker
    /// threads (and background maintenance execution).
    pub background_priority: bool,
}

impl Default for Budget {
    fn default() -> Self {
        Budget {
            cancel: Arc::new(AtomicBool::new(false)),
            deadline: None,
            // Start with one total metadata reader by default to bound host
            // contention. Callers may raise this explicitly when appropriate.
            max_total_readers: 1,
            max_entries: None,
            max_dir_entries: Some(DEFAULT_MAX_DIR_ENTRIES),
            max_seed_candidates: DEFAULT_MAX_SEED_CANDIDATES,
            pace_every: 0,
            pace_delay: Duration::ZERO,
            background_priority: false,
        }
    }
}

impl Budget {
    /// The bounded Mac-canary budget used by unattended `suds maintain`.
    pub fn for_maintenance() -> Self {
        Budget {
            cancel: Arc::new(AtomicBool::new(false)),
            deadline: Some(Instant::now() + Duration::from_secs(300)),
            max_total_readers: 1,
            max_entries: Some(5_000_000),
            max_dir_entries: Some(DEFAULT_MAX_DIR_ENTRIES),
            max_seed_candidates: DEFAULT_MAX_SEED_CANDIDATES,
            pace_every: 2_048,
            pace_delay: Duration::from_millis(1),
            background_priority: true,
        }
    }

    /// Background read-ahead worker slots allowed alongside the caller thread.
    pub fn helper_readers(&self) -> usize {
        self.max_total_readers.saturating_sub(1)
    }
}

/// Live accounting shared across a bounded traversal and any read-ahead
/// workers serving it.
#[derive(Debug)]
pub struct BudgetTracker {
    budget: Budget,
    entries_visited: AtomicU64,
    directories_deferred: AtomicU64,
    exhausted: AtomicBool,
    cancelled: AtomicBool,
    draining_workers: AtomicUsize,
}

/// Snapshot of the budget consumed by a bounded operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetSummary {
    pub outcome: ExecutionOutcome,
    pub entries_visited: u64,
    pub directories_deferred: u64,
    pub draining_workers: usize,
}

impl BudgetTracker {
    pub fn new(budget: Budget) -> Arc<Self> {
        Arc::new(BudgetTracker {
            budget,
            entries_visited: AtomicU64::new(0),
            directories_deferred: AtomicU64::new(0),
            exhausted: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            draining_workers: AtomicUsize::new(0),
        })
    }

    pub fn budget(&self) -> &Budget {
        &self.budget
    }

    pub fn cancel_flag(&self) -> &AtomicBool {
        &self.budget.cancel
    }

    /// Requests cancellation of this operation.
    pub fn cancel(&self) {
        self.budget.cancel.store(true, Ordering::Release);
        self.cancelled.store(true, Ordering::Release);
    }

    /// Returns why the operation stopped, or `None` while work may continue.
    pub fn stop_reason(&self) -> Option<ExecutionOutcome> {
        if self.budget.cancel.load(Ordering::Acquire) || self.cancelled.load(Ordering::Acquire) {
            self.cancelled.store(true, Ordering::Release);
            return Some(ExecutionOutcome::Cancelled);
        }
        if self.exhausted.load(Ordering::Acquire) {
            return Some(ExecutionOutcome::Exhausted);
        }
        if self
            .budget
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.exhausted.store(true, Ordering::Release);
            return Some(ExecutionOutcome::Exhausted);
        }
        None
    }

    /// Whether the operation must admit no additional work.
    pub fn is_stopped(&self) -> bool {
        self.stop_reason().is_some()
    }

    /// Claims one entry from the work budget. Returns `false` after
    /// cancellation or after exhausting the entry or deadline limit.
    pub fn claim_entry(&self) -> bool {
        if self.is_stopped() {
            return false;
        }
        let next = self.entries_visited.fetch_add(1, Ordering::AcqRel) + 1;
        if let Some(limit) = self.budget.max_entries
            && next > limit
        {
            self.entries_visited.fetch_sub(1, Ordering::AcqRel);
            self.exhausted.store(true, Ordering::Release);
            return false;
        }
        if self.budget.pace_every > 0
            && !self.budget.pace_delay.is_zero()
            && next.is_multiple_of(self.budget.pace_every)
            && !self.is_stopped()
        {
            std::thread::sleep(self.budget.pace_delay);
        }
        true
    }

    /// Records that the reader deferred a directory after reaching
    /// `max_dir_entries`.
    pub fn record_deferred_directory(&self) {
        self.directories_deferred.fetch_add(1, Ordering::AcqRel);
    }

    pub fn entries_visited(&self) -> u64 {
        self.entries_visited.load(Ordering::Acquire)
    }

    pub fn directories_deferred(&self) -> u64 {
        self.directories_deferred.load(Ordering::Acquire)
    }

    pub fn worker_enter(&self) {
        self.draining_workers.fetch_add(1, Ordering::AcqRel);
    }

    pub fn worker_exit(&self) {
        self.draining_workers.fetch_sub(1, Ordering::AcqRel);
    }

    pub fn draining_workers(&self) -> usize {
        self.draining_workers.load(Ordering::Acquire)
    }

    /// Resolves the final outcome given whether traversal errors occurred.
    pub fn outcome(&self, had_errors: bool) -> ExecutionOutcome {
        if self.cancelled.load(Ordering::Acquire) || self.budget.cancel.load(Ordering::Acquire) {
            ExecutionOutcome::Cancelled
        } else if self.exhausted.load(Ordering::Acquire) {
            ExecutionOutcome::Exhausted
        } else if self.directories_deferred() > 0 || had_errors {
            ExecutionOutcome::Partial
        } else {
            ExecutionOutcome::Complete
        }
    }

    pub fn summary(&self, had_errors: bool) -> BudgetSummary {
        BudgetSummary {
            outcome: self.outcome(had_errors),
            entries_visited: self.entries_visited(),
            directories_deferred: self.directories_deferred(),
            draining_workers: self.draining_workers(),
        }
    }
}

/// Applies background CPU QoS and throttled disk I/O policy to the calling
/// thread on macOS. Foreground UI threads must never call this function.
#[cfg_attr(target_os = "macos", allow(unsafe_code))]
pub fn apply_background_thread_policy() {
    #[cfg(target_os = "macos")]
    {
        // Apple defines QOS_CLASS_BACKGROUND = 0x09 in <sys/qos.h> and
        // IOPOL_TYPE_DISK = 0, IOPOL_SCOPE_THREAD = 1, IOPOL_THROTTLE = 3 in
        // <sys/resource.h>.
        const QOS_CLASS_BACKGROUND: libc::c_uint = 0x09;
        const IOPOL_TYPE_DISK: libc::c_int = 0;
        const IOPOL_SCOPE_THREAD: libc::c_int = 1;
        const IOPOL_THROTTLE: libc::c_int = 3;

        unsafe extern "C" {
            fn pthread_set_qos_class_self_np(
                qos_class: libc::c_uint,
                relative_priority: libc::c_int,
            ) -> libc::c_int;
            fn setiopolicy_np(
                kind: libc::c_int,
                scope: libc::c_int,
                policy: libc::c_int,
            ) -> libc::c_int;
        }
        // SAFETY: Both functions operate solely on the calling thread's
        // scheduling attributes and accept scalar integer arguments.
        unsafe {
            let _ = pthread_set_qos_class_self_np(QOS_CLASS_BACKGROUND, 0);
            let _ = setiopolicy_np(IOPOL_TYPE_DISK, IOPOL_SCOPE_THREAD, IOPOL_THROTTLE);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use super::{Budget, BudgetTracker, ExecutionOutcome};

    #[test]
    fn entry_budget_stops_and_marks_exhausted() {
        let tracker = BudgetTracker::new(Budget {
            max_entries: Some(3),
            ..Budget::default()
        });
        assert!(tracker.claim_entry());
        assert!(tracker.claim_entry());
        assert!(tracker.claim_entry());
        assert!(!tracker.claim_entry());
        assert_eq!(tracker.entries_visited(), 3);
        assert_eq!(tracker.outcome(false), ExecutionOutcome::Exhausted);
    }

    #[test]
    fn cancel_flag_stops_work_immediately() {
        let cancel = Arc::new(AtomicBool::new(false));
        let tracker = BudgetTracker::new(Budget {
            cancel: Arc::clone(&cancel),
            ..Budget::default()
        });
        assert!(tracker.claim_entry());
        cancel.store(true, Ordering::Release);
        assert!(!tracker.claim_entry());
        assert_eq!(tracker.outcome(false), ExecutionOutcome::Cancelled);
    }

    #[test]
    fn expired_deadline_marks_exhausted() {
        let tracker = BudgetTracker::new(Budget {
            deadline: Some(Instant::now() - Duration::from_secs(1)),
            ..Budget::default()
        });
        assert!(!tracker.claim_entry());
        assert_eq!(tracker.outcome(false), ExecutionOutcome::Exhausted);
    }

    #[test]
    fn deferred_directory_prevents_complete_outcome() {
        let tracker = BudgetTracker::new(Budget::default());
        assert_eq!(tracker.outcome(false), ExecutionOutcome::Complete);
        tracker.record_deferred_directory();
        assert_eq!(tracker.outcome(false), ExecutionOutcome::Partial);
    }
}

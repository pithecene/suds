//! Inspections that run on a background thread and report each root as it
//! completes.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use serde::Serialize;
use suds_core::model::RootResult;
use suds_core::platform::RootSpec;
use suds_core::scan;

use crate::locations::LocationId;

/// An identifier for one run of an inspection.
pub type InspectionId = u64;

/// A message from a running inspection to the window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InspectionEvent {
    /// One root finished.
    Root {
        inspection: InspectionId,
        location: LocationId,
        /// Whether a cancellation request ended the root's scan early.
        cancelled: bool,
        result: Box<RootResult>,
    },
    /// The run ended, after the last root's scan or at a cancellation
    /// request.
    Finished {
        inspection: InspectionId,
        cancelled: bool,
    },
}

struct Running {
    id: InspectionId,
    cancel: Arc<AtomicBool>,
}

/// The inspection in progress, if any. A new inspection cancels the one
/// before it, so at most one scans at a time.
#[derive(Default)]
pub struct Inspections {
    current: Mutex<Option<Running>>,
    started: Mutex<InspectionId>,
}

impl Inspections {
    /// Starts an inspection of `roots` and returns its identifier. The
    /// background thread passes each event to `send`.
    pub fn start<F>(&self, roots: Vec<(LocationId, RootSpec)>, send: F) -> InspectionId
    where
        F: Fn(InspectionEvent) + Send + 'static,
    {
        let id = {
            let mut started = lock(&self.started);
            *started += 1;
            *started
        };
        let cancel = Arc::new(AtomicBool::new(false));
        if let Some(previous) = lock(&self.current).replace(Running {
            id,
            cancel: Arc::clone(&cancel),
        }) {
            previous.cancel.store(true, Ordering::Relaxed);
        }
        thread::spawn(move || run(id, &roots, &cancel, &send));
        id
    }

    /// Requests that the given inspection stop. A request for an inspection
    /// that already ended has no effect.
    pub fn cancel(&self, id: InspectionId) {
        if let Some(running) = lock(&self.current).as_ref()
            && running.id == id
        {
            running.cancel.store(true, Ordering::Relaxed);
        }
    }
}

fn run(
    id: InspectionId,
    roots: &[(LocationId, RootSpec)],
    cancel: &AtomicBool,
    send: &impl Fn(InspectionEvent),
) {
    for (location, spec) in roots {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let scan = scan::scan_root(spec, cancel);
        send(InspectionEvent::Root {
            inspection: id,
            location: *location,
            cancelled: scan.cancelled,
            result: Box::new(scan.to_result()),
        });
    }
    send(InspectionEvent::Finished {
        inspection: id,
        cancelled: cancel.load(Ordering::Relaxed),
    });
}

/// Locks a mutex and recovers its value from a poisoned lock. No update to a
/// guarded value calls code that can panic partway through, so the recovered
/// value stays whole.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::mpsc;

    use suds_core::model::{RootKind, RootStatus};
    use suds_core::platform::RootSpec;
    use tempfile::TempDir;

    use super::{InspectionEvent, Inspections};

    fn spec(path: &std::path::Path) -> RootSpec {
        RootSpec {
            id: "explicit.1".to_owned(),
            kind: RootKind::Unclassified,
            path: path.to_path_buf(),
            anchor: None,
        }
    }

    #[test]
    fn reports_each_root_and_then_the_end() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("file"), b"data").unwrap();
        let (sender, receiver) = mpsc::channel();
        let inspections = Inspections::default();
        let id = inspections.start(
            vec![(4, spec(dir.path())), (5, spec(&dir.path().join("gone")))],
            move |event| sender.send(event).unwrap(),
        );

        let events: Vec<_> = receiver.iter().take(3).collect();
        let InspectionEvent::Root {
            inspection,
            location,
            cancelled,
            result,
        } = &events[0]
        else {
            panic!("expected a root: {events:?}");
        };
        assert_eq!((*inspection, *location, *cancelled), (id, 4, false));
        assert_eq!(result.status, RootStatus::Complete);
        assert_eq!(result.measure.files, 1);
        assert!(matches!(
            &events[1],
            InspectionEvent::Root { location: 5, result, .. } if result.status == RootStatus::Absent
        ));
        assert_eq!(
            events[2],
            InspectionEvent::Finished {
                inspection: id,
                cancelled: false
            }
        );
    }

    #[test]
    fn a_new_inspection_cancels_the_previous_one() {
        let inspections = Inspections::default();
        let first = inspections.start(Vec::new(), |_| {});
        let cancel = inspections
            .current
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .cancel
            .clone();
        let second = inspections.start(Vec::new(), |_| {});
        assert_ne!(first, second);
        assert!(cancel.load(std::sync::atomic::Ordering::Relaxed));
        inspections.cancel(first);
        let current = inspections.current.lock().unwrap();
        assert!(
            !current
                .as_ref()
                .unwrap()
                .cancel
                .load(std::sync::atomic::Ordering::Relaxed)
        );
    }
}

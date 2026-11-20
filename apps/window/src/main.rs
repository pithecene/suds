//! The Suds window.
//!
//! The window measures the workstation on a thread of its own, through the
//! view model, and draws what the reading holds. It changes nothing on disk.

mod present;

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use slint::{ComponentHandle, ModelRc, VecModel, Weak};
use suds_core::development::Environment;
use suds_view::{Observation, Sort, Workstation, arrange, observe};

slint::include_modules!();

/// What the window holds from one event to the next: the last reading and the
/// positions of the checkouts that the list shows, in its order.
#[derive(Default)]
struct State {
    view: Option<Workstation>,
    shown: Vec<usize>,
    /// The home directory, which the window writes as a tilde.
    home: Option<String>,
    /// Whether a measurement runs. A second request waits for none and starts none.
    measuring: bool,
}

impl State {
    /// Marks a measurement as running, unless one runs already.
    fn begin(&mut self) -> bool {
        !std::mem::replace(&mut self.measuring, true)
    }

    /// The path of the checkout at `row` of the list.
    fn path_at(&self, row: i32) -> Option<String> {
        let position = *self.shown.get(usize::try_from(row).ok()?)?;
        Some(self.view.as_ref()?.checkouts.get(position)?.path.clone())
    }

    /// The row of the list that shows the checkout at `path`.
    fn row_of(&self, path: &str) -> Option<i32> {
        let view = self.view.as_ref()?;
        let row = self
            .shown
            .iter()
            .position(|position| view.checkouts[*position].path == path)?;
        i32::try_from(row).ok()
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
        })
}

fn sort_of(index: i32) -> Sort {
    match index {
        1 => Sort::Name,
        2 => Sort::Modified,
        3 => Sort::BuildState,
        _ => Sort::Size,
    }
}

fn model<T: Clone + 'static>(items: Vec<T>) -> ModelRc<T> {
    ModelRc::new(VecModel::from(items))
}

/// Shows the checkout at `row` of the list in the detail pane, or empties the pane.
fn show_detail(window: &Window, state: &State, row: i32) {
    let checkout = usize::try_from(row)
        .ok()
        .and_then(|row| state.shown.get(row))
        .and_then(|position| state.view.as_ref()?.checkouts.get(*position));
    window.set_selected(if checkout.is_some() { row } else { -1 });
    window.set_has_detail(checkout.is_some());
    let Some(checkout) = checkout else {
        window.set_detail_facts(model(Vec::new()));
        window.set_detail_builds(model(Vec::new()));
        return;
    };
    window.set_detail_name(checkout.name.as_str().into());
    window.set_detail_path(present::short(&checkout.path, state.home.as_deref()).into());
    window.set_detail_facts(model(present::facts(checkout)));
    window.set_detail_builds(model(present::builds(checkout)));
}

/// Fills the checkout list with the checkouts that `query` keeps, in the order of `sort`.
/// The checkout at `keep` stays selected while the new list still shows it. A position
/// names another checkout after a new reading, so the selection follows the path.
fn show_checkouts(
    window: &Window,
    state: &mut State,
    query: &str,
    sort: i32,
    keep: Option<String>,
) {
    let Some(view) = &state.view else {
        return;
    };
    state.shown = arrange(&view.checkouts, query, sort_of(sort));
    let rows = state
        .shown
        .iter()
        .map(|position| present::checkout(&view.checkouts[*position], state.home.as_deref()))
        .collect();
    window.set_checkouts(model(rows));
    window.set_checkout_count(
        if state.shown.len() == view.checkouts.len() {
            format!("{}", view.checkouts.len())
        } else {
            format!("{} of {}", state.shown.len(), view.checkouts.len())
        }
        .into(),
    );
    let row = keep.and_then(|path| state.row_of(&path)).unwrap_or(-1);
    show_detail(window, state, row);
}

/// Draws one reading.
fn show(window: &Window, state: &mut State, reading: Result<Observation, String>) {
    state.measuring = false;
    window.set_loading(false);
    let view = match reading {
        Err(reason) => {
            window.set_notice(format!("Suds could not measure the workstation: {reason}").into());
            return;
        }
        Ok(Observation::Unconfigured(settings)) => {
            window.set_measured(false);
            window.set_notice(
                format!("The settings name no root, so Suds has no development storage to measure. Name each workspace directory under \"roots\" in {settings}, then measure again.").into(),
            );
            return;
        }
        Ok(Observation::Measured(view)) => *view,
    };
    window.set_notice(present::notice(view.error_count, view.registry_error.as_deref()).into());
    window.set_observed(view.observed_at.as_str().into());
    if let Some(card) = &view.pressure {
        let (level, rank) = present::level(card.level);
        window.set_usage(suds_view::bytes(card.usage_bytes).into());
        window.set_ceiling(format!("{} ceiling", suds_view::bytes(card.ceiling_bytes)).into());
        window.set_level(level.into());
        window.set_level_rank(rank);
        window.set_pressure_share(card.share);
        window.set_pressure_facts(present::pressure_facts(card).into());
    }
    let classes = view.classes.iter().enumerate();
    window.set_classes(model(
        classes
            .map(|(position, class)| present::class(position, class))
            .collect(),
    ));
    window.set_stores(model(view.stores.iter().map(present::store).collect()));
    // The selection names its checkout by path before the new reading replaces the old positions.
    let keep = state.path_at(window.get_selected());
    state.view = Some(view);
    window.set_measured(true);
    show_checkouts(window, state, &window.get_query(), window.get_sort(), keep);
}

/// Measures the workstation on a thread of its own and draws the reading on the interface's thread.
fn measure(window: &Window, state: Rc<RefCell<State>>) {
    // The button, the menu, and the key all arrive here, so one guard covers each of them.
    if !state.borrow_mut().begin() {
        return;
    }
    window.set_loading(true);
    let weak: Weak<Window> = window.as_weak();
    // The state lives on the interface's thread, so the measuring thread carries only the weak handle.
    STATE.with(|slot| *slot.borrow_mut() = Some(state));
    std::thread::spawn(move || {
        let reading = observe(&Environment::of_process(), now());
        let _ = weak.upgrade_in_event_loop(move |window| {
            STATE.with(|slot| {
                if let Some(state) = slot.borrow().as_ref() {
                    show(&window, &mut state.borrow_mut(), reading);
                }
            });
        });
    });
}

thread_local! {
    /// The interface thread's handle on the window's state, for a reading that arrives from another thread.
    static STATE: RefCell<Option<Rc<RefCell<State>>>> = const { RefCell::new(None) };
}

fn main() -> Result<(), slint::PlatformError> {
    let window = Window::new()?;
    let home = Environment::of_process().home;
    let state = Rc::new(RefCell::new(State {
        home: home.map(|home| home.to_string_lossy().into_owned()),
        ..State::default()
    }));

    window.on_quit(|| {
        let _ = slint::quit_event_loop();
    });
    window.on_refresh({
        let weak = window.as_weak();
        let state = state.clone();
        move || {
            if let Some(window) = weak.upgrade() {
                measure(&window, state.clone());
            }
        }
    });
    window.on_arrange({
        let weak = window.as_weak();
        let state = state.clone();
        move |query, sort| {
            if let Some(window) = weak.upgrade() {
                let mut state = state.borrow_mut();
                let keep = state.path_at(window.get_selected());
                show_checkouts(&window, &mut state, &query, sort, keep);
            }
        }
    });
    window.on_select({
        let weak = window.as_weak();
        let state = state.clone();
        move |row| {
            if let Some(window) = weak.upgrade() {
                show_detail(&window, &state.borrow(), row);
            }
        }
    });

    measure(&window, state);
    window.run()
}

#[cfg(test)]
mod tests {
    use suds_core::development::{Checkout, CheckoutKind, DevelopmentScan};
    use suds_core::model::Measure;
    use suds_core::stores::Budgets;
    use suds_view::{Sort, arrange, workstation};

    use super::State;

    /// A state that shows a reading of the named checkouts, each with its size, in order of size.
    fn showing(checkouts: &[(&str, u64)]) -> State {
        let scan = DevelopmentScan {
            workspaces: Vec::new(),
            checkouts: checkouts
                .iter()
                .map(|(path, bytes)| Checkout {
                    path: path.into(),
                    kind: CheckoutKind::Main,
                    repository: None,
                    own: Measure {
                        allocated_bytes: *bytes,
                        ..Measure::default()
                    },
                    build_state: Vec::new(),
                })
                .collect(),
            stores: Vec::new(),
            skipped_mount_count: 0,
            error_count: 0,
            errors: Vec::new(),
        };
        let view = workstation(&scan, 0, None, &Ok(Default::default()), &Budgets::default());
        let shown = arrange(&view.checkouts, "", Sort::Size);
        State {
            view: Some(view),
            shown,
            ..State::default()
        }
    }

    #[test]
    fn a_selection_follows_its_checkout_into_a_new_reading() {
        let before = showing(&[("/work/a", 30), ("/work/b", 20), ("/work/c", 10)]);
        let selected = before.path_at(1).unwrap();
        assert_eq!(selected, "/work/b");

        // The second reading orders the checkouts anew, and one of them left.
        let after = showing(&[("/work/b", 50), ("/work/c", 10)]);
        assert_eq!(after.row_of(&selected), Some(0));
        assert_eq!(after.row_of("/work/a"), None);
        assert_eq!(after.path_at(-1), None);
        assert_eq!(after.path_at(9), None);
    }

    #[test]
    fn a_second_request_starts_no_measurement_while_one_runs() {
        let mut state = State::default();
        assert!(state.begin());
        assert!(!state.begin());
        state.measuring = false;
        assert!(state.begin());
    }
}

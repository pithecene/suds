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
fn show_checkouts(window: &Window, state: &mut State, query: &str, sort: i32) {
    let Some(view) = &state.view else {
        return;
    };
    // The selected checkout stays selected while the new arrangement still holds it.
    let selected = usize::try_from(window.get_selected())
        .ok()
        .and_then(|row| state.shown.get(row).copied());
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
    let row = selected
        .and_then(|position| state.shown.iter().position(|shown| *shown == position))
        .map_or(-1, |row| row as i32);
    show_detail(window, state, row);
}

/// Draws one reading.
fn show(window: &Window, state: &mut State, reading: Result<Observation, String>) {
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
    state.view = Some(view);
    window.set_measured(true);
    show_checkouts(window, state, &window.get_query(), window.get_sort());
}

/// Measures the workstation on a thread of its own and draws the reading on the interface's thread.
fn measure(window: &Window, state: Rc<RefCell<State>>) {
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
                show_checkouts(&window, &mut state.borrow_mut(), &query, sort);
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

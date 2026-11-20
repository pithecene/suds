//! The Suds window.
//!
//! The window measures the workstation on a thread of its own, through the
//! view model, and draws what the reading holds. It changes nothing on disk.

mod present;

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use slint::{ComponentHandle, ModelRc, VecModel, Weak};
use suds_core::development::Environment;
use suds_view::{Checkout, Observation, Project, Sort, Workstation, map, observe, projects};

slint::include_modules!();

/// One row of the outline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    /// A project, by its position among the projects.
    Project(usize),
    /// A checkout, by its position among the reading's checkouts.
    Checkout(usize),
}

/// What the window has selected, by path. A position names another entry after a new reading.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Pick {
    Project(String),
    Checkout(String),
}

/// The height of one row of the outline, as `window.slint` draws it.
const ROW: f32 = 38.0;

/// What the window holds from one event to the next.
#[derive(Default)]
struct State {
    view: Option<Workstation>,
    /// The projects that the filter keeps, in the order of the sort.
    projects: Vec<Project>,
    rows: Vec<Row>,
    /// The paths of the projects whose checkouts the outline shows.
    open: BTreeSet<String>,
    /// Whether the query holds text. The outline then shows each kept checkout.
    filtered: bool,
    pick: Option<Pick>,
    /// The size of the storage map.
    map: (f32, f32),
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

    fn checkouts(&self) -> &[Checkout] {
        self.view.as_ref().map_or(&[], |view| &view.checkouts)
    }

    /// Groups the checkouts that `query` keeps, in the order of `sort`, and lays the outline out.
    fn group(&mut self, query: &str, sort: Sort) {
        self.projects = projects(self.checkouts(), query, sort);
        self.filtered = !query.trim().is_empty();
        self.lay();
    }

    /// Whether the outline shows the checkouts of the project at `path`.
    fn shows(&self, path: &str) -> bool {
        self.filtered || self.open.contains(path)
    }

    /// Lays the outline out: each project, and beneath an open one, its checkouts.
    fn lay(&mut self) {
        let mut rows = Vec::new();
        for (position, project) in self.projects.iter().enumerate() {
            rows.push(Row::Project(position));
            if self.shows(&project.path) {
                rows.extend(project.members.iter().map(|member| Row::Checkout(*member)));
            }
        }
        self.rows = rows;
    }

    /// What the outline's row at `row` selects.
    fn pick_at(&self, row: i32) -> Option<Pick> {
        Some(match *self.rows.get(usize::try_from(row).ok()?)? {
            Row::Project(position) => Pick::Project(self.projects.get(position)?.path.clone()),
            Row::Checkout(position) => Pick::Checkout(self.checkouts().get(position)?.path.clone()),
        })
    }

    /// The row of the outline that shows `pick`. A checkout of a closed project has none.
    fn row_of(&self, pick: &Pick) -> Option<i32> {
        let row = self.rows.iter().position(|row| match (row, pick) {
            (Row::Project(position), Pick::Project(path)) => self.projects[*position].path == *path,
            (Row::Checkout(position), Pick::Checkout(path)) => {
                self.checkouts()[*position].path == *path
            }
            _ => false,
        })?;
        i32::try_from(row).ok()
    }

    /// The position among the projects of the one that holds the checkout at `checkout`.
    fn project_of(&self, checkout: usize) -> Option<usize> {
        self.projects
            .iter()
            .position(|project| project.members.contains(&checkout))
    }

    /// Selects `pick`, or lets the selection go when the selection is `pick` already.
    fn toggle(&mut self, pick: Pick) {
        self.pick = if self.pick.as_ref() == Some(&pick) {
            None
        } else {
            Some(pick)
        };
    }

    /// Opens or closes the project at `path`.
    fn set_open(&mut self, path: &str, open: bool) {
        if open {
            self.open.insert(path.to_owned());
        } else {
            self.open.remove(path);
        }
        self.lay();
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

/// Marks the selection in the outline and on the map, and fills the detail pane with it.
/// A selection that the reading or the filter no longer holds lets go.
fn show_pick(window: &Window, state: &mut State) {
    let home = state.home.as_deref();
    let found = state.pick.as_ref().and_then(|pick| match pick {
        Pick::Project(path) => {
            let position = state.projects.iter().position(|p| p.path == *path)?;
            let project = &state.projects[position];
            window.set_detail_kind("PROJECT".into());
            window.set_detail_name(project.name.as_str().into());
            window.set_detail_path(present::short(&project.path, home).into());
            window.set_detail_facts(model(present::project_facts(project)));
            window.set_detail_builds(model(Vec::new()));
            Some((-1, i32::try_from(position).ok()?))
        }
        Pick::Checkout(path) => {
            let position = state.checkouts().iter().position(|c| c.path == *path)?;
            let frame = state.project_of(position)?;
            let checkout = &state.checkouts()[position];
            window.set_detail_kind("CHECKOUT".into());
            window.set_detail_name(checkout.name.as_str().into());
            window.set_detail_path(present::short(&checkout.path, home).into());
            window.set_detail_facts(model(present::facts(checkout, home)));
            window.set_detail_builds(model(present::builds(checkout)));
            Some((i32::try_from(position).ok()?, i32::try_from(frame).ok()?))
        }
    });
    if found.is_none() {
        state.pick = None;
        window.set_detail_facts(model(Vec::new()));
        window.set_detail_builds(model(Vec::new()));
    }
    let (tile, frame) = found.unwrap_or((-1, -1));
    window.set_has_detail(found.is_some());
    window.set_selected_tile(tile);
    window.set_selected_frame(frame);
    let row = state.pick.as_ref().and_then(|pick| state.row_of(pick));
    window.set_selected(row.unwrap_or(-1));
}

/// Fills the outline with the state's rows.
fn show_rows(window: &Window, state: &State) {
    let home = state.home.as_deref();
    let largest = state.projects.iter().map(Project::total_bytes).max();
    let rows = state.rows.iter().map(|row| match *row {
        Row::Project(position) => {
            let project = &state.projects[position];
            present::project_row(project, state.shows(&project.path), home)
        }
        Row::Checkout(position) => {
            present::checkout_row(&state.checkouts()[position], largest.unwrap_or(0), home)
        }
    });
    window.set_rows(model(rows.collect()));
}

/// Draws the storage map of the state's projects at the map's size.
fn show_map(window: &Window, state: &State) {
    let (width, height) = state.map;
    let drawn = map(&state.projects, state.checkouts(), width, height);
    let frames = drawn.frames.iter();
    window.set_frames(model(
        frames
            .map(|frame| present::frame(frame, &state.projects[frame.project]))
            .collect(),
    ));
    let tiles = drawn.tiles.iter().map(|tile| {
        let checkout = &state.checkouts()[tile.checkout];
        present::tile(tile, checkout, &state.projects[tile.project])
    });
    window.set_tiles(model(tiles.collect()));
}

/// Groups the checkouts that `query` keeps, in the order of `sort`, and draws the outline,
/// the map, and the selection.
fn show_projects(window: &Window, state: &mut State, query: &str, sort: i32) {
    state.group(query, sort_of(sort));
    let kept: usize = state.projects.iter().map(|p| p.members.len()).sum();
    window.set_checkout_count(
        present::counts(state.projects.len(), kept, state.checkouts().len()).into(),
    );
    show_rows(window, state);
    show_map(window, state);
    show_pick(window, state);
}

/// The outline's offset that shows the row at `row`, from its offset now and the height that
/// it shows. The offsets count downward from the outline's top.
fn revealing(row: i32, offset: f32, visible: f32) -> f32 {
    let top = row as f32 * ROW;
    if top < offset {
        top
    } else if top + ROW > offset + visible {
        top + ROW - visible
    } else {
        offset
    }
}

/// Scrolls the outline until it shows the selected row.
fn reveal(window: &Window) {
    let (row, visible) = (window.get_selected(), window.get_list_visible());
    if row >= 0 && visible > 0.0 {
        window.set_list_y(-revealing(row, -window.get_list_y(), visible));
    }
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
    // The selection names its entry by path, so it follows the entry into the new reading.
    state.view = Some(view);
    window.set_measured(true);
    show_projects(window, state, &window.get_query(), window.get_sort());
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

/// A click on the outline's row at `row`.
fn activate(window: &Window, state: &mut State, row: i32) {
    let Some(pick) = state.pick_at(row) else {
        return;
    };
    match &pick {
        // A click on a project opens or closes it and selects it.
        Pick::Project(path) => {
            let open = !state.open.contains(path);
            state.set_open(path, open);
            state.pick = Some(pick);
            show_rows(window, state);
        }
        Pick::Checkout(_) => state.toggle(pick),
    }
    show_pick(window, state);
}

/// The right or the left key on the outline's row at `row`.
fn open(window: &Window, state: &mut State, row: i32, open: bool) {
    match state.pick_at(row) {
        Some(Pick::Project(path)) => state.set_open(&path, open),
        // The left key on a checkout moves to its project.
        Some(Pick::Checkout(path)) if !open => {
            let position = state.checkouts().iter().position(|c| c.path == path);
            let project = position.and_then(|position| state.project_of(position));
            if let Some(project) = project {
                state.pick = Some(Pick::Project(state.projects[project].path.clone()));
            }
        }
        _ => return,
    }
    show_rows(window, state);
    show_pick(window, state);
    reveal(window);
}

/// A click on the map's tile of the checkout at `checkout`.
fn pick(window: &Window, state: &mut State, checkout: i32) {
    let position = usize::try_from(checkout).ok();
    let found = position.and_then(|position| {
        let path = state.checkouts().get(position)?.path.clone();
        Some((path, state.project_of(position)?))
    });
    let Some((path, project)) = found else {
        return;
    };
    state.toggle(Pick::Checkout(path));
    // The outline opens the project, so the selected checkout has a row.
    let project = state.projects[project].path.clone();
    state.set_open(&project, true);
    show_rows(window, state);
    show_pick(window, state);
    reveal(window);
}

/// A click on the map's frame of the project at `frame`.
fn pick_project(window: &Window, state: &mut State, frame: i32) {
    let position = usize::try_from(frame).ok();
    if let Some(project) = position.and_then(|position| state.projects.get(position)) {
        state.toggle(Pick::Project(project.path.clone()));
        show_pick(window, state);
        reveal(window);
    }
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
    // Each remaining callback changes the state and then draws what changed.
    macro_rules! on {
        (|$window:ident, $state:ident $(, $argument:ident)*| $body:expr) => {{
            let (weak, state) = (window.as_weak(), state.clone());
            move |$($argument),*| {
                if let Some($window) = weak.upgrade() {
                    let $state = &mut *state.borrow_mut();
                    $body
                }
            }
        }};
    }
    window.on_arrange(on!(|window, state, query, sort| show_projects(
        &window, state, &query, sort
    )));
    window.on_activate(on!(|window, state, row| activate(&window, state, row)));
    window.on_move(on!(|window, state, row| {
        state.pick = state.pick_at(row);
        show_pick(&window, state);
        reveal(&window);
    }));
    window.on_open(on!(|window, state, row, wide| open(
        &window, state, row, wide
    )));
    window.on_clear(on!(|window, state| {
        state.pick = None;
        show_pick(&window, state);
    }));
    window.on_pick(on!(|window, state, checkout| pick(&window, state, checkout)));
    window.on_pick_project(on!(|window, state, frame| pick_project(
        &window, state, frame
    )));
    window.on_layout(on!(|window, state, width, height| {
        state.map = (width, height);
        show_map(&window, state);
    }));

    measure(&window, state);
    window.run()
}

#[cfg(test)]
mod tests {
    use suds_core::development::{Checkout, CheckoutKind, DevelopmentScan};
    use suds_core::model::Measure;
    use suds_core::stores::Budgets;
    use suds_view::{Sort, workstation};

    use super::{Pick, ROW, Row, State, revealing};

    /// A state that shows a reading of the named checkouts, each with its size and its repository.
    fn showing(checkouts: &[(&str, u64, Option<&str>)]) -> State {
        let scan = DevelopmentScan {
            workspaces: Vec::new(),
            checkouts: checkouts
                .iter()
                .map(|(path, bytes, repository)| Checkout {
                    path: path.into(),
                    kind: if repository.is_some() {
                        CheckoutKind::Linked
                    } else {
                        CheckoutKind::Main
                    },
                    repository: repository.map(Into::into),
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
        let mut state = State {
            view: Some(view),
            ..State::default()
        };
        state.group("", Sort::Size);
        state
    }

    const READING: [(&str, u64, Option<&str>); 3] = [
        ("/work/a", 30, None),
        ("/work/a/.trees/one", 20, Some("/work/a")),
        ("/work/b", 10, None),
    ];

    fn checkout(path: &str) -> Pick {
        Pick::Checkout(path.into())
    }

    #[test]
    fn the_outline_shows_the_checkouts_of_a_project_once_it_opens() {
        let mut state = showing(&READING);
        assert_eq!(state.rows, [Row::Project(0), Row::Project(1)]);
        assert_eq!(state.pick_at(0), Some(Pick::Project("/work/a".into())));

        state.set_open("/work/a", true);
        assert_eq!(state.rows.len(), 4);
        assert_eq!(state.pick_at(2), Some(checkout("/work/a/.trees/one")));
        assert_eq!(state.pick_at(-1), None);
        assert_eq!(state.pick_at(9), None);

        // A filter shows each kept checkout, whether its project is open or not.
        state.group("/work/b", Sort::Size);
        assert_eq!(state.rows, [Row::Project(0), Row::Checkout(2)]);
    }

    #[test]
    fn a_selection_follows_its_checkout_into_a_new_reading() {
        let selected = checkout("/work/a/.trees/one");
        let mut before = showing(&READING);
        before.set_open("/work/a", true);
        assert_eq!(before.row_of(&selected), Some(2));

        // The second reading orders the projects anew, and one checkout left.
        let mut after = showing(&[
            ("/work/b", 90, None),
            ("/work/a/.trees/one", 20, Some("/work/a")),
        ]);
        assert_eq!(after.row_of(&selected), None);
        after.set_open("/work/a", true);
        assert_eq!(after.row_of(&selected), Some(2));
        assert_eq!(after.row_of(&checkout("/work/a")), None);
    }

    #[test]
    fn a_second_choice_of_the_selection_lets_it_go() {
        let mut state = showing(&READING);
        state.toggle(checkout("/work/b"));
        assert_eq!(state.pick, Some(checkout("/work/b")));
        state.toggle(checkout("/work/b"));
        assert_eq!(state.pick, None);
    }

    #[test]
    fn the_outline_scrolls_only_as_far_as_the_selected_row() {
        let visible = 4.0 * ROW;
        // A row inside the shown rows moves nothing.
        assert_eq!(revealing(2, 0.0, visible), 0.0);
        // A row beneath them arrives at the bottom, and one above them at the top.
        assert_eq!(revealing(9, 0.0, visible), 6.0 * ROW);
        assert_eq!(revealing(1, 6.0 * ROW, visible), ROW);
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

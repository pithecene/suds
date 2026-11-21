//! The Suds window.
//!
//! The window measures the workstation on a thread of its own, through the
//! view model, and draws what the reading holds. It changes the disk only
//! through a reclaim plan that a person approves.

mod plan;
mod present;

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel, Weak};
use suds_core::development::Environment;
use suds_core::reclaim::execute::ReclaimReport;
use suds_core::reclaim::plan::ReclaimPlan;
use suds_view::{
    Checkout, Observation, Project, Proposal, Sort, Workstation, map, observe, projects, propose,
};

slint::include_modules!();

/// One row of the outline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    /// A project, by its position among the projects.
    Project(usize),
    /// A checkout, by its position among the reading's checkouts.
    Checkout(usize),
}

/// What the window has selected: a project by its key, or a checkout by its path. A position
/// names another entry after a new reading.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Pick {
    Project(String),
    Checkout(String),
}

/// The height of one row of the outline, as `window.slint` draws it.
const ROW: f32 = 38.0;

/// The height of one row of the plan's removals, as `window.slint` draws it.
const PLAN_ROW: f32 = 44.0;

/// What the window holds from one event to the next.
#[derive(Default)]
struct State {
    view: Option<Workstation>,
    /// The projects that the filter keeps, in the order of the sort.
    projects: Vec<Project>,
    rows: Vec<Row>,
    /// The keys of the projects whose checkouts the outline shows.
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
    /// The reclaim plan that the window shows.
    proposal: Option<Proposal>,
    /// The removals that a person left out of the run, by their candidates.
    excluded: BTreeSet<usize>,
    /// The removal whose evidence the window shows.
    evidence: Option<usize>,
    /// Whether a plan or a run is in progress. A second request starts none.
    working: bool,
    /// Whether a run is removing directories. The window stays until it ends.
    applying: bool,
    /// Whether a run began since the plan's view opened. A run that fails may still have
    /// changed the disk.
    ran: bool,
    /// Whether a person asked the window to close during a run.
    leaving: bool,
}

impl State {
    /// Marks a measurement as running, unless one runs already.
    fn begin(&mut self) -> bool {
        !std::mem::replace(&mut self.measuring, true)
    }

    /// Marks a plan or a run as in progress, unless one is already.
    fn work(&mut self) -> bool {
        !std::mem::replace(&mut self.working, true)
    }

    /// Begins a run of the marked part of the plan, and returns that part. A run in
    /// progress, or a view without a plan, begins none.
    fn approve(&mut self) -> Option<ReclaimPlan> {
        let chosen = self.proposal.as_ref()?.chosen(&self.excluded);
        if !self.work() {
            return None;
        }
        (self.applying, self.ran) = (true, true);
        Some(chosen)
    }

    /// Ends a run, and returns whether a person asked the window to close during it.
    fn finish(&mut self) -> bool {
        (self.working, self.applying) = (false, false);
        self.leaving
    }

    /// Whether the window may close now. A run in progress keeps it open until the run
    /// ends, so no removal stops halfway and the journal records each outcome.
    fn leave(&mut self) -> bool {
        self.leaving = self.applying;
        !self.applying
    }

    /// Moves the evidence's selection by `step` rows of the plan and returns the new row.
    /// The first move selects the first row.
    fn step(&mut self, step: i32) -> Option<usize> {
        let removals = &self.proposal.as_ref()?.removals;
        let last = removals.len().checked_sub(1)?;
        let now = self.evidence.and_then(|candidate| {
            let mut rows = removals.iter();
            rows.position(|entry| entry.candidate == candidate)
        });
        let row = match now {
            Some(row) => row.saturating_add_signed(step as isize).min(last),
            None => 0,
        };
        self.evidence = Some(removals[row].candidate);
        Some(row)
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
            if self.shows(&project.key) {
                rows.extend(project.members.iter().map(|member| Row::Checkout(*member)));
            }
        }
        self.rows = rows;
    }

    /// What the outline's row at `row` selects.
    fn pick_at(&self, row: i32) -> Option<Pick> {
        Some(match *self.rows.get(usize::try_from(row).ok()?)? {
            Row::Project(position) => Pick::Project(self.projects.get(position)?.key.clone()),
            Row::Checkout(position) => Pick::Checkout(self.checkouts().get(position)?.path.clone()),
        })
    }

    /// The row of the outline that shows `pick`. A checkout of a closed project has none.
    fn row_of(&self, pick: &Pick) -> Option<i32> {
        let row = self.rows.iter().position(|row| match (row, pick) {
            (Row::Project(position), Pick::Project(path)) => self.projects[*position].key == *path,
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
            let position = state.projects.iter().position(|p| p.key == *path)?;
            let project = &state.projects[position];
            window.set_detail_kind("PROJECT".into());
            window.set_detail_name(project.name.as_str().into());
            window.set_detail_path(present::short(&project.key, home).into());
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
            present::project_row(project, state.shows(&project.key), home)
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
    revealing_of(row, ROW, offset, visible)
}

/// The offset of a list with rows of `height` that shows the row at `row`.
fn revealing_of(row: i32, height: f32, offset: f32, visible: f32) -> f32 {
    let top = row as f32 * height;
    if top < offset {
        top
    } else if top + height > offset + visible {
        top + height - visible
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
    window.set_observed(present::observed(&view.observed_at).into());
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

/// Runs `work` on a thread of its own and hands its result to `done` on the interface's thread.
fn background<T: Send + 'static>(
    window: &Window,
    work: impl FnOnce() -> T + Send + 'static,
    done: impl FnOnce(&Window, &mut State, T) + Send + 'static,
) {
    let weak: Weak<Window> = window.as_weak();
    std::thread::spawn(move || {
        let result = work();
        let _ = weak.upgrade_in_event_loop(move |window| {
            STATE.with(|slot| {
                if let Some(state) = slot.borrow().as_ref() {
                    done(&window, &mut state.borrow_mut(), result);
                }
            });
        });
    });
}

/// Draws the plan's sums and the evidence of the selected removal.
fn show_choice(window: &Window, state: &State) {
    let Some(proposal) = &state.proposal else {
        return;
    };
    let removing = proposal.removing(&state.excluded);
    window.set_plan_summary(plan::summary(removing).into());
    window.set_plan_action(plan::action(removing).into());
    window.set_plan_count(i32::try_from(removing.0).unwrap_or(i32::MAX));
    let picked = state.evidence;
    let facts = picked.map(|candidate| plan::evidence(proposal, candidate, state.home.as_deref()));
    let facts = facts
        .unwrap_or_default()
        .into_iter()
        .map(SharedString::from);
    window.set_plan_evidence(model(facts.collect()));
    let selected = picked.and_then(|candidate| i32::try_from(candidate).ok());
    window.set_plan_selected(selected.unwrap_or(-1));
}

/// Opens the plan's view and builds a plan on a thread of its own.
fn plan_reclaim(window: &Window, state: &mut State, tolerate: bool) {
    if !state.work() {
        return;
    }
    (state.proposal, state.evidence, state.ran) = (None, None, false);
    state.excluded.clear();
    window.set_page(1);
    window.set_planning(true);
    window.set_has_outcome(false);
    window.set_confirming(false);
    window.set_plan_error("".into());
    window.set_plan_summary("".into());
    window.set_plan_action(plan::action((0, 0)).into());
    window.set_plan_count(0);
    window.set_plan_hidden("".into());
    window.set_plan_rows(model(Vec::new()));
    window.set_plan_kept(model(Vec::new()));
    window.set_plan_evidence(model(Vec::new()));
    background(
        window,
        move || propose(&Environment::of_process(), tolerate, now()),
        |window, state, proposal| {
            state.working = false;
            window.set_planning(false);
            match proposal {
                Err(reason) => {
                    window.set_plan_error(format!("Suds could not plan: {reason}").into());
                }
                Ok(proposal) => {
                    let rows = proposal.removals.iter();
                    let rows = rows.map(|entry| plan::row(entry, &state.excluded));
                    window.set_plan_rows(model(rows.collect()));
                    window.set_plan_kept(model(proposal.kept.iter().map(plan::kept).collect()));
                    window.set_plan_hidden(plan::notice(&proposal).into());
                    window.set_plan_held(proposal.held_by_hidden());
                    state.proposal = Some(proposal);
                    show_choice(window, state);
                }
            }
        },
    );
}

/// Marks the removal at `candidate` as left out or taken in, and draws each row's mark in place.
fn plan_toggle(window: &Window, state: &mut State, candidate: i32) {
    let (Some(proposal), Ok(candidate)) = (&state.proposal, usize::try_from(candidate)) else {
        return;
    };
    proposal.toggle(&mut state.excluded, candidate);
    // The rows keep their model, so the list keeps its place.
    let rows = window.get_plan_rows();
    for (position, entry) in proposal.removals.iter().enumerate() {
        rows.set_row_data(position, plan::row(entry, &state.excluded));
    }
    window.set_confirming(false);
    show_choice(window, state);
}

/// Applies the approved part of the plan on a thread of its own and shows what the run did.
fn plan_apply(window: &Window, state: &mut State) {
    let Some(chosen) = state.approve() else {
        return;
    };
    window.set_applying(true);
    background(
        window,
        move || suds_view::apply(&Environment::of_process(), &chosen, now()),
        |window, state, report: Result<ReclaimReport, String>| {
            let leaving = state.finish();
            window.set_applying(false);
            match report {
                Err(reason) => {
                    window.set_plan_error(format!("Suds could not run the plan: {reason}").into());
                }
                Ok(report) => {
                    let (rows, line) = plan::outcome(&report, state.home.as_deref());
                    window.set_outcome_rows(model(rows));
                    window.set_outcome_summary(line.into());
                    window.set_has_outcome(true);
                }
            }
            // A person asked the window to close during the run, and the run has ended.
            if leaving {
                let _ = slint::quit_event_loop();
            }
        },
    );
}

/// Moves the evidence's selection by `step` rows and scrolls the plan until it shows the row.
fn plan_step(window: &Window, state: &mut State, step: i32) {
    let Some(row) = state.step(step) else {
        return;
    };
    show_choice(window, state);
    let (row, visible) = (i32::try_from(row).unwrap_or(0), window.get_plan_visible());
    if visible > 0.0 {
        let offset = revealing_of(row, PLAN_ROW, -window.get_plan_y(), visible);
        window.set_plan_y(-offset);
    }
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
                state.pick = Some(Pick::Project(state.projects[project].key.clone()));
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
    let project = state.projects[project].key.clone();
    state.set_open(&project, true);
    show_rows(window, state);
    show_pick(window, state);
    reveal(window);
}

/// A click on the map's frame of the project at `frame`.
fn pick_project(window: &Window, state: &mut State, frame: i32) {
    let position = usize::try_from(frame).ok();
    if let Some(project) = position.and_then(|position| state.projects.get(position)) {
        state.toggle(Pick::Project(project.key.clone()));
        show_pick(window, state);
        reveal(window);
    }
}

fn main() -> Result<(), slint::PlatformError> {
    let window = Window::new()?;
    // A desktop finds the window's name and icon in the entry `suds.desktop`, by this identifier.
    // The window takes it when it first shows.
    slint::set_xdg_app_id("suds")?;
    let home = Environment::of_process().home;
    let state = Rc::new(RefCell::new(State {
        home: home.map(|home| home.to_string_lossy().into_owned()),
        ..State::default()
    }));

    // The key and the desktop's close button both wait for a run in progress.
    window.on_quit({
        let (weak, state) = (window.as_weak(), state.clone());
        move || {
            if state.borrow_mut().leave() {
                let _ = slint::quit_event_loop();
            } else if let Some(window) = weak.upgrade() {
                window.set_closing(true);
            }
        }
    });
    window.window().on_close_requested({
        let (weak, state) = (window.as_weak(), state.clone());
        move || {
            if state.borrow_mut().leave() {
                return slint::CloseRequestResponse::HideWindow;
            }
            if let Some(window) = weak.upgrade() {
                window.set_closing(true);
            }
            slint::CloseRequestResponse::KeepWindowShown
        }
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
    window.on_pick(on!(|window, state, checkout| pick(
        &window, state, checkout
    )));
    window.on_pick_project(on!(|window, state, frame| pick_project(
        &window, state, frame
    )));
    window.on_layout(on!(|window, state, width, height| {
        state.map = (width, height);
        show_map(&window, state);
    }));
    window.on_plan(on!(|window, state, tolerate| plan_reclaim(
        &window, state, tolerate
    )));
    window.on_plan_toggle(on!(|window, state, candidate| plan_toggle(
        &window, state, candidate
    )));
    window.on_plan_pick(on!(|window, state, candidate| {
        let candidate = usize::try_from(candidate).ok();
        state.evidence = candidate.filter(|candidate| state.evidence != Some(*candidate));
        show_choice(&window, state);
    }));
    window.on_plan_step(on!(|window, state, step| plan_step(&window, state, step)));
    window.on_plan_apply(on!(|window, state| plan_apply(&window, state)));
    window.on_plan_close({
        let (weak, state) = (window.as_weak(), state.clone());
        move || {
            let Some(window) = weak.upgrade() else {
                return;
            };
            window.set_page(0);
            state.borrow_mut().proposal = None;
            // A run that began may have changed the disk, whether it ended well or not, so
            // the storage's figures are stale.
            if state.borrow().ran {
                measure(&window, state.clone());
            }
        }
    });

    STATE.with(|slot| *slot.borrow_mut() = Some(state.clone()));

    measure(&window, state);
    window.run()
}

#[cfg(test)]
mod tests {
    use suds_core::development::{Checkout, CheckoutKind, DevelopmentScan};
    use suds_core::model::Measure;
    use suds_core::stores::Budgets;
    use suds_view::{Sort, workstation};

    use suds_core::reclaim::plan::{RECLAIM_SCHEMA, ReclaimPlan, Subject};
    use suds_view::{Entry, Proposal};

    use super::{PLAN_ROW, Pick, ROW, Row, State, revealing, revealing_of};

    /// A state that shows a plan of removals with these candidates.
    fn planned(candidates: &[usize]) -> State {
        let plan = ReclaimPlan {
            schema: RECLAIM_SCHEMA.into(),
            id: "plan-1".into(),
            created_at: "2026-11-21T00:00:00Z".into(),
            idle_seconds: 3600,
            tolerate_unreadable_processes: false,
            candidates: Vec::new(),
        };
        let entry = |candidate: &usize| Entry {
            candidate: *candidate,
            path: format!("/work/{candidate}"),
            checkout: format!("/work/{candidate}"),
            project: "work".into(),
            subject: Subject::LinkedCheckout,
            bytes: 1,
            within: None,
            evidence: Vec::new(),
        };
        State {
            proposal: Some(Proposal {
                removals: candidates.iter().map(entry).collect(),
                ..Proposal::of(plan, Vec::new())
            }),
            ..State::default()
        }
    }

    #[test]
    fn the_keys_move_the_evidence_through_the_plan_and_stop_at_its_ends() {
        let mut state = planned(&[7, 3, 5]);
        assert_eq!(state.step(1), Some(0));
        assert_eq!(state.evidence, Some(7));
        assert_eq!(state.step(1), Some(1));
        assert_eq!(state.step(5), Some(2));
        assert_eq!(state.evidence, Some(5));
        assert_eq!(state.step(-9), Some(0));
        assert_eq!(planned(&[]).step(1), None);
        assert_eq!(State::default().step(1), None);

        // The plan scrolls by its own rows' height.
        assert_eq!(
            revealing_of(5, PLAN_ROW, 0.0, 2.0 * PLAN_ROW),
            4.0 * PLAN_ROW
        );
    }

    #[test]
    fn a_run_keeps_the_window_open_and_marks_the_storage_stale() {
        let mut state = planned(&[7]);
        assert!(state.leave());
        assert!(!state.ran);

        assert!(state.approve().is_some());
        // A second approval starts no run, and the window waits for the first.
        assert!(state.approve().is_none());
        assert!(state.ran);
        assert!(!state.leave());
        // The run ends and reports the wish to close. A run that failed is stale too.
        assert!(state.finish());
        assert!(state.ran);
        assert!(state.leave());
        assert!(State::default().approve().is_none());
    }

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

    #[test]
    fn a_second_request_starts_no_plan_while_one_is_in_progress() {
        let mut state = State::default();
        assert!(state.work());
        assert!(!state.work());
        // A measurement and a plan guard themselves apart.
        assert!(state.begin());
        state.working = false;
        assert!(state.work());
    }
}

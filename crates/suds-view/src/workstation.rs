//! The first view: one workstation's development storage.

use std::path::Path;

use suds_core::development::{self, CheckoutKind, DevelopmentReport, DevelopmentScan, StoreStatus};
use suds_core::pressure::{Level, Pressure};
use suds_core::registry::{self, Event, Reading, Standings};
use suds_core::stores::Budgets;

/// The usage of development storage against its ceiling.
#[derive(Debug, Clone, PartialEq)]
pub struct PressureCard {
    pub level: Level,
    pub usage_bytes: u64,
    pub ceiling_bytes: u64,
    /// The usage as a share of the ceiling, from 0 to 1.
    pub share: f32,
    pub filesystem_available_bytes: Option<u64>,
    pub enforced: bool,
    /// Whether every root answered. A partial measurement is a lower bound.
    pub complete: bool,
}

/// One class of development storage.
#[derive(Debug, Clone, PartialEq)]
pub struct Class {
    pub label: &'static str,
    pub bytes: u64,
    pub count: u64,
    /// The class's share of all classes, from 0 to 1.
    pub share: f32,
}

/// A client's hold on a checkout, as the registry last recorded it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hold {
    pub client: String,
    pub since: String,
    /// Whether the client released the checkout. A released checkout keeps its record.
    pub released: bool,
    /// The commit that a release reported as landed.
    pub landed: Option<String>,
}

/// One directory of build state beneath a checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildState {
    pub path: String,
    pub ecosystem: &'static str,
    pub bytes: u64,
    pub newest_modified: String,
}

/// One checkout with its build state.
#[derive(Debug, Clone, PartialEq)]
pub struct Checkout {
    pub path: String,
    pub name: String,
    pub parent: String,
    pub linked: bool,
    /// The main checkout of a linked checkout's repository.
    pub repository: Option<String>,
    pub own_bytes: u64,
    pub build_bytes: u64,
    pub build_state: Vec<BuildState>,
    pub newest_modified: String,
    pub hold: Option<Hold>,
    /// The checkout's size as a share of the largest checkout's, from 0 to 1.
    pub share: f32,
}

impl Checkout {
    pub fn total_bytes(&self) -> u64 {
        self.own_bytes.saturating_add(self.build_bytes)
    }
}

/// How a store stands against its budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreStanding {
    Absent,
    Unreadable,
    /// The store has no budget, so no figure bounds it.
    Unbudgeted,
    Within,
    Over,
}

/// One shared store or toolchain.
#[derive(Debug, Clone, PartialEq)]
pub struct Store {
    pub id: String,
    pub path: String,
    pub bytes: u64,
    pub budget_bytes: Option<u64>,
    pub standing: StoreStanding,
    /// The store's size as a share of its budget, or of the largest store's without one.
    pub share: f32,
}

/// Everything the first view shows.
#[derive(Debug, Clone, PartialEq)]
pub struct Workstation {
    pub observed_at: String,
    pub pressure: Option<PressureCard>,
    pub classes: Vec<Class>,
    /// The checkouts in order of their size, largest first.
    pub checkouts: Vec<Checkout>,
    /// The stores in order of their size, largest first.
    pub stores: Vec<Store>,
    pub error_count: u64,
    /// The reason that the registry gave no reading. The view then shows no
    /// hold, which differs from a registry that holds none.
    pub registry_error: Option<String>,
}

/// The order of the checkout list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Sort {
    #[default]
    Size,
    Name,
    Modified,
    BuildState,
}

fn share(part: u64, whole: u64) -> f32 {
    if whole == 0 {
        return 0.0;
    }
    (part as f64 / whole as f64).clamp(0.0, 1.0) as f32
}

fn split(path: &Path) -> (String, String) {
    let text = |part: Option<&std::ffi::OsStr>| {
        part.map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    (
        text(path.file_name()),
        text(path.parent().map(Path::as_os_str)),
    )
}

fn pressure_card(pressure: &Pressure) -> PressureCard {
    PressureCard {
        level: pressure.level,
        usage_bytes: pressure.usage_bytes,
        ceiling_bytes: pressure.ceiling_bytes,
        share: share(pressure.usage_bytes, pressure.ceiling_bytes),
        filesystem_available_bytes: pressure.filesystem_available_bytes,
        enforced: pressure.enforced,
        complete: pressure.complete,
    }
}

fn classes(report: &DevelopmentReport) -> Vec<Class> {
    let totals = &report.totals;
    let each = [
        ("Main checkouts", totals.main_checkouts),
        ("Linked checkouts", totals.linked_checkouts),
        ("Build state", totals.build_state),
        ("Shared stores", totals.shared_stores),
        ("Toolchains", totals.toolchains),
        ("Machines", totals.machines),
    ];
    let whole: u64 = each.iter().map(|(_, total)| total.allocated_bytes).sum();
    each.into_iter()
        .map(|(label, total)| Class {
            label,
            bytes: total.allocated_bytes,
            count: total.count,
            share: share(total.allocated_bytes, whole),
        })
        .collect()
}

fn checkouts(scan: &DevelopmentScan, standings: &Standings) -> Vec<Checkout> {
    let shown = |path: &Path| path.to_string_lossy().into_owned();
    let mut rows: Vec<Checkout> = scan
        .checkouts
        .iter()
        .map(|checkout| {
            let (name, parent) = split(&checkout.path);
            let build_state: Vec<BuildState> = checkout
                .build_state
                .iter()
                .map(|build| BuildState {
                    path: shown(&build.path),
                    ecosystem: build.ecosystem.label(),
                    bytes: build.measure.allocated_bytes,
                    newest_modified: build.measure.newest_modified.clone().unwrap_or_default(),
                })
                .collect();
            // The registry names a checkout by its native path, so the lookup takes that path
            // and never its displayed text. A checkout's own record shows a release too, and a
            // registration above it holds it otherwise.
            let above =
                || registry::holder(standings, &checkout.path).map(|(_, standing)| standing);
            let hold = standings
                .get(&checkout.path)
                .or_else(above)
                .map(|standing| Hold {
                    client: standing.client.clone(),
                    since: standing.at.clone(),
                    released: standing.event == Event::Released,
                    landed: standing.landed.clone(),
                });
            Checkout {
                name,
                parent,
                linked: checkout.kind == CheckoutKind::Linked,
                repository: checkout.repository.as_deref().map(shown),
                own_bytes: checkout.own.allocated_bytes,
                build_bytes: build_state.iter().map(|build| build.bytes).sum(),
                build_state,
                newest_modified: checkout.own.newest_modified.clone().unwrap_or_default(),
                hold,
                share: 0.0,
                path: shown(&checkout.path),
            }
        })
        .collect();
    rows.sort_by(|a, b| {
        b.total_bytes()
            .cmp(&a.total_bytes())
            .then_with(|| a.path.cmp(&b.path))
    });
    let largest = rows.first().map_or(0, Checkout::total_bytes);
    for row in &mut rows {
        row.share = share(row.total_bytes(), largest);
    }
    rows
}

fn stores(report: &DevelopmentReport, budgets: &Budgets) -> Vec<Store> {
    let largest = report
        .stores
        .iter()
        .map(|store| store.measure.allocated_bytes)
        .max()
        .unwrap_or(0);
    let mut rows: Vec<Store> = report
        .stores
        .iter()
        .map(|store| {
            let bytes = store.measure.allocated_bytes;
            let budget_bytes = budgets.of(&store.id);
            let standing = match (store.status, budget_bytes) {
                (StoreStatus::Absent, _) => StoreStanding::Absent,
                (StoreStatus::Unreadable, _) => StoreStanding::Unreadable,
                (_, None) => StoreStanding::Unbudgeted,
                (_, Some(budget)) if bytes > budget => StoreStanding::Over,
                (_, Some(_)) => StoreStanding::Within,
            };
            Store {
                id: store.id.clone(),
                path: store.path.clone(),
                bytes,
                budget_bytes,
                standing,
                share: share(bytes, budget_bytes.unwrap_or(largest)),
            }
        })
        .collect();
    rows.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.id.cmp(&b.id)));
    rows
}

/// The first view of `scan`, with the last reading of the pressure, the
/// registry's reading, and each store's budget. The scan's time is
/// `observed_at`, in seconds since the Unix epoch.
pub fn workstation(
    scan: &DevelopmentScan,
    observed_at: i64,
    pressure: Option<&Pressure>,
    registry: &Reading,
    budgets: &Budgets,
) -> Workstation {
    let report = development::build_report(scan, observed_at);
    let none = Standings::new();
    Workstation {
        observed_at: report.observed_at.clone(),
        pressure: pressure.map(pressure_card),
        classes: classes(&report),
        checkouts: checkouts(scan, registry.as_ref().unwrap_or(&none)),
        stores: stores(&report, budgets),
        error_count: report.error_count,
        registry_error: registry.as_ref().err().cloned(),
    }
}

/// The positions in `checkouts` of each checkout whose path holds `query`,
/// in the order of `sort`. An empty query keeps every checkout, and the
/// match ignores the case of ASCII letters.
pub fn arrange(checkouts: &[Checkout], query: &str, sort: Sort) -> Vec<usize> {
    let query = query.trim().to_ascii_lowercase();
    let mut kept: Vec<usize> = checkouts
        .iter()
        .enumerate()
        .filter(|(_, row)| query.is_empty() || row.path.to_ascii_lowercase().contains(&query))
        .map(|(position, _)| position)
        .collect();
    let by_path = |a: &usize, b: &usize| checkouts[*a].path.cmp(&checkouts[*b].path);
    match sort {
        Sort::Size => kept.sort_by(|a, b| {
            checkouts[*b]
                .total_bytes()
                .cmp(&checkouts[*a].total_bytes())
                .then_with(|| by_path(a, b))
        }),
        Sort::Name => kept.sort_by(|a, b| {
            let name = |i: &usize| checkouts[*i].name.to_ascii_lowercase();
            name(a).cmp(&name(b)).then_with(|| by_path(a, b))
        }),
        Sort::Modified => kept.sort_by(|a, b| {
            checkouts[*b]
                .newest_modified
                .cmp(&checkouts[*a].newest_modified)
                .then_with(|| by_path(a, b))
        }),
        Sort::BuildState => kept.sort_by(|a, b| {
            checkouts[*b]
                .build_bytes
                .cmp(&checkouts[*a].build_bytes)
                .then_with(|| by_path(a, b))
        }),
    }
    kept
}

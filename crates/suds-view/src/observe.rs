//! One reading of a live workstation through the core.

use suds_core::arena;
use suds_core::development::{self, Environment};
use suds_core::maintain;
use suds_core::pressure::{self, Measurement, Thresholds};
use suds_core::registry;
use suds_core::state::{Directories, Settings, load_settings};
use suds_core::stores::Budgets;

use crate::workstation::{Workstation, workstation};

/// What one reading of the workstation found.
#[derive(Debug, Clone, PartialEq)]
pub enum Observation {
    /// The settings name no root, so no development storage has a measure.
    /// The value is the settings file that a person edits.
    Unconfigured(String),
    Measured(Box<Workstation>),
}

fn budgets_of(settings: &Settings) -> Budgets {
    let mut budgets = Budgets::default();
    for (id, bytes) in &settings.budgets {
        budgets.set(id, *bytes);
    }
    budgets
}

/// Measures the roots that the settings name and returns the first view. The
/// reading changes nothing: it writes no pressure reading and no journal.
/// `now` is the time in seconds since the Unix epoch.
pub fn observe(environment: &Environment, now: i64) -> Result<Observation, String> {
    let directories = Directories::of(environment)
        .ok_or("the environment names no home directory, so Suds has no settings to read")?;
    let settings = load_settings(&directories.settings())?;
    let Some(first) = settings.roots.first() else {
        return Ok(Observation::Unconfigured(
            directories.settings().to_string_lossy().into_owned(),
        ));
    };
    let scan = development::inspect(&settings.roots, environment);
    let complete = scan.error_count == 0 && scan.workspaces.iter().all(|w| w.measured);
    // The pressure counts every root and every store, as a maintenance run counts them.
    let others: Vec<_> = settings
        .roots
        .iter()
        .skip(1)
        .cloned()
        .chain(development::store_locations(environment))
        .collect();
    let audited = arena::audit(first, &others, settings.ceiling_bytes)?;
    let reading = pressure::next(
        maintain::read_pressure(&directories.pressure()).as_ref(),
        Measurement {
            usage_bytes: pressure::usage(&scan),
            ceiling_bytes: audited.ceiling_bytes,
            enforced: audited.enforced,
            filesystem_available_bytes: Some(audited.available_bytes),
            complete,
            now,
        },
        Thresholds::default(),
    );
    // The view states an unreadable registry, so no hold never reads as an empty registry.
    let registry = registry::reading(&directories.registry());
    Ok(Observation::Measured(Box::new(workstation(
        &scan,
        now,
        Some(&reading),
        &registry,
        &budgets_of(&settings),
    ))))
}

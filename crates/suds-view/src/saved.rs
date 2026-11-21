//! The last reading, saved so that a window opens with figures.
//!
//! A measurement walks every file beneath the roots, and a large workspace
//! takes most of a minute. The window saves each whole reading in the state
//! directory and shows the saved one while it measures again.

use std::fs;

use serde::{Deserialize, Serialize};
use suds_core::development::Environment;
use suds_core::state::{Directories, write_staged};

use crate::workstation::Workstation;

/// The schema of a saved reading. A release that changes the view model
/// names a new one, and reads no reading of another.
pub const READING_SCHEMA: &str = "suds.reading/v1";

#[derive(Serialize, Deserialize)]
struct Saved {
    schema: String,
    view: Workstation,
}

/// Saves `view` in the state directory, over the reading before it.
pub fn save_reading(environment: &Environment, view: &Workstation) -> Result<(), String> {
    let directories = Directories::of(environment)
        .ok_or("the environment names no home directory, so Suds has no place for a reading")?;
    let path = directories.reading();
    let failed = |err: std::io::Error| format!("could not write {}: {err}", path.display());
    let saved = Saved {
        schema: READING_SCHEMA.to_owned(),
        view: view.clone(),
    };
    let text = serde_json::to_vec(&saved).map_err(|err| failed(err.into()))?;
    write_staged(&path, &text).map_err(failed)
}

/// The reading that the last measurement saved. A workstation without one
/// has none, and a reading that this release cannot read counts as none,
/// because the measurement that follows replaces it.
pub fn last_reading(environment: &Environment) -> Option<Workstation> {
    let path = Directories::of(environment)?.reading();
    let saved: Saved = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    (saved.schema == READING_SCHEMA).then_some(saved.view)
}

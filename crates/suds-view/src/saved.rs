//! The last reading, saved so that a window opens with figures.
//!
//! A measurement walks every file beneath the roots, and a large workspace
//! takes most of a minute. The window saves each whole reading in the state
//! directory and shows the saved one while it measures again.

use std::fs;
use std::io::ErrorKind;
use std::path::Path;

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

/// The view of a saved reading's text, or nothing for a text of another
/// form or of another schema.
fn parse(text: &[u8]) -> Option<Workstation> {
    let saved = serde_json::from_slice::<Saved>(text).ok()?;
    (saved.schema == READING_SCHEMA).then_some(saved.view)
}

/// The reading that the last measurement saved. A workstation without one
/// has none. A reading that this release cannot parse counts as none too,
/// because the measurement that follows replaces it. A file that Suds could
/// not read is a failure, and the result states it.
pub fn last_reading(environment: &Environment) -> Result<Option<Workstation>, String> {
    let Some(directories) = Directories::of(environment) else {
        return Ok(None);
    };
    let path = directories.reading();
    match fs::read(&path) {
        Ok(text) => Ok(parse(&text)),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(err) => Err(format!("could not read {}: {err}", path.display())),
    }
}

/// The reading in the file at `path`, which a person names. No measurement
/// follows such a reading, so an absent file and a file of another schema
/// are both failures.
pub fn reading_at(path: &Path) -> Result<Workstation, String> {
    let text = fs::read(path).map_err(|err| format!("could not read {}: {err}", path.display()))?;
    parse(&text).ok_or_else(|| {
        format!(
            "{} holds no reading of the schema {READING_SCHEMA}",
            path.display()
        )
    })
}

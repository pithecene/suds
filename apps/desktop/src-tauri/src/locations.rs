//! The locations that the window lists: the platform's default roots and
//! the folders that the user adds.
//!
//! This module records each location's native path. The webview receives
//! only an identifier and a display path, so no string that the webview
//! returns can name a file.

use std::path::PathBuf;

use serde::Serialize;
use suds_core::model::RootKind;
use suds_core::platform::{self, RootSpec};
use suds_core::roots;

/// An identifier that the webview uses in place of a path.
pub type LocationId = u32;

pub struct Location {
    pub id: LocationId,
    pub label: String,
    pub spec: RootSpec,
}

/// The webview's view of one location.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LocationView {
    pub id: LocationId,
    pub label: String,
    pub kind: RootKind,
    pub display_path: String,
    /// Whether the user added the location, as opposed to a default root.
    pub explicit: bool,
}

/// The location list that the webview requests when the window opens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LocationList {
    pub locations: Vec<LocationView>,
    /// The reason that the platform offers no default location.
    pub profile_error: Option<String>,
}

pub struct Locations {
    entries: Vec<Location>,
    profile_error: Option<String>,
    added: usize,
}

impl Locations {
    /// Starts from the platform's default roots.
    pub fn from_profile() -> Self {
        let (specs, profile_error) = match platform::default_roots() {
            Ok(specs) => (specs, None),
            Err(err) => (Vec::new(), Some(err.to_string())),
        };
        let mut locations = Locations {
            entries: Vec::new(),
            profile_error,
            added: 0,
        };
        for spec in specs {
            let label = default_label(&spec.id).to_owned();
            locations.push(label, spec);
        }
        locations
    }

    pub fn list(&self) -> LocationList {
        LocationList {
            locations: self.entries.iter().map(view).collect(),
            profile_error: self.profile_error.clone(),
        }
    }

    /// Adds a folder that the user chose, or returns the existing location
    /// for the same path.
    pub fn add(&mut self, path: PathBuf) -> LocationView {
        let path = roots::normalize(&path);
        if let Some(existing) = self.entries.iter().find(|entry| entry.spec.path == path) {
            return view(existing);
        }
        self.added += 1;
        let mut spec = roots::explicit_roots(&[path], &PathBuf::new()).remove(0);
        spec.id = format!("explicit.{}", self.added);
        let label = spec.path.file_name().map_or_else(
            || spec.path.to_string_lossy().into_owned(),
            |name| name.to_string_lossy().into_owned(),
        );
        view(self.push(label, spec))
    }

    /// The specifications for the given identifiers, in the given order, or
    /// the first identifier that names no location.
    pub fn specs(&self, ids: &[LocationId]) -> Result<Vec<(LocationId, RootSpec)>, LocationId> {
        ids.iter()
            .map(|&id| {
                self.entries
                    .iter()
                    .find(|entry| entry.id == id)
                    .map(|entry| (id, entry.spec.clone()))
                    .ok_or(id)
            })
            .collect()
    }

    fn push(&mut self, label: String, spec: RootSpec) -> &Location {
        let id = LocationId::try_from(self.entries.len()).unwrap_or(LocationId::MAX);
        self.entries.push(Location { id, label, spec });
        &self.entries[self.entries.len() - 1]
    }
}

fn view(location: &Location) -> LocationView {
    LocationView {
        id: location.id,
        label: location.label.clone(),
        kind: location.spec.kind,
        display_path: location.spec.path.to_string_lossy().into_owned(),
        explicit: location.spec.anchor.is_none(),
    }
}

fn default_label(id: &str) -> &str {
    match id {
        "macos.user-caches" => "Caches",
        "macos.user-logs" => "Logs",
        "macos.xcode-derived-data" => "Xcode Derived Data",
        "macos.downloads" => "Downloads",
        "macos.trash" => "Trash",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::Locations;

    fn empty() -> Locations {
        Locations {
            entries: Vec::new(),
            profile_error: None,
            added: 0,
        }
    }

    #[test]
    fn an_added_folder_receives_an_identifier_and_no_duplicate() {
        let mut locations = empty();
        let first = locations.add(PathBuf::from("/tmp/one/../work"));
        assert_eq!(first.display_path, "/tmp/work");
        assert_eq!(first.label, "work");
        assert!(first.explicit);
        let again = locations.add(PathBuf::from("/tmp/work"));
        assert_eq!(again.id, first.id);
        let second = locations.add(PathBuf::from("/var"));
        assert_ne!(second.id, first.id);
        assert_eq!(locations.list().locations.len(), 2);
    }

    #[test]
    fn specs_keep_the_native_path_and_reject_an_unknown_identifier() {
        let mut locations = empty();
        let added = locations.add(PathBuf::from("/tmp/work"));
        let specs = locations.specs(&[added.id]).unwrap();
        assert_eq!(specs[0].1.path, Path::new("/tmp/work"));
        assert_eq!(specs[0].1.id, "explicit.1");
        assert_eq!(locations.specs(&[added.id, 99]), Err(99));
    }
}

//! The directories and the settings that Suds keeps between runs.
//!
//! Suds keeps its journal, its registry, and its last pressure reading in a
//! state directory, and it reads its settings from a configuration
//! directory. A person or an installer writes the settings, and Suds only
//! reads them.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::development::Environment;

/// The directories of one user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Directories {
    pub state: PathBuf,
    pub config: PathBuf,
}

impl Directories {
    /// The directories for `environment`, or none without a home directory.
    /// Linux follows the XDG variables, and macOS keeps both beneath the
    /// user's Application Support folder.
    pub fn of(environment: &Environment) -> Option<Self> {
        let home = environment.home.as_ref()?;
        if cfg!(target_os = "macos") {
            let support = home.join("Library/Application Support/Suds");
            return Some(Directories {
                state: support.clone(),
                config: support,
            });
        }
        let variable = |name: &str, default: &str| {
            environment
                .variables
                .get(name)
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .unwrap_or_else(|| home.join(default))
                .join("suds")
        };
        Some(Directories {
            state: variable("XDG_STATE_HOME", ".local/state"),
            config: variable("XDG_CONFIG_HOME", ".config"),
        })
    }

    pub fn journal(&self) -> PathBuf {
        self.state.join("journal")
    }

    pub fn registry(&self) -> PathBuf {
        self.state.join("registry.jsonl")
    }

    pub fn pressure(&self) -> PathBuf {
        self.state.join("pressure.json")
    }

    pub fn settings(&self) -> PathBuf {
        self.config.join("settings.json")
    }
}

/// What the owner allows Suds to do without a person.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// Each directory beneath which Suds looks for checkouts.
    pub roots: Vec<PathBuf>,
    /// The most that development storage may occupy, in bytes. Without a
    /// value, the ceiling is the greater of half of the disk and 100 GiB.
    pub ceiling_bytes: Option<u64>,
    /// The time for which a directory must have stood unchanged.
    pub idle_minutes: u64,
    /// The owner's standing approval to remove derived state without a
    /// person (decision 0002). Without it, a scheduled run only measures.
    pub standing_approval: bool,
    pub tolerate_unreadable_processes: bool,
    /// A budget in bytes for each store that departs from its default.
    pub budgets: HashMap<String, u64>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            roots: Vec::new(),
            ceiling_bytes: None,
            idle_minutes: 60,
            standing_approval: false,
            tolerate_unreadable_processes: false,
            budgets: HashMap::new(),
        }
    }
}

/// Reads the settings at `path`. A missing file yields the defaults, and a
/// file that Suds cannot read or parse is an error, so a damaged file never
/// passes for the owner's approval or its absence.
pub fn load_settings(path: &Path) -> Result<Settings, String> {
    let text = match fs::read(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Settings::default()),
        Err(err) => return Err(format!("could not read {}: {err}", path.display())),
    };
    serde_json::from_slice(&text).map_err(|err| {
        format!(
            "{} holds no settings that Suds reads: {err}",
            path.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::path::PathBuf;

    use tempfile::TempDir;

    use super::{Directories, Settings, load_settings};
    use crate::development::Environment;

    #[test]
    fn a_missing_file_yields_the_defaults_and_a_damaged_file_fails() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("settings.json");
        assert_eq!(load_settings(&path).unwrap(), Settings::default());
        assert!(!Settings::default().standing_approval);

        fs::write(&path, br#"{"roots": ["/work"], "standing_approval": true}"#).unwrap();
        let settings = load_settings(&path).unwrap();
        assert_eq!(settings.roots, [PathBuf::from("/work")]);
        assert!(settings.standing_approval);
        assert_eq!(settings.idle_minutes, 60);

        fs::write(&path, br#"{"standing_aproval": true}"#).unwrap();
        assert!(load_settings(&path).is_err());
        fs::write(&path, b"{").unwrap();
        assert!(load_settings(&path).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn follows_the_xdg_variables_on_linux() {
        let mut environment = Environment {
            home: Some(PathBuf::from("/home/a")),
            variables: HashMap::new(),
        };
        let directories = Directories::of(&environment).unwrap();
        assert_eq!(
            directories.journal(),
            PathBuf::from("/home/a/.local/state/suds/journal")
        );
        assert_eq!(
            directories.settings(),
            PathBuf::from("/home/a/.config/suds/settings.json")
        );
        environment
            .variables
            .insert("XDG_STATE_HOME".to_owned(), "/state".into());
        assert_eq!(
            Directories::of(&environment).unwrap().state,
            PathBuf::from("/state/suds")
        );
        environment.home = None;
        assert_eq!(Directories::of(&environment), None);
    }
}

//! The directories and the settings that Suds keeps between runs.
//!
//! Suds keeps its journal, its registry, its last pressure reading, and the
//! desktop application's last reading in a state directory. It reads its
//! settings from a configuration
//! directory. A person or an installer writes the settings, and Suds only
//! reads them.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::development::Environment;
use crate::organize::plan::new_id;

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

    /// Durable pause state preserved across schedule reinstalls, logins, and
    /// reboots. Reading or writing pause never requires the mutation lock.
    pub fn pause(&self) -> PathBuf {
        self.state.join("pause.json")
    }

    /// Bounded-progress cursor persisting completed checkouts across partial
    /// maintenance slices so successive runs advance instead of repeating the
    /// same prefix.
    pub fn maintain_cursor(&self) -> PathBuf {
        self.state.join("maintain-cursor.json")
    }

    /// The last reading that the desktop application showed.
    pub fn reading(&self) -> PathBuf {
        self.state.join("reading.json")
    }

    pub fn settings(&self) -> PathBuf {
        self.config.join("settings.json")
    }
}

/// Writes `text` at `path`, a file of Suds' own state. Each writer stages its
/// text in a file of its own and renames that file into place, so two writers
/// never share a staged file and a reader never sees a part of a text. The
/// staged file is new, so the write follows no link.
pub fn write_staged(path: &Path, text: &[u8]) -> io::Result<()> {
    if let Some(directory) = path.parent() {
        fs::create_dir_all(directory)?;
    }
    let staged = path.with_extension(format!("{}.new", new_id("json")));
    let written = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staged)
        .and_then(|mut file| file.write_all(text).and_then(|()| file.sync_data()))
        .and_then(|()| fs::rename(&staged, path));
    if written.is_err() {
        // A failed writer leaves no staged file behind.
        let _ = fs::remove_file(&staged);
    }
    written
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

/// Schema tag of the durable maintenance pause record.
pub const PAUSE_SCHEMA: &str = "suds.pause/v1";

/// Operational pause state persisted outside the generated scheduler unit so
/// installing, reloading, or restarting a schedule never silently resumes
/// unattended maintenance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PauseState {
    pub schema: String,
    pub paused: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub updated_at: String,
}

/// Reads the pause record at `path`. Missing means not paused (`Ok(None)`).
/// Unreadable or malformed files return `Err` so automatic entries fail closed.
pub fn read_pause(path: &Path) -> Result<Option<PauseState>, String> {
    let text = match fs::read(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(format!("could not read {}: {err}", path.display())),
    };
    let parsed: PauseState = serde_json::from_slice(&text).map_err(|err| {
        format!(
            "{} holds no pause record that Suds reads: {err}",
            path.display()
        )
    })?;
    Ok(Some(parsed))
}

/// Persists `state` atomically at `path` without taking any mutation lock.
pub fn write_pause(path: &Path, state: &PauseState) -> Result<(), String> {
    let failed = |err: io::Error| format!("could not write {}: {err}", path.display());
    let text = serde_json::to_vec_pretty(state).map_err(|err| failed(err.into()))?;
    write_staged(path, &text).map_err(failed)
}

/// Clears any pause record at `path`.
pub fn clear_pause(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(format!("could not remove {}: {err}", path.display())),
    }
}

/// Schema tag of the bounded maintenance progress cursor.
pub const MAINTAIN_CURSOR_SCHEMA: &str = "suds.maintain-cursor/v1";

/// Persisted cursor recording completed and pending checkouts across partial
/// maintenance slices so subsequent bounded runs reach nested worktrees.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaintainCursor {
    pub schema: String,
    pub roots: Vec<PathBuf>,
    pub completed_checkouts: Vec<PathBuf>,
    #[serde(default)]
    pub pending_checkouts: Vec<PathBuf>,
    #[serde(default)]
    pub completed_stores: Vec<String>,
    #[serde(default)]
    pub accumulated_usage_bytes: u64,
    #[serde(default)]
    pub workspaces_counted: bool,
    #[serde(default)]
    pub had_errors: bool,
    pub updated_at: String,
}

/// Reads the maintenance progress cursor at `path`, returning `None` if absent
/// or unreadable so a damaged cursor resets to a fresh pass.
pub fn read_maintain_cursor(path: &Path) -> Option<MaintainCursor> {
    let text = fs::read(path).ok()?;
    let cursor: MaintainCursor = serde_json::from_slice(&text).ok()?;
    (cursor.schema == MAINTAIN_CURSOR_SCHEMA).then_some(cursor)
}

/// Atomically writes `cursor` to `path`.
pub fn write_maintain_cursor(path: &Path, cursor: &MaintainCursor) -> Result<(), String> {
    let failed = |err: io::Error| format!("could not write {}: {err}", path.display());
    let text = serde_json::to_vec_pretty(cursor).map_err(|err| failed(err.into()))?;
    write_staged(path, &text).map_err(failed)
}

/// Removes any maintenance progress cursor at `path`.
pub fn clear_maintain_cursor(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(format!("could not remove {}: {err}", path.display())),
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
    use std::fs;
    use std::path::PathBuf;

    use tempfile::TempDir;

    use super::{Settings, load_settings};

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
        use std::collections::HashMap;

        use super::Directories;
        use crate::development::Environment;

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

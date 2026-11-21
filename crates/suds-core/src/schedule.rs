//! The units that make a scheduler run `suds maintain`, and their standing on
//! this workstation.
//!
//! A unit is text for systemd or for launchd. This module writes that text,
//! places it, and tells whether the placed text is the text that this
//! release writes. A unit that an earlier release wrote keeps its faults
//! until a person installs it again, so each interface states a stale unit.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::development::Environment;

/// The exit status of a maintenance run that kept or refused a directory.
pub const PARTIAL_STATUS: u8 = 1;

const SERVICE: &str = "suds-maintain.service";
const TIMER: &str = "suds-maintain.timer";
const LABEL: &str = "sh.pithecene.suds.maintain";

/// The program that starts a unit at its time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheduler {
    Systemd,
    Launchd,
}

impl Scheduler {
    /// The scheduler of the system that runs this process.
    pub fn native() -> Self {
        if cfg!(target_os = "macos") {
            Scheduler::Launchd
        } else {
            Scheduler::Systemd
        }
    }
}

/// What a unit states.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unit {
    /// The Suds program that the unit runs.
    pub program: PathBuf,
    /// The search path of a run. A scheduler's own path names no program
    /// that a version manager installed.
    pub path: Option<String>,
    /// The minutes between two runs.
    pub minutes: u64,
}

/// One file of a unit, with the path that the scheduler reads it at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitFile {
    pub path: PathBuf,
    pub contents: String,
}

/// One command that makes the scheduler take the placed files up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub command: Vec<String>,
    /// Whether a failure of the command fails the installation.
    pub required: bool,
}

/// Whether the placed files are the files that this release writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Standing {
    /// No file of the unit exists, so no scheduler runs the maintenance.
    Absent,
    /// The files differ from this release's files. The value is the reason.
    Stale(String),
    Current,
}

/// The directory that holds the scheduler's units of this user.
fn directory(scheduler: Scheduler, environment: &Environment) -> Option<PathBuf> {
    let home = environment.home.as_ref()?;
    Some(match scheduler {
        Scheduler::Launchd => home.join("Library/LaunchAgents"),
        Scheduler::Systemd => {
            let named = environment.variables.get("XDG_CONFIG_HOME");
            let named = named.map(PathBuf::from).filter(|path| path.is_absolute());
            let config = named.unwrap_or_else(|| home.join(".config"));
            config.join("systemd/user")
        }
    })
}

/// Quotes `text` as one word of a systemd unit. The manager splits an
/// unquoted line at each space and expands `%`. It expands `$` in a command
/// line alone, so `command` states whether the word stands in one.
fn systemd_quote(text: &str, command: bool) -> String {
    let mut quoted = String::from("\"");
    for character in text.chars() {
        match character {
            '"' | '\\' => {
                quoted.push('\\');
                quoted.push(character);
            }
            '%' => quoted.push_str("%%"),
            '$' if command => quoted.push_str("$$"),
            other => quoted.push(other),
        }
    }
    quoted.push('"');
    quoted
}

/// The word that `systemd_quote` wrote at the start of `text`.
fn systemd_unquote(text: &str, command: bool) -> Option<String> {
    let mut characters = text.strip_prefix('"')?.chars();
    let mut word = String::new();
    loop {
        match characters.next()? {
            '"' => return Some(word),
            '\\' => word.push(characters.next()?),
            doubled @ ('%' | '$') if doubled == '%' || command => {
                characters.next().filter(|next| *next == doubled)?;
                word.push(doubled);
            }
            other => word.push(other),
        }
    }
}

const ESCAPES: [(&str, &str); 3] = [("&", "&amp;"), ("<", "&lt;"), (">", "&gt;")];

fn xml_escape(text: &str) -> String {
    let escape = |text: String, (plain, escaped): &(&str, &str)| text.replace(plain, escaped);
    ESCAPES.iter().fold(text.to_owned(), escape)
}

fn xml_unescape(text: &str) -> String {
    let plain = |text: String, (plain, escaped): &(&str, &str)| text.replace(escaped, plain);
    // The ampersand turns back last, so an escaped ampersand forms no other escape.
    ESCAPES.iter().rev().fold(text.to_owned(), plain)
}

fn systemd_files(unit: &Unit, directory: &Path) -> Vec<UnitFile> {
    // The manager expands `%` in a variable's value and leaves `$` alone there.
    let path = unit.path.as_ref().map_or_else(String::new, |path| {
        let value = systemd_quote(&format!("PATH={path}"), false);
        format!("Environment={value}\n")
    });
    let service = format!(
        "[Unit]\n\
         Description=Suds measures and reclaims development storage\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         {path}\
         ExecStart={program} maintain\n\
         # A run that kept or refused a directory ends with status {PARTIAL_STATUS}.\n\
         SuccessExitStatus={PARTIAL_STATUS}\n",
        program = systemd_quote(&unit.program.to_string_lossy(), true),
    );
    let timer = format!(
        "[Unit]\n\
         Description=Suds maintenance every {minutes} minutes\n\
         \n\
         [Timer]\n\
         OnBootSec=5min\n\
         OnUnitActiveSec={minutes}min\n\
         \n\
         [Install]\n\
         WantedBy=timers.target\n",
        minutes = unit.minutes
    );
    let file = |name: &str, contents| UnitFile {
        path: directory.join(name),
        contents,
    };
    vec![file(SERVICE, service), file(TIMER, timer)]
}

fn launchd_files(unit: &Unit, directory: &Path) -> Vec<UnitFile> {
    let program = xml_escape(&unit.program.to_string_lossy());
    let path = unit.path.as_ref().map_or_else(String::new, |path| {
        format!(
            "\t<key>EnvironmentVariables</key>\n\
             \t<dict>\n\
             \t\t<key>PATH</key>\n\
             \t\t<string>{}</string>\n\
             \t</dict>\n",
            xml_escape(path)
        )
    });
    let contents = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \t<key>Label</key>\n\
         \t<string>{LABEL}</string>\n\
         \t<key>ProgramArguments</key>\n\
         \t<array>\n\
         \t\t<string>{program}</string>\n\
         \t\t<string>maintain</string>\n\
         \t</array>\n\
         {path}\
         \t<key>StartInterval</key>\n\
         \t<integer>{seconds}</integer>\n\
         \t<key>RunAtLoad</key>\n\
         \t<true/>\n\
         </dict>\n\
         </plist>\n",
        seconds = unit.minutes.saturating_mul(60)
    );
    vec![UnitFile {
        path: directory.join(format!("{LABEL}.plist")),
        contents,
    }]
}

/// The files of `unit` for `scheduler`, or none without a home directory.
pub fn files(
    scheduler: Scheduler,
    unit: &Unit,
    environment: &Environment,
) -> Option<Vec<UnitFile>> {
    let directory = directory(scheduler, environment)?;
    Some(match scheduler {
        Scheduler::Systemd => systemd_files(unit, &directory),
        Scheduler::Launchd => launchd_files(unit, &directory),
    })
}

/// The text between `open` and `close` on the first line of `text` that
/// starts with `open`, after the line's indentation.
fn between<'a>(text: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let lines = text.lines().map(str::trim_start);
    let rest = lines.filter_map(|line| line.strip_prefix(open)).next()?;
    rest.strip_suffix(close)
}

/// The unit that the placed files state, or none for files of another form.
fn stated(scheduler: Scheduler, placed: &[String]) -> Option<Unit> {
    match scheduler {
        Scheduler::Systemd => {
            let [service, timer] = placed else {
                return None;
            };
            let program = between(service, "ExecStart=", " maintain")?;
            let path = between(service, "Environment=", "").map(|value| {
                let word = systemd_unquote(value, false)?;
                Some(word.strip_prefix("PATH=")?.to_owned())
            });
            Some(Unit {
                program: systemd_unquote(program, true)?.into(),
                path: path.unwrap_or(None),
                minutes: between(timer, "OnUnitActiveSec=", "min")?.parse().ok()?,
            })
        }
        Scheduler::Launchd => {
            let [agent] = placed else {
                return None;
            };
            let (_, arguments) = agent.split_once("<key>ProgramArguments</key>")?;
            let variables = agent.split_once("<key>PATH</key>");
            let path = variables.and_then(|(_, rest)| between(rest, "<string>", "</string>"));
            let seconds: u64 = between(agent, "<integer>", "</integer>")?.parse().ok()?;
            Some(Unit {
                program: xml_unescape(between(arguments, "<string>", "</string>")?).into(),
                path: path.map(xml_unescape),
                minutes: seconds / 60,
            })
        }
    }
}

/// The standing of the placed files. A person chooses the minutes and the
/// search path, so the standing holds both as the files state them. With a
/// `program`, the files must name it. Without one, the named program must
/// exist.
pub fn standing(
    scheduler: Scheduler,
    environment: &Environment,
    program: Option<&Path>,
) -> Standing {
    let stale = |reason: &str| Standing::Stale(reason.to_owned());
    let Some(directory) = directory(scheduler, environment) else {
        return Standing::Absent;
    };
    // The names of the files depend on no value of the unit.
    let blank = Unit {
        program: PathBuf::new(),
        path: None,
        minutes: 0,
    };
    let Some(expected) = files(scheduler, &blank, environment) else {
        return Standing::Absent;
    };
    let placed: Vec<Option<String>> = expected
        .iter()
        .map(|file| fs::read_to_string(&file.path).ok())
        .collect();
    if placed.iter().all(Option::is_none) {
        return Standing::Absent;
    }
    let Some(placed) = placed.into_iter().collect::<Option<Vec<String>>>() else {
        return stale("a file of the unit is missing");
    };
    let Some(unit) = stated(scheduler, &placed) else {
        return stale("this release cannot read the unit");
    };
    if unit.path.is_none() {
        return stale("the unit states no search path, so a run may not find a store's program");
    }
    match program {
        Some(program) if program != unit.program => {
            return stale("the unit runs another Suds program");
        }
        None if !unit.program.exists() => {
            return stale("the unit runs a program that no longer exists");
        }
        _ => {}
    }
    let fresh = match scheduler {
        Scheduler::Systemd => systemd_files(&unit, &directory),
        Scheduler::Launchd => launchd_files(&unit, &directory),
    };
    if fresh.iter().map(|file| &file.contents).ne(placed.iter()) {
        return stale("an earlier release wrote the unit");
    }
    Standing::Current
}

/// Places each file. A writer stages the text beside the file and renames
/// it into place, so the scheduler never reads a part of a unit.
pub fn place(files: &[UnitFile]) -> Result<(), String> {
    for file in files {
        let failed =
            |err: std::io::Error| format!("could not write {}: {err}", file.path.display());
        if let Some(directory) = file.path.parent() {
            fs::create_dir_all(directory).map_err(failed)?;
        }
        let mut staged = file.path.clone().into_os_string();
        staged.push(format!(".{}.new", std::process::id()));
        let staged = PathBuf::from(staged);
        let mut options = OpenOptions::new();
        let opened = options
            .write(true)
            .create(true)
            .truncate(true)
            .open(&staged);
        let written = opened
            .and_then(|mut out| {
                out.write_all(file.contents.as_bytes())
                    .and_then(|()| out.sync_data())
            })
            .and_then(|()| fs::rename(&staged, &file.path));
        if written.is_err() {
            // A failed writer leaves no staged file behind.
            let _ = fs::remove_file(&staged);
        }
        written.map_err(failed)?;
    }
    Ok(())
}

/// The commands that make `scheduler` take up `files`, in their order.
pub fn activation(scheduler: Scheduler, files: &[UnitFile]) -> Vec<Step> {
    let step = |required: bool, words: &[&str]| Step {
        command: words.iter().map(|word| (*word).to_owned()).collect(),
        required,
    };
    match scheduler {
        Scheduler::Systemd => vec![
            step(true, &["systemctl", "--user", "daemon-reload"]),
            step(true, &["systemctl", "--user", "enable", "--now", TIMER]),
        ],
        Scheduler::Launchd => {
            let domain = format!("gui/{}", rustix::process::getuid().as_raw());
            let agent = files.first().map(|file| file.path.to_string_lossy());
            let agent = agent.unwrap_or_default();
            vec![
                // No agent runs before the first installation, so this command may fail.
                step(
                    false,
                    &["launchctl", "bootout", &format!("{domain}/{LABEL}")],
                ),
                step(true, &["launchctl", "bootstrap", &domain, &agent]),
            ]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{systemd_quote, systemd_unquote, xml_escape, xml_unescape};

    #[test]
    fn a_quoted_word_reads_back_as_it_stood() {
        for word in ["/opt/suds", "/opt/100% \"q\"/$x\\y/suds", "PATH=/a b:/c%d"] {
            for command in [true, false] {
                let quoted = systemd_quote(word, command);
                assert_eq!(systemd_unquote(&quoted, command).as_deref(), Some(word));
                let line = format!("{quoted} maintain");
                assert_eq!(systemd_unquote(&line, command).as_deref(), Some(word));
            }
            assert_eq!(xml_unescape(&xml_escape(word)), word);
        }
        assert_eq!(systemd_quote("100% $x", true), "\"100%% $$x\"");
        assert_eq!(systemd_quote("100% $x", false), "\"100%% $x\"");
        assert_eq!(
            xml_unescape(&xml_escape("a &lt; b & <c>")),
            "a &lt; b & <c>"
        );
        assert_eq!(systemd_unquote("/bare", true), None);
        assert_eq!(systemd_unquote("\"open", true), None);
    }
}

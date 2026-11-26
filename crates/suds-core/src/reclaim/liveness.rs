//! The working directories of the machine's processes.
//!
//! A process that works in a checkout may read or write its build state, so
//! Suds removes nothing beneath such a checkout.

use std::path::{Path, PathBuf};

use crate::helper::HelperRunner;

/// The working directory of each process that the observation could read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Liveness {
    directories: Vec<PathBuf>,
    /// Each process of this user whose working directory the system would
    /// not show. Such a process can still write in this user's checkouts.
    unreadable: Vec<String>,
}

impl Liveness {
    /// A fixed set of working directories.
    pub fn of(directories: Vec<PathBuf>) -> Self {
        Liveness {
            directories,
            unreadable: Vec::new(),
        }
    }

    /// The observation with these processes of unknown working directory.
    #[must_use]
    pub fn with_unreadable(mut self, processes: Vec<String>) -> Self {
        self.unreadable = processes;
        self
    }

    /// Each process of this user whose working directory stayed unknown, by
    /// its identifier and its name.
    pub fn unreadable(&self) -> &[String] {
        &self.unreadable
    }

    /// The working directory of a process that lies at `tree` or beneath it.
    pub fn within(&self, tree: &Path) -> Option<&Path> {
        self.directories
            .iter()
            .map(PathBuf::as_path)
            .find(|directory| directory.starts_with(tree))
    }

    /// Reads each process's working directory from `/proc`, and leaves out
    /// another user's process, whose link the kernel refuses. The kernel
    /// also refuses the link of this user's process that forbids its own
    /// inspection, and the observation names each one as unreadable.
    #[cfg(target_os = "linux")]
    pub fn observe() -> Result<Self, String> {
        use std::os::unix::fs::MetadataExt;

        let this_user = std::fs::metadata("/proc/self")
            .map_err(|err| format!("could not read /proc/self: {err}"))?
            .uid();
        let entries = std::fs::read_dir("/proc")
            .map_err(|err| format!("could not list the processes in /proc: {err}"))?;
        let mut liveness = Liveness::default();
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name();
            let Some(process) = name
                .to_str()
                .filter(|name| !name.is_empty() && name.bytes().all(|byte| byte.is_ascii_digit()))
            else {
                continue;
            };
            let path = entry.path();
            match std::fs::read_link(path.join("cwd")) {
                Ok(directory) => liveness.directories.push(directory),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => {
                    let owner = std::fs::metadata(&path).map(|meta| meta.uid());
                    if owner.is_ok_and(|owner| owner == this_user) {
                        let command = std::fs::read_to_string(path.join("comm"))
                            .map_or_else(|_| "unknown".to_owned(), |text| text.trim().to_owned());
                        liveness.unreadable.push(format!("{process} ({command})"));
                    }
                }
            }
        }
        Ok(liveness)
    }

    #[cfg(target_os = "linux")]
    pub fn observe_with_runner(_runner: &HelperRunner) -> Result<Self, String> {
        Self::observe()
    }

    /// Asks `lsof`, which reads the system's process table, for each
    /// process's working directory within default helper bounds.
    #[cfg(target_os = "macos")]
    pub fn observe() -> Result<Self, String> {
        let runner = HelperRunner::new(crate::helper::HelperPolicy::default());
        Self::observe_with_runner(&runner)
    }

    /// Asks `lsof` through `runner` for each process's working directory.
    /// Same-user processes whose working directory `lsof` could not read
    /// stay recorded as unreadable.
    #[cfg(target_os = "macos")]
    pub fn observe_with_runner(runner: &HelperRunner) -> Result<Self, String> {
        use std::ffi::OsString;

        let this_user = rustix::process::getuid().as_raw();
        let output = runner
            .run(crate::helper::HelperSpec {
                operation: "lsof".to_owned(),
                program: PathBuf::from("/usr/sbin/lsof"),
                args: ["-a", "-d", "cwd", "-F", "pcun"]
                    .into_iter()
                    .map(OsString::from)
                    .collect(),
                current_dir: None,
                env_set: Vec::new(),
                env_remove: Vec::new(),
                stdin: None,
            })
            .map_err(|err| format!("could not run /usr/sbin/lsof: {err}"))?;
        // The program exits with a failure when it cannot read one process,
        // and it still lists the others.
        if output.stdout.is_empty() {
            return Err(format!(
                "/usr/sbin/lsof listed no working directory: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(parse_lsof_with_uid(&output.stdout, this_user))
    }
}

/// Parses `lsof -F pcun` (or `-F n`) output, keeping readable working
/// directories and recording unreadable same-user processes with their PID
/// and command name.
#[cfg(any(target_os = "macos", test))]
fn parse_lsof_with_uid(output: &[u8], this_user: u32) -> Liveness {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let mut liveness = Liveness::default();
    let mut pid: Option<String> = None;
    let mut command: Option<String> = None;
    let mut uid: Option<u32> = None;
    for line in output.split(|byte| *byte == b'\n') {
        if let Some(rest) = line.strip_prefix(b"p") {
            pid = Some(String::from_utf8_lossy(rest).into_owned());
            command = None;
            uid = None;
        } else if let Some(rest) = line.strip_prefix(b"c") {
            command = Some(String::from_utf8_lossy(rest).into_owned());
        } else if let Some(rest) = line.strip_prefix(b"u") {
            uid = std::str::from_utf8(rest).ok().and_then(|s| s.parse().ok());
        } else if let Some(name) = line.strip_prefix(b"n") {
            if name.first() == Some(&b'/') {
                liveness
                    .directories
                    .push(PathBuf::from(OsStr::from_bytes(name)));
            } else if !name.is_empty() && uid == Some(this_user) {
                let proc_id = pid.as_deref().unwrap_or("unknown");
                let proc_cmd = command.as_deref().unwrap_or("unknown");
                liveness.unreadable.push(format!("{proc_id} ({proc_cmd})"));
            }
        }
    }
    liveness
}

/// The paths of the name fields in `lsof -F n` output, one field on a line.
#[cfg(test)]
fn parse_lsof(output: &[u8]) -> Vec<PathBuf> {
    parse_lsof_with_uid(output, 0).directories
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{Liveness, parse_lsof, parse_lsof_with_uid};

    #[test]
    fn finds_a_process_at_a_tree_or_beneath_it() {
        let liveness = Liveness::of(vec![PathBuf::from("/work/repo/src")]);
        assert_eq!(
            liveness.within(Path::new("/work/repo")),
            Some(Path::new("/work/repo/src"))
        );
        assert_eq!(liveness.within(Path::new("/work/repository")), None);
        assert_eq!(liveness.within(Path::new("/work/repo/src/deep")), None);
    }

    #[test]
    fn reads_the_name_fields_of_lsof() {
        let output = b"p101\nfcwd\nn/Users/a/work\np102\nfcwd\nn/\nnpipe\n";
        assert_eq!(
            parse_lsof(output),
            [PathBuf::from("/Users/a/work"), PathBuf::from("/")]
        );
    }

    #[test]
    fn preserves_same_user_unreadable_lsof_entries_and_ignores_other_users() {
        let output = concat!(
            "p101\ncrustc\nu501\nfcwd\nn/Users/a/work\n",
            "p102\ncagent\nu501\nfcwd\nn(readlink: Permission denied)\n",
            "p103\ncrootd\nu0\nfcwd\nn(readlink: Permission denied)\n",
        );
        let liveness = parse_lsof_with_uid(output.as_bytes(), 501);
        assert_eq!(liveness.directories, [PathBuf::from("/Users/a/work")]);
        assert_eq!(liveness.unreadable(), ["102 (agent)"]);
    }

    #[test]
    fn observes_this_process() {
        let here = std::env::current_dir().unwrap();
        let liveness = Liveness::observe().unwrap();
        assert!(liveness.within(&here).is_some());
    }
}

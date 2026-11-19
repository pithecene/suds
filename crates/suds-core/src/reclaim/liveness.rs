//! The working directories of the machine's processes.
//!
//! A process that works in a checkout may read or write its build state, so
//! Suds removes nothing beneath such a checkout.

use std::path::{Path, PathBuf};

/// The working directory of each process that the observation could read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Liveness {
    directories: Vec<PathBuf>,
}

impl Liveness {
    /// A fixed set of working directories.
    pub fn of(directories: Vec<PathBuf>) -> Self {
        Liveness { directories }
    }

    /// The working directory of a process that lies at `tree` or beneath it.
    pub fn within(&self, tree: &Path) -> Option<&Path> {
        self.directories
            .iter()
            .map(PathBuf::as_path)
            .find(|directory| directory.starts_with(tree))
    }

    /// Reads each process's working directory from `/proc`. The kernel
    /// refuses the link of another user's process, and that process cannot
    /// write in this user's checkout without its permissions, so the
    /// observation leaves it out.
    #[cfg(target_os = "linux")]
    pub fn observe() -> Result<Self, String> {
        let entries = std::fs::read_dir("/proc")
            .map_err(|err| format!("could not list the processes in /proc: {err}"))?;
        let directories = entries
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.bytes().all(|byte| byte.is_ascii_digit()))
            })
            .filter_map(|entry| std::fs::read_link(entry.path().join("cwd")).ok())
            .collect();
        Ok(Liveness { directories })
    }

    /// Asks `lsof`, which reads the system's process table, for each
    /// process's working directory. macOS offers no file with that list.
    #[cfg(target_os = "macos")]
    pub fn observe() -> Result<Self, String> {
        let output = std::process::Command::new("/usr/sbin/lsof")
            .args(["-a", "-d", "cwd", "-F", "n"])
            .output()
            .map_err(|err| format!("could not run /usr/sbin/lsof: {err}"))?;
        // The program exits with a failure when it cannot read one process,
        // and it still lists the others.
        if output.stdout.is_empty() {
            return Err(format!(
                "/usr/sbin/lsof listed no working directory: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(Liveness {
            directories: parse_lsof(&output.stdout),
        })
    }
}

/// The paths of the name fields in `lsof -F n` output, one field on a line.
#[cfg(any(target_os = "macos", test))]
fn parse_lsof(output: &[u8]) -> Vec<PathBuf> {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    output
        .split(|byte| *byte == b'\n')
        .filter_map(|line| line.strip_prefix(b"n"))
        .filter(|path| path.first() == Some(&b'/'))
        .map(|path| PathBuf::from(OsStr::from_bytes(path)))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{Liveness, parse_lsof};

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
    fn observes_this_process() {
        let here = std::env::current_dir().unwrap();
        let liveness = Liveness::observe().unwrap();
        assert!(liveness.within(&here).is_some());
    }
}

//! The typed adapter for Git, which owns each checkout's state.
//!
//! Suds asks Git whether a checkout holds work that no remote holds, and it
//! has Git remove a linked checkout. Each call passes its arguments as a
//! list, and no call passes a shell string.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The Git program that Suds runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Git {
    program: PathBuf,
}

impl Default for Git {
    /// The `git` that the process's search path names.
    fn default() -> Self {
        Git {
            program: PathBuf::from("git"),
        }
    }
}

impl Git {
    /// Runs Git in `directory` and returns its standard output. A status
    /// query takes no optional lock, so a query writes nothing.
    fn run(
        &self,
        directory: &Path,
        arguments: &[&str],
        path: Option<&Path>,
    ) -> Result<String, String> {
        let mut command = Command::new(&self.program);
        command
            .arg("--no-optional-locks")
            .arg("-C")
            .arg(directory)
            .args(arguments);
        if let Some(path) = path {
            command.arg("--").arg(path);
        }
        // A variable of the caller's own repository must not redirect the
        // query to another repository.
        for name in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_COMMON_DIR",
        ] {
            command.env_remove(name);
        }
        let output = command
            .output()
            .map_err(|err| format!("could not run {}: {err}", self.program.display()))?;
        if !output.status.success() {
            return Err(format!(
                "git {} failed in {}: {}",
                arguments.join(" "),
                directory.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Whether the checkout holds no changed, staged, or untracked file.
    pub fn is_clean(&self, checkout: &Path) -> Result<bool, String> {
        let status = self.run(
            checkout,
            &["status", "--porcelain=v1", "--untracked-files=all"],
            None,
        )?;
        Ok(status.is_empty())
    }

    /// The number of commits of the checkout's head that no remote-tracking
    /// branch holds.
    pub fn unpushed(&self, checkout: &Path) -> Result<u64, String> {
        let count = self.run(
            checkout,
            &["rev-list", "--count", "HEAD", "--not", "--remotes"],
            None,
        )?;
        count.trim().parse().map_err(|err| {
            format!(
                "git printed no commit count for {}: {err}",
                checkout.display()
            )
        })
    }

    /// Whether Git ignores every file beneath `directory` of `checkout`.
    /// A tracked file there, or one that Git would track, is no derived
    /// state.
    pub fn ignores_all(&self, checkout: &Path, directory: &Path) -> Result<bool, String> {
        let listed = self.run(
            checkout,
            &["ls-files", "--cached", "--others", "--exclude-standard"],
            Some(directory),
        )?;
        Ok(listed.is_empty())
    }

    /// Has Git remove the linked checkout at `checkout` from `repository`.
    /// Git itself refuses a checkout that holds a changed or untracked file.
    pub fn remove_worktree(&self, repository: &Path, checkout: &Path) -> Result<(), String> {
        self.run(repository, &["worktree", "remove"], Some(checkout))
            .map(|_| ())
    }
}

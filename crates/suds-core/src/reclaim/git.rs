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
            .args(arguments)
            // Object traversal in a partial clone must stay local.
            .env("GIT_NO_LAZY_FETCH", "1")
            .env("GIT_ALLOW_PROTOCOL", "");
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

    /// The full name of the commit at the checkout's head.
    pub fn head(&self, checkout: &Path) -> Result<String, String> {
        self.run(checkout, &["rev-parse", "--verify", "HEAD"], None)
            .map(|name| name.trim().to_owned())
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

    /// The name of a local remote-tracking reference that contains `commit`.
    pub(crate) fn remote_tracking_ref(
        &self,
        checkout: &Path,
        commit: &str,
    ) -> Result<Option<String>, String> {
        let output = self.run(
            checkout,
            &[
                "--no-replace-objects",
                "for-each-ref",
                &format!("--contains={commit}"),
                "--count=1",
                "--format=%(refname)",
                "refs/remotes/",
            ],
            None,
        )?;
        Ok(output.lines().next().map(str::to_owned))
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

    /// Whether `directory` is the top of a repository of its own. A directory
    /// inside another checkout answers Git's queries for that checkout, so
    /// a judgment of a nested repository asks this first.
    pub fn is_repository_root(&self, directory: &Path) -> Result<bool, String> {
        let top = self.run(directory, &["rev-parse", "--show-toplevel"], None)?;
        let resolve = |path: &Path| {
            std::fs::canonicalize(path)
                .map_err(|err| format!("could not resolve {}: {err}", path.display()))
        };
        Ok(resolve(Path::new(top.trim()))? == resolve(directory)?)
    }

    /// Whether `directory` is a repository of its own that holds only what
    /// it fetched: a clean tree, no second worktree, and no commit of a
    /// head, branch, tag, or stash that a remote-tracking branch lacks. A
    /// local tag proves no fetch.
    pub fn holds_only_fetched(&self, directory: &Path) -> Result<bool, String> {
        if !self.is_repository_root(directory)? || !self.is_clean(directory)? {
            return Ok(false);
        }
        let worktrees = self.run(directory, &["worktree", "list", "--porcelain"], None)?;
        let count = worktrees
            .lines()
            .filter(|line| line.starts_with("worktree "))
            .count();
        if count > 1 {
            return Ok(false);
        }
        let local = self.run(
            directory,
            &["rev-list", "HEAD", "--all", "--not", "--remotes"],
            None,
        )?;
        if local.trim().is_empty() {
            return Ok(true);
        }
        // A shallow fetch records each commit at which it ended, and only a
        // remote supplies such a commit.
        let file = self.run(directory, &["rev-parse", "--git-path", "shallow"], None)?;
        let Ok(ends) = std::fs::read_to_string(directory.join(file.trim())) else {
            return Ok(false);
        };
        Ok(local
            .lines()
            .all(|commit| ends.lines().any(|end| end.trim() == commit.trim())))
    }

    /// Has Git remove the linked checkout at `checkout` from `repository`.
    /// Git itself refuses a checkout that holds a changed or untracked file.
    pub fn remove_worktree(&self, repository: &Path, checkout: &Path) -> Result<(), String> {
        self.run(repository, &["worktree", "remove"], Some(checkout))
            .map(|_| ())
    }
}

//! The typed adapter for Git, which owns each checkout's state.
//!
//! Suds asks Git whether a checkout holds work that no remote holds, and it
//! has Git remove a linked checkout. Each call passes its arguments as a
//! list, and no call passes a shell string.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::model::EntryKind;

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
        self.run_bytes(directory, arguments, path)
            .map(|output| String::from_utf8_lossy(&output).into_owned())
    }

    fn run_bytes(
        &self,
        directory: &Path,
        arguments: &[&str],
        path: Option<&Path>,
    ) -> Result<Vec<u8>, String> {
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
        Ok(output.stdout)
    }

    /// Each checkout of the repository of `checkout`, the main one first,
    /// as Git lists them.
    pub fn worktrees(&self, checkout: &Path) -> Result<Vec<PathBuf>, String> {
        let listed = self.run_bytes(checkout, &["worktree", "list", "--porcelain", "-z"], None)?;
        Ok(listed
            .split(|byte| *byte == 0)
            .filter_map(|field| field.strip_prefix(b"worktree "))
            .map(|path| PathBuf::from(OsStr::from_bytes(path)))
            .collect())
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

    pub(crate) fn is_clean_for_removal(&self, checkout: &Path) -> Result<bool, String> {
        let status = self.run(
            checkout,
            &[
                "-c",
                "core.fsmonitor=false",
                "--no-replace-objects",
                "status",
                "--porcelain=v1",
                "--untracked-files=all",
                "--ignore-submodules=none",
            ],
            None,
        )?;
        Ok(status.is_empty())
    }

    /// The entries that Git records in the landed tree and checks for changes.
    pub(crate) fn tracked_at(
        &self,
        checkout: &Path,
        commit: &str,
    ) -> Result<HashMap<PathBuf, EntryKind>, String> {
        let tree = self.run_bytes(
            checkout,
            &[
                "--no-replace-objects",
                "ls-tree",
                "-rtz",
                "--full-tree",
                commit,
            ],
            None,
        )?;
        let malformed = || {
            format!(
                "Git printed an invalid tree entry for {}",
                checkout.display()
            )
        };
        let mut tracked = HashMap::new();
        for record in tree
            .split(|byte| *byte == 0)
            .filter(|entry| !entry.is_empty())
        {
            let mut fields = record.splitn(2, |byte| *byte == b'\t');
            let mode = fields
                .next()
                .and_then(|header| header.split(|byte| *byte == b' ').next());
            let name = fields.next().ok_or_else(malformed)?;
            let kind = match mode {
                Some(b"100644" | b"100755") => EntryKind::File,
                Some(b"120000") => EntryKind::Symlink,
                Some(b"040000") => EntryKind::Directory,
                // Git records a submodule commit without recording its files.
                Some(b"160000") => continue,
                _ => return Err(malformed()),
            };
            tracked.insert(PathBuf::from(OsStr::from_bytes(name)), kind);
        }

        // Git's clean status skips paths marked assume-unchanged or skip-worktree.
        let indexed = self.run_bytes(checkout, &["ls-files", "-vz"], None)?;
        for record in indexed
            .split(|byte| *byte == 0)
            .filter(|entry| !entry.is_empty())
        {
            let [tag, b' ', name @ ..] = record else {
                return Err(format!(
                    "Git printed an invalid index entry for {}",
                    checkout.display()
                ));
            };
            if *tag == b'S' || tag.is_ascii_lowercase() {
                tracked.remove(Path::new(OsStr::from_bytes(name)));
            }
        }
        Ok(tracked)
    }

    /// The full name of the commit at the checkout's head.
    pub fn head(&self, checkout: &Path) -> Result<String, String> {
        self.run(checkout, &["rev-parse", "--verify", "HEAD"], None)
            .map(|name| name.trim().to_owned())
    }

    /// Worktree-private references (`refs/worktree`, `refs/bisect`,
    /// `refs/rewritten`) that disappear when this worktree is removed.
    fn private_refs(&self, checkout: &Path) -> Result<Vec<String>, String> {
        let output = self.run(
            checkout,
            &[
                "for-each-ref",
                "--format=%(refname)",
                "refs/worktree",
                "refs/bisect",
                "refs/rewritten",
            ],
            None,
        )?;
        Ok(output
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect())
    }

    /// The number of commits of the checkout's head and worktree-private
    /// references that no remote-tracking branch holds.
    pub fn unpushed(&self, checkout: &Path) -> Result<u64, String> {
        let private = self.private_refs(checkout)?;
        let mut args: Vec<&str> = vec!["rev-list", "--count", "HEAD"];
        args.extend(private.iter().map(String::as_str));
        args.extend(["--not", "--remotes"]);
        let count = self.run(checkout, &args, None)?;
        count.trim().parse().map_err(|err| {
            format!(
                "git printed no commit count for {}: {err}",
                checkout.display()
            )
        })
    }

    /// The number of commits held by worktree-private references that no
    /// remote-tracking branch holds.
    pub fn unpushed_private(&self, checkout: &Path) -> Result<u64, String> {
        let private = self.private_refs(checkout)?;
        if private.is_empty() {
            return Ok(0);
        }
        let mut args: Vec<&str> = vec!["rev-list", "--count"];
        args.extend(private.iter().map(String::as_str));
        args.extend(["--not", "--remotes"]);
        let count = self.run(checkout, &args, None)?;
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
        self.ignores_contents(checkout, directory, &[])
    }

    /// A whole checkout also needs proof for empty directories, which Git
    /// leaves out of a file listing.
    pub(crate) fn ignores_all_entries(
        &self,
        checkout: &Path,
        directory: &Path,
    ) -> Result<bool, String> {
        self.ignores_contents(checkout, directory, &["--directory"])
    }

    fn ignores_contents(
        &self,
        checkout: &Path,
        directory: &Path,
        options: &[&str],
    ) -> Result<bool, String> {
        let mut arguments = vec![
            "--literal-pathspecs",
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
        ];
        arguments.extend_from_slice(options);
        let listed = self.run(checkout, &arguments, Some(directory))?;
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

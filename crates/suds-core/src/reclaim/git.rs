//! The typed adapter for Git, which owns each checkout's state.
//!
//! Suds asks Git whether a checkout holds work that no remote holds, and it
//! has Git remove a linked checkout. Each call passes its arguments as a
//! list, and no call passes a shell string.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{self, Read};
use std::os::fd::AsFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use rustix::process::{Pid, Signal, kill_process_group};

const REMOTE_TIMEOUT: Duration = Duration::from_secs(10);

type AdvertisedRefs = HashMap<String, String>;

/// The references Git reads once per repository in one reclaim run.
#[derive(Default)]
pub struct RemoteRefs {
    repositories: RefCell<HashMap<PathBuf, Result<AdvertisedRefs, String>>>,
}

impl RemoteRefs {
    pub(crate) fn reference(
        &self,
        git: &Git,
        repository: &Path,
        commit: &str,
        query_remote: bool,
    ) -> Result<Option<String>, String> {
        let mut repositories = self.repositories.borrow_mut();
        if query_remote && !repositories.contains_key(repository) {
            let refs = git.advertised_refs(repository);
            repositories.insert(repository.to_path_buf(), refs);
        }
        match repositories.get(repository) {
            Some(Ok(refs)) => Ok(refs.get(commit).cloned()),
            Some(Err(reason)) => Err(reason.clone()),
            None => Err("the plan has no remote lookup for the repository".to_owned()),
        }
    }
}

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
    /// A status query takes no optional lock, so a query writes nothing.
    fn command(&self, directory: &Path) -> Command {
        let mut command = Command::new(&self.program);
        command
            .arg("--no-optional-locks")
            .arg("-C")
            .arg(directory)
            .env("GIT_NO_LAZY_FETCH", "1");
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
        command
    }

    fn run(
        &self,
        directory: &Path,
        arguments: &[&str],
        path: Option<&Path>,
    ) -> Result<String, String> {
        let mut command = self.command(directory);
        command.args(arguments);
        if let Some(path) = path {
            command.arg("--").arg(path);
        }
        let output = command
            .output()
            .map_err(|err| format!("could not run {}: {err}", self.program.display()))?;
        git_output(directory, &arguments.join(" "), output)
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

    /// The name of a remote-tracking branch that contains `commit`.
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

    fn advertised_refs(&self, repository: &Path) -> Result<AdvertisedRefs, String> {
        let canonical = std::fs::canonicalize(repository)
            .map_err(|err| format!("could not resolve {}: {err}", repository.display()))?;
        if canonical != repository || !self.is_repository_root(repository)? {
            return Err(format!(
                "{} is no longer the repository's main checkout",
                repository.display()
            ));
        }
        let remote = self.default_remote(repository)?;
        let url = self.run(repository, &["remote", "get-url", "--", &remote], None)?;
        let local = url.trim().strip_prefix("file://").unwrap_or(url.trim());
        if let Ok(path) = std::fs::canonicalize(repository.join(local))
            && (path == repository || path == repository.join(".git"))
        {
            return Err(format!(
                "the remote {remote} points to the repository itself"
            ));
        }
        let mut command = self.command(repository);
        command
            .args([
                "-c",
                "credential.interactive=false",
                "-c",
                "protocol.ext.allow=never",
                "ls-remote",
                "--quiet",
                "--",
                &remote,
            ])
            .env("GIT_TERMINAL_PROMPT", "0");
        let output = output_with_timeout(&mut command, REMOTE_TIMEOUT)
            .map_err(|err| format!("git ls-remote failed in {}: {err}", repository.display()))?;
        let stdout = git_output(repository, "ls-remote", output)?;
        let mut refs = AdvertisedRefs::new();
        for (commit, reference) in stdout.lines().filter_map(|line| line.split_once('\t')) {
            if reference.starts_with("refs/") && !refs.contains_key(commit) {
                refs.insert(commit.to_owned(), reference.to_owned());
            }
        }
        Ok(refs)
    }

    fn default_remote(&self, repository: &Path) -> Result<String, String> {
        let remotes = self.run(repository, &["remote"], None)?;
        let mut names = remotes.lines();
        if let (Some(only), None) = (names.next(), names.next()) {
            return Ok(only.to_owned());
        }
        if !remotes.is_empty() {
            let branch = self.run(repository, &["branch", "--show-current"], None)?;
            if !branch.trim().is_empty() {
                let key = format!("branch.{}.remote", branch.trim());
                let configured = self.run(
                    repository,
                    &["config", "--default", "origin", "--get", &key],
                    None,
                )?;
                if remotes.lines().any(|name| name == configured.trim()) {
                    return Ok(configured.trim().to_owned());
                }
            }
            if remotes.lines().any(|name| name == "origin") {
                return Ok("origin".to_owned());
            }
        }
        Err("the repository has no default remote for git ls-remote".to_owned())
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

fn git_output(directory: &Path, arguments: &str, output: Output) -> Result<String, String> {
    if !output.status.success() {
        return Err(format!(
            "git {arguments} failed in {}: {}",
            directory.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn output_with_timeout(command: &mut Command, timeout: Duration) -> io::Result<Output> {
    let deadline = Instant::now() + timeout;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()?;
    let output = read_output(&mut child, deadline);
    if output.is_err() {
        // Git can start a transport or credential helper that keeps a pipe open.
        let _ = kill_process_group(Pid::from_child(&child), Signal::KILL);
        child.wait()?;
    }
    output
}

fn read_output(child: &mut Child, deadline: Instant) -> io::Result<Output> {
    let mut stdout_pipe = child.stdout.take().expect("piped standard output");
    let mut stderr_pipe = child.stderr.take().expect("piped standard error");
    for pipe in [stdout_pipe.as_fd(), stderr_pipe.as_fd()] {
        fcntl_setfl(pipe, fcntl_getfl(pipe)? | OFlags::NONBLOCK)?;
    }
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    loop {
        let stdout_done = read_chunk(&mut stdout_pipe, &mut stdout)?;
        let stderr_done = read_chunk(&mut stderr_pipe, &mut stderr)?;
        if stdout_done
            && stderr_done
            && let Some(status) = child.try_wait()?
        {
            return Ok(Output {
                status,
                stdout,
                stderr,
            });
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the remote lookup timed out",
            ));
        }
        std::thread::sleep(remaining.min(Duration::from_millis(10)));
    }
}

fn read_chunk(pipe: &mut impl Read, output: &mut Vec<u8>) -> io::Result<bool> {
    // A stream that keeps writing must still give the deadline a turn.
    const CHUNK: u64 = 64 * 1024;
    match pipe.take(CHUNK).read_to_end(output) {
        Ok(bytes) => Ok((bytes as u64) < CHUNK),
        Err(err) if err.kind() == io::ErrorKind::WouldBlock => Ok(false),
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use std::io::ErrorKind;
    use std::process::Command;
    use std::time::Duration;

    use super::output_with_timeout;

    #[test]
    fn stops_waiting_for_a_command_that_holds_its_pipes_open() {
        let mut command = Command::new("sleep");
        command.arg("60");
        let result = output_with_timeout(&mut command, Duration::from_millis(50));
        assert_eq!(result.unwrap_err().kind(), ErrorKind::TimedOut);
    }
}

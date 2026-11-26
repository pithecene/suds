//! The typed adapter for Git, which owns each checkout's state.
//!
//! Suds asks Git whether a checkout holds work that no remote holds, and it
//! has Git remove a linked checkout. Each call passes its arguments as a
//! list, and no call passes a shell string.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::helper::{HelperOutput, HelperPolicy, HelperRunner, HelperSpec};
use crate::model::EntryKind;

/// Worktree-local pseudoreferences that Git can create and that leave with a
/// linked worktree.
const WORKTREE_PSEUDOREFS: [&str; 6] = [
    "ORIG_HEAD",
    "MERGE_HEAD",
    "CHERRY_PICK_HEAD",
    "REVERT_HEAD",
    "BISECT_HEAD",
    "AUTO_MERGE",
];

/// Explicit three-state observation of one worktree pseudoreference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PseudoRefState {
    /// The pseudoreference resolves to a commit object.
    PresentCommit(String),
    /// The pseudoreference exists and resolves to a non-commit object, such as
    /// Git's tree-valued `AUTO_MERGE`.
    PresentNonCommit { name: String, object_type: String },
    /// Git positively confirmed that the pseudoreference does not exist.
    Absent,
    /// Suds could not establish the pseudoreference state, so callers preserve
    /// the candidate.
    Unknown(String),
}

/// The Git program that Suds runs, governed by a bounded [`HelperRunner`].
#[derive(Debug, Clone)]
pub struct Git {
    program: PathBuf,
    runner: Arc<HelperRunner>,
}

impl PartialEq for Git {
    fn eq(&self, other: &Self) -> bool {
        self.program == other.program
    }
}

impl Eq for Git {}

impl Default for Git {
    /// The `git` that the process's search path names, with default helper bounds.
    fn default() -> Self {
        Git {
            program: PathBuf::from("git"),
            runner: HelperRunner::new(HelperPolicy::default()),
        }
    }
}

impl Git {
    /// Creates a Git adapter pointing at `program` with default helper bounds.
    pub fn with_program(program: PathBuf) -> Self {
        Git {
            program,
            runner: HelperRunner::new(HelperPolicy::default()),
        }
    }

    /// Replaces the helper runner governing this Git adapter.
    #[must_use]
    pub fn with_runner(mut self, runner: Arc<HelperRunner>) -> Self {
        self.runner = runner;
        self
    }

    /// The helper runner governing this Git adapter.
    pub fn runner(&self) -> &Arc<HelperRunner> {
        &self.runner
    }

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

    fn build_spec(
        &self,
        directory: &Path,
        arguments: &[&str],
        path: Option<&Path>,
        stdin: Option<Vec<u8>>,
    ) -> HelperSpec {
        let mut args: Vec<OsString> = Vec::with_capacity(arguments.len() + 5);
        args.push(OsString::from("--no-optional-locks"));
        args.push(OsString::from("-C"));
        args.push(directory.as_os_str().to_os_string());
        args.extend(arguments.iter().map(OsString::from));
        if let Some(path) = path {
            args.push(OsString::from("--"));
            args.push(path.as_os_str().to_os_string());
        }
        HelperSpec {
            operation: format!("git {}", arguments.first().copied().unwrap_or("")),
            program: self.program.clone(),
            args,
            current_dir: None,
            // Object traversal in a partial clone must stay local.
            env_set: vec![
                (OsString::from("GIT_NO_LAZY_FETCH"), OsString::from("1")),
                (OsString::from("GIT_ALLOW_PROTOCOL"), OsString::from("")),
            ],
            // A variable of the caller's own repository must not redirect the
            // query to another repository.
            env_remove: vec![
                OsString::from("GIT_DIR"),
                OsString::from("GIT_WORK_TREE"),
                OsString::from("GIT_INDEX_FILE"),
                OsString::from("GIT_COMMON_DIR"),
            ],
            stdin,
        }
    }

    fn run_raw(
        &self,
        directory: &Path,
        arguments: &[&str],
        path: Option<&Path>,
        stdin: Option<Vec<u8>>,
    ) -> Result<HelperOutput, String> {
        let spec = self.build_spec(directory, arguments, path, stdin);
        self.runner.run(spec).map_err(|err| err.to_string())
    }

    fn run_bytes(
        &self,
        directory: &Path,
        arguments: &[&str],
        path: Option<&Path>,
    ) -> Result<Vec<u8>, String> {
        let output = self.run_raw(directory, arguments, path, None)?;
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

    /// Queries every per-worktree pseudoreference in a single bounded
    /// `git cat-file --batch-check` call and cross-checks any `missing` reply
    /// against the worktree's `git_dir` so unreadable, malformed, or unresolved
    /// pseudoreferences never classify as absent.
    pub fn probe_pseudorefs(&self, checkout: &Path) -> Result<Vec<PseudoRefState>, String> {
        let raw_git_dir = self.run(checkout, &["rev-parse", "--git-dir"], None)?;
        let git_dir_trimmed = raw_git_dir.trim();
        if git_dir_trimmed.is_empty() {
            return Err(format!(
                "git rev-parse --git-dir returned empty output for {}",
                checkout.display()
            ));
        }
        let git_dir = {
            let path = PathBuf::from(git_dir_trimmed);
            if path.is_absolute() {
                path
            } else {
                checkout.join(path)
            }
        };
        let git_dir_meta = std::fs::metadata(&git_dir).map_err(|err| {
            format!(
                "could not inspect git directory {} for {}: {err}",
                git_dir.display(),
                checkout.display()
            )
        })?;
        if !git_dir_meta.is_dir() {
            return Err(format!(
                "git directory {} for {} is not a directory",
                git_dir.display(),
                checkout.display()
            ));
        }
        let mut stdin = String::new();
        for pseudo in WORKTREE_PSEUDOREFS {
            stdin.push_str(pseudo);
            stdin.push('\n');
        }
        let output = self.run_raw(
            checkout,
            &["cat-file", "--batch-check"],
            None,
            Some(stdin.into_bytes()),
        )?;
        if !output.status.success() {
            return Err(format!(
                "git cat-file --batch-check failed in {}: {}",
                checkout.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let lines: Vec<&str> = text.lines().collect();
        if lines.len() != WORKTREE_PSEUDOREFS.len() {
            return Err(format!(
                "git cat-file --batch-check returned {} lines for {} pseudoreferences in {}",
                lines.len(),
                WORKTREE_PSEUDOREFS.len(),
                checkout.display()
            ));
        }
        let mut states = Vec::with_capacity(WORKTREE_PSEUDOREFS.len());
        for (pseudo, line) in WORKTREE_PSEUDOREFS.iter().zip(lines) {
            let line = line.trim();
            if line == format!("{pseudo} missing") {
                // Git returns `exit 0` and `<NAME> missing` with empty stderr
                // even when `<git-dir>/<NAME>` exists on disk with mode `0000`
                // or malformed content. Require positive filesystem proof that
                // the loose pseudoreference file does not exist.
                let pseudo_path = git_dir.join(pseudo);
                match std::fs::symlink_metadata(&pseudo_path) {
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                        states.push(PseudoRefState::Absent);
                    }
                    Ok(_) => {
                        states.push(PseudoRefState::Unknown(format!(
                            "pseudoreference {pseudo} exists at {} but Git reported it missing",
                            pseudo_path.display()
                        )));
                    }
                    Err(err) => {
                        states.push(PseudoRefState::Unknown(format!(
                            "could not verify absence of pseudoreference {pseudo} at {}: {err}",
                            pseudo_path.display()
                        )));
                    }
                }
                continue;
            }
            let mut parts = line.split_whitespace();
            let Some(oid) = parts.next() else {
                states.push(PseudoRefState::Unknown(format!(
                    "empty batch-check reply for {pseudo}"
                )));
                continue;
            };
            let Some(kind) = parts.next() else {
                states.push(PseudoRefState::Unknown(format!(
                    "unrecognized batch-check reply for {pseudo}: {line}"
                )));
                continue;
            };
            let valid_oid = matches!(oid.len(), 40 | 64)
                && oid
                    .bytes()
                    .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'));
            if !valid_oid {
                states.push(PseudoRefState::Unknown(format!(
                    "unrecognized batch-check status for {pseudo}: {line}"
                )));
                continue;
            }
            match kind {
                "commit" => states.push(PseudoRefState::PresentCommit(oid.to_owned())),
                "tag" => {
                    let peeled = self.run(
                        checkout,
                        &["rev-parse", "--verify", &format!("{oid}^{{commit}}")],
                        None,
                    );
                    match peeled {
                        Ok(commit) if !commit.trim().is_empty() => {
                            states.push(PseudoRefState::PresentCommit(commit.trim().to_owned()));
                        }
                        Ok(_) => states.push(PseudoRefState::PresentNonCommit {
                            name: (*pseudo).to_owned(),
                            object_type: "tag".to_owned(),
                        }),
                        Err(reason) => states.push(PseudoRefState::Unknown(reason)),
                    }
                }
                other => states.push(PseudoRefState::PresentNonCommit {
                    name: (*pseudo).to_owned(),
                    object_type: other.to_owned(),
                }),
            }
        }
        Ok(states)
    }

    /// Worktree-private references (`refs/worktree`, `refs/bisect`,
    /// `refs/rewritten`) and per-worktree pseudoreferences (`ORIG_HEAD`,
    /// `MERGE_HEAD`, etc.) that leave with this worktree.
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
        let mut refs: Vec<String> = output
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect();
        for state in self.probe_pseudorefs(checkout)? {
            match state {
                PseudoRefState::PresentCommit(oid) => refs.push(oid),
                PseudoRefState::PresentNonCommit { name, object_type } => {
                    return Err(format!(
                        "worktree {} holds non-commit pseudoreference {name} ({object_type})",
                        checkout.display()
                    ));
                }
                PseudoRefState::Absent => {}
                PseudoRefState::Unknown(reason) => {
                    return Err(format!(
                        "could not verify pseudoreferences in {}: {reason}",
                        checkout.display()
                    ));
                }
            }
        }
        Ok(refs)
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
        self.remove_worktree_holding(repository, checkout, ())
    }

    /// Like [`remove_worktree`], keeping `guard` held inside the helper worker
    /// until Git exits or terminates and reaps, preserving removal exclusion
    /// even when the call times out or cancels.
    pub(crate) fn remove_worktree_holding<G: Send + 'static>(
        &self,
        repository: &Path,
        checkout: &Path,
        guard: G,
    ) -> Result<(), String> {
        let spec = self.build_spec(repository, &["worktree", "remove"], Some(checkout), None);
        let (output, _guard) = self
            .runner
            .run_holding(spec, guard)
            .map_err(|err| err.to_string())?;
        if !output.status.success() {
            return Err(format!(
                "git worktree remove failed in {}: {}",
                repository.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(())
    }
}

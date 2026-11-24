# Reclamation

This guide describes the `suds reclaim` command, which removes build state that no process uses.

```sh
suds reclaim --root ~/workspace                 # the plan, with nothing changed
suds reclaim --root ~/workspace --apply         # remove what the plan names
suds reclaim --root ~/workspace --checkouts     # consider linked checkouts too
suds reclaim --root ~/workspace --idle-minutes 240
```

The command inspects the [development profile](inspect.md#development-profile) of
each root and plans the removal of each checkout's build state. Without
`--apply` it prints the plan and changes nothing. Each candidate carries
its decision and the evidence for it, and `--format json` writes the whole
`suds.reclaim/v1` plan.

Suds keeps a directory of build state in each of these cases:

- A process works in the checkout that owns the directory. Suds reads each
  process's working directory from `/proc` on Linux and from `lsof` on
  macOS.
- An entry in the directory changed within the idle time, which is 60
  minutes unless `--idle-minutes` names another. A recent change suggests
  a build in progress. No age ever causes a removal.
- Suds could not read the processes' working directories. On Linux, the
  kernel hides the working directory of a process that forbids its own
  inspection. While this user runs such a process, Suds keeps every
  directory, and the plan names the process. The option
  `--tolerate-unreadable-processes` accepts those processes.
- Git tracks a file in the directory, or would track one. Suds removes a
  directory only when Git ignores every file beneath it.
- A repository beneath it holds a changed file, or a commit that no
  remote-tracking branch holds. A build fetches some dependencies as
  repositories, and one that holds only what it fetched keeps nothing.
- A mount point lies beneath it, or Suds could not read an entry there.
- Another user owns a directory in it, as a container that runs as the
  administrator leaves it. Suds removes nothing with an administrator's
  rights, so an administrator removes such a directory.

The plan also names each toolchain that a checkout fetched into its own
tree as a repository, such as a vcpkg checkout, which the checkout's build
fetches again. The same protections hold, and Git must report that the
repository holds only what a remote holds.

A repository inside a candidate leaves with it only when a layout proves
that a build fetched it. CMake's `_deps/<name>-src` beside
`_deps/<name>-subbuild` is such a layout. Such a repository must hold a clean tree, no
second worktree, and no commit, branch, tag, or stash that a remote lacks.
Any other repository is a checkout, and it keeps the candidate. A
registration of a directory inside a candidate keeps the candidate too.

The `suds reclaim` plan considers released linked checkouts by default and
considers all linked checkouts with `--checkouts`. Git must report no
changed, staged, or untracked file there. No process may work in the
checkout, no entry may have changed within the idle time, and no client may
hold an unreleased registration.

Git removes a checkout together with the files that it ignores. Each run
therefore accounts for every entry, whoever approves it. The head's tree
accounts for tracked files, and the build-state rules account for ignored build
output. Any other entry keeps the checkout, such as an ignored `.env` or a
tool's own records, and the plan names the first one. A repository that a build
fetched keeps the checkout until its build state leaves.

A remote-tracking branch can show that a remote holds the checkout's
head. A person who runs `suds reclaim --apply` can also use a release with
`--landed COMMIT` as evidence while the head still matches `COMMIT`.

A scheduled run with standing approval requires a release with
`--landed COMMIT`. At planning and before removal, Suds checks that the
head matches the release and that a local remote-tracking reference
contains it. Suds accounts for every entry in the checkout. The landed
tree accounts for tracked files and their directories. The build-state
rules above account for ignored build output. Any other file or directory
keeps the checkout, as does a checkout beneath it, a mount point, or an
entry Suds could not read. The plan or journal names the first path that
keeps the checkout.

Suds has Git remove the linked checkout without force. The branch and its
commits stay in the repository. Suds never names a main checkout.

With `--apply`, Suds records the plan in the journal and judges each
candidate again immediately before its removal. The directory must be the
entry that the plan measured, with no symbolic link in its path. Suds then
observes the processes, measures the directory, and asks Git anew, and it
applies every rule of the plan to what it finds. One refusal leaves the other candidates to proceed. The journal records the
plan, the authority for the run, and each removal or refusal. When Suds
finds a local remote-tracking reference that contains the released head at
removal, the journal records it. The `suds journal` command lists the run
with its removals. A removal has no undo.

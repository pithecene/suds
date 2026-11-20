# Changelog

This file records every notable change to Suds.

This changelog follows the format of [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and Suds adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

---

## [Unreleased]

### Added

- **A native window shows the workstation's development storage**.

  The program `suds-window` measures the settings' roots. It shows the
  pressure against the ceiling and each class of development storage. It
  groups the checkouts of one repository into a project. A storage map
  draws each project and each checkout as a rectangle whose area is its
  size on disk, and an outline lists the same projects with their sums.
  Each checkout states its build state and the tool that uses it. A filter
  and four orders arrange the projects, and a pane shows the selected
  project or checkout. Each shared store stands against its budget. The
  window runs on Linux and changes nothing on disk.

## [0.1.3] - 2026-11-20

### Added

- **Scheduled runs can reclaim landed checkouts**.

  With standing approval, a scheduled run removes a clean, idle linked
  checkout when its head matches a client's landed release and a local
  remote-tracking reference contains it. At planning and before removal,
  Suds verifies the head, the reference, and every entry. The landed tree
  and ignored build state must account for the contents. A nested checkout,
  a mount point, or an unreadable entry keeps the checkout. The journal
  records the reference or the first path that keeps the checkout.

## [0.1.2] - 2026-11-20

### Added

- **Virtual machines count toward development storage**: the development profile classifies each Lima and Colima machine from its layout. It finds one at the program's own location and inside a checkout. The pressure counts each machine's disk. `suds stores` reports a machine beyond 60 GiB and each machine that a checkout holds, and it trims none.

## [0.1.1] - 2026-11-20

### Added

- **A release reports a landed head**: `suds release --landed COMMIT` records that a remote holds the change at the checkout's head, though by another commit. A forge that squashes a branch and deletes it left such a checkout in place for good, because no remote held its commits. A person's own `suds reclaim --apply` can remove it while its head is still that commit and its tree is clean.

## [0.1.0] - 2026-11-20

### Added

- **Releases**: a tag `vX.Y.Z` now publishes a release. It holds an archive of the `suds` command for macOS on Apple silicon and for Linux on x64 and arm64, so a workstation installs Suds without building it.
- **Caches inside a checkout count as build state**: a project may point `GOCACHE`, `GOMODCACHE`, or `CCACHE_DIR` into its own checkout, where each checkout then fills a copy. The development profile now classifies such a Go build cache, Go module cache, and ccache cache from the files that its program writes. `suds reclaim` removes each one under the protections of other build state.
- **`suds reclaim` removes a toolchain that a checkout fetched**: a vcpkg checkout inside a checkout's own tree enters the plan beside that checkout's build state. It leaves under the same protections, and only when Git reports that its repository holds only what a remote holds. The development report names the checkout that owns each such toolchain.
- **`suds arena` audits the ceiling of development storage**: the command reports the ceiling, the root's filesystem, and whether a filesystem enforces the ceiling. Where none does, it prints the commands that would for APFS, btrfs, and XFS, and it runs none of them. Each pressure reading now states whether a filesystem enforces its ceiling.
- **Clients register checkouts and read the pressure of development storage**: `suds register` and `suds release` record which client holds a checkout, and Suds removes nothing beneath a registered one. `suds maintain` records a pressure reading against the ceiling, and `suds pressure` prints it. Under the standing approval of the settings, `suds maintain` also reclaims build state and trims the stores without a person. `suds schedule` prints the units that run it for systemd and for launchd.

- **`suds stores` holds each shared store to a budget**: the command measures the stores of Go, pnpm, ccache, and mise and asks Docker for its images, its build cache, and its unused volumes. With `--apply`, a store beyond its budget takes the trim of its own program, and the journal records its size before and after. Suds removes no entry of a store itself, and it lists each unused named volume for a person without removing one.
- **`suds reclaim` removes build output that no process uses**: the command plans the removal of each checkout's build state and prints the evidence for each decision. With `--apply`, it verifies each candidate again, removes it, and records each removal in the journal. It keeps a directory while a process works in its checkout or while an entry in it changed within the idle time. With `--checkouts`, it has Git remove a clean linked checkout whose commits a remote holds. It never names a main checkout.
- **`suds inspect --profile development` classifies development storage**: the command finds each main and linked Git checkout beneath the named directories and measures the build output of Cargo, Node, CMake, and vcpkg beneath each one. It also measures the shared stores of Go, Cargo, pnpm, sccache, ccache, and vcpkg, and the toolchains of mise. Each class rests on proof from its program's own layout. The command writes a table or one `suds.development/v1` document on macOS and on Linux, and it changes nothing.
- **Suds moves Downloads entries through reversible plans**: `suds plan move` writes a `suds.plan/v1` plan without moving anything, `suds apply` runs a plan that Suds recorded, `suds journal` lists the runs, and `suds undo` returns each moved entry. Each move verifies its preconditions and renames once within one filesystem, without overwriting. An append-only journal records every step, and the next run reconciles an interrupted move. The desktop application selects Downloads entries, shows the plan in a sheet, applies the plan, and undoes the run.
- **The Suds desktop application displays storage measurements in a window on macOS**: it lists the default roots and each folder that the operator picks, and it scans them on a background thread. It displays each root's measurements as that root's scan ends, and it cancels a scan on request. Its web view may call four application commands and holds no filesystem, shell, or network permission.
- **`suds inspect` measures macOS userland storage without changing it**: the command reads metadata beneath the user's caches, logs, Xcode derived data, downloads, and trash. It reports each root's logical and allocated sizes, entry counts, range of modification times, and largest direct children. On macOS and Linux, the `--root` option replaces these default roots with roots that the operator names. Given the `--format json` option, the command writes one `suds.inspection/v1` document. The scan follows no symbolic link, crosses no mount point, counts hard-linked storage once, and reports every traversal error.

### Changed

- **A scan reads each entry's metadata once**: the walk asked the system about each entry twice, once for the boundary and once for the count. One call now serves both, and an inspection of a 770 GiB development tree took 17 seconds in place of 25.
- **The journal has a default directory on Linux**: it lies in `~/.local/state/suds/journal`, and `XDG_STATE_HOME` moves it. The option `--journal` still names another directory.
- **A shared `suds-core` crate holds the inspection code**: the `suds` command and the desktop application each call it. The scan records native paths and names. The `suds inspect` command's JSON report, terminal table, and exit statuses remain unchanged.

### Fixed

- **The systemd unit accepts a partial run**: `suds maintain` ends with status 1 when it refuses a removal or cannot read an entry, and systemd then marked each such run as failed. The unit from `suds schedule` now names that status as a success.
- **A run removes a checkout and its build state together**: a run removed a checkout's build state first, which changed the checkout, so the checkout counted as recently modified and waited for the next run. Git now removes the checkout first, with the build state in it.
- **A plan keeps what another user owns**: a container that runs as the administrator leaves directories that the user cannot remove, and each run tried and failed. The plan now keeps such a directory with the reason `owned_by_another_user`.
- **A repository inside a candidate needs proof of its fetch**: only CMake's own layout now proves that a build fetched a repository. Any other repository in build state is a checkout, so the build state stays. A fetched repository counts as work when it holds a local branch, tag, stash, or second worktree. A linked checkout stays while a fetched repository in its build state holds work, and a registration inside a candidate keeps the candidate.
- **Readable plans**: the reclaim table names the reason for each kept directory before its fact, and a store that Suds could not measure shows no size and one line of the failure. The default budget of ccache rises to 10 GiB, above the 5 GiB at which ccache bounds its own cache, so the plan no longer trims a cache at its own limit.
- **Build state with a fetched repository**: `suds reclaim` kept every build directory that held a Git repository, which kept each CMake build that fetches a dependency. It now asks Git about each such repository and keeps the directory only where one holds a change or a commit that no remote and no tag holds. The development profile lists such a repository as part of its build state and as no checkout.

[Unreleased]: https://github.com/pithecene/suds/compare/v0.1.3...HEAD
[0.1.3]: https://github.com/pithecene/suds/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/pithecene/suds/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/pithecene/suds/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/pithecene/suds/releases/tag/v0.1.0

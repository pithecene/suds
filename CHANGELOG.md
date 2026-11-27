# Changelog

This file records every notable change to Suds.

This changelog follows the format of [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and Suds adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

---

## [Unreleased]

### Changed

- **A paused scheduled run still measures**: while `pause.json` is active, `suds maintain` takes the same bounded observation slice as `suds maintain --observe` and records the pressure. It reclaims nothing, trims no store, and leaves the pause as written. Before this change a paused scheduled run returned the last recorded pressure, so the scheduled run did not refresh the reading: only an explicit `suds maintain --observe` did, and a client that held work on a hard reading waited on one.

## [0.1.10] - 2026-11-27

### Added

- **Bounded observation while paused**: `suds maintain --observe` runs a bounded pressure observation slice while `pause.json` stays active. It never reclaims storage or modifies durable pause state.

### Changed

- **Batched macOS metadata scanning with `getattrlistbulk(2)`**: `suds` reads macOS directory entries, metadata, and APFS clone flags in batches through `getattrlistbulk(2)`. This replaces per-entry `lstat` and `getattrlist` calls.

### Fixed

- **Multi-slice pressure completion across bounded runs**: `maintain-cursor.json` now records completed stores and accumulated usage across slices. Once the final slice finishes every checkout and store, Suds publishes a complete reading and clears the cursor.
- **Multi-slice cursor schema, mode separation, and cross-slice deduplication**: `maintain-cursor.json` now uses schema `suds.maintain-cursor/v2` and separates observation from reclamation passes. Slices track checkout-local and workspace toolchains, exclude reclaimed build state, and carry committed deduplication state.

## [0.1.9] - 2026-11-26

### Added

- **Durable maintenance pause**: `suds maintain --pause`, `--resume`, and `--status` record and inspect a durable pause file (`pause.json`) in the Suds state directory. A paused or unreadable pause state stops scheduled maintenance before taking the maintenance lock or starting a filesystem scan.
- **Bounded observation, seeding, and macOS background priority**: `suds-core` carries an explicit execution budget (`Budget` and `BudgetTracker`) through directory scans, read-ahead traversal, development inspection, maintenance, and sibling seed selection. Maintenance runs with one total metadata reader, applies macOS background QoS and throttled I/O policy, bounds sibling seed inspection, and refuses to publish an incomplete scan as a complete pressure reading.
- **Background scheduling metadata for installed services**: `suds schedule` writes macOS `ProcessType=Background`, `LowPriorityIO`, `LowPriorityBackgroundIO`, and `Nice=10` keys to the LaunchAgent plist. On Linux, it writes `Nice=10` and `IOSchedulingClass=idle` to the systemd service unit.
- **Bounded helper runner, safe pseudoreferences, CLI shutdown, and maintenance cursor**: Git and macOS `lsof` run through a bounded `HelperRunner` with deadlines, pacing, and output limits. CLI `SIGINT` and `SIGTERM` handling kills and reaps mutating helper groups before dropping locks. Pseudoreference probes cross-check `git_dir` so unreadable entries keep candidates intact. Maintenance shares one `BudgetTracker` across scans and verification, persists pending nested worktrees in `maintain-cursor.json`, preserves `lower_since` across partial post-scans, and skips external store adapters.

## [0.1.8] - 2026-11-25

### Added

- **A plan states who claims each checkout**: `suds register --claim TEXT` records a text such as a session's name, and `suds reclaim` shows it in the column `CLAIM` and the desktop's evidence. A run keeps a linked checkout or its build output when the claims changed after the plan.

### Changed

- **A claim keeps a linked checkout and its build output**: `suds release --claim-ended` states that the work in a checkout ended. Without that statement for each client and attempt, Suds keeps the checkout and its build output, so a merge or a release of an earlier version no longer frees them.
- **A person's reclaim applies a saved plan**: `suds reclaim --root DIR --save` records the plan in the journal, and `suds reclaim --apply PLAN` removes what the plan of that identifier names. The form `--root DIR --apply` of earlier releases built a new plan that nobody had read, and Suds now refuses it. An entry that changed after the plan keeps its candidate.
- **A scheduled run removes no checkout**: `suds maintain` names build state and stores alone under the standing approval. Earlier releases also removed a landed linked checkout.
- **A measurement on macOS counts a cloned file once**: a scan counts a file of 16 KiB or more that shares every block with a clone in the first place that it reaches. A seeded checkout therefore raises the usage by what it adds. On one workstation the measured workspace fell from 284 GiB to 199 GiB. A file that shares only a part of its blocks counts in full.
- **Go's build cache keeps its recent entries at a trim**: `suds stores` and a maintenance run evict the entries that Go used least recently, until the cache meets its budget. Entries that Go used within the idle time and one more hour stay. Earlier releases ran `go clean -cache`, which removed the whole cache, and kept a whole cache after a recent write. The plan's JSON states the decision `evict`.

### Fixed

- **Post-maintenance pressure recovery, pre-scan locking, and macOS `lsof` visibility**: `suds maintain` serializes concurrent runs before scanning and publishes a post-cleanup pressure reading that keeps the five-minute transition window. The report also records filesystem free bytes before and after maintenance. On macOS, `lsof` keeps unreadable working-directory entries for the current user with process and command context, while ignoring other users' processes.
- **Worktree-private references and registration races**: `suds reclaim` counts unpushed commits on worktree-private references (`refs/worktree`, `refs/bisect`, `refs/rewritten`) and per-worktree pseudoreferences (`ORIG_HEAD`, `MERGE_HEAD`, etc.), and requires a surviving remote-tracking reference even for `--landed` heads. Registration and reclamation share a local coordination lock: reclamation re-verifies ownership under the lock before removal, and `suds register` verifies the checkout exists under the lock before acknowledging protection.
- **A person's reclaim keeps a checkout with ignored files**: `suds reclaim --apply` removed a released linked checkout together with files that Git ignores, such as `.env`. It now accounts for every entry as a scheduled run does, and the plan names the first entry that keeps the checkout.

## [0.1.7] - 2026-11-22

### Changed

- **A measurement on Linux counts a cloned block once**: a scan asks the kernel for the shared extents of each file of 16 KiB or more. It counts each extent in the first file that reaches it. A seeded checkout counted its clones in full before, so the pressure rose on space that the disk did not hold. On one workstation the reported usage fell from 103.4 GiB to 95.1 GiB, and the inspection took 4.8 seconds where it took 3.5 seconds before.

- **The arena's audit reads a quota group of btrfs**: `suds arena` and each pressure reading now report a ceiling as enforced where a quota group limits the root's subvolume to the ceiling or less. The audit read no quota group before, so it reported such a root as not enforced. The report of `suds arena --format json` gains the fields `quota_limit_bytes` and `quota_error`.

### Fixed

- **The plan states the removal rule of pnpm's trim**: the plan said that the trim removes each package that no project references. pnpm reads a reference from a hard link, so on a filesystem that clones files the trim removes every package. The plan and the guide now state the rule of pnpm.

## [0.1.6] - 2026-11-21

### Added

- **The `seed` command gives a new checkout another checkout's build state**: `suds seed CHECKOUT --apply` clones Cargo's build state from a checkout of the same repository. A clone shares its blocks with its source. On one workstation, a seed of 5.4 GiB took 0.6 seconds and 11 MiB, and the first build compiled 5 packages where it compiled 378 before. A seed changes no existing entry, and it writes nothing on a filesystem that clones no file.

## [0.1.5] - 2026-11-21

### Added

- **The desktop application shows a reading from a named file**: the variable `SUDS_SHOW_READING` names a file in the form of the saved reading. The window shows that reading and measures nothing, so a picture of the window shows no real workstation.
- **A build states its commit, its owner, and its licence**: the `suds --version` command prints the version, then the commit of the build with its date. The copyright of Pithecene LLC, the licence, and the contact address follow. The `suds -V` command still prints the version alone. The desktop application shows the same in About.
- **A release for Linux holds the desktop application**: each archive for Linux holds `bin/suds-desktop` and the script `share/suds/desktop/install.sh`. The script installs the binary, the desktop entry, and the icon for one user, where the window built only from source before.

### Changed

- **The README shortens, and guides hold the reference**: the README states what Suds does, its guarantees, and a picture of the desktop application on an invented workstation. The reference of each command moves to a guide beneath `docs/guides`.
- **The stores' pane names the group of mise Polyglot**: the group of a store that serves each language read Many languages before.
- **A measurement reads directories on several threads**: an inspection, a reclaim plan, and a scheduled run read each directory's metadata on up to eight threads, and count the entries in one order as before. A report holds the same figures. On one workstation with 2.6 million files, the development inspection took 20 seconds before and takes 4.3 seconds now. A claim of a checkout or of build state now reads the directory's listing, so it asks the filesystem for no marker file.

### Removed

- **The web-view application leaves the repository**: the Tauri application of decision 0001, its React frontend, and the pinned Node.js and pnpm go. Each window of Suds is native: the Slint window on Linux, and on macOS the Swift window that decision 0005 proposes. macOS holds no window before that one. The window of the web view showed the inspection of a root and planned the moves of Downloads entries, and the command line still serves both.

### Fixed

- **A size that rounds to a whole unit reads in that unit**: a count one byte below one gibibyte read `1024.0 MiB`. The command and the desktop application now write `1.0 GiB`, and the same holds at each unit.
- **The tests of the store adapters no longer fail by chance**: a test wrote a stub program and ran it at once. The system refused the stub while another test's new process held the file open. A shell now writes each stub.

## [0.1.4] - 2026-11-21

### Added

- **A native window shows the workstation's development storage**.

  The program `suds-desktop` measures the settings' roots and shows the
  pressure against the ceiling. It groups the checkouts of one repository
  into a project. A storage map draws each project and each checkout as a
  rectangle with the area of its size on disk, and an outline lists the
  same projects. A pane shows the selected entry, and each shared store
  stands against its budget. The window runs on Linux.

- **The window plans a reclaim and removes what a person approves**.

  The button Free space plans as `suds reclaim` plans. The window lists each
  removal with its size and its evidence, and counts the kept directories
  by reason. A person leaves a removal out with its mark, and the journal
  records the approved run.

- **The window shows the journal's runs as a history**.

  The button History lists each reclaim, each store trim, and each move
  that the journal records, most recent first. A run shows its outcome, the
  bytes that it freed, and whose approval admitted it. A selected run lists
  each of its steps, and a step that did not succeed states its reason.

- **The window states a fault of the scheduled maintenance**.

  A scheduled run that fails told nobody. The window now states units that
  are absent or stale, a schedule without a run in a day, and each store
  that the last scheduled run could not trim.

- **The `schedule` command installs and checks its units**.

  With `--install`, `suds schedule` plans the installation of its units,
  and with `--apply` too it places them and starts the schedule through
  `systemctl --user` or `launchctl`. The journal records the run. With
  `--check`, the command states whether the placed units are the units of
  this release, so a unit from an earlier release no longer keeps its
  faults unseen. The command now writes for the scheduler of this system
  unless `--scheduler` names one.

### Changed

- **The native application takes the name `suds-desktop`**: its crate moves to `apps/desktop`, and its tasks are `task desktop`, `task desktop:install`, and `task desktop:check`. The web-view application becomes `suds-webview` in `apps/webview`, with the tasks `task webview` and `task webview:check`.

- **The desktop application states each time in the system's time zone**: the reading's time, a run's start, and each date read as the person's clock reads them, where they read in UTC before.

- **The desktop application answers the pointer**: the pointer becomes a hand above each button, each row, and each rectangle of the storage map, and the map outlines the rectangle beneath the pointer.

- **The desktop application lists its notices behind a chip**: a chip beside the pressure's level counts the notices of a reading and lists them on a click. The notices stood in a card above the storage before, and each line moved the storage down.

- **The desktop application groups the shared stores**: the stores' pane shows one row for each ecosystem, for each tool, or for each class of storage. An ecosystem is a language, such as Rust for Cargo and sccache. A row states the group's size and the count of its stores over budget, and a click lists the group's stores.

- **The desktop application opens with its last reading**: a measurement of a large workspace takes most of a minute, and the window showed no figure before it ended. The window now saves each whole reading in the state directory and opens with the saved one while it measures again.

### Fixed

- **The desktop application's spinner draws a smooth ring**: the toolkit's spinner drew its arc in steps at the window's size, so the window now draws a ring of its own with round ends.

- **A scheduled run finds the program of each store**.

  A scheduler gives each unit a short search path, and a version manager
  installs a program such as `pnpm` in another directory. Each scheduled
  run therefore failed to trim the program's store. The units of
  `suds schedule` now carry the search path of the shell that ran the
  command. A run also takes a program that the search path lacks from the
  directory of a version manager, so a unit of an earlier release trims
  its stores too.

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

[Unreleased]: https://github.com/pithecene/suds/compare/v0.1.10...HEAD
[0.1.10]: https://github.com/pithecene/suds/compare/v0.1.9...v0.1.10
[0.1.9]: https://github.com/pithecene/suds/compare/v0.1.8...v0.1.9
[0.1.8]: https://github.com/pithecene/suds/compare/v0.1.7...v0.1.8
[0.1.7]: https://github.com/pithecene/suds/compare/v0.1.6...v0.1.7
[0.1.6]: https://github.com/pithecene/suds/compare/v0.1.5...v0.1.6
[0.1.5]: https://github.com/pithecene/suds/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/pithecene/suds/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/pithecene/suds/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/pithecene/suds/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/pithecene/suds/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/pithecene/suds/releases/tag/v0.1.0

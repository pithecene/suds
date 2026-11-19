# Suds

Suds is a local-first desktop and command-line environment manager for
developer workstations. It treats files, repositories, worktrees,
toolchains, configuration, and agent activity as parts of one environment.
People use Suds through its desktop application, and automation calls the
same `suds-core` crate through the `suds` command.

## Current Release Boundary

This release measures storage and makes one change. It moves a
directory's direct children into another directory on the same
filesystem, and only through a plan that a person reviews. The `suds inspect`
command and the desktop application read filesystem metadata beneath a set
of user-owned roots. For each root, they report its storage use, its entry
counts, and its largest direct children. The command writes a terminal
table or one JSON document, and the desktop application shows the same
measurements in a window.

The `suds inspect --profile development` command finds each Git checkout
beneath the directories that the caller names. It reports each checkout,
the build output beneath it, and each shared store and toolchain, as
[Development Profile](#development-profile) describes.

The `suds reclaim` command removes build output that no process uses, and
only with `--apply`, as [Reclamation](#reclamation) describes. A build
regenerates that output, so the removal is permanent.

The `suds stores` command holds each shared store to a budget, and only
with `--apply`, as [Store Budgets](#store-budgets) describes. Each store's
own program trims it, and Suds removes no entry of a store itself.

Beyond those two commands, the release deletes, quarantines, copies,
rewrites, installs, and repairs nothing. It calls Git, on macOS `lsof`, and
each store's own program through typed adapters, and no other program. Between runs it keeps the
operation journal that [Moves](#moves) describes, and the registry and the
pressure reading that [Clients and Schedule](#clients-and-schedule)
describes. [AGENTS.md](AGENTS.md) states the rules that
bind any operation that changes a file: an inspectable plan, stated
preconditions, and a journal entry. In Suds, optimization means a measured
reduction in storage use, structural friction, or operational residue.
Suds makes no claim about machine performance.

## Default Roots on macOS

Without `--root`, `suds inspect` on macOS reads these roots in the user's
home directory. A root that does not exist appears as `absent`, and an
absent root is no error.

| Identifier | Kind | Path |
| :--- | :--- | :--- |
| `macos.user-caches` | `cache` | `~/Library/Caches` |
| `macos.user-logs` | `logs` | `~/Library/Logs` |
| `macos.xcode-derived-data` | `generated` | `~/Library/Developer/Xcode/DerivedData` |
| `macos.downloads` | `inbox` | `~/Downloads` |
| `macos.trash` | `trash` | `~/.Trash` |

A kind names a root's usual contents. It grants no authority to remove
anything.

The profile holds no system-owned path, such as `/System`, `/Library`, or
`/private`, and no other user's home directory. It also leaves
`~/Library/Application Support`, `~/Library/Containers`, Xcode archives,
simulator data, browser profiles, mail and message databases, and
synchronized documents unread.

On macOS, the system's privacy controls protect some locations, including
`~/.Trash` and parts of `~/Library/Caches`. Unless the terminal holds
Full Disk Access, Suds reports such a root as `unreadable` or `partial` and
lists each refusal in the report.

## Linux

On Linux, the storage profile requires each root as an argument, as in
`suds inspect --root ~/.cache`. It has no default roots there, so
`suds inspect` without `--root` exits with status 2. The development
profile needs no root on either system.

## Desktop Application

The desktop application runs on macOS. Its window lists the default roots
and each folder that the user adds through the native folder picker. The
application inspects them on a background thread and shows each root's result as soon
as that root's scan ends, and the user can cancel a scan in progress. A
cancelled root's result lists only the children that the scan finished
before the cancellation, and its status is `partial`. For
each root, the window shows the sizes, the counts, the status, the ranked
direct children, the skipped mount points, and the traversal errors.

In the Downloads location, the user can select direct children and choose
**Move…**. The native folder picker asks for the destination, and a sheet
shows the exact plan before anything moves. The sheet lists each move's
outcome after the user applies the plan and offers to undo the run. The
**History** sheet lists every run in the journal, with an undo for each run
whose moves remain applied. The sheet first settles each move that an
interruption left without an outcome.

The application uses [Tauri](https://tauri.app/). The system's web view
renders the window, and a Rust process holds every path. The web view may
call eight application commands: list the locations, add a folder, start an
inspection, cancel an inspection, build a move plan, apply a plan, list the
journal, and undo a run. It receives no filesystem, shell, or network
permission. The web view names each location, plan, and run by an opaque
identifier, and each selected entry by its position in an inspection's
result, rather than by a path. The file `apps/desktop/src-tauri/capabilities/main.json`
grants these commands.

## Installation for Contributors

Contributors build Suds from source. The [mise](https://mise.jdx.dev/)
version manager installs the pinned Rust toolchain, Node.js, pnpm,
[Task](https://taskfile.dev/), and yamlfmt:

```sh
mise install
task build
./target/release/suds inspect
task desktop            # the desktop application, on macOS
```

The repository is a Cargo workspace. The `crates/suds-core` crate holds the
inspection, the planner, the executor, and the journal, `apps/cli` holds the `suds` command, and `apps/desktop` holds
the desktop application. Neither interface invokes the other.

## Usage

```sh
suds inspect                            # the default profile, as a table
suds inspect --format json              # one JSON document on standard output
suds inspect --root PATH                # one root in place of the profile
suds inspect --root PATH --root PATH    # several roots, in the order given
suds inspect --top 20                   # list 20 children of each root in the table
```

Explicit roots replace the default profile. Suds resolves a relative root
against the working directory and shows each path in absolute, normalized
form. The `--top` option limits only the number of children in the table,
and the JSON document
lists every direct child.

The table lists each root's allocated and logical sizes in binary units
(KiB, MiB, GiB), its counts of files, directories, and symbolic links, its
status, and the duration of its scan. Each root's largest direct
children follow, ranked by allocated size, then by logical size, then by
name. The table escapes each control character in a name, a path, or an
error message, so a file's name cannot rewrite the terminal. The JSON
document keeps every name unchanged.

## Development Profile

```sh
suds inspect --profile development --root ~/workspace
suds inspect --profile development --root ~/workspace --format json
suds inspect --profile development      # the stores and toolchains alone
```

Each `--root` names a directory beneath which Suds looks for checkouts. The
profile works on macOS and on Linux. It writes a terminal table or one
`suds.development/v1` document, and it changes nothing.

Each class rests on proof from its program's own layout, and a name alone
proves none:

| Class | Proof |
| :--- | :--- |
| Main checkout | A directory whose `.git` is a directory |
| Linked checkout | A directory whose `.git` file names an entry of a main checkout's `worktrees` directory |
| Cargo build state | A `target` directory that holds `CACHEDIR.TAG`, beside a `Cargo.toml` |
| Node build state | A `node_modules` directory beside a `package.json` or a `pnpm-workspace.yaml`, or one that holds the state file of pnpm or npm |
| CMake build state | A directory that holds `CMakeCache.txt` |
| vcpkg build state | A `vcpkg_installed` directory beside a `vcpkg.json` |
| vcpkg checkout | A directory that holds `.vcpkg-root` |

Suds claims build state only beneath a checkout, so build output that no
checkout owns stays unclassified. A submodule stays a part of its parent
checkout. A checkout's own size leaves out its build state and each
checkout beneath it.

The profile then measures each shared store and toolchain directory at the
location that its program documents: the program's own variable first, and
its default beneath the home directory otherwise.

| Identifier | Variable | Default on Linux |
| :--- | :--- | :--- |
| `go.build-cache` | `GOCACHE` | `~/.cache/go-build` |
| `go.module-cache` | `GOMODCACHE`, `GOPATH` | `~/go/pkg/mod` |
| `cargo.registry` | `CARGO_HOME` | `~/.cargo/registry` |
| `pnpm.store` | `PNPM_HOME`, `XDG_DATA_HOME` | `~/.local/share/pnpm/store` |
| `sccache.cache` | `SCCACHE_DIR` | `~/.cache/sccache` |
| `ccache.cache` | `CCACHE_DIR` | `~/.cache/ccache` |
| `vcpkg.binary-cache` | `VCPKG_DEFAULT_BINARY_CACHE` | `~/.cache/vcpkg/archives` |
| `mise.installs` | `MISE_DATA_DIR` | `~/.local/share/mise/installs` |

On macOS, the cache directory is `~/Library/Caches`, pnpm's store lies in
`~/Library/pnpm/store`, and sccache's cache lies in
`~/Library/Caches/Mozilla.sccache`. A store is `present` when its directory
exists, `absent` when none exists, and `unrecognized` when a directory
exists without the entry that its program writes there. A store is
`unreadable` when Suds cannot examine its location, or when a symbolic link
stands on its path beneath the home directory. Suds follows no such link,
and the report's errors name the path.

A root may be a mount point, and Suds enters no mount point beneath a root.
When one root lies beneath another, Suds inspects the outer root alone, so
no checkout counts twice.

A file with several links counts once, toward the first class that reaches
it. The checkouts come before the stores, so the checkouts' sizes sum to
the figure that `du` reports for the same directories.

The profile reads the content of one file in each linked checkout. The
checkout's `.git` file holds one line that names its repository, and Suds
reads that line.

## Reclamation

```sh
suds reclaim --root ~/workspace                 # the plan, with nothing changed
suds reclaim --root ~/workspace --apply         # remove what the plan names
suds reclaim --root ~/workspace --checkouts     # consider linked checkouts too
suds reclaim --root ~/workspace --idle-minutes 240
```

The command inspects the [development profile](#development-profile) of
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
- A directory beneath it holds a Git repository.
- A mount point lies beneath it, or Suds could not read an entry there.

With `--checkouts`, the plan also considers each linked checkout, under
the same two protections as build state. Suds has Git remove one only when
Git reports no changed, staged, or untracked file there. A remote-tracking
branch must also hold every commit of its head. The branch and its commits
stay in the repository. Suds never names a main checkout.

With `--apply`, Suds records the plan in the journal and judges each
candidate again immediately before its removal. The directory must be the
entry that the plan measured, with no symbolic link in its path. Suds then
observes the processes, measures the directory, and asks Git anew, and it
applies every rule of the plan to what it finds. One refusal leaves the other candidates to proceed. The journal records the
plan, the authority for the run, and each removal or refusal, and
`suds journal` lists the run with its removals. A removal has no undo, because a build regenerates build state and the remote holds a
removed checkout's work.

## Store Budgets

```sh
suds stores                                  # the plan, with nothing changed
suds stores --apply                          # run each trim that the plan names
suds stores --budget go.build-cache=20G --budget docker.images=40G
```

The command measures each store of the
[development profile](#development-profile) and asks Docker for its own
usage. A store that exceeds its budget takes the trim of its own program.
Without `--apply` the command prints the plan, with each command that it
would run, and changes nothing.

| Store | Default budget | Trim | What the trim removes |
| :--- | :--- | :--- | :--- |
| `go.build-cache` | 10 GiB | `go clean -cache` | Every cached build output |
| `go.module-cache` | 10 GiB | `go clean -modcache` | Every downloaded module |
| `pnpm.store` | 10 GiB | `pnpm store prune --store-dir DIR` | Each package that no project references |
| `ccache.cache` | 5 GiB | `ccache --trim-dir DIR --trim-max-size SIZE` | The least recently used results beyond the budget |
| `mise.installs` | 20 GiB | `mise prune --yes` | Each tool version that no tracked configuration names |
| `docker.images` | 20 GiB | `docker image prune --all --force` | Each image that no container uses |
| `docker.build-cache` | 10 GiB | `docker builder prune --force` with the builder's size option | The build cache beyond the budget |
| `docker.anonymous-volumes` | 1 GiB | `docker volume prune --force` | Each anonymous volume that no container uses |

Each command names the store that Suds measured. The commands of Go and
mise receive it in `GOCACHE`, `GOMODCACHE`, or `MISE_DATA_DIR`, and the
commands of pnpm and ccache receive it as an argument. Each command runs in
the root directory, so no project's configuration redirects it to another
store. Docker's own builder bounds its cache with `--keep-storage`, and the
buildx builder bounds it with `--max-used-space`, so Suds reads the
command's help to choose.

A program's trim decides what leaves. Go offers no partial trim, so a cache
beyond its budget leaves whole, and the next build fills it again. The
trims of pnpm, mise, and Docker's images remove what nothing references,
so a store can stay beyond its budget after its trim.

Suds reports these stores and trims none of them:

- `cargo.registry`, `sccache.cache`, and `vcpkg.binary-cache` have no
  budget here. Cargo and sccache bound their own stores, and vcpkg offers
  no trim.
- A store whose directory lacks its program's marker has no proven owner.
- A named Docker volume that no container uses may hold data that nothing
  regenerates. The plan lists each one as `awaits_person`, and Suds removes
  none.

A store whose newest entry changed within the idle time stays, because a
build may write it. The journal records each trim with the store's size
before and after it, and `suds journal` lists the run.

## Clients and Schedule

A client is a program that works in checkouts, such as Bosun or an agent
that a person drives. Each client uses the same commands.

```sh
suds register --checkout DIR --client bosun --attempt wf-12
suds release --checkout DIR --client bosun --attempt wf-12
suds pressure --format json
suds maintain
suds schedule --scheduler systemd       # or launchd
```

The `register` command records that a client holds a checkout, and
`release` records that the client's work there ended. Both append one
`suds.registry/v1` record to `registry.jsonl` in the state directory. A
registration adds knowledge and no permission:

- Suds removes nothing beneath a checkout that a client registered and has
  not released, and it reads the registry anew before each removal.
- A released linked checkout enters each reclaim plan, before every other
  checkout. Every other rule of [Reclamation](#reclamation) still decides
  whether it leaves.

The `maintain` command is the run that a schedule starts. It inspects the
roots that the settings name, and it records one `suds.pressure/v1` reading
in the state directory. The usage counts each root, each checkout with its
build state, and each store. Docker keeps its own storage, which the usage
leaves out. The `pressure` command prints the last reading and measures
nothing, so a client can read it before each start.

| Level | Usage | A client's response |
| :--- | :--- | :--- |
| `normal` | Below 80 percent of the ceiling | Start work |
| `soft` | From 80 percent | Reclaim before more work |
| `hard` | From 95 percent | Start no work |

The level rises at once. It falls only after the usage has stayed lower for
five minutes, so a reading near a threshold holds one level. The ceiling is
the greater of half of the disk and 100 GiB unless the settings name
another. Each reading says in its `enforced` field whether a filesystem
limit enforces the ceiling, as [Arena](#arena) describes.

The `maintain` command changes nothing unless the settings hold the
owner's standing approval. With it, the command reclaims build state and
trims the stores as `suds reclaim --apply` and `suds stores --apply` do,
and the journal records the standing approval as each run's authority.

Suds reads its settings from the file `settings.json`. On Linux the file
lies in `~/.config/suds`, and on macOS it lies in
`~/Library/Application Support/Suds`. No command writes the file, and a
file that Suds cannot parse stops the run.

```json
{
  "roots": ["/home/me/workspace"],
  "standing_approval": true,
  "idle_minutes": 60,
  "ceiling_bytes": null,
  "tolerate_unreadable_processes": false,
  "budgets": { "docker.images": 42949672960 }
}
```

The `schedule` command prints the units that run `suds maintain` every 30
minutes: a service and a timer for systemd, or an agent for launchd. Suds
installs nothing, so a person or an installer places the units.

## Arena

```sh
suds arena                       # the first root of the settings
suds arena --root ~/workspace --format json
```

The arena is the development storage under its ceiling. The command reports
the ceiling, the size and the free space of the root's filesystem, and
whether a filesystem enforces the ceiling. It writes a table or one
`suds.arena/v1` document.

A filesystem enforces the ceiling when the root lies on a filesystem of its
own that is no larger than the ceiling: an APFS volume under a quota, or a
partition of that size. Every other root and every store must lie on that
same filesystem, and the report lists each one that lies outside it. Suds
audits no root through a symbolic link. Development storage then cannot fill the system's
volume, whatever fills the arena. Suds reads what any user can read, so it
cannot see a quota group of btrfs or a project quota of XFS, and it reports
such a root as not enforced.

Where nothing enforces the ceiling, the command prints the commands that
would enforce it. APFS takes `diskutil apfs addVolume` with a quota. A btrfs
filesystem takes a subvolume under a quota group, and XFS takes a project
quota. Most of the commands need an administrator. Suds runs none of them and requests no privilege. Each command quotes its
paths for a shell. The plan
creates an empty location under the limit and moves nothing.

Until a filesystem enforces the ceiling, Suds holds it by measurement
alone, through the pressure reading of each maintenance run.

## JSON Output

The `--format json` option writes exactly one JSON document to standard
output and nothing else. The document carries the schema tag
`suds.inspection/v1`, the operating system and architecture, the time of
the observation, and the overall status, `complete` or `partial`. Each
root's entry carries these fields:

- `id`, `kind`, and the normalized `path`
- `status`, one of `complete`, `partial`, `absent`, or `unreadable`
- `elapsed_ms`, the duration of the root's scan
- `logical_bytes` and `allocated_bytes`
- `files`, `directories`, `symlinks`, and `others`
- `oldest_modified` and `newest_modified`, in RFC 3339 form and UTC
- `children`, every direct child with the same measurements and its type
- `skipped_mount_count` and `skipped_mounts`, the mount points that the
  scan did not enter
- `error_count` and `errors`, the traversal errors with their paths, kinds,
  and messages

The `errors` and `skipped_mounts` lists hold at most ten entries each, and
the counts beside them cover every occurrence. A root's measurement covers
every entry beneath the root, and a child's measurement covers the child
and every entry beneath it. Paths that are not valid UTF-8 appear with
replacement characters. Suds records each native path internally, so the
replacement affects only the report.

## Moves

```sh
suds plan move --from ~/Downloads/report.pdf --to ~/Documents/Research --output plan.json
suds apply plan.json        # one suds.run/v1 JSON document on standard output
suds journal                # every run, most recent first (--format json for JSON)
suds undo RUN               # return each moved entry to its original path
```

The `plan` command reads metadata and moves nothing. It writes one
`suds.plan/v1` document, which names every source and destination by its
path's bytes in hexadecimal beside a display string, and records each
entry's device, inode, type, size, and modification time. Every `--from`
entry must lie in one directory. Planning resolves both directories to
their canonical paths, and the preview shows them. The command refuses a
symbolic link or a special file, a destination that exists, a destination
inside the source, a directory that contains a mount point, and a
destination on another filesystem. It also refuses to replace an existing
output file. Where the measurement of a selected directory cannot read an
entry, the plan and its preview name that entry with the reason.

The `apply` command accepts only a plan that `suds plan` or the desktop
application built and recorded in the journal, unchanged, and each plan
runs once. Immediately before each move, Suds verifies the entry's
recorded identity, the identity and canonical path of both directories,
and that the destination is free, and it refuses an entry that holds the
journal directory. Suds then renames the entry with the
kernel's no-replace flag, so the move is atomic and overwrites nothing.
After the rename, Suds confirms that the destination holds the planned
entry. When another process swapped the source in the meantime, Suds
returns the swapped entry and refuses the move. A refused move leaves the
other moves to proceed. The `undo` command
reverses each move that remains applied, in reverse order. It refuses a move
whose entry changed after the move and a move whose original path now holds
another entry. Suds never merges, renames to avoid a conflict, copies, or
deletes.

The journal is an append-only file of `suds.journal/v1` records. On macOS
it lies in `~/Library/Application Support/Suds/journal`. Its place on Linux
is `~/.local/state/suds/journal`, and `XDG_STATE_HOME` moves it. The option
`--journal DIR` names another directory. Suds records each
move before and after its rename. When a run stops between those records,
the next `apply` or `undo` examines both paths. If the entry lies at
exactly one of them, Suds records which, and otherwise it records the move as
`unresolved` and changes nothing. Suds opens no journal file through a
symbolic link. A full disk or an interruption can cut an append short, and
the bytes that remain form no record, so a reader sets them aside and the
next writer removes them.

| Status | Meaning for `plan`, `apply`, and `undo` |
| :--- | :--- |
| `0` | Suds wrote the plan, or every move succeeded. |
| `1` | Suds refused the plan, or it refused one or more moves. |
| `2` | The invocation failed, or Suds refused the plan file, the run, or the journal before any move. |

### Known Limitation

No system call renames an entry on the condition of its inode. Another
process can therefore swap a source between Suds's last check and the
rename. Suds detects the swap and returns the swapped entry. When yet
another entry has taken the source path by then, Suds overwrites nothing,
leaves both entries in place, and records the move as `unresolved` with
what it found at each path.

## Measurements

- **Logical bytes** sum the sizes of regular files.
- **Allocated bytes** sum the 512-byte blocks that each entry occupies,
  including directories and symbolic links. On APFS, clones, compression,
  snapshots, and purgeable storage can make the space that removal would
  recover differ from this estimate.
- **Hard-linked files** contribute their bytes once for each root, through
  the first link that the scan reaches, and each link still counts as a
  file.
- **Symbolic links** count as entries, and the scan never follows them. A
  root that is itself a symbolic link, such as `/tmp` on macOS, counts as
  one link, and the scan reads nothing beyond it. A default root whose path
  beneath the home directory passes through a symbolic link is
  `unreadable`, and its `errors` name the link. The operating system
  resolves the directories above an explicit root, because the operator
  names that path.
- **Mount points** bound the scan. The scan stays on each root's filesystem
  and reports every mount point it skips. On Linux 5.8 and later, the scan
  also stops at a bind mount on the root's own device. When that check fails
  for a directory, the scan records the error and leaves the directory
  unmeasured.
- **Read failures**, such as a permission refusal or a vanished entry, make
  the root `partial` and appear in its `errors`. Suds never suppresses such
  an error.

## Exit Status

| Status | Meaning |
| :--- | :--- |
| `0` | The inspection completed. An absent root leaves this status unchanged. |
| `1` | Suds produced a report, but one or more present roots are `partial` or `unreadable`. |
| `2` | The invocation failed, no default profile exists for the platform, or Suds could not write the report. |

## Safety Properties

- Suds sends no telemetry and makes no network request.
- Suds reads metadata and directory listings, and it reads no extended
  attribute. The development profile opens the `.git` file of each linked
  checkout, and no other inspection opens a regular file.
- Suds never requests elevated privileges.
- Suds moves an entry only through a recorded plan, and it copies and
  overwrites nothing. It deletes only what a recorded reclaim plan names:
  build state that its ecosystem's layout proves, and a linked checkout
  that Git finds clean and that a remote holds.
- The default profile holds no system-owned path. For an explicit root,
  Suds reads only the path that the caller names, with the caller's own
  permissions.
- The report can reveal private file and directory names. Treat it as local
  machine data.
- The desktop application's web view loads only the application's own
  files and may call the eight application commands and nothing else. It
  never holds a path that it could pass back to name a file.

## Development

| Command | Description |
| :--- | :--- |
| `task build` | Build the release binary |
| `task test` | Run the unit and command-line tests |
| `task lint` | Check formatting, Clippy lints, and YAML formatting |
| `task format` | Format the Rust sources and YAML files (alias `task fmt`) |
| `task check` | Build, test, and lint |
| `task inspect -- ARGS` | Run the debug build of `suds inspect` with `ARGS` |
| `task desktop` | Run the desktop application in development mode |
| `task desktop:check` | Build the desktop frontend, then test and lint the desktop crate |

The `task check` command needs no network service, container, or elevated
privilege once `mise install` has run. The `task desktop:check` command
also installs the frontend's pinned packages, and it runs on macOS. [CONTRIBUTING.md](CONTRIBUTING.md)
describes the conventions of the repository.

## License

Suds is available under the [Apache License 2.0](LICENSE).

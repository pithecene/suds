# Suds

Suds is a local-first desktop and command-line environment manager for
developer workstations. It treats files, repositories, worktrees,
toolchains, configuration, and agent activity as parts of one environment.
People use Suds through its desktop application, and automation uses the
same machinery through the `suds` command.

## Current Release Boundary

This release measures storage and changes nothing. The `suds inspect`
command and the desktop application read filesystem metadata beneath a set
of user-owned roots. For each root, they report its storage use, its entry
counts, and its largest direct children. The command writes a terminal
table or one JSON document, and the desktop application shows the same
measurements in a window.

The release deletes, moves, quarantines, rewrites, installs, and repairs
nothing. It interprets no package manager's state, keeps no state between
runs, and calls no external program. A later release may organize and
maintain the workstation, and each of its operations will follow the
rules in [AGENTS.md](AGENTS.md): an inspectable plan, stated
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

On Linux, Suds requires each root as an argument, as in
`suds inspect --root ~/.cache`. Linux has no default profile, so
`suds inspect` without `--root` exits with status 2.

## Desktop Application

The desktop application runs on macOS. Its window lists the default roots
and each folder that the user adds through the native folder picker. It
inspects them on a background thread and shows each root's result as soon
as that root's scan ends, and the user can cancel a scan in progress. For
each root, the window shows the sizes, the counts, the status, the ranked
direct children, the skipped mount points, and the traversal errors.

The application uses [Tauri](https://tauri.app/). The system's web view
renders the window, and a Rust process holds every path. The web view may
call four application commands: list the locations, add a folder, start an
inspection, and cancel an inspection. It receives no filesystem, shell, or
network permission, and it names each location by an opaque identifier
rather than by a path. The file
`apps/desktop/src-tauri/capabilities/main.json` grants these commands.

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
inspection, `apps/cli` holds the `suds` command, and `apps/desktop` holds
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
replacement characters. Suds keeps each native path internally, so the
replacement affects only the report.

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
- Suds reads metadata and directory listings. It never opens a regular file
  and reads no extended attribute.
- Suds never requests elevated privileges.
- The default profile holds no system-owned path. For an explicit root,
  Suds reads only the path that the caller names, with the caller's own
  permissions.
- The report can reveal private file and directory names. Treat it as local
  machine data.
- The desktop application's web view loads only the application's own
  files, receives the four inspection commands alone, and never receives a
  path that it could pass back to name a file.

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

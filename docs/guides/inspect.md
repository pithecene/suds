# Inspection

This guide describes the `suds inspect` command: its options, its two profiles, its JSON document, its measurements, and its exit statuses.

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
| Go cache in a checkout | A directory that holds `README`, `trim.txt`, `00`, and `ff`, or one that holds `cache/download` |
| ccache cache in a checkout | A directory whose `0` directory holds `CACHEDIR.TAG` and `stats` |
| vcpkg checkout | A directory that holds `.vcpkg-root` |
| Virtual machine | A directory that holds Lima's `lima.yaml` beside a `diffdisk` or a `basedisk`, or one that holds a `datadisk` |

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
| `lima.machines` | `LIMA_HOME` | `~/.lima` |
| `colima.machines` | `COLIMA_HOME` | `~/.colima` |

On macOS, the cache directory is `~/Library/Caches`, pnpm's store lies in
`~/Library/pnpm/store`, and sccache's cache lies in
`~/Library/Caches/Mozilla.sccache`. A store is `present` when its directory
exists, `absent` when none exists, and `unrecognized` when a directory
exists without the entry that its program writes there. A store is
`unreadable` when Suds cannot examine its location, or when a symbolic link
stands on its path beneath the home directory. Suds follows no such link,
and the report's errors name the path.

A virtual machine's disk holds its container engine's images and volumes.
The profile counts each one toward the development storage, beside the
stores or inside a checkout. `suds stores` reports a
machine whose disk exceeds 60 GiB, and it trims none, because no build
makes a volume again. With `--root DIR`, the plan also looks beneath that
directory and reports each machine inside a checkout.

A root may be a mount point, and Suds enters no mount point beneath a root.
When one root lies beneath another, Suds inspects the outer root alone, so
no checkout counts twice.

A file with several links counts once, toward the first class that reaches
it. The checkouts come before the stores, so the checkouts' sizes sum to
the figure that `du` reports for the same directories.

The profile reads the content of one file in each linked checkout. The
checkout's `.git` file holds one line that names its repository, and Suds
reads that line.

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

## Measurements

- **Logical bytes** sum the sizes of regular files.
- **Allocated bytes** sum the 512-byte blocks that each entry occupies,
  including directories and symbolic links. On APFS, clones, compression,
  snapshots, and purgeable storage can make the space that removal would
  recover differ from this estimate.
- **Hard-linked files** contribute their bytes once for each root, through
  the first link that the scan reaches, and each link still counts as a
  file.
- **Cloned blocks** contribute their bytes once for each scan on Linux,
  through the first file that the scan reaches. Btrfs and XFS clone a
  file, and the kernel names each extent that a file shares. The scan asks
  for the extents of each file of 16 KiB or more, so a smaller clone counts
  in full. On one workstation, that floor kept 89 percent of the
  correction, and the inspection took 5.6 seconds where it took 4.4
  seconds before. A file that the scan cannot open counts in full. The
  first file's checkout holds the bytes in a report, so the removal of
  that checkout alone frees less than its figure.
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

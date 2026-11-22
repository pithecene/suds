# 0006 Seeded Build State

Status: Accepted (2026-11-21).

## Context

Decision 0002 made Suds the program that bounds and reclaims development
storage, and it named build state as derived state that a build
regenerates. Suds removes that state from an idle checkout. A client such
as Bosun then creates the next checkout, whose first build compiles each
dependency again and writes each one a second time.

Cargo is the ecosystem that shares no build state between checkouts. One
shared directory serializes the builds of parallel agents, because Cargo
locks the directory for a whole build. A compiler cache saves a build's
time and none of its size.

A filesystem that clones a file shares the file's blocks between both
copies until one of them changes. Btrfs and XFS do so on Linux, and APFS
does so on macOS. A measurement on one Linux workstation with btrfs, for
the workspace of Suds itself:

| Measure | A new checkout without a seed | With a seed |
| :--- | :--- | :--- |
| The build state at the first build's start | none | 5.4 GiB, cloned in 0.6 seconds |
| Space that the clone occupies | none | 11 MiB |
| The first build | 378 packages compiled in 100 seconds | 5 packages compiled in 23 seconds |
| Space that the first build writes | 5.4 GiB | 2.8 GiB |

The owner asked on 2026-11-21 that the worktrees of one repository share
their build output, so that Bosun's work costs less disk and less time.

## Decision

1. Suds seeds a checkout's build state. The `suds seed` command clones the
   Cargo build state of one checkout into another checkout of the same
   repository, and Cargo then decides what it compiles again.
2. A seed clones and never copies. Where the filesystem clones no file,
   the seed writes nothing.
3. A seed changes no existing entry. It clones only to a path that holds
   nothing, beside the manifest that proves a build there would use it.
4. A seed takes build state only while no build holds it. Cargo's own lock
   proves that, and Suds holds the lock in shared form during the clone.
5. A client may apply a seed without a person's inspection. Item 3 of
   decision 0002 lets the owner's standing approval admit a removal of
   derived state. A seed adds derived state, changes no file, and a
   removal under that approval reverses it, so it needs no approval of
   its own. The journal records each seed with its client.

## Alternatives

- **One shared build directory.** Cargo's lock would run the builds of
  parallel agents one at a time.
- **A compiler cache alone.** It saves time and no space, and it serves
  neither a procedural macro nor an incremental build.
- **A copy where no clone exists.** A copy doubles the space, which is the
  growth that Suds exists to bound.
- **The client clones by itself.** Each client would then hold the
  knowledge of the layouts, of the locks, and of the filesystems, which
  Suds already holds for every client.
- **Cargo's shared cache of build units.** The Rust project plans it, and
  no release holds it yet. A seed leaves once Cargo shares by itself.

## Consequences

- A measurement counts a cloned file in each checkout that holds it, as
  `du` counts it. The usage that Suds reports therefore exceeds the space
  that the disk holds for those files, and the pressure errs toward an
  early warning.
- A clone keeps the time of each file. Cargo compiles a workspace's own
  packages again, because the new checkout's sources are newer, and it
  takes each dependency from the clone.
- A seed of another lock file's build state stays correct and saves less,
  so a plan prefers the checkout whose lock file equals the destination's.
- The first release seeds Cargo alone. A build directory of CMake names
  its source directory by an absolute path, and pnpm shares its store by
  links already.

## Open Questions

- Whether a measurement should count a shared block once. The filesystem
  can state it, at the cost of one more call for each file.
- Whether `node_modules` gains from a seed where a package manager copies
  each package.

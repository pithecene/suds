# Changelog

This file records every notable change to Suds.

This changelog follows the format of [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and Suds adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

---

## [Unreleased]

### Added

- **`suds reclaim` removes build output that no process uses**: the command plans the removal of each checkout's build state and prints the evidence for each decision. With `--apply`, it verifies each candidate again, removes it, and records each removal in the journal. It keeps a directory while a process works in its checkout or while an entry in it changed within the idle time. With `--checkouts`, it has Git remove a clean linked checkout whose commits a remote holds. It never names a main checkout.
- **`suds inspect --profile development` classifies development storage**: the command finds each main and linked Git checkout beneath the named directories and measures the build output of Cargo, Node, CMake, and vcpkg beneath each one. It also measures the shared stores of Go, Cargo, pnpm, sccache, ccache, and vcpkg, and the toolchains of mise. Each class rests on proof from its program's own layout. The command writes a table or one `suds.development/v1` document on macOS and on Linux, and it changes nothing.
- **Suds moves Downloads entries through reversible plans**: `suds plan move` writes a `suds.plan/v1` plan without moving anything, `suds apply` runs a plan that Suds recorded, `suds journal` lists the runs, and `suds undo` returns each moved entry. Each move verifies its preconditions and renames once within one filesystem, without overwriting. An append-only journal records every step, and the next run reconciles an interrupted move. The desktop application selects Downloads entries, shows the plan in a sheet, applies the plan, and undoes the run.
- **The Suds desktop application displays storage measurements in a window on macOS**: it lists the default roots and each folder that the operator picks, and it scans them on a background thread. It displays each root's measurements as that root's scan ends, and it cancels a scan on request. Its web view may call four application commands and holds no filesystem, shell, or network permission.
- **`suds inspect` measures macOS userland storage without changing it**: the command reads metadata beneath the user's caches, logs, Xcode derived data, downloads, and trash. It reports each root's logical and allocated sizes, entry counts, range of modification times, and largest direct children. On macOS and Linux, the `--root` option replaces these default roots with roots that the operator names. Given the `--format json` option, the command writes one `suds.inspection/v1` document. The scan follows no symbolic link, crosses no mount point, counts hard-linked storage once, and reports every traversal error.

### Changed

- **A shared `suds-core` crate holds the inspection code**: the `suds` command and the desktop application each call it. The scan records native paths and names. The `suds inspect` command's JSON report, terminal table, and exit statuses remain unchanged.

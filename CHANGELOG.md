# Changelog

This file records every notable change to Suds.

This changelog follows the format of [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and Suds adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

---

## [Unreleased]

### Added

- **The Suds desktop application displays storage measurements in a window on macOS**: it lists the default roots and each folder that the operator picks, and it scans them on a background thread. It displays each root's measurements as that root's scan ends, and it cancels a scan on request. Its web view may call four application commands and holds no filesystem, shell, or network permission.
- **`suds inspect` measures macOS userland storage without changing it**: the command reads metadata beneath the user's caches, logs, Xcode derived data, downloads, and trash. It reports each root's logical and allocated sizes, entry counts, range of modification times, and largest direct children. On macOS and Linux, the `--root` option replaces these default roots with roots that the operator names. Given the `--format json` option, the command writes one `suds.inspection/v1` document. The scan follows no symbolic link, crosses no mount point, counts hard-linked storage once, and reports every traversal error.

### Changed

- **A shared `suds-core` crate holds the inspection code**: the `suds` command and the desktop application each call it. The scan records native paths and names. The `suds inspect` command's JSON report, terminal table, and exit statuses remain unchanged.

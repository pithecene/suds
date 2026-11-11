# Suds Guardrails

This file states the guardrails for work on Suds. It encodes discipline and
constraints, not architecture.

## Development Principles

- Prefer direct control flow and descriptive names to compact idioms, so
  that a reviewer can verify each change.
- Prefer explicit behavior to hidden mechanisms.
- Implement only the behavior that the maintainer requests. Invent no
  feature.
- Make no performance claim without a measurement.
- Introduce no asynchronous design without evidence that a workload needs
  one.

## Boundary

Suds owns the semantics of workstation maintenance and reconciliation. The
current release observes filesystem metadata and nothing more.

- Add no operation that mutates the filesystem: no cleanup, move,
  quarantine, rewrite, installation, or repair.
- Request no elevated privilege and never invoke `sudo`.
- Add no system-owned path to a default profile. When a caller names an
  explicit root, Suds reads only that root.
- Make no network request.
- Invoke no external program. In particular, never call `du`, `find`,
  `brew`, a package manager, or a cleanup program.
- Read no file contents and no extended attribute.
- Follow no symbolic link.
- Suppress no permission error or traversal error. Record each one in the
  report.

## Crate Structure

- Build no broad provider framework and no plugin interface.
- Write concrete types before traits. Introduce a trait only when two
  implementations exist.
- Retain a single crate until a proposed second crate would serve a
  consumer independent of the first.
- Keep the binary free of a public library interface.

## Source Code Conventions

- Write comments that explain the reason for code, not its mechanics.
- Write no `unsafe` code, because the crate forbids it.
- Pin each dependency to an exact version, and state the reason for each
  new dependency in its pull request.
- Run `task check` before each commit.

## Commits

Commit messages and pull request titles follow Conventional Commits, with a gitmoji
after the scope, as in `feat(inspect): ✨ report skipped mount points`.

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

Suds owns the semantics of workstation organization and maintenance. The
current release observes filesystem metadata and modifies nothing. These
rules hold for every release:

- Mutate nothing outside an explicit plan that a person can inspect before
  it applies. Each operation states its preconditions and its evidence, and
  each writes a journal entry. The current release mutates nothing.
- Delete nothing permanently by default, and remove nothing because of its
  age alone.
- Let no proposal that a model generates apply itself.
- Request no elevated privilege and never invoke `sudo`.
- Execute no shell string. Call an external program only through a typed
  adapter for the tool that owns the state. The current release calls none.
- Read file contents only through an explicit capability or an action of
  the user. Background inspection reads metadata alone.
- Make no network request outside a remote connector that the user
  authorizes separately. The current release makes none.
- Follow no symbolic link by accident. Add no system-owned path to a
  default profile, and act on no path outside an operation's declared
  scope.
- Suppress no permission error or traversal error. Record each one in the
  result.

## Crate Structure

- The `suds-core` crate holds the model and the inspection that the
  interfaces share. The command line and the desktop application each call
  `suds-core`, and neither invokes the other.
- The core serves this workspace and publishes no crate.
- Split a crate only when a second consumer exists.
- Build no provider framework. Add an adapter when an integration arrives.
- Write concrete types before traits. Introduce a trait only when two
  implementations exist.
- Keep each file's native path as its identity. A displayed string never
  names a file in an operation.
- Grant the desktop web view application commands alone, with no generic
  filesystem, shell, or network permission. Review the capability file as
  code.

## Source Code Conventions

- Write comments that explain the reason for code, not its mechanics.
- Write no `unsafe` code, because the crate forbids it.
- Pin each dependency to an exact version, and state the reason for each
  new dependency in its pull request.
- Run `task check` before each commit, and `task desktop:check` before a
  commit that changes the desktop application.

## Commits

Commit messages and pull request titles follow Conventional Commits, with a gitmoji
after the scope, as in `feat(inspect): ✨ report skipped mount points`.

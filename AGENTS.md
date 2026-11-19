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

Suds owns the semantics of workstation organization and maintenance, and
it allocates and reclaims the workstation's development storage. These
rules hold for every release:

- Mutate nothing outside an explicit plan. Each operation states its
  preconditions and its evidence, and Suds writes a journal entry for each
  operation.
- Apply a plan only after a person inspects it. The one exception is a plan
  that removes derived state inside the development arena, which the
  owner's standing approval admits
  ([0002](docs/decisions/0002-development-storage-allocation.md)).
- Treat state as derived only on proof from its ecosystem's own layout.
  An uncommitted change, an unpushed commit, and a main checkout never
  count as derived state.
- Delete nothing permanently by default. Derived state inside the arena is
  the exception, because a build regenerates it.
- Remove nothing because of its age alone.
- Let no proposal that a model generates apply itself.
- Request no elevated privilege and never invoke `sudo`. Where a
  filesystem limit needs a privileged command, plan the command and leave
  its execution to a person.
- Execute no shell string. Call an external program only through a typed
  adapter for the program that owns the state. The current release calls
  Git, which owns each checkout, and on macOS `lsof`, which reads the
  process table. It calls Go, pnpm, ccache, mise, and Docker, and each one
  trims only its own store.
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

- The `suds-core` crate holds the model and the inspection code that the
  command line and the desktop application share. Each of the two calls
  `suds-core`, and neither invokes the other.
- Publish no crate from this workspace, `suds-core` included.
- Split a crate only when a second consumer exists.
- Build no provider framework. Add an adapter only when a requested
  change integrates the program that the adapter serves.
- Write concrete types before traits. Introduce a trait only when two
  implementations exist.
- Keep each file's native path as its identity. A displayed string never
  names a file in an operation.
- Let the desktop web view call application commands and nothing else: no
  generic filesystem, shell, or network permission. Review the capability
  file as code.

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

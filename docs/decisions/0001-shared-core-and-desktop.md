# 0001 Shared Core and Desktop

Status: Accepted (2026-11-11).

## Context

Suds began as one binary whose `suds inspect` command owned the scanner,
the report, and the terminal output. Two consumers now need the
inspection: the command line, which automation drives, and a desktop
application, which people use. An operation that moves or organizes files
must name each file by its native path rather than by a displayed string.

## Decision

1. The repository becomes a Cargo workspace. The `suds-core` crate holds
   the model, the platform profiles, and the inspection. The `suds` binary
   in `apps/cli` and the desktop application in `apps/desktop` each call
   `suds-core`, and neither invokes the other. No crate of the workspace
   reaches a registry.
2. The scan records each path and name in its native form. The
   `suds.inspection/v1` report converts them to display strings and
   remains one compatibility surface rather than the internal model.
3. The desktop application uses Tauri 2, with a Rust backend and a React
   frontend in the system's web view. The web view names each location by
   an opaque identifier and calls only application commands that a
   checked-in capability file grants. It receives no generic filesystem,
   shell, or network permission.
4. macOS is the primary target of the desktop application. The Linux CI
   job builds and tests the core and the command line. A Linux build of the
   desktop application waits until its window and commands stabilize.

## Alternatives

- **A native shell for each platform**, such as SwiftUI with AppKit on
  macOS and GTK on Linux, would integrate most deeply with each system but
  would require two frontends. Suds can add native bridges, such as Quick
  Look and the Finder's reveal, where a web view falls short of a native
  control.
- **A desktop application that runs `suds inspect`** and parses its output
  would couple the interfaces through a text contract and would lose each
  native path.

## Consequences

- The workspace adds Node.js and pnpm to the pinned programs, and the macOS CI
  job builds the frontend before it compiles the desktop crate.
- A plain `cargo` command builds only the core and the command line, so a
  Linux machine needs no web view libraries.
- The window adopts Pithecene's shared design values once Pithecene
  publishes their package. Until then it uses a neutral system palette and
  copies none of those values by hand.

# 0003 Native Interface

Status: Proposed (2026-11-19).

## Context

Decision 0001 chose Tauri 2 for the desktop application, with a React
frontend in the system's web view. The window shows one table for each
root: its storage use, its entry counts, and its largest children.

Decision 0002 gave Suds a second subject. Suds now classifies each
checkout and its build state, measures each store against a budget, reads
the pressure against a ceiling, keeps a registry of clients, and journals
each removal. The window shows none of these, so a person reads the
workstation's state from the command line alone.

The owner asks for a modern interface that is native and that shows the
workstation as a whole.

## Decision

1. The interface's first subject is the workstation's development storage.
   Its first view shows the pressure against the ceiling, each checkout
   with its build state, each store against its budget, each client's
   registrations, and the journal. The table of a root's children becomes
   a second view.
2. The window is native. It draws without a web view, and it calls
   `suds-core` in process. This item replaces item 3 of decision 0001, and
   the other items of that decision stand.
3. One interface in Rust serves macOS and Linux. A workstation of either
   system fills in the same way, and one frontend keeps the two windows
   alike.
4. A trial chooses the toolkit before this decision becomes accepted. The
   trial builds the first view in each candidate. It compares a screen
   reader's access, the system's own menus and dialogs, and a package
   without web view libraries. It also times each candidate's drawing of
   100,000 directories.
5. The interface removes nothing by a path of its own. Each change runs
   through the plan, the approval, and the journal that the command line
   uses.

## Alternatives

- **A new frontend in the web view.** Tauri stays, and the work is the
  least. The window still depends on the system's web view, and the owner
  asks for a native one.
- **A native shell for each system**, such as SwiftUI on macOS and GTK on
  Linux. Each window would fit its system most closely. Two frontends
  would then need the same change twice, and the Swift one would reach the
  core through a foreign interface.
- **The command line alone.** A table of text shows a list well and a
  whole poorly. The proportions of a full disk need a drawing.

## Consequences

- Node.js and pnpm leave the pinned programs once the web frontend leaves.
- Linux gains the window, which decision 0001 deferred.
- The capability file of the web view leaves with it. Code in the window
  then shares a process with the core, so the core's own interface must
  refuse what the capability file refused.
- The frontend is new work, and the React components do not carry over.
- The window still adopts Pithecene's shared design values once Pithecene
  publishes them.

## Open Questions

- The owner's word native may mean the system's own controls. Item 3 then
  yields to a shell for each system, and the second alternative becomes
  the decision.
- The candidates of the trial are open. Slint, iced, egui, and GPUI each
  draw without a web view.

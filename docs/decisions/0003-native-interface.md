# 0003 Native Interface

Status: Accepted (2026-11-20).

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
4. The toolkit is Slint. A trial built the first view in Slint, GPUI, and
   egui, and the section Trial below holds its results. Suds takes Slint
   under its royalty-free licence for desktop applications, so the window
   shows Slint's attribution in an About dialog.
5. The interface removes nothing by a path of its own. Each change runs
   through the plan, the approval, and the journal that the command line
   uses.

## Trial

The trial of 2026-11-20 built the first view in each candidate on a Linux
workstation under Wayland. Each build drew one saved development report of
425 checkouts and then the same rows repeated to 100,000.

| Measure | Slint 1.19.0 | GPUI 0.2.2 | egui 0.37.0 |
| :--- | :--- | :--- | :--- |
| Median frame, 425 rows | 17.3 ms | 16.7 ms | 16.9 ms |
| Median frame, 100,000 rows | 17.4 ms | 16.7 ms | 16.9 ms |
| Nodes on the accessibility bus | 112 | 0 | 83 |
| Roles on the accessibility bus | A list box, its list items, and labels | None | Labels alone |
| Menu bar | A declared menu bar | The application's menu | None of its own |
| Builds as published | Yes | No | Yes |
| Release binary | 32.1 MB | 23.7 MB | 29.2 MB |

Every candidate drew each frame within the display's refresh interval at
both lengths, so drawing speed separates none of them. The accessibility
bus is AT-SPI, read while the session reported a running screen reader.
GPUI's published crate holds no accessibility layer. It also needed a
corrected copy of one dependency before it compiled, and the trial built it
for Wayland alone.

Slint is the only candidate whose list reached the bus as a list. It also
declares its menu bar in the interface's own language, which a system that
owns the menu bar can take over.

The trial ran on no macOS workstation, opened no file dialog, and ran no
screen reader's speech. Each candidate drew with its default renderer.

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
- The trial's macOS half is open. Slint's menu bar, its dialogs, and a
  screen reader's access on macOS have no measurement yet.

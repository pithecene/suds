# 0005 macOS Window

Status: Proposed (2026-11-21).

## Context

Decision 0003 chose one interface in Rust for macOS and Linux, drawn with
Slint. Its open questions named a second reading of the owner's word
native: the system's own controls. That reading makes a shell for each
system the decision.

On 2026-11-21 the owner stated that reading. Suds has a window, and the
owner asks for a macOS window to match it, built with Swift and the
system's own interface kit.

The trial of decision 0003 ran on no macOS workstation. It measured no
menu bar, no dialog, and no screen reader there.

## Decision

1. On macOS, Suds's window is a native application in Swift. It draws
   with the system's own controls. This item replaces item 3 of decision
   0003 on macOS.
2. On Linux, the window stays the Slint interface of decision 0003.
3. The macOS window shows the subjects of item 1 of decision 0003. It
   matches the Slint window's views, so a person who uses both systems
   meets the same workstation.
4. The macOS window removes nothing by a path of its own. Item 5 of
   decision 0003 stands, and each change runs through the plan, the
   approval, and the journal.
5. `suds-core` stays the one implementation of each measurement and each
   rule. The Swift window computes none of them itself.

## Alternatives

- **Slint on macOS too.** One frontend serves both systems, and each
  change lands once. The window then draws its own controls on macOS, and
  the owner asks for the system's own.
- **SwiftUI on macOS and GTK on Linux.** Each system receives its own
  controls. Linux would lose a working window for a new one, and the owner
  asked only about macOS.
- **The web view of decision 0001.** It reuses the React components. The
  owner asked for a native window, and decision 0003 removed the web view
  for that reason.

## Consequences

- A change to a view lands twice, once in Swift and once in Slint.
- The repository pins a Swift toolchain, and its check gains a job on
  macOS.
- The macOS window reaches `suds-core` across a language boundary, which
  the Slint window does not cross.
- The Slint window's attribution dialog stays on Linux alone.

## Open Questions

- **The bridge to the core.** The window could call `suds-core` through
  generated bindings in process, or read the command line's JSON output in
  a separate process. Decision 0003 put the core in the window's process.
- **The kit.** SwiftUI gives the newer declarative views, and AppKit gives
  finer control of tables with many rows. The trial of decision 0003 drew
  100,000 rows, and neither kit has that measurement yet.
- **Memory as a subject.** The owner named memory on macOS beside this
  window. Suds measures storage alone today, so a view of memory would
  give Suds a third subject.

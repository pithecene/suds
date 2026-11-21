# 0005 macOS Window

Status: Proposed (2026-11-21).

## Context

Decision 0003 specifies one Slint interface in Rust for macOS and Linux.
It leaves open whether a native window must use the system's own controls.

On 2026-11-21 the owner asked for a macOS window that matches the Slint
window and uses Swift and the system's own controls.

The trial for decision 0003 ran on Linux. Menu bars, dialogs, and screen
reader access still need testing on macOS.

## Decision

1. On macOS, Suds's window is a native application in Swift. It draws
   with the system's own controls. This item replaces items 3 and 4 of
   decision 0003 on macOS.
2. On Linux, the window stays the Slint interface of decision 0003.
3. The macOS window follows the Slint window's views and covers the
   development storage subjects listed in item 1 of decision 0003.
4. The macOS window follows the plan, approval, and journal requirements
   of item 5 of decision 0003. The window presents the plan for a person's
   approval, then calls `suds-core` in process to apply the approved plan
   and record the result in the same journal the command line uses.
5. Both windows call `suds-core` in process for measurements and
   maintenance rules, as item 2 of decision 0003 requires.

## Alternatives

- **Slint on macOS too.** Developers would maintain one frontend for both
  systems. Slint draws its own controls on macOS, and the owner asked
  for the system's own controls.
- **SwiftUI on macOS and GTK on Linux.** Each system receives its own
  controls. Developers would replace the working Linux window as well, and
  the owner asked only about macOS.
- **The web view of decision 0001.** Developers could reuse the React
  components. The owner asked for the system's own controls.

## Consequences

- Contributors update both frontends when they change a shared view.
- Contributors pin a Swift toolchain and add a check for the Swift window
  to the macOS CI job.
- The Slint window's attribution dialog stays on Linux alone.

## Open Questions

- **Swift bindings.** Which bindings should the Swift window use to call
  `suds-core`?
- **SwiftUI or AppKit.** The Slint trial drew up to 100,000 rows on Linux.
  How does each macOS toolkit handle a table of that size?

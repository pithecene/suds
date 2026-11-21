# Window

This guide tells a contributor how the Suds window works, how to run and
check it, and how to give it a new view. Decision
[0003](../decisions/0003-native-interface.md) states why the window is
native and why its toolkit is Slint.

## Layers

The window stands on three crates, and each one knows only the crate
beneath it.

| Crate | Path | Holds |
| :--- | :--- | :--- |
| `suds-core` | `crates/suds-core` | The measurements, the plans, the registry, and the journal |
| `suds-view` | `crates/suds-view` | Each view as plain values, without an interface toolkit |
| `suds-window` | `apps/window` | The Slint interface and its filling code |

The crate `suds-view` exists so that a view has one definition. A test reads a view
without a display, and the window of another system draws the same values.
A figure that the window shows belongs in `suds-view`. Only its text and
its color belong in `apps/window`.

Inside `apps/window`, three files divide the work.

- `ui/window.slint` declares every element, its layout, and its
  accessible role and label. It holds no logic beyond a selection or a key.
- `src/present.rs` turns a value of the view model into an item's text. Its
  functions are pure, and its tests need no display.
- `src/main.rs` measures on a thread of its own, keeps the last reading,
  and answers the interface's callbacks.

## Run

The command `task window` builds the release binary and opens the window. The window
measures the roots that `settings.json` names, as `suds maintain` measures
them. A workstation without a root shows the settings file's path and no
figure.

The command `task window:install` installs the window for one user: the
binary, the desktop entry `suds.desktop`, and the icon. The entry and the
icon go beneath `XDG_DATA_HOME`, or beneath `~/.local/share` without one,
and the entry names the binary by its full path. A desktop names the window
and shows its icon through that entry, which the window claims by the
identifier `suds`. The window draws no menu bar on Linux. Its first card
carries the two buttons, to measure again and to show About. The Tab key
reaches each button, and Return or Space presses it.

The window writes nothing. It records no pressure reading, opens no
journal, and removes no file.

| Key | Act |
| :--- | :--- |
| Control and Q | Closes the window |
| Control and F | Moves to the filter |
| Control and R | Measures again |
| Up and Down | Move through the outline |
| Right and Left | Open and close the selected project |
| Escape | Lets the selection go |

A click on a project's row opens or closes it. A second click on a selected
checkout, or a click on the map's empty space, also lets the selection go.

### Projects

A project is one repository, whichever directories hold its clones. The
view model names a project by the remote `origin` in the main checkout's
Git configuration, so each clone of that remote and each worktree joins one
project. A repository without that remote stands alone under its path.

### Storage Map

The map follows the squarified layout of Bruls, Huizing, and van Wijk. A
rectangle's area is the entry's size on disk. A project's frame holds a tile
for each of its checkouts, and a tile shows its build state's part in the
color of build state. The outline's bars use the same colors and measure
every row against the largest project, so a checkout's bar is a part of its
project's bar. The function `suds_view::map` computes each rectangle, and
`window.slint` only places them.

A selection in the map and a selection in the outline are one selection.
The window names it by path, so it follows its entry into a new reading.

## Check

The command `task window:check` builds the window, runs the tests of `suds-window` and
`suds-view`, and lints both. CI runs the same task on Linux.

A contributor without a free display can still see the window. An X server
that draws to memory holds it, and ImageMagick saves the picture.

```sh
Xvfb :96 -screen 0 1280x820x24 &
env -u WAYLAND_DISPLAY DISPLAY=:96 target/release/suds-window &
sleep 30
DISPLAY=:96 import -window root window.png
```

The first measurement of a large workspace takes some seconds, so the
picture waits for it.

### Screen Reader Access

Slint publishes the window's elements on the accessibility bus only while
the session reports a running screen reader. On Linux, this command sets
that report, and the same command with `false` clears it.

```sh
busctl --user set-property org.a11y.Bus /org/a11y/bus \
  org.a11y.Status ScreenReaderEnabled b true
```

A script can then read the window's tree through AT-SPI, for example with
Python's `gi.repository.Atspi`. The checkout list must reach the bus as a
list whose items each carry a label. An element without a label reaches a
screen reader as nothing, so each new element takes `accessible-role` and
`accessible-label` in `window.slint`.

## New Views

A new view takes four steps, in this order.

1. Add the view's values and their test to `suds-view`. Read the core
   through its public functions. Only an approved run writes, as
   `reclaim.rs` applies a plan through the core.
2. Add the items' text to `present.rs`, with a test for each rule of
   wording.
3. Declare the view's elements in `window.slint`, each with its role and
   its label.
4. Fill the elements from `main.rs`, on the interface's thread.

A view that changes the disk follows item 5 of decision 0003. It builds a
plan through the core, shows the plan, and applies it only on a person's
approval, so the journal records the change as it records one from the
command line.

## Limits

- The window runs on Linux alone. On macOS, the menu bar, the dialogs, and
  a screen reader's access have no measurement.
- The window shows one view. The registry's records, the journal, a
  reclaim plan, and the stores' trims have no view yet.
- The map has no key of its own. A person reaches each of its entries
  through the outline.
- A release publishes no archive of the window.
- The desktop application of decision 0001 still holds the inspection of a
  root and the moves of Downloads entries. It leaves once the window holds
  both.

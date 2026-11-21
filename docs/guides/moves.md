# Moves

This guide describes how Suds moves entries through a plan, a journal, and an undo.

```sh
suds plan move --from ~/Downloads/report.pdf --to ~/Documents/Research --output plan.json
suds apply plan.json        # one suds.run/v1 JSON document on standard output
suds journal                # every run, most recent first (--format json for JSON)
suds undo RUN               # return each moved entry to its original path
```

The `plan` command reads metadata and moves nothing. It writes one
`suds.plan/v1` document, which names every source and destination by its
path's bytes in hexadecimal beside a display string, and records each
entry's device, inode, type, size, and modification time. Every `--from`
entry must lie in one directory. Planning resolves both directories to
their canonical paths, and the preview shows them. The command refuses a
symbolic link or a special file, a destination that exists, a destination
inside the source, a directory that contains a mount point, and a
destination on another filesystem. It also refuses to replace an existing
output file. Where the measurement of a selected directory cannot read an
entry, the plan and its preview name that entry with the reason.

The `apply` command accepts only a plan that `suds plan` built and
recorded in the journal, unchanged, and each plan runs once. Immediately before each move, Suds verifies the entry's
recorded identity, the identity and canonical path of both directories,
and that the destination is free, and it refuses an entry that holds the
journal directory. Suds then renames the entry with the
kernel's no-replace flag, so the move is atomic and overwrites nothing.
After the rename, Suds confirms that the destination holds the planned
entry. When another process swapped the source in the meantime, Suds
returns the swapped entry and refuses the move. A refused move leaves the
other moves to proceed. The `undo` command
reverses each move that remains applied, in reverse order. It refuses a move
whose entry changed after the move and a move whose original path now holds
another entry. Suds never merges, renames to avoid a conflict, copies, or
deletes.

The journal is an append-only file of `suds.journal/v1` records. On macOS
it lies in `~/Library/Application Support/Suds/journal`. Its place on Linux
is `~/.local/state/suds/journal`, and `XDG_STATE_HOME` moves it. The option
`--journal DIR` names another directory. Suds records each
move before and after its rename. When a run stops between those records,
the next `apply` or `undo` examines both paths. If the entry lies at
exactly one of them, Suds records which, and otherwise it records the move as
`unresolved` and changes nothing. Suds opens no journal file through a
symbolic link. A full disk or an interruption can cut an append short, and
the bytes that remain form no record, so a reader sets them aside and the
next writer removes them.

| Status | Meaning for `plan`, `apply`, and `undo` |
| :--- | :--- |
| `0` | Suds wrote the plan, or every move succeeded. |
| `1` | Suds refused the plan, or it refused one or more moves. |
| `2` | The invocation failed, or Suds refused the plan file, the run, or the journal before any move. |

## Known Limitation

No system call renames an entry on the condition of its inode. Another
process can therefore swap a source between Suds's last check and the
rename. Suds detects the swap and returns the swapped entry. When yet
another entry has taken the source path by then, Suds overwrites nothing,
leaves both entries in place, and records the move as `unresolved` with
what it found at each path.

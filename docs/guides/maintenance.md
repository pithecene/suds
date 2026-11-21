# Clients and Schedule

This guide describes the registry of clients, the pressure reading, and the scheduled maintenance.

A client is a program that works in checkouts, such as Bosun or an agent
that a person drives. Each client uses the same commands.

```sh
suds register --checkout DIR --client bosun --attempt wf-12
suds release --checkout DIR --client bosun --attempt wf-12
suds pressure --format json
suds maintain
suds schedule                          # print the units of this system
suds schedule --install                # plan the installation
suds schedule --install --apply        # place the units and start the schedule
suds schedule --check                  # state whether the placed units are current
```

The `register` command records that a client holds a checkout, and
`release` records that the client's work there ended. Both append one
`suds.registry/v1` record to `registry.jsonl` in the state directory. A
registration adds knowledge and no permission:

- Suds removes nothing beneath a checkout that a client registered and has
  not released, and it reads the registry anew before each removal.
- A release ends the registration for its client and attempt. In
  `suds reclaim`, a linked checkout that its clients have released takes
  priority over other checkouts. The rules of [Reclamation](reclaim.md)
  decide whether it leaves.
- A client can report delivery with `suds release --checkout DIR --client
  bosun --attempt wf-12 --landed COMMIT`. After a squash merge, a pull
  request's head can serve as evidence if Git fetched it into a local
  remote-tracking reference. A person can also approve removal based on a
  matching release when no local reference contains the released head.

The `maintain` command is the run that a schedule starts. It inspects the
roots that the settings name, and it records one `suds.pressure/v1` reading
in the state directory. The usage counts each root, each checkout with its
build state, and each store. Docker keeps its own storage, which the usage
leaves out. The `pressure` command prints the last reading and measures
nothing, so a client can read it before each start.

| Level | Usage | A client's response |
| :--- | :--- | :--- |
| `normal` | Below 80 percent of the ceiling | Start work |
| `soft` | From 80 percent | Reclaim before more work |
| `hard` | From 95 percent | Start no work |

The level rises at once. It falls only after the usage has stayed lower for
five minutes, so a reading near a threshold holds one level. The ceiling is
the greater of half of the disk and 100 GiB unless the settings name
another. Each reading says in its `enforced` field whether a filesystem
limit enforces the ceiling, as [Arena](arena.md) describes.

With the owner's standing approval in the settings, `maintain` reclaims
derived state, removes eligible landed linked checkouts, and trims the
stores. The journal records the standing approval as the run's authority.

Suds reads its settings from the file `settings.json`. On Linux the file
lies in `~/.config/suds`, and on macOS it lies in
`~/Library/Application Support/Suds`. No command writes the file, and a
file that Suds cannot parse stops the run.

```json
{
  "roots": ["/home/me/workspace"],
  "standing_approval": true,
  "idle_minutes": 60,
  "ceiling_bytes": null,
  "tolerate_unreadable_processes": false,
  "budgets": { "docker.images": 42949672960 }
}
```

The `schedule` command prints the units that run `suds maintain` every 30
minutes: a service and a timer for systemd, or an agent for launchd. It
writes for the scheduler of this system, and `--scheduler` names the other
one. The option `--minutes` sets another time between two runs.

With `--install`, the command plans the installation and changes nothing.
The plan lists each file that it creates, replaces, or keeps, each command
of the scheduler that it runs, and the text of each unit. With `--apply`
too, the command writes each file beneath the user's own directory of units
and then runs `systemctl --user` or `launchctl`, so it needs no
administrator. The journal records each file and each command, and
`suds journal` lists the run. A file that changed after the plan stays as
it stands, and no command runs after it. Suds follows no symbolic link at a
unit's path, so a linked unit fails the plan.

With `--check`, the command reads the placed units and changes nothing. Its
status is 0 when the units are the units of this release, and 1 when no
unit stands in place or the placed units are stale. Units are stale when an
earlier release wrote them, when they state no search path, or when they
run another Suds program than the one that checks. The minutes and the
search path are a person's choice, so the check accepts each value. A
file that Suds cannot read fails the check with the reason.

A scheduler starts a unit with a short search path of its own, which names
no program that a version manager installed. The units therefore carry the
search path of the shell that ran `suds schedule`, so a scheduled run finds
the program of each store as that shell finds it.

A run also looks beyond the search path. When the path resolves no program
of a store, Suds takes the program from the first known directory that
holds it. Those are the directories of mise, asdf, Cargo, Go, pnpm, Volta,
Nix, and Homebrew, with `~/.local/bin` and `/usr/local/bin`. A unit with a
stale search path therefore still trims its stores.

A program from such a directory may start another, as a launcher of pnpm
starts Node. Its command therefore runs with those directories on its
search path, and the plan shows that path. A directory that the search
could not read stands on the error stream, since a program may lie there
unseen.

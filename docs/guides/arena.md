# Arena

This guide describes the `suds arena` command, which reports the ceiling of development storage.

```sh
suds arena                       # the first root of the settings
suds arena --root ~/workspace --format json
```

The arena is the development storage under its ceiling. The command reports
the ceiling, the size and the free space of the root's filesystem, and
whether a filesystem enforces the ceiling. It writes a table or one
`suds.arena/v1` document.

A filesystem enforces the ceiling when the root lies on a filesystem of its
own that is no larger than the ceiling: an APFS volume under a quota, or a
partition of that size. Every other root and every store must lie on that
same filesystem, and the report lists each one that lies outside it. Suds
audits no root through a symbolic link. Development storage then cannot fill the system's
volume, whatever fills the arena. Suds reads what any user can read, so it
cannot see a quota group of btrfs or a project quota of XFS, and it reports
such a root as not enforced.

Where nothing enforces the ceiling, the command prints the commands that
would enforce it. APFS takes `diskutil apfs addVolume` with a quota. A btrfs
filesystem takes a subvolume under a quota group, and XFS takes a project
quota. Most of the commands need an administrator. Suds runs none of them and requests no privilege. Each command quotes its
paths for a shell. The plan
creates an empty location under the limit and moves nothing.

Until a filesystem enforces the ceiling, Suds holds it by measurement
alone, through the pressure reading of each maintenance run.

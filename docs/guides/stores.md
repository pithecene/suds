# Store Budgets

This guide describes the `suds stores` command, which holds each shared store to a budget.

```sh
suds stores                                  # the plan, with nothing changed
suds stores --apply                          # run each trim that the plan names
suds stores --budget go.build-cache=20G --budget docker.images=40G
```

The command measures each store of the
[development profile](inspect.md#development-profile) and asks Docker for its own
usage. A store that exceeds its budget takes the trim of its own program, and Suds
evicts from Go's build cache itself.
Without `--apply` the command prints the plan, with each command that it
would run, and changes nothing.

| Store | Default budget | Trim | What the trim removes |
| :--- | :--- | :--- | :--- |
| `go.build-cache` | 10 GiB | Suds evicts entries itself | The least recently used entries beyond the budget |
| `go.module-cache` | 10 GiB | `go clean -modcache` | Every downloaded module |
| `pnpm.store` | 10 GiB | `pnpm store prune --store-dir DIR` | Each package file that no project holds by a hard link |
| `ccache.cache` | 10 GiB | `ccache --trim-dir DIR --trim-max-size SIZE` | The least recently used results beyond the budget |
| `mise.installs` | 20 GiB | `mise prune --yes` | Each tool version that no tracked configuration names |
| `docker.images` | 20 GiB | `docker image prune --all --force` | Each image that no container uses |
| `docker.build-cache` | 10 GiB | `docker builder prune --force` with the builder's size option | The build cache beyond the budget |
| `docker.anonymous-volumes` | 1 GiB | `docker volume prune --force` | Each anonymous volume that no container uses |

Each command names the store that Suds measured. The commands of Go and
mise receive it in `GOCACHE`, `GOMODCACHE`, or `MISE_DATA_DIR`, and the
commands of pnpm and ccache receive it as an argument. Each command runs in
the root directory, so no project's configuration redirects it to another
store. Docker's own builder bounds its cache with `--keep-storage`, and the
buildx builder bounds it with `--max-used-space`, so Suds reads the
command's help to choose.

A program's trim decides what leaves. Go offers no trim to a size, so Suds
evicts the entries of its build cache that Go used least recently, until the
cache meets its budget. An entry stays when Go used it within the idle time
or within one more hour, by which Go's record of a use may lag. A cache whose
recent entries exceed the budget stays above it. The eviction leaves Go's
fuzz corpus, its cached executables, and every file that is no entry
([0007](../decisions/0007-go-cache-eviction.md)). Go's module cache leaves
whole, and the next build downloads it again. The
trims of pnpm, mise, and Docker's images remove what nothing references,
so a store can stay beyond its budget after its trim.

The trim of pnpm reads a reference from a file's count of hard links. On a filesystem
that clones files, such as btrfs or APFS, pnpm clones each package into a
project, so no file of the store holds a second link. The trim then removes
every package of the store, and the next installation downloads each one
again. Each project keeps its own clones. On one workstation with btrfs, a
trim took the store from 12.9 GiB to 1.4 GiB.

Each version of pnpm trims its own layout alone, such as `v10` or `v11`
beneath the store. A layout of another version of pnpm stays until that
version trims it.

Suds reports these stores and trims none of them:

- `cargo.registry`, `sccache.cache`, and `vcpkg.binary-cache` have no
  budget here. Cargo and sccache bound their own stores, and vcpkg offers
  no trim.
- A store whose directory lacks its program's marker has no proven owner.
- A named Docker volume that no container uses may hold data that nothing
  regenerates. The plan lists each one as `awaits_person`, and Suds removes
  none.

Another store whose newest entry changed within the idle time stays, because
a build may write it. The journal records each trim with the store's size
before and after it, and `suds journal` lists the run.

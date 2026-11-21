# Store Budgets

This guide describes the `suds stores` command, which holds each shared store to a budget.

```sh
suds stores                                  # the plan, with nothing changed
suds stores --apply                          # run each trim that the plan names
suds stores --budget go.build-cache=20G --budget docker.images=40G
```

The command measures each store of the
[development profile](inspect.md#development-profile) and asks Docker for its own
usage. A store that exceeds its budget takes the trim of its own program.
Without `--apply` the command prints the plan, with each command that it
would run, and changes nothing.

| Store | Default budget | Trim | What the trim removes |
| :--- | :--- | :--- | :--- |
| `go.build-cache` | 10 GiB | `go clean -cache` | Every cached build output |
| `go.module-cache` | 10 GiB | `go clean -modcache` | Every downloaded module |
| `pnpm.store` | 10 GiB | `pnpm store prune --store-dir DIR` | Each package that no project references |
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

A program's trim decides what leaves. Go offers no partial trim, so a cache
beyond its budget leaves whole, and the next build fills it again. The
trims of pnpm, mise, and Docker's images remove what nothing references,
so a store can stay beyond its budget after its trim.

Suds reports these stores and trims none of them:

- `cargo.registry`, `sccache.cache`, and `vcpkg.binary-cache` have no
  budget here. Cargo and sccache bound their own stores, and vcpkg offers
  no trim.
- A store whose directory lacks its program's marker has no proven owner.
- A named Docker volume that no container uses may hold data that nothing
  regenerates. The plan lists each one as `awaits_person`, and Suds removes
  none.

A store whose newest entry changed within the idle time stays, because a
build may write it. The journal records each trim with the store's size
before and after it, and `suds journal` lists the run.

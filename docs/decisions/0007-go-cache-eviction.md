# 0007 Go Cache Eviction

Status: Proposed (2026-11-24).

## Context

Decision 0002 bounds each shared store by a budget, and the program that
owns a store trims it. Go offers no trim to a size. Its own trim removes
what stood unused for five days, and `go clean -cache` removes the whole
cache.

On one macOS workstation with many parallel agents, the cache held 61 GiB
against its budget of 10 GiB, and no entry was older than five days. Suds
kept the cache in the plan of 2026-11-24, because some build had written it
within the idle time. A trim would have removed every entry, so each next
build would have compiled its dependencies again.

An entry's modification time records its last use, at most once an hour. A
lookup that finds no entry is a miss, and Go compiles the package again.

## Decision

1. Suds evicts entries from Go's build cache itself, from the least
   recently used, until the entries that it may remove meet the budget.
2. The budget causes each eviction, and the last use of an entry sets only
   its order.
3. Suds keeps an entry that Go used within the idle time, or within the hour
   by which Go's record may lag, whatever the budget.
4. Suds removes only a regular file with one link and the name of an entry,
   in that entry's directory of the cache, which belongs to the user. The
   layout proves the entry, and Go's own trim removes a file by a weaker rule.
   Go's fuzz corpus, its cached executables, and every other path stay.
   Suds evicts from the planned directory alone, and from no cache inside
   a checkout.
5. `go clean -modcache` stays the trim of the module cache.

## Alternatives

- **`go clean -cache` at a higher threshold.** It still removes the entries
  that builds use.
- **`-trimpath`, so that checkouts share entries.** A test result then
  passes from one checkout to another, and Go tracks no file outside the
  module, so a test reported a pass for a checkout with a broken file.
- **A program that implements `GOCACHEPROG`.** Suds would then serve every
  build, and a failure of Suds would fail the builds.

## Consequences

- Suds removes entries that another program owns, which no other store
  allows. A change to the layout of the cache in a release of Go makes Suds
  refuse the cache or keep entries whose names it does not know.
- A cache whose recent entries exceed the budget stays above it.
- A build that looks an entry up right before its removal opens it too
  late. That build fails, and its next run compiles the package again. The
  same happens to a build that holds an entry's path for longer than the
  kept time. Go's own trim carries both risks, with a longer time, and no
  program outside Go can close them.

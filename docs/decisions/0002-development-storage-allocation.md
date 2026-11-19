# 0002 Development Storage Allocation

Status: Proposed (2026-11-19).

## Context

A macOS workstation ran about ten coding agents in parallel in November
2026. Each agent worked in its own Git worktree, and each worktree gathered
its own dependencies and build output. A container engine's disk grew
beside them. The disk filled, the owner cut the machine's power, and the
machine no longer boots.

No program owned the sum. Each tool isolated its work by copying, and
nothing bounded the total or observed it. Bosun's research notes on
workstation storage (pithecene/bosun,
`research/workstation-storage/RESEARCH.md`) hold the measurements and the
prior art.

Suds measures storage today and changes nothing. Its guardrails require a
person's inspection of every plan, which suits a person's own files. Build
output and caches differ: a build regenerates them, and they grow while no
person watches.

## Decision

1. Suds allocates and reclaims a workstation's development storage. That
   storage is an arena under a ceiling, and it holds the checkouts, the
   shared stores of each ecosystem, the toolchains, and the container
   engine's data.
2. Every program that develops is a client of Suds: an agent that a person
   drives, Bosun, and the person. Bosun manages no storage. A client
   registers a checkout, releases it, reads the arena's pressure, and
   requests a reclamation.
3. Authority for a removal follows the class of the state and never its
   author. Suds removes derived state inside the arena under one standing
   approval of the owner. Unique state, which includes an uncommitted
   change, an unpushed commit, and every file outside the arena, needs a
   person's approval of each plan.
4. A main checkout belongs to the owner. Suds never proposes its removal,
   and it treats the build output inside it as derived state.
5. Suds removes nothing because of its age. A shared store leaves by its
   budget, and a checkout leaves on evidence that no client uses it and
   that a remote holds its work.
6. The ceiling and each budget are settings. The default ceiling is the
   greater of half of the disk and 100 GB.

## Alternatives

- **Bosun as the owner of storage.** Bosun knows which attempt owns a
  checkout, but agents that a person drives never pass through Bosun, and
  they fill the same disk.
- **A provisioning script in an infrastructure repository.** A script
  creates a bounded volume once and never observes or reclaims it.
- **A person's approval of every removal.** Build output would then outgrow
  the disk between approvals, as the incident showed.
- **Removal by age.** An old checkout may hold the only copy of a change,
  and a recent cache entry may be the largest waste.

## Consequences

- Suds keeps state between runs, runs on a schedule, and calls each
  ecosystem's own program through a typed adapter.
- Permanent removal becomes the default for derived state inside the arena,
  because a quarantine frees no space.
- Suds needs proof that a path holds derived state, so it classifies by
  each ecosystem's own layout and never by a name alone.
- A hard ceiling needs a filesystem limit, which a privileged command
  creates once for each machine. Suds plans that command, and a person runs
  it. Until then Suds holds the ceiling by measurement and reports that no
  filesystem enforces it.
- The moves of Downloads entries keep the person's approval of each plan.

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

1. Suds bounds and reclaims a workstation's development storage. That
   storage is an arena under a ceiling, and it holds the checkouts, the
   shared stores of each ecosystem, the toolchains, and the container
   engine's data. Suds reserves no space and hands none out. The
   filesystem allocates, and Suds records who holds a checkout and whether
   a build can make a directory again. The word arena names the boundary
   of the standing approval.
2. Every program that develops is a client of Suds: an agent that a person
   drives, Bosun, and the person. Bosun manages no storage. A client
   registers a checkout, releases it, reads the arena's pressure, and
   requests a reclamation.
3. Authority for a removal follows the class of the state and never its
   author. The owner's standing approval covers derived state inside the
   arena, and it covers no checkout.
   Unique state, which includes an uncommitted change, an
   unpushed commit, and every file outside the arena, needs a person's
   approval of each plan.
4. A main checkout belongs to the owner. Suds never proposes its removal,
   and it treats the build output inside it as derived state.
5. Suds removes nothing because of its age. A shared store leaves by its
   budget. A linked checkout leaves only through a saved plan that a person
   approved, and the run applies that plan and no other. The run keeps a
   candidate that changed after the plan. Suds accounts for every entry of
   the checkout at the plan and again before the removal.
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
- **Use a remote-tracking reference as the sole proof of delivery.**
  Git can discard ignored files and nested worktrees even when it reports
  a clean checkout. The reference accounts for commits, and Suds needs
  evidence for the contents of the checkout too.
- **Removal by age.** An old checkout may hold the only copy of a change,
  and a recent cache entry may be the largest waste.

## Consequences

- Suds keeps state between runs, runs on a schedule, and calls each
  ecosystem's own program through a typed adapter.
- Permanent removal becomes the default for derived state inside the arena,
  because a quarantine frees no space.
- Suds needs proof that a path holds derived state, so it classifies by
  each ecosystem's own layout and never by a name alone.
- A person's `suds reclaim --apply` can accept a release with
  `--landed COMMIT` as evidence while the linked checkout's head still
  matches. The tree must have no changed, staged, or untracked file, and
  no client may hold an unreleased registration. No process may work
  there, and no entry may have changed within the idle time.
- Git removes a linked checkout without force and keeps its branch. The
  journal records the local remote-tracking reference when Suds finds one
  that contains the released head at removal. The plan and journal name
  the first path that keeps a checkout when Suds cannot account for it.
- A hard ceiling needs a filesystem limit, which a privileged command
  creates once for each machine. Suds plans that command, and a person runs
  it. Until then Suds holds the ceiling by measurement and reports that no
  filesystem enforces it.
- The moves of Downloads entries keep the person's approval of each plan.

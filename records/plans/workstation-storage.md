---
plan: workstation-storage
---
# Workstation Storage

Suds becomes the allocator and the collector of a workstation's development
storage, as
[decision 0002](../../docs/decisions/0002-development-storage-allocation.md)
states. Each task works on Linux and on macOS. The owner's macOS workstation
returns with about 160 GB free of 970 GB, so the tasks through **stores**
must reclaim on that machine before any agent starts there.

## Tasks

- [ ] **north-star**: Decision on development storage allocation
  - Outcome: Decision 0002 and the amended guardrails state that Suds allocates and reclaims development storage under a standing approval for derived state.
  - Scope: `docs/decisions/0002-development-storage-allocation.md`, `AGENTS.md`, and this plan. No code.
  - Acceptance: `bosun check` passes, and the owner accepts the decision.
  - Rationale: Each later task changes what Suds may do, and the guardrails bind every change.
- [ ] **classify**: Classes of development state in an inspection
  - After: north-star
  - Outcome: The command `suds inspect --profile development` reports each main checkout, each linked checkout, the build state under each, and each shared store, with its class and its size.
  - Scope: The profile, the classification by each ecosystem's own layout for Cargo, pnpm, Go, CMake with vcpkg, and mise, and a Linux default profile. The command changes nothing.
  - Acceptance: Tests classify a fixture tree that holds a main checkout, a linked worktree with build output, and an unrelated directory of the same name, which stays unclassified. On the Linux workstation, the report's total for linked worktrees matches `du` within one percent.
  - Rationale: A removal needs proof of the class, and a name alone proves none.
- [ ] **reclaim**: Removal of derived state under the standing approval
  - After: classify
  - Outcome: The command `suds reclaim` plans and applies the removal of build state that no process uses, and of a linked checkout whose tree is clean and whose branch a remote holds. The journal records each removal.
  - Scope: The planner, the liveness checks, the executor's permanent removal, and the journal's record of the authority. The moves of the reversible plans keep their approval.
  - Acceptance: Tests refuse a checkout with an uncommitted change, an unpushed commit, or a process inside it, refuse every main checkout, and remove the build state of an idle one. A run on the Linux workstation reports the bytes that it freed, and every worktree's `git status` reads as before.
  - Rationale: Build output accounts for nearly all of a worktree's size, and it returns with one build.
- [ ] **stores**: Budgets of the shared stores
  - After: reclaim
  - Outcome: Suds holds each shared store to its budget through the owning program: Go's caches, pnpm's store, sccache, mise's toolchains, and the container engine's images, build cache, and labelled volumes. An unlabelled volume enters a plan that waits for a person.
  - Scope: One typed adapter for each program, the budgets' settings, and the audit of each store's location.
  - Acceptance: Tests drive each adapter against a stub of its program and show the exact arguments. On the Linux workstation, a run brings Go's build cache and the engine's images under their budgets and removes no volume without a label.
  - Rationale: The stores hold several times the worktrees' size, and each owning program can trim its own store safely.
- [ ] **clients**: Registry, pressure, and schedule
  - After: reclaim
  - Outcome: A client registers and releases a checkout, reads the arena's pressure, and requests a reclamation through the `suds` command. A scheduled run reclaims without a person.
  - Scope: The registry between runs, the pressure's thresholds with their transition period, the `suds.pressure/v1` document, and a user timer for systemd and for launchd.
  - Acceptance: Tests register and release a checkout, hold pressure steady across a threshold's transition period, and reclaim a released checkout before an unregistered one.
  - Rationale: Bosun and each hook need one stable surface, and a schedule keeps the storage out of the owner's attention.
- [ ] **arena**: Ceiling and its filesystem limit
  - After: clients
  - Outcome: Suds measures the arena against its ceiling, plans the filesystem limit for btrfs and for APFS, and reports whether a filesystem enforces the ceiling.
  - Scope: The ceiling's default and its setting, the provisioning plan with its privileged command for a person, and the audit. Suds runs no privileged command.
  - Acceptance: Tests compute the default ceiling for a small and a large disk, and print the plan for each filesystem. On the Linux workstation, a subvolume under a quota group refuses a write past its limit while the system's volume stays writable.
  - Rationale: Every softer bound can fail, and a full system volume cost the owner a workstation.

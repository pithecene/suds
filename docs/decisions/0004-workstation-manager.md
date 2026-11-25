# 0004 Workstation Manager

Status: Proposed (2026-11-20).

## Context

Suds's commands are primitives. `suds inspect` measures, `suds plan` and
`suds apply` move entries through a reviewed plan, `suds reclaim` and
`suds stores` remove derived state, `suds register` and `suds release`
record a client's hold, and `suds pressure` reads the arena. A person who
keeps a workstation with them must know which command to run, in which
order, and what each result means.

The owner intends Suds as an application of its own, with a window and a
command line, for a developer or an ordinary user who has no other
Pithecene program. After an analysis, that person should reach a well-kept
workstation without choosing each command, and should keep every control.

Bosun also uses Suds, as a program that operates for agents. It knows
which session, track, and pull request stands behind a checkout, and Suds
knows none of the three (decision 0002, item 2).

A measurement of 2026-11-20 shows what the primitives lack on their own.
`suds reclaim --checkouts` planned the removal of 506 linked checkouts on
one workstation. Of those, 12 belonged to an open pull request, and 55 had
moved their head the same day with no pull request. Each was clean, held
only pushed commits, and had stood idle for an hour. The plan followed its
rules, and those rules read nothing that tells open work from finished
work.

## Decision

1. Suds divides into primitives, a manager, and interfaces. The
   primitives measure, plan, apply, journal, and record holds. The
   manager reads the primitives' measurements and writes plans. The interfaces, which are the window, the command line,
   and a client's calls, show and invoke both.
2. The manager's only output is a plan of the form that `suds apply`
   takes, with a reason and its evidence for each entry. The manager
   removes nothing by a path of its own, as decision 0003 holds the window
   to in its item 5. Each guardrail therefore lives in the primitives, and
   no rule of the manager can weaken one.
3. The manager needs no client. It reasons from what Suds reads itself:
   Git's account of a checkout, the processes at work, each directory's
   class, the sizes, the budgets, and the pressure. A client's hold is a
   constraint that the manager never overrides, and a client's label for
   its work is text that the manager shows.
4. Age and idle time may order the manager's recommendations to a person.
   Neither admits an unattended removal, and item 5 of decision 0002
   stands.
5. A person chooses how far the manager acts: it explains, it recommends a
   plan for approval, or it applies under the standing approval. The
   standing approval covers what item 3 of decision 0002 covers and no
   more.
6. Every primitive stays open to a person beside the manager, in the
   window and at the command line.
7. A client calls the primitives. It gives Suds facts, which are its holds
   and its releases. It may give the manager a goal, such as an amount to
   free beneath a root, and it never gives a list of paths to remove.
8. A linked checkout leaves only through a plan that a person approves, and
   only after a release states that each claim on it ended. Until then its
   build output stays as well.
9. The first manager is a set of rules in code, and each recommendation
   names its rule and its evidence. A model may later explain a plan or
   propose one, and no proposal that a model generates applies itself.

## Alternatives

- **The manager lives in Bosun.** Bosun knows the work best. A person
  without Bosun would then hold primitives alone, and the owner asks for
  an application that stands by itself.
- **Suds reads sessions and pull requests itself.** The manager would then
  know open work without a client. Suds would need a forge's credentials
  and a network request in an unattended run, and the guardrails allow
  neither.
- **A model as the first manager.** A model could weigh more evidence than
  rules state. A person trusts a removal only when its reason is exact,
  and a rule gives the same answer for the same evidence.
- **No manager.** The primitives stay the product. The measurement of the
  context shows a correct primitive producing a plan that no person wants.

## Consequences

- `suds maintain` becomes the manager's unattended form, and its plan
  carries the reasons that the window shows.
- A registration gains a label for the client's work, which Suds stores
  and shows without reading it.
- `suds reclaim --checkouts` no longer serves a client's call. Bosun's
  specification of its storage calls (pithecene/bosun,
  `records/specs/STORAGE.md`) states which of Bosun's records end a hold.
- A checkout that no client ever registered stays until a person ends its claim,
  so the window needs a view of such checkouts with their evidence.
- The window of decision 0003 gains the manager's recommendations as a
  subject beside the arena's state.

## Open Questions

- The layer's name is open. The owner named an orchestrator, a manager,
  and an intelligence layer.
- Item 8 needs a way for the command line to tell a person's approval from
  an agent's call, and this decision names none.
- Bosun asks for reclamation today through `suds maintain` at soft
  pressure. Whether Bosun needs any further goal of item 7 is unmeasured.

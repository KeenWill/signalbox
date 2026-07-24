# Goal-mode operating rules

Rules for autonomous milestone-delivering runs (for example Codex Goal mode).
[`AGENTS.md`](../../AGENTS.md) carries the rules that bind every agent and every
pull request; this file adds only what governs choosing, executing, and
finishing a milestone.

## Selecting a milestone

- Milestones come from the [priority order](../target-model.md#priority-order)
  in the target model: take the earliest unfinished step whose blocking
  decisions are accepted, or propose that step's blocking decision as the
  milestone. [The work backlog](backlog.md) is the owner-curated granular
  expansion of that order: when a goal names no milestone, take the highest
  backlog entry that is `ready` (or whose blocker the goal itself clears) and
  whose `Owns`/`Collides-with` groups are free of the concurrent claims the
  owner names at launch. Agents never reorder the backlog. The priority order is
  binding for selection; the target model's concept descriptions are directional
  and yield to accepted records.
- A milestone delivers one coherent capability toward its step — or, when the
  step is blocked, the proposal that unblocks it.
- Any new public domain or application type ships with a consumer in the same
  pull request or stack.
- Domain machinery for steps that cannot yet execute is frozen: no new public
  items and no semantic changes. A step's freeze lifts when that step becomes
  the selected milestone. Mechanical fixes required by CI or accepted review
  feedback are allowed.

## Executing

- Split independent tracks across subagents, each in its own worktree and
  branch; no two agents edit the same checkout. The root agent owns
  architecture, reconciliation, stack ordering, final review, and pull-request
  management.
- When one track hits an owner gate — a needed foundation-weight decision, a
  dependency approval, an unclear priority — stop that track and report the
  precise decision needed rather than inventing semantics; continue all other
  unblocked work. Delegating services, re-export batches, and polish of
  unconsumed machinery are not substitutes for blocked work.
- Maintain compact progress checkpoints naming the current track, what has been
  verified, what remains, and any semantic or external blocker.

## Finishing

A milestone is complete when all of its pull requests are finished (per
`AGENTS.md`) and merged by the owner; finished pull requests awaiting merge are
not a reason to stop other work. When the milestone's work is delivered, request
an owner alignment review before selecting the next milestone. That request
includes each pull request's one-line review-wave history — accepted and
declined counts partitioned per wave, in wave order, never aggregated across
waves — so the owner sees the degradation curve.

## Writing a goal

A goal prompt contains, in order: the outcome (the capability, not the
activity); milestone-specific constraints and exclusions; and a verifiable
stopping condition the run can check itself — every intended pull request open
and finished, validation green from the top of the stack, and no unresolved
blocker attributable to the new work. Durable process rules stay out of the
goal; they live here and in `AGENTS.md`.

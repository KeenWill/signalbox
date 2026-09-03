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
  expansion of that order. When a goal names no milestone, propose for owner
  confirmation at launch — do not silently commit to it — in this order: (1) if
  the backlog flags a next major milestone (the tool-loop foundation today),
  propose that flagged milestone first, and when it is blocked on an owner
  decision (as the tool loop is), propose scheduling that decision or design
  pass rather than dropping to a lower `ready` item; (2) only when nothing is
  owner-flagged, fall to the highest backlog entry that is `ready` (or whose
  blocker the goal itself clears) and whose `Owns`/`Collides-with` groups are
  free of the concurrent claims the owner names at launch. Agents never reorder
  the backlog. The priority order is binding for selection; the target model's
  concept descriptions are directional and yield to accepted records.
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

### Long-running commands

- Describe a long-running background command as progressing only on advancing
  evidence: process CPU time increasing between two samples, output growing, or
  a progress counter moving. Absence of failures is not progress, and neither is
  silence.
- Before electing to wait on a background run, sample the process itself —
  elapsed and CPU time, for example `ps -o etime,time -p <pid>` — and record
  what that sample showed, so the next check has a prior observation to compare
  against.
- A run whose CPU time and output have not advanced across two samples roughly
  five minutes apart is presumed wedged, not slow. Capture state first — process
  table, container status, output tail — then kill the run and its containers,
  remediate, and rerun the targeted subset before the full suite. Never wait for
  a zero-CPU command to exit on its own.
- Before launching a heavyweight suite, find and kill the leaked runs and
  containers an earlier turn left behind. A suite queued behind an abandoned run
  of its own waits on resources that run still holds, advances not at all, and
  reports no failure — the case these rules exist for.

## Finishing

A milestone is complete when all of its pull requests are finished (per
`AGENTS.md`) and merged by the owner; finished pull requests awaiting merge are
not a reason to stop other work. When the milestone's work is delivered, request
an owner alignment review before selecting the next milestone.

## Writing a goal

A goal prompt contains, in order: the outcome (the capability, not the
activity); milestone-specific constraints and exclusions; and a verifiable
stopping condition the run can check itself — every intended pull request open
and finished, validation green from the top of the stack, and no unresolved
blocker attributable to the new work. Durable process rules stay out of the
goal; they live here and in `AGENTS.md`.

# Goal mode

**Implemented behavior.** This page owns the cross-crate contract for one
commissioned goal attached to a session: its immutable statements, event-sourced
state, user commands, model declarations, scheduler continuation, process wire,
and terminal-client verbs. The domain and persistence surface was verified
through PR #383 (`agent/goal-mode`). The scheduling, model-tool, process, and
terminal surfaces were verified through its immediate child PR #384
(`agent/goal-mode-runtime`). This bottom specification diff owns both stack
slices. Identity and durable-command mechanics remain owned by
[identity and commands](identity-and-commands.md), turn execution by
[turn lifecycle and scheduling](turn-lifecycle-and-scheduling.md), tool dispatch
by [tool loop](tool-loop.md), and framing by
[process protocol](process-protocol.md). INV-048 is the lifecycle enforcement
family indexed by [the invariant test index](../invariants.md).

## Statement lineage and state

**Implemented behavior.** A goal statement is exact, nonempty UTF-8 bounded to 1
MiB and immutable after admission. Attaching a goal commissions generation one
when no lineage exists. After an achieved or user-stopped generation, attach may
commission the next generation. A pursuing or blocked generation is active and
rejects attach; changing its scope requires supersede. Mid-goal guidance that
does not change scope uses steer while pursuing or resume while blocked. No
statement-edit operation exists.

**Implemented behavior.** Supersede is one atomic event: it marks the active
generation `superseded { by_generation }` and commissions its immutable
successor as `pursuing`. The event retains the replacement statement and user
command provenance. All earlier generations and events remain readable, and
exactly one generation can be active at a time.

**Implemented behavior.** The current state is derived only by replaying the
session's append-only goal event stream; no mutable goal-state column is
authoritative. The state algebra is `pursuing`, `blocked { reason, need }`,
`achieved { report_ref }`, `user_stopped`, and `superseded { by_generation }`.
Blocked is scheduler-terminal but admits explicit resume or supersede. Achieved
and user-stopped end that generation; a later explicit attach may start another
generation. Superseded is terminal for the replaced generation while its same
event starts the successor.

**Implemented behavior.** The closed event vocabulary is `commissioned`,
`blocked`, `resumed`, `achieved`, `user_stopped`, and `superseded`. Positive
event ordinals are contiguous within one session. Positive statement generations
are contiguous across commission and supersession. Domain replay rejects a
missing first commission, a noncontiguous event, or a transition that is invalid
from the preceding derived state (INV-048).

## Transition authority and provenance

**Implemented behavior.** User transitions carry their user-global durable
command identity. The user commands are attach, resume with optional guidance,
stop, and supersede with a replacement statement. Their immutable receipts
record either the appended event ordinal or a closed rejection. Equal replay
returns the recorded result; structurally different reuse is a conflict.

**Implemented behavior.** A model may declare only `blocked` or `achieved`
through the session-scoped goal declaration tool. The declaration has no
caller-supplied session identity. Trusted tool-dispatch correlation supplies the
invoking session, turn, and tool-request identity, and persistence requires that
exact triple to name the request. An achieved event stores the exact final
report and derives its transcript reference from that same invocation.

**Implemented behavior.** Model-selectable blocked reasons are
`user_input_required`, `external_change_required`, and `authorization_required`.
Every blocked event carries exact nonempty need text. `execution_failure` is the
fourth stored reason and is scheduler-only: its provenance shape requires the
failed turn and cannot be constructed from a model declaration.

**Implemented behavior.** Stop is explicit user authority and yields
`user_stopped`, distinct from model-declared achievement and blocking. Supersede
is also explicit user authority and is admitted only while the current
generation is pursuing or blocked. Resume is admitted only while blocked; its
optional guidance becomes the next turn's input. Existing steer behavior is
unchanged and remains the only mid-pursuit guidance path.

## Scheduler continuation

**Implemented behavior.** While the current goal state is pursuing, successful
turn terminalization causes the daemon scheduler to create and start the next
turn without user input. It repeats after each successful turn while replayed
state remains pursuing. Goal state is the only continuation stopping condition:
there is no goal turn count, elapsed-time budget, verdict counter, or silent
model fallback.

**Implemented behavior.** A failed goal turn is not retried. In the same
scheduler disposition path, the daemon appends `blocked` with reason
`execution_failure`, need text describing the execution repair required, and the
exact failed-turn provenance. Continuation stops on blocked, achieved,
user-stopped, and a superseded generation; supersession's successor is pursuing
and therefore independently eligible to continue.

**Implemented behavior.** Attaching or superseding commissions a pursuing
generation and schedules its first turn. Resuming schedules exactly one next
turn and supplies guidance as that turn's accepted input when present. Durable
event and input correlation makes retrying command delivery idempotent rather
than duplicating continuation work.

## Persistence and process surfaces

**Implemented behavior.** Migration `202608020013` owns `goal_command` and
`goal_event`. Both are append-only and reject truncation. Relational checks
close every discriminator and payload shape; a session-row lock serializes event
append, a trigger enforces ordinal and generation continuity, composite foreign
keys enforce user-command, model-invocation, and scheduler-turn provenance, and
loads replay complete rows through the domain aggregate rather than reading a
mutable current-state projection (INV-048).

**Implemented behavior.** The process protocol exposes attach, show, resume,
stop, and supersede requests. Show returns the current generation and complete
ordered event history. The terminal client provides exactly the corresponding
verbs; session creation may compose an explicit attach immediately after
creation, while the two durable commands retain separate replay identities.

## Compatibility constraints

**Committed unimplemented functionality.** No present goal-mode surface
delegates work or creates child sessions. Future child sessions will compose
with goal mode rather than becoming a goal transition, so version-one goal
events and commands reserve no delegation variant.

**Committed unimplemented functionality.** No present goal-mode surface starts
or governs a review workflow. Future review composition must refer to goal and
session evidence without adding review states to the goal state algebra.

**Committed unimplemented functionality.** No present goal-mode surface chooses
runner placement. Future runner placement of goal sessions must leave goal
statements, transitions, and stopping authority independent of placement.

**Committed unimplemented functionality.** No present goal-mode surface
automatically falls back between models. Future fallback work must not turn a
failed goal turn into a silent retry or bypass execution-failure blocking.

**Committed unimplemented functionality.** No present goal-mode surface has a
goal priority or more than one concurrent goal per session. Future extension
must preserve immutable statements, full lineage, and the version-one rule that
at most one generation is pursuing or blocked.

## Open edges

**Deferred or undecided work.** No goal-mode open question is recorded by this
version-one contract.

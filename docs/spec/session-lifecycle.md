# Session lifecycle

Session lifecycle gives every session one durable state, an ownership bit,
deadlines, and a closed set of terminal outcomes; its goal is that every owned
session reaches an outcome or a human.

## Overview

Every session is in exactly one of eight lifecycle states: created, dispatched,
active, waiting, recovering, blocked, parked, and terminal. The state is a
core-owned column on the session's lifecycle satellite row, and
`SessionLifecycleState` in the domain crate defines the states and the
transitions between them. The turn machine that
[turn lifecycle and scheduling](turn-lifecycle-and-scheduling.md) owns runs
beneath the session machine. For a session that is neither terminal nor parked,
the session state is a projection of its live turn: a running turn makes the
session active, a turn awaiting approval or a child makes it waiting, and a turn
in any recovery phase makes it recovering. A blocked goal with no live turn
makes the session blocked. With no live turn and no blocked goal, the held state
and whether a turn is queued decide: created becomes dispatched once a turn is
queued unless the start gate is held, dispatched holds until a turn activates,
and every other session reads active, which the attention classifier reads as
idle. A terminal session stays terminal. Core writes the session state in the
same transaction as the turn or goal transition that changes the projection, so
the two machines never disagree.

Waiting carries a typed kind and the party expected to end the wait. Only an
owned session carries a deadline, and its state sets the kind: admission covers
created and dispatched, active stall covers active and recovering, and waiting
covers waiting, each of those states with exactly one deadline; blocked and
parked carry none. The admission and waiting bounds come from configuration. A
parked session, reached only by an expired waiting deadline or a module park,
carries a machine-readable cause and the responder who must act, the operator
queue or one module. The operator queue is every parked session; the recorded
responder does not filter it.

Terminal carries one outcome from a closed vocabulary. Achievement is verified
when a finish check passed and declared when no check ran; a failed check blocks
the goal instead of achieving it. A failure is retryable when a retry could
clear it, structural when the same input will fail again (a compaction wall, a
broken toolchain, a moderation block that a resume re-trips), and unknown when
no cause was classified. A session is stopped by a human or a rule, and the stop
records whether it is sticky. It is superseded when the caller names an existing
session other than the one closed; the outcome also admits a successor-free
form, for work that is gone. It is abandoned when an operator writes off a
parked session, and retired when it never did the work and never will.

Every session carries an owned-or-unmonitored bit, set at creation and flipped
by a journaled adopt or release. Owned means the daemon holds a liveness
obligation: deadlines and a driven path to a terminal outcome. Unmonitored means
a conversation the daemon does not drive. Every lifecycle command and every
state transition records a lifecycle actor: core, the operator, a named module,
or the watchdog. A transition classifies that actor from the domain actor
[identity and commands](identity-and-commands.md) defines; a lifecycle command
carries no actor and derives one from the authenticated principal.

The command surface creates a session, releases its start gate, submits input,
attaches, resumes, or stops a goal, adopts, and releases; a release also settles
a held start gate, so a session that already has queued work becomes dispatched.
Five further commands close a session or lift a park: a session-level stop
closes any non-terminal session, supersede closes it in favour of a named
successor, abandon and close as failed close a parked session, and each of these
closures is refused while a different terminal outcome is already pending;
resume returns a parked session with no pending terminal outcome to its mapped
state. A parked session with a blocked goal resumes through the goal's
resume-with-guidance command; one with a pursuing goal may use the session-level
resume. The goal command that [goal mode](goal-mode.md) calls supersede starts a
new goal generation in the same session and is unrelated to the session outcome
superseded.

Modules observe the lifecycle through seven event kinds with typed payloads on
the transactional outbox that [persistence protocol](persistence-protocol.md)
owns; the other outbox kinds are core-internal. The compaction funnel and the
five lifecycle metrics are read-only views over durable columns.

## Design decisions

The admission deadline is the one deadline whose expiry terminalizes: it retires
the session, because before first activity nothing live is guarded and no human
attention is owed. Every other implemented deadline expiry, the waiting
deadline, parks; active-stall expiry is planned.

The lifecycle actor classifies the domain actor rather than replacing it; the
domain actor algebra, its wire projection, and its replay-equality rule are
untouched.

A turn terminal with disposition retired extends the turn vocabulary, not the
lineage rules: it contributes no terminal frontier and is excluded from
predecessor selection like any retired queued work.

The turn watchdog's terminalization of a provably dead turn, which blocks the
goal and arms goal recovery, is turn disposition, not session disposition; this
page governs session disposition only.

Automatic resumption of a structurally failed session is refused, because the
same input fails again.

A module keeps a park state of its own only for an obligation that is not a
session. Why: a session held waiting on a human outside core parked is missing
from the operator queue.

The lifecycle metrics are read-only views; targets and the decision to start
substrate work are owner decisions made outside the daemon.

## Boundary contracts

Order comes from commit-ordered sequences, never from comparing wall-clock
times. A liveness check that cannot query some kind of evidence skips the turn
instead of ending it, and any event it does not recognize counts as progress.
When a lifecycle guard trips on an admitted session, the daemon waits, asks, or
parks the session; it never ends work on staleness evidence alone.

An owned session that waits for an operator is parked, blocked on a goal that no
automatic resumption will lift, or held in an exhausted recovery wait; a pending
tool-approval decision is the separate waiting state. An ambiguous model call
whose automatic reconciliation budget is exhausted is a further operator wait
until the operator reconciles the turn; an ambiguous external-effect tool
attempt whose budget is exhausted stays an exhausted recovery wait, flagged for
the operator, with no releasing command until the deferred tool-recovery surface
exists. A turn awaiting runner recovery is an operator wait too; the replacement
and abandonment commands that leave the lost state are planned. A module that
parks something wrapping an owned session drives the session itself to parked.
Attention states shown to operators are derived from durable facts by one
classifier, and a read that encounters a state it does not recognize returns an
error rather than a guess.

Lifecycle state, deadlines, budgets, recovery, and staleness detection live in
daemon core. A dispatched [repo-watch](repo-watch.md) session is the exception:
the module owns a dispatch-attempt budget and holds a start lease. An obligation
that exhausts the dispatch-attempt budget parks the owned sessions it wraps as
the module. The start lease covers a dispatched session's wait for its first
model call, and an expired lease ends the commissioned goal generation through a
composed goal stop rather than a lifecycle deadline, leaving the session
non-terminal. Lifecycle behavior or an event kind a module needs and core does
not provide is added to core, and modules never reconstruct events by joining
core tables.

The attention classifier that
[sessions and the transcript](sessions-and-transcript.md) owns is a projection
of lifecycle state and turn phase, never an independent machine.

An absent configured bound leaves a deadline unbounded. An owned session in a
deadline-bearing state with no deadline row is a violation.

Parking overrides the turn projection: it suspends a live turn in place, the
turn keeps its phase, and no new turn starts while the session is parked. Work
already in flight continues, including the tool execution an in-flight call's
response requests; that call runs to its end and records its result.

Verified achievement is recorded only when the declared finish check passes.

Lifecycle members default from the creation cause: module dispatch creates an
owned session whose finish condition is the external gate, a delegated child is
owned with no finish condition, and an interactive session is unmonitored with
no finish condition. Attaching a goal to an unmonitored session records it as
owned, with the adoption journaled to the attaching actor, in the same
transaction.

An unmonitored session has no deadlines and no automatic resumption; no external
sweep other than the repo-watch start lease acts on it, and it is excluded from
occupancy accounting only on the passes the reconciliation sweep admits under
its ownership marker. Turn-liveness recovery covers its turns, because a dead
turn left active would block its next input.

Release never interrupts a live operation: a running turn completes to its
boundary under the resources already held.

Core mints every lifecycle identity except the caller-supplied durable command
identity; no module pre-allocates a turn, input, or frontier identity inside its
own transaction.

Ownership is advisory: an owner module observes events and issues commands like
any other client, and it never sits between core and the session; a
core-integrated module parks its own session directly, inside the transaction
that records the cause.

A lifecycle command carries no domain actor: its lifecycle actor derives from
the authenticated principal, and a module principal classifies as that module.

Message injection (operator text, coordinator guidance, steering) is legal in
every non-terminal state regardless of ownership, except while a terminal
outcome is pending: a session in that window rejects injection. An injection is
never silently lost: every accepted injection settles with a durable
injection_settled receipt, and pending injections never block terminalization. A
turn awaiting a tool approval refuses interrupt delivery; next-safe-point and
after-current-turn delivery stay legal.

On session closure, remaining queued turns retire with cause session_closed and
an open goal generation closes as session_closed; a user-stopped generation
admits only the stopped outcome, and an achieved generation admits only an
achievement outcome.

The compaction funnel is queryable from durable state: requested, prepared,
applied, and failed are each stamped.

The five lifecycle metrics are defined on durable columns, never on proxies.

## Planned

- Failure parking of owned sessions: a structural failure, an unknown failure,
  or an exhausted retry budget on a live owned session parks it with the typed
  cause instead of terminalizing it or stopping silently; see
  [session lifecycle design](../design/session-lifecycle.md).
- Supersession by redispatch: a module redispatch that owns the retry closes the
  parked predecessor, a structurally failed one included, as superseded by the
  successor; see [session lifecycle design](../design/session-lifecycle.md).
- Deadline events for modules: modules and the program substrate subscribe to
  deadline expiries instead of running their own watchdogs; see
  [session lifecycle design](../design/session-lifecycle.md).
- Session state-change events: the eighth module-facing event kind, so modules
  observe park, resume, and other non-terminal transitions; see
  [session lifecycle design](../design/session-lifecycle.md).
- Program-run lifecycle actor: a run-scoped actor for commands issued by a
  program run; see [session lifecycle design](../design/session-lifecycle.md).
- Dispatch payload measurement: every dispatched session records the token and
  byte size of its initial payload at creation, and an interactive session
  records them at its first accepted input; see
  [session lifecycle design](../design/session-lifecycle.md).
- Active-stall expiry: the configured active-stall bound and the deadline pass
  that parks an active or recovering session on it; see
  [session lifecycle design](../design/session-lifecycle.md).
- Sticky-stop suppression: re-dispatch of stopped work stays suppressed until
  the dispatch source is updated; see
  [session lifecycle design](../design/session-lifecycle.md).
- Worktree and container cleanup on closure: a closed session's worktree and
  container are removed; see
  [session lifecycle design](../design/session-lifecycle.md).

# Session lifecycle

Session lifecycle gives every session one durable state, an ownership bit,
deadlines, and a closed set of terminal outcomes, so no session waits unseen and
every owned session is driven to an outcome or a human.

## Map

Every session is in exactly one of eight lifecycle states: created, dispatched,
active, waiting, recovering, blocked, parked, and terminal. The state is a
core-owned column on the session's lifecycle satellite row, and
`SessionLifecycleState` in the domain crate defines the states and the
transitions between them. The turn machine that
[turn lifecycle and scheduling](turn-lifecycle-and-scheduling.md) owns runs
beneath the session machine. For a session that is not parked, the session state
is a projection of its live turn: a running turn makes the session active, a
turn awaiting approval or a child makes it waiting, a turn in any recovery phase
makes it recovering, and a blocked goal with no live turn makes it blocked. Core
writes the session state in the same transaction as the turn or goal transition
that changes the projection, so the two machines never disagree.

Waiting carries a typed kind, a deadline, and the party expected to end the
wait. An owned session has three configured deadlines: admission, which covers
created and dispatched; active stall; and waiting. Parked is the one state in
which a session waits on a human; it carries a machine-readable cause and the
responder who must act. The operator queue is exactly the set of parked
sessions.

Terminal carries one outcome from a closed vocabulary. Achievement is verified
when a declared finish check passed and declared when no finish condition was
declared. A failure is retryable when a retry could clear it, structural when
the same input will fail again (a compaction wall, a broken toolchain, a
moderation block that a resume re-trips), and unknown when no cause was
classified. A session is stopped by a human or a rule, superseded when a newer
session owns the work or the work is gone, abandoned when an operator writes off
a parked session and releases its worktrees, containers, and slots, and retired
when it never did the work and never will.

Every session carries an owned-or-unmonitored bit, set at creation and flipped
by a journaled adopt or release. Owned means the daemon holds a liveness
obligation: deadlines and a driven path to a terminal outcome. Unmonitored means
a conversation the daemon does not drive. Every lifecycle command and every
state transition records a lifecycle actor (core, the operator, a named module,
or the watchdog) classified from the domain actor that
[identity and commands](identity-and-commands.md) defines.

The command surface creates a session, releases its start gate, submits input,
attaches, resumes, or stops a goal, adopts, and releases; five further commands
make every outcome and transition reachable: a session-level stop, supersede,
abandon, close as failed, and resume. A parked session with a blocked goal
resumes through the goal's resume-with-guidance command; one with a pursuing
goal may use the session-level resume. The goal command that
[goal mode](goal-mode.md) calls supersede starts a new goal generation in the
same session; it is goal replacement, unrelated to the session outcome
superseded.

Modules observe the lifecycle through eight event kinds with typed payloads on
the transactional outbox that [persistence protocol](persistence-protocol.md)
owns; the other outbox kinds are core-internal. The compaction funnel and the
five lifecycle metrics are read-only views over durable columns.

## Decisions

The admission deadline is the one deadline whose expiry terminalizes: it retires
the session, because before first activity nothing live is guarded and no human
attention is owed. Every other deadline expiry parks.

The lifecycle actor classifies the domain actor rather than replacing it; the
domain actor algebra, its wire projection, and its replay-equality rule are
untouched.

A turn terminal with disposition retired extends the turn vocabulary, not the
lineage rules: it contributes no terminal frontier and is excluded from
predecessor selection like any retired queued work.

The turn watchdog's terminalization of a provably dead turn, which blocks the
goal and arms goal recovery, is turn disposition, not session disposition; this
page governs session disposition only.

A structurally failed session is closed by a fresh session that supersedes it
rather than resumed, and automatic resumption into the same compaction wall is
refused, because the same input fails again.

A module keeps a park state of its own only for an obligation that is not a
session. Why: a session held waiting on a human outside core parked is missing
from the operator queue.

The lifecycle metrics are read-only views; targets and the decision to start
substrate work are owner decisions made outside the daemon.

## Contracts

Order comes from commit-ordered sequences, never from comparing wall-clock
times. A liveness check that cannot query some kind of evidence skips the turn
instead of ending it, and any event it does not recognize counts as progress.
When a guard trips, the daemon waits, asks, or parks the session; it never ends
work on staleness evidence alone.

A session that is waiting on a human is in the parked state and no other. When a
module parks a thing that is or contains a session, the module also moves that
session to parked. One classifier derives the attention states shown to
operators from durable facts. A read that encounters a state it does not
recognize returns an error rather than a guess.

Lifecycle state, deadlines, budgets, recovery, and staleness detection live in
daemon core; no module implements any of them. Lifecycle behavior or an event
kind a module needs and core does not provide is added to core, and modules
never reconstruct events by joining core tables.

The attention classifier that
[sessions and the transcript](sessions-and-transcript.md) owns is a projection
of lifecycle state and turn phase, never an independent machine.

A missing deadline is unbounded, not a violation.

Parking overrides the turn projection: it suspends a live turn in place, the
turn keeps its phase, and no model call, tool execution, or delivery proceeds
while the session is parked.

Verified achievement is recorded only when the declared finish check passes; for
[repo-watch](repo-watch.md) work the check re-tests the external gate on the
exact head.

A sticky stop suppresses re-dispatch of the stopped work until the dispatch
source is updated.

Lifecycle members default from the creation cause: module dispatch creates an
owned session whose finish condition is the external gate, and an interactive
session is unmonitored with no finish condition. Attaching a goal to an
unmonitored session records it as owned, with the adoption journaled to the
attaching actor, in the same transaction.

An unmonitored session has no deadlines, no watchdog, no automatic resumption,
and no held slot; no external sweep acts on it, and it is excluded from
occupancy accounting.

Release never interrupts a live operation: a running turn completes to its
boundary under the resources already held, and the slot releases at that
boundary.

Core mints every lifecycle identity; no module pre-allocates a turn, input, or
frontier identity inside its own transaction.

Ownership is advisory: an owner module observes events and issues commands like
any other client, and it never sits between core and the session.

The lifecycle actor of a command derives from its principal and its domain
actor: a module principal classifies as that module; otherwise the domain actor
classifies.

Message injection (operator text, coordinator guidance, steering) is legal in
every non-terminal state regardless of ownership. An injection is never rejected
for lifecycle state and never silently lost: every accepted injection settles
with a durable injection_settled receipt, and pending injections never block
terminalization.

On session closure, remaining queued turns retire with cause session_closed and
an open goal generation closes as session_closed; a user-stopped generation
admits only the stopped outcome, and an achieved generation admits only an
achievement outcome.

The compaction funnel is fully queryable from durable state: requested,
prepared, applied, and failed are each stamped, and the failure path records
input size and fit result.

The five lifecycle metrics are defined on durable columns, never on proxies.

## Not built

- Failure parking of owned sessions: a structural failure, an unknown failure,
  or an exhausted retry budget on a live owned session parks it with the typed
  cause instead of terminalizing it or stopping silently; see
  [session lifecycle design](../design/session-lifecycle.md).
- Supersession by redispatch: a module redispatch that owns the retry closes the
  parked predecessor as superseded by the successor; see
  [session lifecycle design](../design/session-lifecycle.md).
- Deadline events for modules: modules and the program substrate subscribe to
  deadline expiries instead of running their own watchdogs; see
  [session lifecycle design](../design/session-lifecycle.md).
- Program-run lifecycle actor: a run-scoped actor for commands issued by a
  program run; see [session lifecycle design](../design/session-lifecycle.md).
- Dispatch payload measurement: every dispatched session records the token and
  byte size of its initial payload at creation, and an interactive session
  records them at its first accepted input; see
  [session lifecycle design](../design/session-lifecycle.md).

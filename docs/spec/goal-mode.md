# Goal mode

Goal mode attaches one commissioned goal to a session and keeps the scheduler
starting turns toward it until the goal's own state stops them.

## Overview

A goal is one statement of work attached to a session. The domain type `Goal` in
`crates/domain/src/goal.rs` holds the session's goal lineage as an append-only
stream of events, and the current state is derived by replaying that stream.
Each statement is one generation. A generation is pursuing from its commission
until it is blocked, achieved, stopped by the user, superseded by a replacement
statement, or closed with its session. A blocked generation admits resume or
supersede; the other endings are final, and a later attach may start a new
generation after an achieved or stopped one. The event vocabulary is closed: a
commission, each block and resumption, and the ending.

Users act on a goal through four commands: attach, resume with optional
guidance, stop, and supersede with a replacement statement. Supersede changes an
active generation's scope; guidance that leaves the scope alone is a steer while
the goal is pursuing or a resume while it is blocked. A model reaches the goal
only through the session-scoped `goal_declare` tool, and may declare only
blocked or achieved. The repository-watch session-command vocabulary contains
checked goal operations, but the inactive module dispatches no sessions or
goals.

While a generation is pursuing, each successful turn's end makes the scheduler
create and start the next turn without user input. A failed goal turn is not
retried; the daemon appends a blocked event with the execution-failure reason,
need text, and the failed turn's provenance. Every goal turn is either scheduled
by this machinery or bound to a turn a command already accepted.

The planner in `apps/signalboxd/src/goal_mode.rs` resumes an execution-failure
block on an owned session automatically and within bounds. It derives from the
event history how many attempts the current run has spent, schedules one resume
after a backoff, and writes into each block's need text either the scheduled
resumption or the operator repair. When a block's commit acknowledgement is
lost, the daemon re-reads the lineage with bounded retries and arms the
execution-failure block it finds there. A resume attempt the database leaves
unsettled is retried within the same bound. Startup repairs the rest: the daemon
finds execution-failure blocks whose need promises resumption and treats their
lost timers as immediately due.

Command identity and replay are owned by
[identity and commands](identity-and-commands.md), turn execution by
[turn lifecycle and scheduling](turn-lifecycle-and-scheduling.md), tool dispatch
by [tool loop](tool-loop.md), session state and the parked rule by
[sessions and transcript](sessions-and-transcript.md), and wire framing by
[process protocol](process-protocol.md).

## Design decisions

A statement is immutable after admission and no edit operation exists; a change
of scope is a supersede, which commissions a new generation and leaves the old
one readable.

Steer is the only mid-pursuit guidance path.

Goal state is the only continuation stopping condition: there is no goal turn
count, elapsed-time budget, verdict counter, or fallback to another model.

Resumption never bypasses execution-failure blocking: the block is appended
first and every attempt is a recorded resumed event, so no failed goal turn
becomes a silent retry.

Only an execution-failure block arms an automatic resumption. Why: each
model-selectable blocked reason names a condition no retry can clear.

A chargeable failure resumes with fixed guidance to inspect durable state and
choose a different safe approach; an unchargeable failure resumes without
guidance and reuses the statement. Why: infrastructure recovery must not invent
a model instruction.

One execution-failure class requires an operator instead of automatic
resumption: a failed turn carrying the durable cause that no context-compaction
boundary fits the model window. Why: an unchanged successor would fail for the
same cause.

No goal-mode surface delegates work or creates child sessions, and the goal
events and commands reserve no delegation variant.

## Boundary contracts

When an execution failure blocks a session that has an owner, the daemon
automatically resumes the session within a bound. The execution-failure class
that requires an operator is excluded. The daemon derives the command identity
of that resumption from the session and the blocked event it responds to; it
never generates a new identity. A retry therefore cannot resume the session
twice.

The current state is derived only by replaying the session's append-only goal
event stream; no mutable goal-state column is authoritative.

On an unmonitored session an execution-failure block names no resumption. When
the session is adopted, the daemon appends an effective-need overlay naming the
scheduled resumption under the session lock and arms it; the blocked event
itself is unchanged.

The automatic-resumption run is the trailing alternation of execution-failure
blocks and the resumptions that answered them; every other event ends it, and a
resume carrying any identity other than the derived one is an operator's and
ends it too. Why: the planner and the attention projection must agree on this
definition.

Two independent limits end a run. The chargeable budget counts only failures the
session caused; a failure that durable evidence attributes outside the session,
including a context-compaction boundary the daemon owns, is not charged. The
lifetime ceiling counts every attempt, so a run whose every failure is exempt
still ends. The operator projection reads the same two limits, so it and the
planner end a run together.

The compaction cause is read wherever an execution-failure block is planned, not
only on the disposition path that fails the turn. Why: the direct disposition,
the reconciliation of a still-terminal turn, and the arming of an ambiguously
acknowledged block all plan blocks, and nothing else forces them to agree.

A goal turn whose credential pool is exhausted blocks with the ordinary
execution-failure reason when
[credential availability](credential-availability.md) selects no wait; when it
selects a wait, the turn remains the current goal turn and no event is appended.

A goal event a command authored projects the session's actor from that command's
issuer, so a resumption the daemon issues never reads as the operator's.

Every goal turn records the generation it belongs to, and a consumer reads that
recorded generation rather than the session's current one. Why: a supersession
while the turn is parked must not broaden what the consumer sees.

A synthesized statement's template is system-authored, but the identifiers it
renders come from the watched repository, so a consumer that places it in a
model prompt quotes it as it quotes any session text.

An achievement is gated on the session's finish check: a failing verdict appends
a block for the failed check with the check's result as its need, a passing
verdict commits a verified achievement to the session's terminal handoff in the
same transaction, and a declaration no check verifies commits a declared
achievement.

The command claim and replay protocol and the attribution rule are stated on
[identity and commands](identity-and-commands.md), the lock order on
[persistence protocol](persistence-protocol.md), and the parked-state rule on
[sessions and transcript](sessions-and-transcript.md).

## Planned

No committed unbuilt design is recorded for goal mode; undecided items are in
[open questions](../open-questions.md).

# Turn lifecycle and scheduling

This subsystem turns accepted inputs into turns, activates at most one turn per
session at a time, drives it, and recovers it after a crash.

## Overview

A turn is one durable logical request for one conversational outcome, made from
one accepted-input origin under one frozen effective configuration. A turn is
queued, active, or terminal, and a terminal turn carries one closed disposition;
the types live in `crates/domain/src/turn_lifecycle.rs`. A retired turn is a
queued turn that never activated. At most one turn per session is active, and it
holds the session's progressing slot. An active turn is running or parked in a
durable wait: on a tool approval, on a recovery decision after an ambiguous
operation, on a lost runner, or on a foreground delegated child. Every wait
retains the slot. Which credential a model call uses, and what happens to the
turn when none is available, is owned by
[credential-availability](credential-availability.md).

A turn attempt is one exclusive physical orchestration tenure;
`CurrentTurnAttempt` is the live attempt of a running turn. A turn has at most
one live attempt, and a tool round ends the attempt so that the next attempt in
the chain owns the tools and the next model call. An ended attempt records how
it ended, and the domain types make an end that contradicts its stop cause
unrepresentable.

Eligibility is derived from durable facts on every pass. Acceptance positions
and interrupt relations define a total order over a session's inputs
(`derive_accepted_input_total_order`), and the earliest queued turn is eligible
when no turn holds the slot. Activation is one transaction under the session's
scheduler lock: it fixes the turn's lineage, its starting context frontier, and
its initial attempt, and moves the row from queued to active. The lock protocol
is owned by [persistence-protocol](persistence-protocol.md).

The scheduler is a loop of per-session authoritative passes. An in-process nudge
after an accepted input whose applied result is a turn origin feeds it first,
and a periodic sweep over the durable rows (`PostgresEligibilitySweep`) backs it
up. The sweep finds four shapes: a queued turn with no active turn, an active
turn holding a prepared model call, an active tool round, a terminal pursuing
goal turn that still lacks its goal disposition. A pass activates a turn and
then drives its model call through the execution ports owned by
[model-call-execution](model-call-execution.md) and [tool-loop](tool-loop.md).

Every component deadline covers one physical operation. A running turn with no
model call, tool attempt, or durable wait outstanding is reached by none of them
and would hold its slot forever. A turn-liveness watchdog
(`crates/persistence/src/turn_liveness.rs`) observes such turns on a separate
timer and fails one whose evidence has not changed across repeated observations.
Two more checks run on their own timers at the same interval: one inventories
turns that hold a live operation past every deadline, and one retries
reconciliation of ambiguous operations.

At startup, before it admits any request, the daemon runs one transaction per
session with an active turn or a nonterminal standalone compaction call and
classifies the abandoned tenure from its durable evidence
(`crates/persistence/src/startup.rs`). The liveness watchdogs and the
scheduler's pass-expiry handoff hand turns to that same classification.

An input submitted while a turn holds the slot is rejected, recorded as pending
steering for the active turn, queued behind it, or applied as an interrupt,
according to the delivery mode the client chose. Wire shapes are owned by
[process-protocol](process-protocol.md) and command identity by
[identity-and-commands](identity-and-commands.md).

A context frontier snapshot is the ordered set of transcript entries a turn
starts from or a call was prepared against (`ContextFrontierId`). Every turn
start, model call, and terminal outcome of an activated turn names one, and the
scheduling projection validates the chain of snapshots, including compaction
results, before it trusts any stored start. Transcript entries and compaction
visibility are owned by [sessions-and-transcript](sessions-and-transcript.md).

A parent turn that awaits a spawned child in the foreground parks in an active
phase with no current attempt; a background await and inter-session messages
instead wake the recipient through a delegation-origin turn. Delegation
relationships and their cascade are owned by
[sessions-and-transcript](sessions-and-transcript.md); this page owns the
parent's wait, the wake turn, and their scheduling.

The daemon runtime acquires the single-daemon guard owned by
[process-protocol](process-protocol.md), migrates, completes the startup scan,
then binds its sockets and starts admission, dispatch, scheduling, and the
watchdog together.

## Design decisions

Eligibility is a derived predicate, never a durable state, because acceptance
positions, priority relations, and the active-slot owner are already durable and
a second eligibility state could only diverge from them.

The durable rows are the only queue and every in-process structure is a latency
hint, because acting on a false hint changes no rows and the sweep recovers a
lost true hint.

A separate timer, not a scheduler pass, owns turn liveness, because the sessions
it exists to reach are exactly the ones no pass is scheduled for.

Pending steering does not remove a turn from the quiescent shape. Why: a steered
but otherwise quiescent turn is wedged rather than working, and excluding it
would hide it from the watchdog.

No lifecycle table stores an activity timestamp; staleness is decided by
repeated observation of unchanged evidence.

The progress frontier is the outbox sequence rather than the transcript's,
because the outbox assigns its sequence in commit order and every session-scoped
transition lands there. An identity ordering would let a backward clock
adjustment or a skewed mint hide progress.

The progress scope is written as exclusions rather than as the kinds that count,
because counting an unrelated event only delays ending a wedged turn while
missing a real one ends a working turn.

An abandoned tenure with no outstanding operation fails, because it has no
completion, refusal, or confirmed-interrupt evidence and no path retries a call
automatically ([model-call-execution](model-call-execution.md)).

There is no process-incarnation column and no lease: under the single-daemon
contract every nonterminal attempt observed at startup is a prior-process
abandonment.

Frontier equality is identity rather than content, because two independently
created snapshots may hold equal entries without being the same fixed frontier.

The reconstitution seam rejects rather than repairs, because an omission inside
a claimed complete observation is indistinguishable from acknowledged work
disappearing; the general rule is on
[persistence-protocol](persistence-protocol.md).

Signal-driven shutdown affects latency, not correctness: abrupt exit is safe
because durable rows plus the next guarded startup scan recover work and the
outbox cursor redelivers.

A goal stop or supersede command is a goal-state transition and creates no
turn-interrupt authority ([goal-mode](goal-mode.md)).

Single-source native-fork ancestry is unschedulable and fails reconstitution.
Imported ancestry does not alter lifecycle order, eligibility, slot ownership,
or lineage, because it is creation provenance, not a scheduler mode.

The slot-held inventory excludes every durable parked phase, including the
tool-approval wait, because approval remains the judge's surface.

The watchdog introduces no terminal state, disposition, or direct row edit; it
hands a due turn to the shared recovery transaction, which reconstitutes and
classifies the exact current durable shape.

Automatic reconciliation and the user reconciliation command neither claim what
the provider or tool did nor rewrite the ambiguous operation; both reclassify
pending steering through the same terminal boundary.

The stop-turn request is the client surface for an interrupt and adds no
authority beyond the interrupt treatment under Contracts.

No runner-loss projection turns ambiguous effect evidence into known failure.

No transition terminalizes a turn directly on a fatal mismatch; the domain
derives the values and nothing commits them.

A queued turn never fails without activating; activation is the only eligibility
outcome.

## Boundary contracts

Order comes from commit-ordered sequences, never from comparing wall-clock
times. A liveness check that cannot query some kind of evidence skips the turn
instead of ending it, and any event it does not recognize counts as progress.
When a lifecycle guard trips on an admitted session, the daemon waits, asks, or
parks the session; it never ends work on staleness evidence alone.

The only way to derive a new transcript snapshot is to append to the old one, so
every earlier entry stays in order. Two frontiers are equal only if they are the
same frontier; comparing content is a separate explicit operation. Compaction
changes which entries are visible to the model, never what is stored. A summary
cannot hide an unsummarized prefix, and its end boundary must close every tool
exchange it covers.

A retired turn contributes no terminal frontier and stays out of queue order and
predecessor selection. A completed tool-using model round ends the current
attempt as a tool-round yield; approval completion creates the next attempt in
that chain without creating a new turn.

The authoritative pass reconstitutes one complete session-scoped scheduling
projection and fails closed on any omission or cross-wiring. Queued turns store
no predecessor pointer; the immediate predecessor is fixed once, at eligibility.
Lineage is first-in-session when the session has no earlier turn; otherwise it
names the exact terminal turn ordered immediately before it. The starting
frontier is the predecessor's terminal frontier, then a model-identity boundary
exactly when the frozen direct model differs from the predecessor's, then the
origin entry. A first-in-session turn starts with the origin entry alone or,
under imported ancestry, with the imported seed frontier followed by the origin
entry; the first native turn is first-in-session, imported entries are a context
prefix and never a synthetic predecessor, and no other lifecycle check depends
on ancestry. Imported ancestry is admitted only when its seed satisfies the
imported-session contract on
[sessions-and-transcript](sessions-and-transcript.md). An
interrupt-immediately-after origin proves its named predecessor and may precede
queued inputs with lower acceptance positions. The application layer supplies
fresh identity candidates for each pass and never selects a target turn.
Turn-start entries, snapshot, start, active slot, and attempt commit in one
transaction, so no start references a missing or partial snapshot.

The admission cap bounds concurrent per-session passes, not the durable queue; a
cap of zero pauses execution and an absent cap admits every eligible session.
Each pass first reconciles an active running tool round, then drives a retained
prepared model call, and only then activates a queued turn; failure of either
lookup is an ordinary failed pass, and only a failure after active-turn
execution begins trips fatal recovery supervision. A pass releases its slot
during attachment or blob-store I/O and reacquires one before send
authorization, and its guarded transaction revalidates authority. A
model-originated blob read authorizes no later send, so it reacquires its slot
before the correlated tool result commits. A pass that cannot immediately get an
attachment-preparation permit ends and leaves only the durable prepared row for
a later sweep. When a pass exceeds its occupancy bound, the handoff invokes the
startup-recovery transaction only for a turn whose attempt and turn-progress
frontier did not change between two observations, and a resumability read that
does not settle counts as a resumption; a pass that expires inside
pre-activation compaction instead hands off only the exact compaction call that
window made durable.

A quiescent candidate is an active turn with an accepted-input origin in the
running phase, with no tool round, approval, or recovery attempt, and no live
model call or tool attempt. Neither the quiescent nor the slot-held inventory
considers a turn whose session is parked. A turn is due only once its evidence
has been observed unchanged for at least the bound governing its watchdog; the
configured staleness bound governs only the quiescent watchdog, and the
slot-held watchdog uses a separate fixed ceiling. Every turn is observed on
every scan, and a turn missing from one scan has left the quiescent shape and is
forgotten rather than credited on return. A rotation that cannot be drained ends
the scan with no decision and the ledger unchanged. A scan whose reads outlast
the interval delays the next scan rather than overlapping it. Terminalizations
run one at a time and the next scan waits for the last, so a scan ends at most
the configured window of turns and leaves the remainder. A lap fixes its
membership from the turns due when it opens, and successive scans consume that
membership until it is exhausted, so every member still due is attempted before
the next lap begins. No scan interval leaves the liveness task idle until
shutdown; no staleness bound leaves automatic reconciliation active without
stale-turn terminalization. A due quiescent turn ends through the same committed
failed-turn transition startup recovery commits, with the candidate predicate
re-decided under the scheduler lock. Terminalization emits a key-bearing
operator log line with the cause code `turn_liveness_watchdog_stale`, the
session, the turn, and the bound in force; that code is reserved for a committed
terminalization, a candidate left alone carries
`turn_liveness_candidate_superseded`, and an unacknowledged commit carries
`turn_liveness_terminalization_ambiguous`. A due slot-held turn is instead
handed, with its evidence revalidated under the scheduler lock, to the startup
classification of its current durable shape and takes that classification's
outcome; a prepared call stays resumable and in-flight work ends ambiguous and
parks.

If a daemon disappears while a reconciliation attempt is in progress, its
recorded deadline lets the next daemon classify it as an infrastructure failure.
A concurrent operator decision or other authoritative transition wins by
ordinary row locking and records the automatic attempt as superseded. When the
configured attempt budget is spent, the recovery row becomes exhausted, the wait
remains unchanged, and the process transcript sets operator action required.

Startup acquires the single-daemon guard, fences the prior pool incarnation once
the fence migration has run, runs the remaining migrations, completes the
generic scan, initializes every configured blob store, marks prior-process
runner connections lost, binds the runner socket, binds the process socket, and
then starts enrollment, admission, dispatch, and scheduling concurrently. No
request, dispatch cursor advance, scheduler pass, or runner admission occurs
before recovery completes. Any phase failure is a failed startup with a
classified, key-bearing log line and a failure exit code.

Each scan transaction classifies the lost tenure by its durable evidence and
never fabricates a live end. A running turn with no model call ends its attempt
lost and fails. A turn holding a prepared call proves no send authorization
existed, so startup validates its frontier and leaves call, attempt, and turn
for the scheduler to retry. A turn holding an unstopped in-flight call ends the
call ambiguous and the attempt lost, and stays active in the model-call recovery
wait with no failure entry or frontier; the transaction appends the call's
terminal transition event and no turn event, and the scan reports the session as
awaiting a recovery decision. A stop-requested attempt with a
cancellation-requested call ends both and terminalizes reconciliation-required
with that call as its exact ambiguity set. A turn already parked in the
model-call recovery wait is not reclassified; the transaction rolls back and
reports the session as awaiting a recovery decision. An approval wait remains
parked unchanged. A running tool attempt follows its stored effect class:
prepared or effect-free work closes known-failed and fails the turn, and
in-flight external-effect work closes ambiguous and parks. A running tool batch
whose requests are all resolved with no current tool attempt is returned as
resumable work for a scheduler pass. In the two failing branches only, one
failure entry is appended, preceded in the tool branch by one correlated result
entry per request in proposal order. Identity collisions are retried with fresh
candidates; infrastructure failures and fail-closed corruption stop startup
visibly. The scan is idempotent: a rerun inventories only work still active, and
a stale observation rolls back.

Every terminal transition of a source turn, whether by interrupt, model-call
outcome, startup recovery, or the watchdog, reclassifies its pending steering
rows into queued successor origins in ascending acceptance position, inheriting
the source turn's configuration, except a committed closure handoff, which
closes them not delivered. Steering pending on a due turn therefore never
refuses or blocks a terminalization.

A start-when-idle input against an occupied slot records an active-turn-present
rejection, an active-work mode against an idle slot records no-active-turn, and
a stale expected active turn records an active-turn mismatch. A next-safe-point
input is recorded as pending steering bound to the exact active source turn,
with its acceptance position derived from the validated tail, and creates no
turn. An after-current-turn input creates an ordinary queued origin turn with
frozen configuration and an immutable acceptance position and fixes no
predecessor until eligibility; while the source turn holds the slot it cannot
activate, and after that turn terminalizes queued origins activate in ascending
acceptance order. Every accepted input settles one injection receipt: origins
delivered at acceptance, steering delivered when consumed or reclassified,
closures not delivered, rejections rejected. An interrupt targeting the active
turn atomically accepts a configured immediate-successor origin, constructs the
applied-interrupt proof, and applies the predecessor transition. A matching
interrupt against a recovery-decision wait preserves the terminal ambiguous call
and ended attempt, records the proof on the reconciliation marker, and
terminalizes reconciliation-required; the reconcile-turn request is the only
operator surface that supplies it and is admitted only for a turn observed
parked in the model-call recovery wait. The authoritative transaction
revalidates the expected active turn under the scheduler lock and records an
active-turn mismatch, or no-active-turn when a winning decision emptied the
slot. Equal interrupt replay returns the original applied result; a distinct
later interrupt records interrupt-already-applied without accepting an input or
replacing the proof. An interrupt delivered while the active turn is parked on a
tool-approval wait records interrupt-unavailable without accepting an input.

An active turn already at a runner boundary moves to the runner-recovery wait
when its placement is marked lost; the loss projection is owned by
[runner-protocol](runner-protocol.md).

Reconstitution rejects any stored snapshot whose resolved membership disagrees
with the complete entry set, so one identifier never resolves differently.
Before validating any stored turn start, the scheduling scan reconstructs every
compaction call, summary entry, source and result snapshot, summarized range,
and predecessor link. Every compaction record requires its terminal completed
call and an exact source-plus-summary result, and the predecessor chain must be
single-rooted, linear, and prefix-preserving. A standalone prepared or in-flight
compaction call blocks ordinary activation until startup terminalizes it
known-failed or ambiguous, creating neither a summary nor a result frontier.
Compaction authorization and terminalization serialize on the session row and
exactly replay an already-landed transition after an ambiguous commit.
Unreferenced snapshots and compaction records fail closed. A historical start's
prefix is the immediate predecessor's terminal snapshot or the imported seed, or
the validated compaction chain tip's result when the start follows it; a start
committed before the chain tip stays admissible only when its entire frontier is
an exact semantic prefix of the tip's source. The only remaining start suffix is
the model-identity boundary when applicable followed by the turn's exact origin;
a summary entry is never an extra suffix. New eligibility uses the unique latest
compaction result when it preserves the applicable seed or predecessor terminal
prefix. Imported ancestry resolves only through the checked session-creation
producer, and its one-to-one seed must name the exact stored frontier whose
membership matches the selected prefix.

Stored active phases are conclusions derived from complete owner facts, never
trusted discriminators. A recovery-decision wait reconstitutes from an ambiguous
terminal model call or tool attempt correlated with its ended turn attempt; an
ambiguous continuation call is admitted when its whole frontier is the round's
batch-correlated result projection, and the wait extends it by no entry. A
stop-requested current attempt reconstructs only when its interrupt command,
predecessor, configured successor, applied result, and cancellation-requested
call form the exact proof. An approval wait reconstructs only from the complete
tool batch proving its earliest undecided request and the absence of a live
attempt; a bare wait subject cannot become a phase. A failed turn's provenance
is validated fail-closed rather than accepted as an evidence-free failure
record, and a deferred assertion re-closes the shape at every commit. A
tool-denied result entry whose request lacks an exact user-sourced deny
resolution fails reconstitution rather than fabricating a denial. A failed turn
may instead name the round's own continuation call, accepted exactly when the
terminal frontier extends the call frontier by the failure marker alone; a
round-completed continuation window never contains a turn-end closure. A
cancelled turn reconstructs only when its ended attempt carries the cancelled
end and the same complete applied-interrupt result as the disposition; it names
no call, its one correlated terminal cancelled call, or, when cancellation
terminalized a tool round, that round's completed producing call, and its
terminal frontier extends the starting or call frontier by exactly the
cancellation marker, preceded, when cancellation terminalized a tool round, by
one result entry per request in proposal order. A refused turn names its ended
attempt and correlated terminal refused call, and its terminal frontier is an
equal-content boundary over that call's frontier. A reconciliation-required turn
names its ended attempt and exactly one terminal ambiguous model call or tool
attempt; the attempt end is lost or ambiguous without a stop, with a later
applied interrupt or a durable automatic recovery attempt, or it is a
cancellation end carrying the interrupt proof. Automatic reconciliation
authority binds the exact session, turn, and the model call or tool attempt it
reconciles. A model-call reconciliation terminal frontier is an equal-content
boundary over the ambiguous call's source frontier; a tool reconciliation adds
one result per request with the ambiguous request closed. A consumed steering
input reconstitutes only against its exact consuming call, whose frontier is the
start or round-result projection extended by the consumed entries in acceptance
order; a consumer that completed by proposing a tool round stays correlated
through its validated assistant history for the rest of the turn. Every active
turn's projection carries a session-scoped acceptance tail anchored at the
turn's origin and extending gap-free through the last observed acceptance
position; a position consumed by the predecessor remains in that tail after a
queued origin activates, and only steering consumed by the new active turn
enters its execution aggregate. A tail entry recording an accepted interrupt is
admitted only when the current stop or recovery state carries its exact proof.

A spawned child's first turn has a closed delegated-task origin naming the exact
spawning request, with a starting frontier containing the delegated-task entry
and no synthetic accepted input. The child wait retains the parent's slot and
survives restart unchanged until one deliverable terminal result or an applied
interrupt terminates the parent turn. Result consumption atomically appends the
delivered-result entry, returns the result as that tool request's content, and
moves the same turn back to running with a fresh continuation attempt. A
delegated-task or delegation-wake turn owns the same session-local active slot,
so input submission uses that exact turn for mismatch, rejection, steering, and
interrupt predecessor checks. A background await commits a delivery registration
and returns its receipt immediately, creating no child wait and retaining no
slot; the child's result commit appends one parent-scoped wake event and makes
the parent eligible for a delegation-origin turn. An active recipient consumes
its inter-session message inbox in order at the next model-call safe point.
Pending messages and background results are ordered by their shared, gap-free
recipient delivery sequence, not by relationship-local ordinals. An idle
recipient has at most one queued delegation-origin wake turn; later items
coalesce into its starting frontier until activation. Reconstitution checks
delivery sequence, message ordinal, relationship, sender and recipient, and
semantic-entry identity before any content becomes model-visible. Startup and
the sweep recognize child waits, pending inbox content, and undelivered results
from durable rows and infer nothing from child transcript state or process
memory.

Losing the guard session is a fatal fencing event: admission, dispatch, and
scheduling are cancelled without the graceful window and the process exits
rather than reacquiring. On SIGINT or SIGTERM the listener stops accepting
requests, follow streams close, the dispatcher stops starting transactions, the
scheduler stops admitting passes, and liveness stops scanning. Finite handlers,
the current dispatcher transaction, in-flight scheduler passes, and an in-flight
liveness read or terminalization share a grace window of the configured longest
model exchange plus a fixed cleanup margin. An admitted pass stops spending its
occupancy bound and drains under that window. After its in-flight operation
reaches a durable boundary, a pass checkpoints the active turn and returns
without issuing another, and a successor resumes from that boundary.

## Planned

- Runner-loss recovery: replacement and abandonment of a lost runner, and the
  runner-loss projection's effect on queued activation and runner execution;
  design in
  [turn-lifecycle-and-scheduling design](../design/turn-lifecycle-and-scheduling.md).
- Recovery-only startup: a runner reconciliation phase between migrations and
  the generic scan; design in
  [turn-lifecycle-and-scheduling design](../design/turn-lifecycle-and-scheduling.md).
- The credential-pool availability wait as a distinct active phase, with its
  attempt yield, scheduler wake conditions, and release; design in
  [turn-lifecycle-and-scheduling design](../design/turn-lifecycle-and-scheduling.md).
- The instruction-eligibility freeze in the activation transaction and the
  replacement command's lock order; design in
  [turn-lifecycle-and-scheduling design](../design/turn-lifecycle-and-scheduling.md).

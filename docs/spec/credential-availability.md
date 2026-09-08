# Credential availability

Credential availability decides how a model call that draws its credential from
a configured pool ends when the pool admits a member, admits none, or a member's
call fails with a cause the pool can route around.

## Overview

A credential pool is a ranked set of credential profiles a session's model calls
may use. Its grammar, ranking, trigger vocabulary and admission rules are owned
by [configuration and credentials](configuration-and-credentials.md). This page
owns the endings of a credential-pool selection attempt.

The machine runs in `crates/persistence/src/model_execution.rs` at two points.
Preparation resolves the session's pool, admits a member, and pins the policy
snapshot to the call. A fresh admission reselects the member the session's most
recent call on that pool used while that member remains admissible; otherwise
selection applies the pool's priority and tie-break rule to admissible members.
The commit that closes a failed call applies the action the pinned policy fixes
for the failure's cause, and either terminalizes the turn or prepares a
successor attempt, whose member is admitted and call created at that attempt's
preparation.

An availability chain begins at a fresh admission inside one turn and holds the
call that admission prepares, if any, and every successor that follows a
qualifying failure. [Model-call execution](model-call-execution.md) owns what
bounds a chain and when a turn starts a fresh one.

The durable records are the policy snapshot pinned to each call, one exclusion
row for each member a qualifying failure in the turn removed, one successor row
linking a prepared successor attempt to the predecessor call and its cause, and
one exhaustion header when a turn fails because the pool admitted no member.
Members are also removed by the exclusions the pool's trigger policy writes,
which [configuration and credentials](configuration-and-credentials.md) owns;
the operator surface that clears them is on
[process protocol](process-protocol.md).

Pool policies have immutable identities; each clearable exclusion retains its
generation and origin, and selection ignores cleared generations.

An admission selects a member, selects an exhausted wait, or fails. A bound
same-credential retry whose credential is excluded before preparation fails
before wait selection; `park` applies only to pool admission. A released wait
that selects no member or wait fails through a fresh attempt, retaining a
predecessor provider cause exactly when its chain issued a call.

## Design decisions

A rejected daemon-owned OAuth refresh or a credential-home identity that failed
its walk never enters this machine: each precedes any provider request, is typed
as its own failure and quarantines the profile under
[configuration and credentials](configuration-and-credentials.md). Why: a
deployment misconfiguration is not a provider condition the pool's trigger
policy routes around. A `file` delivery that cannot produce a usable credential
quarantines nothing; its prepared call fails with that typed cause and reaches
terminal.

## Boundary contracts

Every credential-pool selection attempt ends in exactly one ending of this
machine, and that ending fixes every projection any specification page states of
it.

Turn phase and attempt disposition belong to
[turn lifecycle and scheduling](turn-lifecycle-and-scheduling.md); continuation
origins and durable records to [persistence protocol](persistence-protocol.md);
transcript producers and entries to
[sessions and the transcript](sessions-and-transcript.md); snapshot state, live
events and rejection details to [process protocol](process-protocol.md);
terminal evidence and the outcome it derives to
[runtime substrate](runtime-substrate.md) and
[model-call execution](model-call-execution.md).

A condition that asks whether a call has already been issued is scoped to the
availability chain, not to the turn;
[model-call execution](model-call-execution.md) owns what bounds a chain.

Selected: a member was admitted. The selecting preparation inserts the call's
prepared record, adds no turn phase, and consumes the pending `switch_next_turn`
displacement of the member it excluded, never of the member it selected, because
the displacement is the excluded member's record and consuming any other leaves
it pending forever.

Pre-call fail: the attempt that finds every member excluded is call-free, either
a fresh chain's admission or the later preparation of a deferred successor. The
turn terminalizes Failed and that attempt ends KnownFailure. The pre-call
exhaustion producer appends the `TurnFailed` transcript entry after the ended
attempt's starting frontier in the transaction that terminalizes the turn. The
ending carries no terminal evidence, because the attempt that ends issued no
provider request and no other attempt's call is its evidence; its terminal cause
is pool exhaustion, never a provider failure.

The pre-call exhaustion record names the pool-policy revision the admission
resolved and carries contiguous member rows in policy order, each naming the
member's exclusion, widest scope first. Partial, foreign or stale evidence fails
reconstitution closed. The terminalizing commit emits `turn_failed` and
`turn_credential_pool_exhausted`; its header retains the resolved pool-policy
identity and member evidence rows attach in a separate table.

Post-failure fail: the observation that closes a qualifying provider failure
finds every member excluded. The turn terminalizes Failed and adds no further
attempt; the predecessor attempt has already ended KnownFailure. The terminal
evidence is the last observed provider cause as a provider error, never pool
exhaustion, because reporting the pool's emptiness would discard the only
evidence naming what failed. The rotation test in
`crates/persistence/tests/postgres_integration/model_call_execution_and_recovery.rs`
pins this ending.

Successor: a stop was not requested, and either a transient cause has
same-credential attempts remaining with adapter proof that the provider never
accepted the request, or the pinned action is `switch_now` and a member remains.
`switch_now` also admits credential rejection without separate non-acceptance
proof; rejection never admits same-credential retry. Every other availability
successor requires the adapter's [non-acceptance proof](runtime-substrate.md);
for Codex this requires the typed failed-turn gate, without assistant activity
or an earlier retryable error. The turn stays active and keeps its slot; the
predecessor attempt ends KnownFailure without terminalizing, and the same commit
prepares a successor attempt. That commit appends no `TurnFailed`: one commit
never both terminalizes the turn and authorizes a successor. The rotation and
transient retry tests pin this ending.

A same-credential retry additionally requires the failed member itself to remain
admitted when the observation commits. If a durable action already excludes it,
the pinned `switch_now` action may rotate to another admitted member; every
other pinned action terminalizes the known failure.

A transient successor commit writes a credential-scoped durable exclusion whose
reset equals the successor's durable retry deadline. Every session's call
preparation skips that credential until the reset passes; after it passes, the
member is admitted again. A chain exclusion is written when a failure rotates
the pool and removes that member for the remainder of the turn. If another
durable action excludes a retry successor's credential before preparation, that
successor fails instead of selecting another member; when a fallback member
remains admissible, the failure keeps the generic failed projection and records
no pool exhaustion.

A successor prepared after a rate-limit, overload or provider-internal failure
waits the greater of the provider's reported delay and a local exponentially
increasing jittered delay, each capped at five minutes
(`MAX_AVAILABILITY_BACKOFF` in `crates/persistence/src/model_execution.rs`). A
successor after a quota failure is immediate. Quota and authentication failures
bypass same-credential retry and apply their pinned actions immediately. A
same-credential retry derives local backoff from that credential's recorded
attempt count; rotation starts the successor's backoff count at one.

The required finite positive
`numeric_bounds.max_same_credential_attempts_per_turn` bounds recorded calls on
one credential in one turn. At the bound a transient failure applies its pinned
action instead of readmitting that credential.

Terminal: a known failure no successor is authorized to follow terminalizes the
turn Failed exactly as it would with no pool. A stop request, missing pre-stream
proof except for credential-rejection rotation, non-transient cause without
`switch_now`, or transient cause at its bound without `switch_now` authorizes no
successor.

Exhausted-wait: no member is admissible and the frozen policy selects a wait.
Exhaustion under `fail` never selects a wait; `park` selects one only when some
member's every active exclusion can be cleared by a wake. A chain exclusion
never qualifies. The attempt ends call-free WithoutStop(YieldedToDurableWait),
the active turn keeps its session slot, and the transaction appends no
transcript entry.

A fresh availability chain resolves the current catalog; calls and waits retain
the policy identity governing their chain. The wait retains its latest frontier,
complete member exclusion snapshot and optional deadline. A member contributes a
deadline only when every active exclusion expires: its deadline is their latest
reset, and the wait's is the earliest member deadline. Chain exclusions,
displacements and quarantines do not expire by time passage.

An eligible wait reruns admission against current exclusions under the session
lock. Re-parking rewrites the same wait's evidence and deadline without another
attempt. Successful release consumes the wait and prepares the selected call on
a fresh immediate-successor attempt in one transaction. Its wait-release origin
retains any predecessor call and non-acceptance proof; release never readmits a
chain-excluded member.

A release that selects neither a member nor another wait consumes the wait,
opens a fresh call-free attempt, ends it KnownFailure, reclassifies pending
steering as queued successors, and terminalizes Failed. Without a predecessor
call its producer and evidence are pre-call exhaustion's; with a predecessor
call its cause is that provider failure, never pool exhaustion. An accepted stop
instead consumes the wait, opens a fresh immediate successor with its applied
interrupt proof, ends it AfterCancellation(Cancelled), and appends TurnCancelled
after the wait frontier while reclassifying pending steering.

A due deadline makes a wait eligible through the scheduler's existing
reconciliation sweep. A durable member-availability update or a successful
operator clear grants eligibility to waits naming that member in the same
transaction. Eligibility prepares no call and consumes no wait. Startup alone
leaves exhausted waits unchanged; deadline-free waits have no timer.

Contended-wait: no member is admitted and at least one otherwise-admissible
member is skipped only for its configured invocation bound. Either exhaustion
policy enters the same call-free wait with cause contended, retaining every
bounded member and its reservation identities alongside excluded members.
Preparation reserves only its selected `codex_home` member; invocation
completion releases that reservation and grants eligibility to contended waits
naming that bounded member in one transaction. Competing releases admit only the
waiter that acquires capacity. Re-parking recomputes all exclusions, reservation
identities and the deadline. When every bounded member becomes excluded,
admission reruns the exhaustion policy and converts to exhausted-wait in place
or terminalizes. Startup re-evaluates contended waits against current
registrations and retained reservations; restart alone grants no eligibility.
The daemon rechecks retained invocation groups until their exit permits
reservation release.

A parked turn projects `active_awaiting_credential_availability` with its ended
wait attempt and closed cause. Transcript reads and initial follow snapshots
retain the active turn and its slot without rejection detail. A released wait
uses its retained effective serving target with its retained pool policy through
call preparation and send authorization.

A terminal release after a predecessor call projects
`failed_after_credential_wait` with the fresh terminal attempt and the
predecessor's provider failure. A release without a predecessor uses the typed
pre-call exhaustion state and live event.

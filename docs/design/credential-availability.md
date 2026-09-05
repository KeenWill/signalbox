# Credential availability design

This design is not built; it extends
[credential availability](../spec/credential-availability.md) with the wait
endings a pool configured to park reaches, their release, and three smaller
items the built pre-call exhaustion ending owes.

## Goal

A turn whose pool admits no member and is configured to park waits durably for a
member to become admissible instead of failing, keeps its session slot, and
resumes the chain that parked when a wake arrives. Capacity reservation bounds
concurrent invocations per member and makes contention a wait distinct from
exhaustion. The pre-call exhaustion ending gains a typed live event and
per-member evidence rows.

## Design

The machine's endings partition on questions asked in order: did selection admit
a member; if not, was any otherwise-admissible member skipped only for its
concurrency bound; does the exhaustion select a wait; is the admission a fresh
one or the release of a parked wait; and was a call issued, by the failing
attempt at an admission or by this chain at a release. The four endings below
join the five the spec page states, making nine, and nothing else divides them.
The release question divides only the endings that select no wait: a release
that selects a member reaches selected, and one that re-parks re-enters the wait
ending, because neither ending's projections change with the path that reached
it.

Contended-wait: nothing is admissible and at least one otherwise-admissible
member was skipped only for its bound. The turn enters the
credential-availability wait phase with closed cause contended. The attempt ends
call-free WithoutStop(YieldedToDurableWait); the turn keeps its slot, appends no
transcript entry, and is not terminal. Five wakes make it eligible to re-run
admission: a reservation release by one of the bounded members the wait names;
the wait's deadline, derived over the exclusions it records by the same
per-member rule as an exhausted wait's; startup's re-evaluation of retained
contended waits against current registrations; a durable member-availability
update; an operator clear of an exclusion. A wake grants eligibility rather than
release: the wait is consumed by the rerun of admission that selects a member
and prepares its call in the same transaction, and where bounded members compete
for one freed reservation, only by the transaction that acquires it. The last
two wakes matter because the wait also records excluded members, and one can
become admissible while every bounded member stays saturated. A restart alone is
not a wake.

Exhausted-wait: nothing is admissible, nothing was skipped merely for a bound,
and a wait is selected. The same wait phase with closed cause exhausted, and the
same attempt disposition. Its wakes are the deadline, a durable
member-availability update, and an operator clear of an exclusion. The deadline
is computed per member, not per exclusion: a member becomes admissible by time
passage exactly when every one of its active exclusions expires at the reset it
reports, that is, at the latest of those resets. The wait's deadline is the
earliest such time across the members that qualify. A chain exclusion, a
`switch_next_turn` displacement and a profile quarantine never contribute a
deadline, because none of them expires by time passage. A chain exclusion clears
at turn end, a displacement when another member is prepared or an operator
clears it, and a quarantine by operator command. The wait is deadline-free only
when no member can become admissible by time passage, and no timer ends a
deadline-free wait.

Wait-transition fail (no call): a released wait finds the pool exhausted, no
wait is selected again, and this chain has issued no call. The wait's own
attempt is immutable, so one transaction consumes the wait, opens a fresh
call-free attempt, ends it KnownFailure, reclassifies any steering still pending
on the source turn as a queued successor, and terminalizes the turn Failed. Both
terminalizing releases owe that reclassification, as the parked-stop transaction
does. The fresh attempt's continuation origin is the wait-release origin naming
the consumed wait; the unique continuation chain is total over attempts, so an
attempt without an origin could not be reconstituted. The producer, records,
wire shape and evidence are pre-call fail's, plus the consumed wait.

Wait-transition fail (after call): the same release and exhaustion where this
chain had already issued a call. The predecessor attempt ended KnownFailure
earlier without terminalizing and the wait's attempt is immutable, so the same
fresh call-free attempt is opened and ended KnownFailure and the turn
terminalizes Failed. The continuation origin is the wait-release origin naming
the consumed wait and this chain's predecessor call, its qualifying cause and
its non-acceptance proof. The producer is the wait-transition failure producer
of `TurnFailed`, differing from the pre-call producer only in naming the
predecessor call that supplied the cause. The terminal evidence is the
predecessor's provider cause as a provider error, never pool exhaustion. This
ending owes a wire shape that correlates the predecessor call with a terminal
attempt that owns no call; the built failed shape carries a provider cause only
inside a nonnull terminal model call, so it cannot serve.

Wait selection: `park` and `fail` act through one question, whether this
exhaustion selects a wait; `fail` never selects one, and `park` selects one only
when some member's every active exclusion is one a wake can clear, so that one
wake can readmit that whole member. No wake clears a chain exclusion, so a
member holding one never qualifies, whatever else it holds. A pending
`switch_next_turn` displacement is clearable, because an operator clear removes
it and publishes the member-availability update that wakes the wait. Where no
member qualifies, no wait is selected and the exhaustion ends in a failure
ending exactly as a `fail` pool would. At an admission the failing attempt
chooses which, pre-call fail when that attempt is call-free and post-failure
fail when the observation closing a provider failure finds the exhaustion; at a
release, whether this chain has issued a call chooses wait-transition fail (no
call) or wait-transition fail (after call).

A contended wait becomes exhausted when a woken waiter finds every formerly
bounded candidate durably excluded, and then re-runs the exhaustion policy
rather than staying parked: the pool's configured value decides afresh, a wait
that is still selected is rewritten in place to the exhausted form, and a `fail`
pool terminalizes.

Release: releasing a parked wait resumes the chain that parked and re-evaluates
admission from current state. A chain exclusion stays as the spec page states
it; every other exclusion the wait recorded is re-read from its current active
state at release, so a passed reset or an authorized clear readmits that member.
The release origin carries the predecessor call, cause and proof exactly where
this chain had observed a qualifying failure before parking, because the release
may then select a remaining member only as that failure's authorized successor.

Cancellation leaves this machine rather than ending inside it. An accepted stop
against a parked wait consumes the wait, creates a fresh immediate-successor
attempt, records the applied-interrupt proof, ends that attempt
AfterCancellation(Cancelled), appends `TurnCancelled` after the wait's latest
frontier, and terminalizes the turn Cancelled, under
[turn lifecycle and scheduling](../spec/turn-lifecycle-and-scheduling.md).

Storage: one wait row per parked turn, of form contended or exhausted, carrying
the frozen policy identity, every durable exclusion that removed a member, and
the optional deadline; the contended form also carries the complete nonempty set
of otherwise-admissible bounded members with their reservation identities. A
capacity reservation is one per-member invocation reservation, taken by the
selecting preparation for the selected member only and released when the
invocation ends; no reservation is taken against a member no invocation will
start. A contended wait has six committing transactions that are never
conflated: the admission that finds every candidate at its bound and inserts the
wait; the rewrite from an exhausted wait whose cleared exclusion leaves the
newly admissible member at its bound, which recomputes the complete contended
snapshot from current state — the remaining exclusions, the derived deadline,
and every otherwise-admissible bounded member with its reservation identities;
the reservation completion that frees capacity and makes the waiters it wakes
eligible; the evidence rewrite by which a woken transaction that still finds
every candidate at its bound replaces the wait's reservation identities,
exclusion evidence and derived deadline from current state and stays parked; and
the release, the call preparation that puts its Prepared call on a fresh
successor attempt and consumes the wait in that same transaction, inserting a
reservation only where the selected member is a `codex_home` one. An exhausted
wait has five: the admission; the rewrite from a contended wait; the evidence
rewrite by which a woken transaction that reruns admission and still selects an
exhausted wait replaces the wait's exclusion evidence and deadline from current
state and stays parked, so a past deadline never wakes it again; and the
release. Either form also has a terminalizing release, where a rerun of
admission that selects no wait consumes the wait, opens and ends the call-free
failure attempt, and terminalizes the turn as wait-transition fail (no call) or
(after call). Lock order is
[persistence protocol](../spec/persistence-protocol.md)'s.

Wire: a parked turn projects an active transcript turn state that retains the
turn and its slot, never a terminal one, and no rejection detail. Pre-call fail
and wait-transition fail (no call) project a turn state naming pool exhaustion,
the live event `turn_failed`, and a typed `turn_credential_pool_exhausted` live
event. Neither ending owns a call, so the member evidence is read through the
pool-policy revision the admission resolved, which the failure or wait record
carries. The read's rejection detail for a revision it cannot resolve names the
session, turn and policy. Which member served an ordinary selection, and whether
a completed successor chain is shown to a client, stay undecided in
[open questions](../open-questions.md).

Per-member evidence: the pre-call exhaustion record names the pool-policy
revision the admission resolved and carries contiguous member rows in policy
order, each naming the member's exclusion, widest scope first. Partial, foreign
or stale evidence fails reconstitution closed.

Park on the pre-call path: a fresh admission that finds the pool exhausted
consults the exhaustion value of the pool-policy revision it resolved. `park`
selects a wait under the wait-selection rule above; `fail` terminalizes as the
spec page states.

## Compatibility constraints

- A chain exclusion stays insert-only and turn-local. The release rule re-reads
  every other exclusion from current state and depends on chain exclusions never
  being cleared; no code path may delete or expire one before the turn ends.
- The admission resolves the session's immutable pool-policy revision before any
  call exists; a wait is selected from that revision's exhaustion value, never
  from live configuration, and the wait row carries that revision as its frozen
  policy identity. A prepared call pins the same revision.
- A parked wait reuses the WithoutStop(YieldedToDurableWait) disposition that
  completed tool-bearing calls and runner-recovery waits already write; the wait
  row, not the disposition, identifies a credential-availability park, and no
  reader may infer one from the disposition alone.
- The pre-call exhaustion header row gains the resolved pool-policy identity and
  otherwise keeps its present shape; member evidence rows attach to it in a new
  table rather than replacing it.
- The pre-call producer's commit shape, a `TurnFailed` appended after the ended
  attempt's starting frontier in the terminalizing transaction, is reused
  unchanged by wait-transition fail (no call).
- Wait-transition fail (after call) keeps the predecessor's provider cause as
  its terminal evidence. The built preparation path classifies every call-free
  exhaustion as a pre-call one carrying no provider cause, so this ending needs
  a closure that preserves it.
- One commit never both terminalizes a turn and authorizes a successor, and a
  wait commit appends no `TurnFailed`; a parked turn is never indistinguishable
  from a terminal one.
- Every attempt a release opens names the wait-release origin; the continuation
  chain stays total over attempts.
- The contended re-park rewrite extends the storage contract in
  [persistence protocol](../spec/persistence-protocol.md): it replaces the
  wait's exclusion evidence and derived deadline as well as its reservation
  identities.
- A release extends the built preparation transaction, which admits a member and
  inserts its `Prepared` call on an attempt that already exists, to also open
  the successor attempt and consume the wait; no release commits a `Prepared`
  attempt that owns no call.

## Acceptance criteria

- A `park` pool whose exhaustion snapshot skips no member only for its
  concurrency bound and holds a member whose every active exclusion expires
  enters exhausted-wait, keeps its slot, appends no `TurnFailed`, and becomes
  eligible to rerun admission at the computed deadline; absent a competing state
  change that rerun admits that member and reaches selected.
- A `park` pool in which every member holds a chain exclusion of this turn,
  alone or beside an expiring exclusion, fails rather than parking: post-failure
  fail at the observation that closes the failure, wait-transition fail (after
  call) at a release.
- A contended wait whose bounded candidates all become durably excluded re-runs
  the exhaustion policy: a `fail` pool terminalizes, and a `park` pool converts
  to exhausted-wait in place only where some member's every active exclusion is
  one a wake can clear, terminalizing otherwise.
- A released wait that finds exhaustion terminalizes through a fresh call-free
  attempt; the wait attempt and any predecessor attempt are unchanged.
- A wake that reruns admission and still selects an exhausted wait rewrites the
  wait in place with current evidence and deadline; the turn stays parked and
  opens no attempt.
- A durable member-availability update or an operator clear that readmits an
  excluded member a contended wait records makes that wait eligible to admit the
  member; the wake grants eligibility, not release.
- An accepted stop against a parked wait terminalizes Cancelled with
  `TurnCancelled`, never Failed.
- A restart alone releases no wait; retained contended waits are re-evaluated
  against current registrations at startup.
- A deadline-free exhausted wait is released only by a durable
  member-availability update or an operator clear.
- The pre-call exhaustion ending emits the typed
  `turn_credential_pool_exhausted` event and stores per-member exclusion rows,
  and an integration test asserts the pre-call shape.

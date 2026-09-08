# Credential availability design

The remaining design extends
[credential availability](../spec/credential-availability.md) with contention,
capacity reservations, and the terminal projection of a wait release after a
predecessor call.

## Goal

A turn whose pool admits no member and is configured to park waits durably for a
member to become admissible instead of failing, keeps its session slot, and
resumes the chain that parked when a wake arrives. Capacity reservation bounds
concurrent invocations per member and makes contention a wait distinct from
exhaustion.

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

Wait-transition failure after a predecessor call needs a wire shape correlating
that call with a terminal attempt that owns no call. The provider cause remains
the predecessor's; the terminal evidence never reports pool exhaustion.

A contended wait becomes exhausted when a woken waiter finds every formerly
bounded candidate durably excluded, and then re-runs the exhaustion policy
rather than staying parked: the pool's configured value decides afresh, a wait
that is still selected is rewritten in place to the exhausted form, and a `fail`
pool terminalizes.

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

Wire: Wait-transition fail (no call) projects a turn state naming pool
exhaustion, the live event `turn_failed`, and a typed
`turn_credential_pool_exhausted` live event. The ending owns no call, so its
evidence uses the frozen pool-policy revision. The read's rejection detail for a
revision it cannot resolve names the session, turn and policy. Which member
served an ordinary selection, and whether a completed successor chain is shown
to a client, stay undecided in [open questions](../open-questions.md).

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

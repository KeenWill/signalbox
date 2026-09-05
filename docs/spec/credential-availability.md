# Credential availability

Credential availability decides how a model call that draws its credential from
a configured pool ends when the pool admits a member, admits none, or a member's
call fails with a cause the pool can route around.

## Map

A credential pool is a ranked set of credential profiles a session's model calls
may use. Its grammar, ranking, trigger vocabulary and admission rules are owned
by [configuration and credentials](configuration-and-credentials.md). This page
owns the endings of a credential-pool selection attempt.

The machine runs in `crates/persistence/src/model_execution.rs` at two points.
Preparation resolves the session's pool, admits the first member no durable
exclusion removes, and pins the policy snapshot to the call. The commit that
closes a failed call applies the action the pinned policy fixes for the
failure's cause, and either prepares a successor call or terminalizes the turn.

An availability chain is one call and the successors that follow it inside one
turn after qualifying failures. [Model-call execution](model-call-execution.md)
owns what bounds a chain, when a turn starts a fresh one, and the rule that no
attempt is ever sent again with the credential that failed.

The durable records are the policy snapshot pinned to each call, one exclusion
row for each member a qualifying failure in the turn removed, one successor row
linking a successor call to its predecessor and cause, and one exhaustion header
when a turn fails because the pool admitted no member. Members are also removed
by the exclusions the pool's trigger policy writes, which
[configuration and credentials](configuration-and-credentials.md) owns; the
operator surface that clears them is on [process protocol](process-protocol.md).

A selection attempt reaches one of five endings: selected, pre-call fail,
post-failure fail, successor and terminal. They split on whether selection
admitted a member, whether this chain had already issued a call when exhaustion
was found, and whether a failed call's cause, pinned action and proof authorize
a successor. Every exhaustion fails, whatever exhaustion value the pool
configures.

## Decisions

A credential failure in the delivery layer, such as a rejected daemon-owned
OAuth refresh or a credential-home identity that failed its walk, never enters
this machine: it precedes any provider request, is typed as its own failure and
quarantines the profile under
[configuration and credentials](configuration-and-credentials.md). Why: a
deployment misconfiguration is not a provider condition the pool's trigger
policy routes around.

## Contracts

Every credential-pool selection attempt ends in exactly one ending of this
machine, and that ending fixes every projection any specification page states of
it.

Every other page's treatment of this machine is one link to this page, with at
most a sentence naming the projection that page owns; no other page describes a
pool-selection ending, and where another page and this page disagree, this page
governs. Turn phase and attempt disposition belong to
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
prepared record and adds no turn phase. The same preparation consumes the
pending `switch_next_turn` displacement of the member it excluded, never of the
member it selected, because the displacement is the excluded member's record and
consuming any other leaves it pending forever.

Pre-call fail: a fresh admission finds every member excluded and this chain has
issued no call. The turn terminalizes Failed and the call-free attempt ends
KnownFailure. The pre-call exhaustion producer is the third producer of the
`TurnFailed` transcript entry; its entry is appended after the ended attempt's
starting frontier in the transaction that terminalizes the turn. The ending
carries no terminal evidence, because this chain issued no provider request and
an earlier round's successful call is not this chain's evidence; its terminal
cause is pool exhaustion, never a provider failure.

Post-failure fail: a fresh admission finds every member excluded after this
chain observed a qualifying provider failure. The turn terminalizes Failed and
adds no further attempt; the predecessor attempt has already ended KnownFailure.
The terminal evidence is the last observed provider cause as a provider error,
never pool exhaustion, because reporting the pool's emptiness would discard the
only evidence naming what failed. The rotation test in
`crates/persistence/tests/postgres_integration/model_call_execution_and_recovery.rs`
pins this ending.

Successor: the pinned action for a qualifying cause is `switch_now`, a member
remains, and the adapter supplied pre-stream proof that the provider never
accepted the request. The turn stays active and keeps its slot; the predecessor
attempt ends KnownFailure without terminalizing, and the same commit prepares a
successor attempt against the next admitted member and writes the failed
member's chain exclusion. That commit appends no `TurnFailed`: one commit never
both terminalizes the turn and authorizes a successor. The same rotation test
pins this ending.

A chain exclusion removes the failed member for the remainder of the turn, not
merely for the chain, and is insert-only: no passed reset, operator clear or
availability update readmits that member before the turn ends. Why: it forbids
an automatic retry against the profile that just failed. The turn-scoped key on
`credential_pool_chain_exclusion` in the model-calls migration enforces the
scope; nothing enforces that no path deletes a row.

A successor prepared after a rate-limit or overload failure is not sent before
the provider's reported delay has passed, capped at five minutes
(`MAX_AVAILABILITY_BACKOFF` in `crates/persistence/src/model_execution.rs`); a
successor after a quota failure is immediate.

Terminal: a known failure no successor is authorized to follow terminalizes the
turn Failed exactly as it would with no pool. Four gates are checked in order,
and the first to fail decides terminal rather than successor: a stop was
requested while the call was in flight; the cause is not one of the three
qualifying causes; the pinned action for that cause is not `switch_now`; the
adapter supplied no pre-stream proof. Why ordered: ordinary inputs fail several
gates at once, and the first names the actionable reason.

## Not built

- Contended-wait: an exhaustion in which at least one member was skipped only
  for its concurrency bound parks the turn with closed cause contended
  ([design](../design/credential-availability.md)).
- Exhausted-wait: an exhaustion in which nothing was skipped for a bound, and a
  wait is selected, parks the turn with closed cause exhausted
  ([design](../design/credential-availability.md)).
- Wait-transition fail (no call): a released wait that finds the pool exhausted
  before this chain issued a call terminalizes the turn Failed through a fresh
  call-free attempt ([design](../design/credential-availability.md)).
- Wait-transition fail (after call): the same release and exhaustion after this
  chain issued a call; a fresh call-free attempt is opened and ended
  KnownFailure ([design](../design/credential-availability.md)).
- Park selects a wait only when at least one exclusion in the snapshot is one a
  wake can clear ([design](../design/credential-availability.md)).
- Releasing a parked wait resumes the chain that parked and re-evaluates
  admission from current state ([design](../design/credential-availability.md)).
- Every exclusion a wait recorded other than a chain exclusion is re-read from
  its current active state at release, so a passed reset or an authorized clear
  readmits that member ([design](../design/credential-availability.md)).
- The typed `turn_credential_pool_exhausted` live event for the pre-call
  exhaustion ending ([design](../design/credential-availability.md)).
- Per-member exclusion evidence rows on the pre-call exhaustion record
  ([design](../design/credential-availability.md)).
- Honoring `on_pool_exhausted = "park"` on the pre-call path
  ([design](../design/credential-availability.md)).

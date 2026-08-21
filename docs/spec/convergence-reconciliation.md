# Pull-request convergence reconciliation

This page is verified against PR #1056
(`agent/daemon-live-commissioned-escalation-resume`; via PR #1059
`agent/daemon-live-reconciliation-attempt-bound`). The daemon-native convergence
sweep, predicate, fenced commission, durable retry and park records, and
explicit configuration throttle are its implementation scope.

## Authority and target selection

**Implemented behavior.** Convergence reconciliation is an independent
`signalboxd` module composed beside repository watch. It observes GitHub and
commissions work; it never merges a pull request, replies to or resolves a
review thread, or mutates Git. Repository watch continues to own event-driven
dispatch. This periodic pass supplies the missing liveness source for watched
pull requests whose relevant provider state has gone quiet.

The sweep is opt-in twice: `[repository_watch.convergence_sweep]` supplies one
review-response session template and timing policy, and each watched repository
supplies an explicit `convergence_pull_requests` list. Neither a policy without
targets nor targets without a policy are valid. Absence of both starts no sweep;
there is no rule that discovers or enrolls every open pull request. At most 256
pull requests may be enrolled in one daemon configuration.

The interval and per-pull-request dispatch cool-off are positive whole seconds.
Configuration can lower, never raise, their hard ceilings of 300 seconds and
1,800 seconds respectively. Five minutes bounds the period for rediscovering
quiet work while avoiding provider churn; thirty minutes gives a commissioned
session time to make durable progress before another dispatch attempt.

## Census and convergence

**Implemented behavior.** Every pass obtains the selected pull request's open
state, draft flag, base and head branches, head repository and SHA, unresolved
review threads, current-head status rollup, and mergeability. Review-thread and
check connections paginate to completion under a 100-page ceiling. Provider
requests use the watched repository's own reread credential, fixed GitHub HTTPS
endpoint, disabled redirects, proxy discovery, and transport retries, a
30-second request timeout, a 4 MiB response ceiling, and a 64 KiB credential
ceiling. A malformed, closed, missing, partial, oversized, or provider-refused
census is a facts-fetch failure rather than evidence of convergence.

A snapshot is converged exactly when no review thread is unresolved, the status
rollup belongs to the current head, every gating check is green, and
mergeability is `mergeable`. A completed check run is green only with `SUCCESS`,
`NEUTRAL`, or `SKIPPED`; a status context is green only with `SUCCESS`. Names
ending exactly in `(report only)` are non-gating, as is a case-insensitive
`CodeRabbit` status context; a check run with that name remains gating. Draft
state is commissioned context, not a convergence blocker.

## Dispatch, retry, and parking

**Implemented behavior.** An unconverged target outside cool-off is submitted
only through the atomic commissioned-session transaction: session creation,
recorded `CommissionedSessionFence`, goal attachment, and first input are one
commit. The input is stable JSON containing the complete census and ordered
blockers. A durable command and content digest fence makes an ambiguous retry
replay the same request. Before committing, commissioning takes a transaction
advisory lock for the repository and pull-request identity and refuses with the
existing live session when any commissioned dispatch for that target has a live
goal. This final guard covers races after the sweep's earlier liveness read.

Facts-fetch failure, commission refusal, template drift, and recoverable sweep
state-access failure each append a typed event and advance an independent
consecutive-failure lineage. Automatic retry uses exponential delays from a
60-second base under a 900-second ceiling. The fifth consecutive failure parks
the target and exposes an operator need through
`convergence_sweep_parked_target`; a successful observation resets a transient
lineage. A storage outage that also prevents the failure record is logged and
retried at the next census because no system can durably record through an
unavailable durable authority. The target row is a mutable scheduler projection,
while its event rows are append-only audit facts.

A live commissioned session suppresses another commission. Once the latest
commissioned session's cool-off has elapsed, repeated sweeps with unchanged head
SHA and unresolved-thread count and no recorded model call park immediately as
typed `no_model_activity`, whether that session remains live or is terminal,
with the operator need `inspect_inactive_session`. A terminal commissioned
session that recorded model activity suppresses re-dispatch until its cool-off
expires. Skip decisions are recorded as converged, cooling-off, or live-session
events rather than silently re-entering the queue.

## Open edges

- Porting this deliberately shallow daemon loop to the program substrate is a
  follow-on; the current module creates no reusable program primitive.
- Richer prioritization and scheduling are deferred. Fleet-wide projections
  belong to issue #992; the present operator surface is the parked-target view.

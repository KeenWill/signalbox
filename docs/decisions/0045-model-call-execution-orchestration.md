# ADR-0045: Model-call execution orchestration

- Date: 2026-07-20
- Supersedes: none
- Superseded by: none
- Depends on: [ADR-0004](0004-turn-and-attempt-lifecycle.md),
  [ADR-0005](0005-model-call-retry-semantics.md),
  [ADR-0017](0017-credential-lifecycle.md),
  [ADR-0022](0022-persistence-representation.md),
  [ADR-0031](0031-direct-fatal-terminalization.md),
  [ADR-0033](0033-identity-generation-supply-and-encoding.md),
  [ADR-0035](0035-domain-owned-persistence-reconstitution.md),
  [ADR-0040](0040-transactional-outbox.md),
  [ADR-0041](0041-evidence-bearing-reconstitution.md),
  [ADR-0042](0042-assistant-content-and-completion.md),
  [ADR-0043](0043-provider-failure-classification.md), and
  [ADR-0044](0044-hub-runtime-foundations.md)
- Refines: the architecture's application-orchestration boundary for provider
  effects with the first purpose-specific execution shape
- Resolves: the application port and transaction boundary for one model-call
  execution; provider-target identity evidence thresholds and later semantic
  variants remain open
- Decision questions: prepare-call transaction ownership; provider port
  ownership; observation-commit transaction ownership; transaction scope around
  the provider effect; correlation and authority between stages; per-effect
  one-call/no-retry policy; crash and failure handling between stages

## Context

The application crate currently coordinates use cases whose durable work fits
inside one purpose-specific transaction port. A model call cannot fit that
shape. ADR-0005 requires durable model-call identity, exact target, and context
frontier before provider interaction, while the provider interaction is a
network effect whose duration and outcome are outside Postgres. Its final
content, evidence, physical disposition, and any resulting attempt or turn
transition become authoritative only when committed afterward.

Leaving this split to the first scripted-provider slice would let any of three
boundaries become precedent. A persistence adapter could call the provider while
holding a transaction; a provider adapter could mutate durable lifecycle state;
or application code could commit preparation, call the provider, and then retry
whichever stage failed. Each shape obscures which component owns domain
decisions and can repeat an effect after a commit or provider acknowledgement
becomes uncertain.

ADR-0040 already excludes external delivery from database transactions and makes
its outbox the path for client-visible durable-transition events, not provider
dispatch. ADR-0031 and ADR-0041 require complete authoritative aggregate state,
rather than caller-supplied guard summaries, for lifecycle closure and
reconstitution. The model-call application boundary must preserve all three
rules while accepting the unavoidable failure window between a durable issue
authorization and its external effect.

ADR-0017 fixes another ordering constraint inside preparation: the exact call is
committed as a durable `Prepared` record before credential lookup, the provider
adapter is the only consumer of credential values, and successful local send
preparation is required before the call's committed transition to `InFlight`.
Failure to read or prepare that credential has an accepted known-failure
lifecycle rather than an external-effect authorization.

[ADR-0044](0044-hub-runtime-foundations.md) owns the hub runtime, observability
facade, shared operator-error taxonomy, identity-collision retry rule, and
composition root. This record depends on that taxonomy and collision rule while
leaving its other choices unchanged. Its per-invocation construction rule can
host this use case; the application boundary remains runtime-agnostic and
preserves which stage failed.

## Decision

### One application use case, three ordered effect roles

One application service owns a single linear execution:

```text
prepare-call transaction
  create and commit exact Prepared call, then stop
    OR
  load a previously committed Prepared call, then close transaction
prepare opaque one-shot send capability (no database transaction open)
acquire shared attempt dispatch/stop gate
authorize-send transaction: recheck authority, commit Prepared -> InFlight
close transaction, then begin the physical send while retaining the gate
        |
        | issued-call metadata + send capability
        v
provider interaction (no database transaction open)
        |                         |
        | early trusted          | terminal or later physical
        | observations           | observations
        v                         v
commit-observation transaction(s), serialized per observation
```

The service composes purpose-specific prepare, authorize-send, provider, and
commit-observation ports plus the provider adapter's local
capability-preparation operation. These are separate effects, and orchestration
failure retains the failing stage. ADR-0044 independently governs how the
adapters' errors map into the shared operator taxonomy; this record adds no
competing classification. The service invokes the provider interaction at most
once per invocation, and exactly once only after gated send authorization
succeeds. A proven hub-minted identity collision may repeat a rolled-back
prepare, authorize, or observation transaction with fresh candidates; no other
failure repeats a port whose outcome is uncertain, and no such retry repeats
credential I/O or provider work.

An invocation that creates a new `Prepared` call ends after committing that
checkpoint and returns no provider authorization. A later invocation may reload
that same committed call, close the load transaction, prepare its capability,
and perform the gated `InFlight` authorization. This is a staged continuation of
one durable call, not a retry or a second provider interaction. An
ambiguity-replacement call already has that committed checkpoint when this use
case receives it.

The shapes in this record are semantic roles, not final Rust trait, method, or
record names. Provider-neutral values exposed to application orchestration are
domain or application values, never SQL rows, transaction handles, provider SDK
values, or framework types. Separately, the service moves an opaque,
nonserializable, one-shot capability owned by the provider adapter. It is not an
application, domain, or provider-neutral value, and application or domain code
cannot inspect, clone, format, persist, or log it.

### Prepare-call transaction

The prepare port receives a scheduling hint or exact aggregate identity plus the
application-minted candidate identities needed by any fact this invocation may
create, never a caller-prepared lifecycle result. The application service mints
those candidates under ADR-0033 immediately before calling the port; persistence
can use or discard them according to the authoritative domain transition but
never mints replacements. Inside one serialized transaction, the prepare
adapter:

01. loads the complete purpose-specific authoritative projection and validates
    it through the domain-owned reconstitution boundary;
02. derives whether the exact turn and attempt may authorize a provider effect;
03. selects one of three preparation paths:
    - for a new initial call or intentional continuation, resolves and pins the
      target where required, derives the exact ordered context frontier,
      consumes eligible steering atomically, and creates the distinct call and
      frontier from the supplied `ModelCallId` and `ContextFrontierId`
      candidates as `Prepared`;
    - for an ordinary initial or continuation call already committed as
      `Prepared`, reloads that exact call and preserves its identity, pinned
      target, context frontier, and completed steering consumption unchanged;
    - for an owner-authorized ambiguity replacement, loads the existing new
      attempt and its fully targeted `Prepared` call created by the authority
      transfer, validates that it still owns outcome authority, and uses its
      call identity, pinned target, frontier, and steering consumption
      unchanged;
04. if target resolution fails before call creation, leaves the supplied call
    and frontier candidates unused, applies the accepted no-call attempt and
    turn failure, commits that closure and its client-visible ADR-0040 outbox
    rows, and returns no provider authorization;
05. if this transaction created a new `Prepared` call, commits that checkpoint
    and its client-visible ADR-0040 outbox rows, returns a durably-prepared
    result without a capability, and invokes no later port in this service
    invocation;
06. only when the exact `Prepared` call was already committed before this
    transaction began, returns its checked request-preparation material and
    closes the transaction without an outbox append;
07. after that transaction closes, asks the provider adapter to prepare a send
    capability through ADR-0017's credential port; the credential value reaches
    only the adapter's outbound authenticator, while application and domain
    values see no secret material;
08. acquires the process-shared attempt dispatch/stop gate, then opens a
    distinct authorize-send transaction that reloads authority and either
    rejects stale or stopped work, or commits `Prepared -> InFlight` with its
    attempt, lifecycle, and outbox facts;
09. if credential lookup or local capability preparation returned a trustworthy
    ordinary failure, uses a guarded closure transaction to apply the accepted
    `Prepared -> Terminal(KnownFailed)` call transition and matching
    known-failure attempt and turn transitions, commits their client-visible
    ADR-0040 outbox rows, and returns no send authorization; fail-closed
    corruption, a caller or hub bug, or another adapter defect instead follows
    ADR-0044's operator taxonomy, does not terminalize the call as a provider
    failure, and invokes no later port; a hub-minted identity collision can
    arise only from the guarded closure transaction, which retries with fresh
    candidates without repeating capability preparation; and
10. after the `InFlight` commit is known, closes its transaction and begins the
    physical send while retaining the dispatch/stop gate. It releases the gate
    only after the adapter proves no acceptance crossing or reports crossing as
    possible. Every in-process stop, invalidation, or cancellation transition
    for the attempt acquires the same gate before its transaction, so stopped
    authority and a new physical send cannot pass one another.

The issued-call metadata is bound to the durable model call, current turn
attempt, exact pinned target, and exact context frontier. Together with the
capability, it is evidence that this invocation may perform that one provider
interaction; neither value is authority to choose a later call disposition or
turn outcome. The capability is bound to that exact call and request, cannot be
rebound or reused, and has no serializable or loggable representation. These are
semantic requirements, not a decision about its final Rust API.

A new `Prepared` call is committed before any credential access for it. It is
never advanced to `InFlight` in its creating transaction. A precreated
ambiguity-replacement call is not created again, retargeted, given a new
frontier, or made to consume steering again. A no-work, stale, already issued,
or terminal result returns no provider authorization.

Every prepare-transaction commit has the same conservative acknowledgement rule.
If the target-resolution no-call closure, newly `Prepared` checkpoint, or
credential-failure known-failure closure has an ambiguous commit result, the
service returns a prepare-stage commit-ambiguous failure, invokes no later port,
and requires authoritative state to be reread before any later action. If the
`InFlight` commit result is ambiguous after capability preparation, the
capability is additionally discarded and never returned. A known rollback is
reported distinctly and authorizes no later port in that invocation.

### Provider-interaction port

After the authorize-send transaction has ended and its `InFlight` commit is
known, the service invokes the provider port once with the issued-call metadata
and its opaque one-shot capability. No database transaction, row lock, or
persistence transaction handle crosses this call.

The provider adapter used the credential during local preparation to construct
the request-bound send capability. The provider interaction consumes that
capability exactly once to perform one physical interaction and translates what
it observes back into a provider-neutral observation bound to the same model
call. It does not perform a second credential lookup. The observation carries
provider facts and boundary knowledge, not a caller-selected domain disposition.
It must distinguish evidence that proves the request never crossed the
provider-acceptance boundary from uncertainty that it may have crossed;
provider-target identity evidence thresholds remain provider-contract work.

ADR-0043's full-request-send boundary and classification table govern the
observation's physical disposition. This orchestration record chooses neither a
second classification nor a weaker evidence threshold.

Ordinary pre-send failure facts, provider outcomes, and uncertain transport
facts produce trustworthy correlated observations for the commit stage. The
provider port may report an observation before returning when trusted headers or
stream metadata first establishes ADR-0005's target mismatch. The application
immediately commits that observation and its durable best-effort cancellation
request while the same physical provider interaction may remain active. The port
then continues only to observe that already-started interaction and may return
its later physical outcome or audit evidence; this is neither a second provider
interaction nor authority for new semantic content from the terminalized call.
Fail-closed corruption, a caller or adapter bug, or any other condition in which
the adapter cannot construct a trustworthy observation instead returns a
provider-stage operator failure. The service calls no commit port or retry and
escalates the failure as fatal to hubd supervision. It does not return to
ordinary scheduling: the process terminates, and the next incarnation performs
ADR-0004 startup recovery for the issued call.

Transient draft delivery may occur while the provider port is running. Such
delivery is replaceable, non-authoritative, outside Postgres, and outside the
ADR-0040 outbox. Its exact sink or callback API is implementation scope and
cannot affect the terminal observation or authorize a durable transition.

The adapter disables SDK or transport retries after the point at which the
provider might have accepted the request. Low-level work proven not to have
crossed that boundary may continue preparing the same durable call as ADR-0005
allows; it is not a second physical provider interaction. Once crossing is
possible, any lost acknowledgement is reported as uncertainty for durable
classification, never hidden by another send.

### Commit-observation transaction

Whenever the provider port reports a trustworthy observation while the process
remains live, the application service mints the ADR-0033 candidate identities
required by the observation's possible fact-creation transitions, including a
`ProviderTargetEvidenceId` when target evidence may be recorded, and passes the
observation and candidates to the commit port. A provider-stage operator failure
with no trustworthy observation does not enter this stage. Persistence uses a
supplied candidate only when the authoritative domain transition creates that
fact and never mints an identity. Inside one serialized transaction, the commit
adapter:

1. reloads and validates the complete authoritative aggregate for the exact
   issued call rather than trusting the earlier projection;
2. validates call, turn-attempt, target, evidence, and current outcome-authority
   correlation;
3. applies ADR-0005's fixed classification and authority precedence through
   domain transitions, deriving terminal guards, fatal causes, and ambiguity
   sets from complete state rather than accepting them from the service or
   provider adapter; and
4. atomically commits the physical call outcome and evidence, the resulting
   lifecycle transitions required by accepted semantics, and any client-visible
   ADR-0040 outbox rows. When the observation is the definitive successful
   response, that same transaction also commits ADR-0042's complete ordered
   assistant-content sequence and any supported `TurnCompleted` marker under its
   all-or-nothing final-response boundary.

Provider completion does not by itself make assistant content authoritative;
only ADR-0042's checked final-response transaction does. Mismatch, refusal,
cancellation, ambiguity, late evidence, transferred outcome authority, and
direct fatal closure retain the semantics of ADR-0005 and ADR-0031; this port
decomposition adds no competing precedence.

An early mismatch observation is committed before waiting for final response or
physical cancellation. Later chunks, completion, refusal, or cancellation are
committed only as the audit or physical evidence ADR-0005 permits and cannot
reopen the terminal call or create authoritative semantic content from that
execution.

The commit port may find that newer durable state made the observation stale,
late, or non-authoritative. It applies the accepted evidence and monotonicity
rules instead of reopening a terminal call or trusting the service's earlier
view.

### Between-effect failure and crash policy

The application service follows this exhaustive stage policy:

| Point of failure or interruption                                                              | Required behavior                                                                                                                                                                                                                                                                                          |
| --------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Target resolution fails before a call exists                                                  | Atomically commit the accepted no-call attempt and turn failure plus outbox rows. Return the closed no-call result and invoke no later port.                                                                                                                                                               |
| A newly created `Prepared` call commits                                                       | Return its durably-prepared result with no capability and invoke no later port. A later invocation may reload that same call; it does not recreate it.                                                                                                                                                     |
| No-work, stale, validation refusal, or definite prepare rollback occurs                       | Return the prepare-stage failure or closed no-call result. Do not invoke the provider or commit port.                                                                                                                                                                                                      |
| Credential lookup or capability preparation has a trustworthy ordinary failure                | Commit its accepted known-failure closure and outbox rows in a new guarded transaction. Return no capability and do not invoke the provider or commit port.                                                                                                                                                |
| Capability preparation reports corruption or a caller/hub bug                                 | Apply ADR-0044's operator taxonomy without recording a provider known-failure closure. Invoke no later port; capability preparation has no hub-minted identity-collision retry.                                                                                                                            |
| A no-call, `Prepared`, known-failure, or `InFlight` prepare commit is ambiguous               | Return a prepare-stage commit-ambiguous failure, invoke no later port, and reread authoritative state before any later action; discard any locally prepared capability.                                                                                                                                    |
| Prepare commits `InFlight`, then the process stops before or during provider invocation       | Leave the durably issued call for ADR-0004/ADR-0005 startup classification. Recovery uses evidence; it neither assumes the request was sent nor invokes the provider for that authorization.                                                                                                               |
| Provider work observes an ordinary pre-send failure, provider outcome, or uncertain transport | Report each trustworthy correlated observation to the commit port. Commit an early trusted mismatch immediately; later physical outcome is audit evidence only. The application does not choose lifecycle results.                                                                                         |
| An early streaming-observation commit rolls back or is ambiguous                              | Report failure to the still-running provider control path, request in-process best-effort cancellation, and retain ownership until the interaction stops or is drained. Make no further durable commit claim, then escalate fatally so startup recovery rereads authoritative state and provider evidence. |
| Provider work cannot construct a trustworthy observation                                      | Escalate fatally to hubd supervision, call no commit port, and terminate the process so the next incarnation performs ADR-0004 startup recovery.                                                                                                                                                           |
| The process stops after provider interaction but before outcome commit                        | Startup recovery classifies the issued call from durable and provider evidence under INV-034. It never repeats that call merely because its in-process observation was lost.                                                                                                                               |
| Outcome commit proves rollback because a hub-minted identity collided                         | Mint fresh candidates and retry committing the same in-memory observation without invoking the provider again.                                                                                                                                                                                             |
| A terminal-observation commit has any other failure, including commit ambiguity               | Return the commit-stage failure. Do not invoke the provider again or retry the observation. Recovery or a later authoritative pass first rereads durable state.                                                                                                                                            |
| Outcome commit succeeds but its acknowledgement is lost                                       | The recorded call state and evidence remain authoritative. A later pass observes or replays that result without creating another call or provider interaction.                                                                                                                                             |

The durable issue boundary deliberately creates a conservative crash window:
after preparation commits, durable state can establish authorization but may not
establish whether the process reached the provider. This record does not
fabricate certainty across that window. Provider-specific evidence may prove a
known outcome; otherwise accepted ambiguity handling applies.

### One call means one physical interaction

The one-call/no-retry rule has two layers:

- Per application invocation, the service invokes the provider interaction at
  most once, and exactly once only after gated send authorization succeeds.
  Credential lookup and capability preparation occur after the load transaction
  closes and before the gated authorize transaction. An invocation that
  establishes the durable `Prepared` checkpoint ends there; a later invocation
  may continue that exact unissued call. Each observation is committed once,
  apart from ADR-0044's fresh-candidate retry after a proven identity-collision
  rollback; early and terminal observations from one interaction are distinct
  durable inputs. A proven identity-collision rollback may repeat only the
  affected transaction with fresh candidates while preserving an already
  prepared, unconsumed capability; it repeats neither credential I/O nor
  provider work.
- Per durable issue authorization, the provider port performs no more than one
  physical interaction that might reach provider acceptance, using the
  call-bound capability at most once.

A later scheduler pass cannot treat an already-issued unclassified call as a
fresh authorization. Another provider interaction requires a new `ModelCallId`
and an accepted ADR-0005 transition: an intentional continuation, or an explicit
owner-authorized ambiguity replacement that atomically transfers outcome
authority. Version one still has no automatic known-failure or ambiguous-result
retry.

Database adapters likewise propagate a commit-ambiguous result rather than
silently rerunning a transaction whose outcome they cannot establish. A proven
rollback caused by a hub-minted identity collision is the sole same-invocation
outcome-commit retry: the service supplies fresh candidates for the unchanged
in-memory observation and does not repeat the provider effect. Other ordinary
rolled-back work may be retried only by a later authoritative orchestration pass
after it rereads durable state.

### Relationship to the client-update outbox

The prepare, authorize, and commit transactions append ADR-0040 outbox rows for
whichever durable transitions the protocol declares client-visible. None
transaction publishes after commit, performs provider network I/O, or writes
transient drafts to the outbox. ADR-0017 credential lookup and adapter-local
capability construction happen only while no database transaction is open. The
outbox publisher's post-commit nudge remains a hint and is not part of
model-call outcome authority.

ADR-0040's client-update outbox is not an outbound provider-effect queue. A
future decision may introduce a durable provider-dispatch mechanism, but it must
preserve this record's durable authorization, one-physical-interaction,
correlation, and observation-commit boundaries.

## Invariants

This record's application boundary depends on the accepted rules indexed by the
[invariant catalog](../invariants.md): INV-002, INV-005, INV-006, INV-009,
INV-014, INV-015, INV-016, INV-018, INV-025, INV-026, INV-032, INV-034, and
INV-035. Their owning records remain normative; this section does not duplicate
or replace them.

No invariant catalog row claims executable enforcement until the corresponding
application and persistence slices land.

## Strongest alternative

Hold one database transaction open across the provider's network interaction and
stream, then commit preparation and outcome together.

This offers an attractive single application call and could roll back local rows
when the provider invocation fails. It is rejected because rollback cannot roll
back a provider effect, a connection loss can still make both commits uncertain,
and a long network stream would hold database locks and a pool connection while
no database work occurs. It would also violate ADR-0040's no-external-effect
transaction discipline and hide the durable pre-send authorization ADR-0005
requires. ADR-0017 credential read and adapter-local capability construction
occur only after the load transaction closes and before the authorize
transaction opens.

## Rejected alternatives

- **Let the provider adapter own both transactions.** That gives an outward
  adapter aggregate and lifecycle authority, admits SQL concerns into provider
  code, and makes a second adapter a second transaction implementation.
- **Pass a database callback or transaction handle into the provider port.** The
  type may look abstract, but it still holds a transaction across external I/O
  and lets the adapter choose persistence timing.
- **Let application or persistence code read the credential and build the
  provider request.** That makes a non-adapter component a credential-value
  consumer and risks admitting secret or provider-specific values into durable,
  application, domain, or log representations. The selected capability
  preparation is adapter-owned and exposes only an opaque one-shot value.
- **Look up the credential after committing `InFlight`.** A missing credential
  would leave an issued call even though the accepted ADR-0017 known-failure
  transition could have been committed before any send authorization.
- **Recreate or retarget an owner-authorized ambiguity replacement during
  prepare.** The authority transfer already created the new attempt and its
  fully targeted `Prepared` call. Re-deriving its frontier or consuming steering
  again would change the authorized request.
- **Expose one combined persistence-and-provider effect port.** The application
  can no longer enforce that provider I/O occurs between two committed
  boundaries or test each effect count independently.
- **Call the provider before recording issue authorization.** A crash can
  produce an external effect with no durable model-call identity to classify.
- **Retry a failed stage indiscriminately inside the service.** A transaction or
  provider acknowledgement can be lost after success; blind repetition would
  duplicate durable transitions or physical provider work. The narrow proven
  identity-collision rollback retry is safe because it reuses the unchanged
  observation, supplies fresh hub-minted candidates, and performs no provider
  interaction.
- **Use ADR-0040's outbox as provider dispatch.** Its rows and publisher are
  explicitly client-update infrastructure. Reusing them would silently add a
  durable external-effect worker, delivery acknowledgement, and recovery
  protocol that ADR-0040 does not decide.

## Consequences

The first scripted-provider application slice needs purpose-specific
transaction, capability, provider, gate, and observation fakes. Its tests must
carry the meaningful scenario and invariant identifiers in their names or doc
comments and cover these groups:

- S02 / [INV-006](../invariants.md), [INV-009](../invariants.md),
  [INV-014](../invariants.md), and [INV-015](../invariants.md): port order and
  correlation, transaction closure around capability I/O and provider entry, one
  provider interaction, identity-collision-only transaction retries, the
  separately committed `Prepared` checkpoint, its ordinary resume path without
  identity/frontier/steering changes, and no later stage after an earlier-stage
  or commit-ambiguous failure.
- S02 / [INV-002](../invariants.md) and [INV-035](../invariants.md): only the
  provider adapter consumes the credential value, trustworthy ordinary
  credential failure commits known failure without a capability, adapter defects
  retain ADR-0044's operator classification without that closure, and `InFlight`
  commits before the capability reaches the provider port.
- S04 / [INV-016](../invariants.md), [INV-025](../invariants.md), and
  [INV-026](../invariants.md): the replacement path issues the precreated call
  without changing its identity, target, frontier, or steering consumption and
  never repeats an uncertain interaction.
- S02 and S04 / [INV-018](../invariants.md) and [INV-034](../invariants.md):
  ordinary provider observations enter the commit port once, an identity
  collision retries the unchanged observation with fresh candidates after proven
  rollback, an early trusted mismatch commits before later physical evidence,
  fail-closed operator failures leave recovery authority untouched, persistence
  integration tests exercise both atomic boundaries, and startup tests cover
  crash points after issue and after provider return.
- S02 / [INV-032](../invariants.md): transient drafts remain replaceable and
  cannot become authoritative final content.

The implementation pull request that makes any of these tests enforcement of an
accepted invariant must add the concrete test link to that invariant's
enforcement cell in the same change. This ADR claims no executable enforcement.

The shape is deliberately more explicit than one repository method. In return,
every durable write and external effect has one owner, provider latency consumes
no database transaction, and failure reports the stage whose outcome needs
reconciliation.

The issue-before-effect crash window can conservatively classify an interaction
that may never have left the process. Removing that uncertainty would require a
separate durable dispatcher or provider idempotency/status contract. This record
prefers honest ambiguity to an invented exactly-once claim.

## Scenario walkthroughs

- **S02:** One prepare invocation records and commits the exact `Prepared` call
  and frontier, then stops. A later invocation reloads that committed call; only
  then does the load transaction close before the provider adapter resolves its
  credential and prepares the one-shot capability. Credential failure is closed
  by a later guarded transaction and returns no authorization. On success, the
  authorize transaction commits `InFlight` before the scripted provider port
  receives the capability, streams replaceable drafts, and reports its
  observations. A trusted streaming mismatch commits immediately with the
  durable cancellation request; later physical outcome is audit evidence only.
  For a definitive successful response, the observation transaction commits
  lifecycle state, ADR-0042's complete ordered assistant sequence and supported
  completion marker, and their client-visible outbox rows atomically.
- **S04:** An owner-authorized replacement enters preparation as an already
  fully targeted `Prepared` call on its new attempt. Preparation neither creates
  another call nor changes its target, frontier, or steering consumption. A
  crash after issue authorization leaves that exact durable call for the startup
  scan, whether the provider was never reached, was still running, or returned
  before the process died. Recovery uses available evidence, preserves
  uncertainty when necessary, ends the abandoned attempt in its matching
  `...Lost` branch, and never repeats that call automatically.

## Open questions

- ADR-0042 fixes assistant text, logical tool-use references, completed-turn
  markers, and their final-response commit boundary. Later semantic variants,
  rich assistant content, and provider/client rendering remain open under the
  identity-representation inventory.
- Provider-specific known-failure, ambiguity, identity, and request-status
  evidence remain ADR-0007 and provider-contract work.
- The exact transient-draft sink, streaming checkpoint policy, and provider
  cancellation delivery mechanism remain later implementation or decision scope
  under their existing open questions.
- The concrete in-memory representation, drop behavior, and zeroization strategy
  for the opaque one-shot send capability remain implementation scope subject to
  ADR-0017's non-escape rules.
- Concrete port names, storage schema, SQL isolation and locking, and
  provider-adapter error variants land with their slices while preserving this
  three-role boundary.

## Explicit non-decisions

This record adds no code, schema, crate, dependency, provider SDK, provider,
runtime, task model, wire type, or assistant-content variant. It does not select
fallback, known-failure retry, refusal remediation, provider-target identity
evidence thresholds, provider identity normalization, request idempotency keys,
or a durable provider-dispatch worker. It does not change ADR-0005's call
dispositions, authority transfer, or outcome precedence; ADR-0017's credential
lifecycle or failure transition; ADR-0031's fatal closure; ADR-0040's outbox
scope; ADR-0042's final-response boundary; ADR-0043's provider-failure
classification; or ADR-0044's runtime, taxonomy, and composition-root contract.

# ADR-0045: Model-call execution orchestration

- Date: 2026-07-20
- Supersedes: none
- Superseded by: none
- Depends on: [ADR-0004](0004-turn-and-attempt-lifecycle.md),
  [ADR-0005](0005-model-call-retry-semantics.md),
  [ADR-0017](0017-credential-lifecycle.md),
  [ADR-0022](0022-persistence-representation.md),
  [ADR-0031](0031-direct-fatal-terminalization.md),
  [ADR-0035](0035-domain-owned-persistence-reconstitution.md),
  [ADR-0040](0040-transactional-outbox.md), and
  [ADR-0041](0041-evidence-bearing-reconstitution.md)
- Refines: the architecture's application-orchestration boundary for provider
  effects with the first purpose-specific execution shape
- Resolves: the application port and transaction boundary for one model-call
  execution; provider-specific evidence thresholds and assistant-content
  semantics remain open
- Decision questions: prepare-call transaction ownership; provider port
  ownership; outcome-commit transaction ownership; transaction scope around the
  provider effect; correlation and authority between stages; per-effect
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

[ADR-0044](0044-hub-runtime-foundations.md) now owns the hub runtime,
observability facade, shared operator-error taxonomy, and composition root. This
record leaves those choices unchanged. Its per-invocation construction rule can
host this use case, and its independent taxonomy rule governs the adapters'
operator-facing classifications. The application boundary below remains
runtime-agnostic, preserves which stage failed, and relies on none of ADR-0044's
technology or composition-root choices, so this record does not make it a
normative dependency.

## Decision

### One application use case, three ordered effects

One application service owns a single linear execution:

```text
prepare-call transaction
        |
        | committed issue authorization
        v
provider interaction (no database transaction open)
        |
        | provider-neutral observation
        v
commit-outcome transaction
```

The service composes three purpose-specific ports: a prepare-call transaction
port, a provider-interaction port, and a commit-outcome transaction port. These
are separate effects, and orchestration failure retains the failing stage.
ADR-0044 independently governs how the adapters' errors map into the shared
operator taxonomy; this record adds no competing classification. The service
calls each applicable port at most once per invocation and never hides a retry
loop around one of them.

The shapes in this record are semantic roles, not final Rust trait, method, or
record names. Port values exposed to application orchestration are domain or
application values, never SQL rows, transaction handles, provider SDK values, or
framework types.

### Prepare-call transaction

The prepare port receives a scheduling hint or exact aggregate identity, never a
caller-prepared lifecycle result. Inside one serialized transaction, its
adapter:

1. loads the complete purpose-specific authoritative projection and validates it
   through the domain-owned reconstitution boundary;
2. derives whether the exact turn and attempt may authorize a provider effect;
3. for the first call, resolves the frozen selection and pins its exact target
   before call creation; for every call, derives and persists the exact ordered
   context frontier and consumes eligible steering atomically where the accepted
   lifecycle requires it;
4. creates the distinct durable model call, advances the current turn attempt
   from `Prepared` to `Running` when applicable, and records the call issue
   authorization required before the provider boundary, including the accepted
   call `Prepared -> InFlight` transition; and
5. commits every other atomic lifecycle fact and any client-visible ADR-0040
   outbox rows belonging to that transition.

The port returns a provider-neutral issued-call value only after that
transaction commits. The value is bound to the durable model call, current turn
attempt, exact pinned target, and exact context frontier. It is evidence that
this invocation may perform that one provider interaction; it is not authority
to choose a later call disposition or turn outcome. A no-work, stale, already
issued, or terminal result returns no provider authorization.

Creating `Prepared` and advancing it to `InFlight` may occur within the same
transaction. No intermediate commit is required. What is required is that a
provider invocation cannot begin while the durable call is absent or still lacks
its issue authorization.

### Provider-interaction port

After the prepare transaction has ended, the service invokes the provider port
once with the issued-call value. No database transaction, row lock, or
persistence transaction handle crosses this call.

The provider adapter translates the provider-neutral request into one physical
provider interaction and translates what it observes back into a
provider-neutral observation bound to the same model call. The observation
carries provider facts and boundary knowledge, not a caller-selected domain
disposition. It must distinguish evidence that proves the request never crossed
the provider-acceptance boundary from uncertainty that it may have crossed;
provider-specific evidence thresholds remain provider-contract work.

Credential access under ADR-0017 occurs inside this external-effect boundary for
the exact pinned credential reference. Secret material never enters the durable
issued-call value or a transaction record.

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

### Commit-outcome transaction

When the provider port returns while the process remains live, the service
passes its observation to the commit port exactly once. Inside one serialized
transaction, that adapter:

1. reloads and validates the complete authoritative aggregate for the exact
   issued call rather than trusting the earlier projection;
2. validates call, turn-attempt, target, evidence, and current outcome-authority
   correlation;
3. applies ADR-0005's fixed classification and authority precedence through
   domain transitions, deriving terminal guards, fatal causes, and ambiguity
   sets from complete state rather than accepting them from the service or
   provider adapter; and
4. atomically commits the physical call outcome and evidence, any authoritative
   semantic content allowed by its owning decision, the resulting attempt and
   turn transition, and any client-visible ADR-0040 outbox rows.

Provider completion is not authoritative assistant content before this
transaction commits. Mismatch, refusal, cancellation, ambiguity, late evidence,
transferred outcome authority, and direct fatal closure retain the semantics of
ADR-0005 and ADR-0031; this port decomposition adds no competing precedence.

The commit port may find that newer durable state made the observation stale,
late, or non-authoritative. It applies the accepted evidence and monotonicity
rules instead of reopening a terminal call or trusting the service's earlier
view.

### Between-effect failure and crash policy

The application service follows this exhaustive stage policy:

| Point of failure or interruption                                                            | Required behavior                                                                                                                                                                                               |
| ------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Prepare port returns without a committed authorization                                      | Return the prepare-stage failure or closed no-call result. Do not invoke the provider or commit port.                                                                                                           |
| Prepare commits, then the process stops before or during provider invocation                | Leave the durably issued call for ADR-0004/ADR-0005 startup classification. Recovery uses evidence; it neither assumes the request was sent nor invokes the provider for that authorization.                    |
| Provider work returns a known pre-acceptance failure, provider outcome, or uncertain result | Construct the correlated provider-neutral observation and call the commit port once. The application does not reinterpret it or choose the lifecycle result.                                                    |
| The process stops after provider interaction but before outcome commit                      | Startup recovery classifies the issued call from durable and provider evidence under INV-034. It never repeats that call merely because its in-process observation was lost.                                    |
| Commit port fails, including a commit-ambiguous infrastructure failure                      | Return the commit-stage failure. Do not invoke the provider again and do not call the commit port a second time in this service invocation. Recovery or a later authoritative pass first rereads durable state. |
| Outcome commit succeeds but its acknowledgement is lost                                     | The recorded call state and evidence remain authoritative. A later pass observes or replays that result without creating another call or provider interaction.                                                  |

The durable issue boundary deliberately creates a conservative crash window:
after preparation commits, durable state can establish authorization but may not
establish whether the process reached the provider. This record does not
fabricate certainty across that window. Provider-specific evidence may prove a
known outcome; otherwise accepted ambiguity handling applies.

### One call means one physical interaction

The one-call/no-retry rule has two layers:

- Per application invocation, the service calls each of its three effect ports
  no more than once and in the order above.
- Per durable issue authorization, the provider port performs no more than one
  physical interaction that might reach provider acceptance.

A later scheduler pass cannot treat an already-issued unclassified call as a
fresh authorization. Another provider interaction requires a new `ModelCallId`
and an accepted ADR-0005 transition: an intentional continuation, or an explicit
owner-authorized ambiguity replacement that atomically transfers outcome
authority. Version one still has no automatic known-failure or ambiguous-result
retry.

Database adapters likewise propagate a commit-ambiguous result rather than
silently rerunning a transaction whose outcome they cannot establish. Ordinary
database work proven to have rolled back may be retried only by a later
authoritative orchestration pass; that pass must first observe durable state and
still cannot repeat the provider effect.

### Relationship to the client-update outbox

The prepare and commit transactions append ADR-0040 outbox rows for whichever of
their durable transitions the protocol declares client-visible. Neither
transaction publishes after commit, performs provider I/O, or writes transient
drafts to the outbox. The outbox publisher's post-commit nudge remains a hint
and is not part of model-call outcome authority.

ADR-0040's client-update outbox is not an outbound provider-effect queue. A
future decision may introduce a durable provider-dispatch mechanism, but it must
preserve this record's durable authorization, one-physical-interaction,
correlation, and outcome-commit boundaries.

## Invariants

This record fixes an application enforcement boundary for:

- INV-002 and INV-005: application values remain independent of SQL, provider
  SDK, framework, outbox-record, transient-stream, and presentation types.
- INV-006 and INV-009: both transactions derive transitions from complete
  authoritative state and release or retain the progressing slot only through
  accepted aggregate transitions.
- INV-014 and INV-015: exact target, model-call identity, issue authorization,
  and exact context frontier are durable before provider interaction.
- INV-016: eligible steering is consumed only inside the preparing aggregate
  transaction and never mutates an issued request.
- INV-018, INV-025, and INV-026: the commit transaction applies refusal and
  ambiguity precedence, and no application or provider retry overwrites or
  repeats an uncertain physical interaction.
- INV-032 and INV-034: transient drafts remain replaceable, while crashes at
  either between-effect boundary leave durable state for the authoritative
  startup scan.
- INV-035: provider credentials remain behind the hub-controlled credential port
  and outside clients, runners, and durable call records.

No invariant catalog row claims executable enforcement until the corresponding
application and persistence slices land.

## Strongest alternative

Hold one database transaction open while the provider adapter runs, then commit
preparation and outcome together.

This offers an attractive single application call and could roll back local rows
when the provider invocation fails. It is rejected because rollback cannot roll
back a provider effect, a connection loss can still make both commits uncertain,
and a long network stream would hold database locks and a pool connection while
no database work occurs. It would also violate ADR-0040's no-external-effect
transaction discipline and hide the durable pre-send authorization ADR-0005
requires.

## Rejected alternatives

- **Let the provider adapter own both transactions.** That gives an outward
  adapter aggregate and lifecycle authority, admits SQL concerns into provider
  code, and makes a second adapter a second transaction implementation.
- **Pass a database callback or transaction handle into the provider port.** The
  type may look abstract, but it still holds a transaction across external I/O
  and lets the adapter choose persistence timing.
- **Expose one combined persistence-and-provider effect port.** The application
  can no longer enforce that provider I/O occurs between two committed
  boundaries or test each effect count independently.
- **Call the provider before recording issue authorization.** A crash can
  produce an external effect with no durable model-call identity to classify.
- **Retry a failed stage inside the service.** A transaction or provider
  acknowledgement can be lost after success; blind repetition would duplicate
  durable transitions or physical provider work.
- **Use ADR-0040's outbox as provider dispatch.** Its rows and publisher are
  explicitly client-update infrastructure. Reusing them would silently add a
  durable external-effect worker, delivery acknowledgement, and recovery
  protocol that ADR-0040 does not decide.

## Consequences

The first scripted-provider application slice needs three fakes and tests that
prove order, correlation, transaction closure before provider entry, exactly one
call per applicable port, no later-stage call after an earlier-stage failure,
and no retry after a commit-ambiguous or provider-ambiguous result. Persistence
integration tests must exercise both atomic boundaries; startup tests own crash
points after issue and after provider return.

The shape is deliberately more explicit than one repository method. In return,
every durable write and external effect has one owner, provider latency consumes
no database transaction, and failure reports the stage whose outcome needs
reconciliation.

The issue-before-effect crash window can conservatively classify an interaction
that may never have left the process. Removing that uncertainty would require a
separate durable dispatcher or provider idempotency/status contract. This record
prefers honest ambiguity to an invented exactly-once claim.

## Scenario walkthroughs

- **S02:** The prepare transaction resolves and pins the target, records the
  exact frontier and call, and commits issue authorization. Only then does the
  scripted provider port stream replaceable drafts and return its observation.
  The outcome transaction makes final assistant content and lifecycle state
  authoritative together with their client-visible outbox rows.
- **S04:** A crash after issue authorization leaves an exact durable call for
  the startup scan, whether the provider was never reached, was still running,
  or returned before the process died. Recovery uses available evidence,
  preserves uncertainty when necessary, ends the abandoned attempt in its
  matching `...Lost` branch, and never repeats that call automatically.

## Open questions

- The assistant-content and outcome semantic-entry variants, provider/client
  rendering, and final-content commit granularity remain open under the
  identity-representation inventory.
- Provider-specific known-failure, ambiguity, identity, and request-status
  evidence remain ADR-0007 and provider-contract work.
- The exact transient-draft sink, streaming checkpoint policy, and provider
  cancellation delivery mechanism remain later implementation or decision scope
  under their existing open questions.
- Concrete port names, storage schema, SQL isolation and locking, and
  provider-adapter error variants land with their slices while preserving this
  three-effect boundary.

## Explicit non-decisions

This record adds no code, schema, crate, dependency, provider SDK, provider,
runtime, task model, wire type, or assistant-content variant. It does not select
fallback, known-failure retry, refusal remediation, evidence thresholds,
provider identity normalization, request idempotency keys, or a durable
provider-dispatch worker. It does not change ADR-0005's call dispositions,
authority transfer, or outcome precedence; ADR-0031's fatal closure; ADR-0040's
outbox scope; or ADR-0044's runtime, taxonomy, and composition-root contract.

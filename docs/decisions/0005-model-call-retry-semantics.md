# ADR-0005: Model-call retry and configuration identity

- Status: Proposed
- Date: 2026-07-12
- Owners: Repository owner
- Reviewers: Provider, domain, and reliability reviewers unassigned
- Supersedes: none
- Superseded by: none
- Decision-ledger questions: target-before-call creation; no automatic known-failure retry; ambiguous provider-call disposition; acceptance-time alias meaning; model or configuration change identity; turn disposition after provider refusal

## Context

A turn can require multiple provider interactions because it receives steering, consumes tool results, or encounters a provider failure. Provider libraries may also retry transports invisibly. If those cases share one “call” identity, Signalbox cannot explain billing, context, model provenance, ambiguous acceptance, or which external request produced committed content.

Retry rules must preserve user intent without assuming that one turn has one context. They must also prevent a changed model or configuration from being smuggled into recovery.

## Decision

After the hub has resolved and pinned an exact provider/model target and fixed the call's context frontier, every authorization to attempt that provider interaction creates a distinct durable **model call** before the request may be sent. A call therefore exists if later send preparation fails, but target-resolution failure occurs before `ModelCallId` creation and is recorded as attempt and turn failure. A provider-call retry always creates a new `ModelCallId`; no externally issued interaction is overwritten or grouped as though it never occurred.

Provider adapters must not perform hidden automatic retries after the point at which the provider could have accepted a request. A low-level operation proven not to have crossed that boundary may continue preparing the same durable call, but uncertainty about whether the provider accepted it is an ambiguous outcome, not proof that it was unsent.

```text
ModelCallState =
    Prepared
  | InFlight
  | CancellationRequested
  | Terminal(ModelCallDisposition)

ModelCallDisposition =
    Completed
  | KnownFailed
  | Refused
  | Cancelled
  | Ambiguous
```

| From | To | Rule |
| --- | --- | --- |
| Prepared | InFlight | Persist send authorization before crossing the provider boundary; requested selection, exact resolved target, and context frontier already exist on the prepared call |
| Prepared | Terminal(KnownFailed or Cancelled) | Evidence proves the request was not accepted |
| InFlight | CancellationRequested | Best-effort cancellation was durably requested |
| InFlight or CancellationRequested | Terminal(any disposition) | Provider evidence or recovery classification is durably recorded |
| Terminal(any) | any state | Prohibited; late evidence is separate audit/reconciliation evidence |

A provider-call retry may remain in the same turn only when an owner has explicitly authorized continuation from `AwaitingRecoveryDecision` under ADR-0004 and all of these additional conditions hold:

- the prior call produced no committed assistant outcome;
- the prior call is terminally `Ambiguous`, and the exact-set owner decision accepts the duplicate-provider-effect risk;
- the new call uses the same complete frozen effective configuration and exact hub-resolved provider/model target;
- the new call records its own immutable context frontier containing every accepted input and committed semantic fact present in the prior call's frontier, including steering already marked consumed, plus any newly classified failure evidence or later eligible context; and
- retry policy and resource limits authorize another call.

Version one performs no automatic retry after a known provider failure. While orchestration tenure is still live, a known failure ends the current attempt as `KnownFailure` and the turn as `Failed`, including when cancellation was previously requested: the request does not rewrite known failure as cancellation. If startup recovery discovers the call failure, the abandoned attempt remains `Lost` and the turn likewise becomes `Failed`. A future ADR may introduce an explicit known-failure retry command and resource policy, but it must preserve the identity and provenance rules in this record.

A later model call in the same turn is **continuation**, not retry, when it intentionally consumes a newer context frontier containing safe-point steering, tool results, or other committed turn history. It still gets a new model-call identity and uses the same frozen configuration and exact target. No retry or continuation frontier may silently omit accepted steering already consumed by an earlier call, even if that earlier call failed before send or ended ambiguously.

The turn's complete effective configuration, including requested model selection and an immutable version or value snapshot of any alias definition used to interpret it, freezes when the turn is created. ADR-0027 defines its closed version-one semantic membership and its explicitly late-bound exclusions. Every field in that value is identity-significant; recovery compares immutable typed values for semantic value equality rather than record identifiers or a judgment about whether a difference is “material.” Before creating the first `ModelCallId`, the hub resolves that frozen selection meaning to an exact target and pins the target as a durable turn fact. All calls in that turn use that target. Failure to resolve produces no targetless model call and fails the attempt and turn. Re-resolving to a different target, manually choosing another model, or changing any effective-configuration field creates a new logical turn. A future explicitly frozen fallback policy could permit a target change within a turn only after a separate ADR; version one does not infer such permission.

An execution fingerprint or digest may detect equal request material, but it never determines whether a call, attempt, or turn retains identity.

While orchestration tenure is live, `Refused` is a terminal model-call disposition that becomes an explicit committed refusal outcome, ends the current turn attempt `TurnRefused`, and makes the turn `Terminal(Refused)`. If startup recovery first observes refusal for an in-flight call, the call becomes `Refused`, the abandoned attempt remains `Lost`, and the turn becomes `Refused`. If a call and attempt already ended `Ambiguous`, later resolving evidence leaves those terminal physical records unchanged while allowing the waiting turn to become `Refused`. All three paths remain distinct from successful completion, infrastructure failure, cancellation, and ambiguity. A future refusal-remediation ADR may add a typed durable wait or another explicit continuation policy, with corresponding progressing-slot and input-delivery rules.

A non-cancelled `Ambiguous` model call deterministically ends its current turn attempt as `Ambiguous` and places the turn in `Active(AwaitingRecoveryDecision)` carrying that call as an exact wait subject. The turn retains the session slot. No retry occurs until an explicit owner decision authorizes a new call and accepts the duplicate-provider-effect risk, or separately recorded evidence resolves what happened for turn-level decision-making. Owner-authorized continuation preserves the terminal `Ambiguous` call and adds a separate durable `DuplicateRiskAccepted` marker; a later retry outcome may then terminalize the turn normally without pretending the original call was resolved. The owner may instead terminalize the turn as `ReconciliationRequired`. If cancellation was already requested on the running turn, ADR-0004 terminalizes it as `ReconciliationRequired` without entering the wait; cancellation of an existing recovery wait closes that wait and reaches the same terminal disposition atomically.

## Terminology

- **Model call:** One durable hub authorization to attempt a provider interaction; it may terminate before send.
- **Provider-call retry:** A new model call intended to recover a prior model call that did not yield a committed conversational outcome.
- **Continuation call:** A new model call in the same turn that intentionally consumes a later context frontier.
- **Known failure:** Evidence adequately establishes that no usable provider response completed; exact provider acceptance or billing may still be recorded separately when observable.
- **Ambiguous model-call outcome:** Evidence cannot establish whether the provider accepted or completed the request. It is not automatically retryable.
- **Effective-configuration equality:** Semantic value equality over the complete frozen typed configuration value. The baseline has no partially material subset; a value is either inside ADR-0027's closed semantic categories and identity-significant or inside its explicit operational exclusions. A future ADR may add an operational category only by proving that it cannot alter those semantic choices.

## Invariants

- INV-004, INV-006, INV-014–INV-018, INV-025, INV-029, and INV-034 are preserved and made precise.
- Every externally issued provider interaction has one model-call identity and immutable recorded frontier.
- A retry never overwrites the prior call or its cost, timing, provenance, partial evidence, or disposition.
- Every retry or continuation preserves accepted steering already committed into the turn's semantic history.
- No automatic retry follows an ambiguous call outcome.
- No automatic retry follows a known provider failure in version one; it fails the attempt and turn.
- Every model call has an exact target at creation; target-resolution failure creates no model call.
- A call with a different exact resolved target or any different effective-configuration field cannot remain in the same turn under the baseline policy.
- A live refused call ends its attempt as `TurnRefused`; startup-observed refusal leaves the abandoned attempt `Lost`; and evidence arriving after terminal ambiguity leaves call and attempt unchanged. Every path may make the turn `Refused` and none is rewritten as completion or known failure.
- Cancellation request is nonterminal until outcome evidence is classified.
- A non-cancelled ambiguous call retains the turn's active slot in `AwaitingRecoveryDecision`; an ambiguous call is never mapped to failure or terminal reconciliation by scheduler timing alone.

## Strongest alternative

Let each provider adapter use its SDK's ordinary retry behavior and record one logical call around the entire adapter invocation. This is operationally simple and can hide transient failures from orchestration.

It is rejected because an SDK may send more than once, incur multiple charges, receive substitutions, or lose acknowledgements. A single outer record would make those physical interactions and their ambiguity invisible precisely where Signalbox promises inspectable provenance.

## Rejected alternatives

- **Treat every provider failure as a new turn.** Known physical recovery would fragment unfinished user intent.
- **Let any retry stay in the same turn if prompt bytes match.** Equal bytes do not preserve configuration, target, effect evidence, or user authorization.
- **Re-resolve aliases silently on retry.** This can change the actual model while presenting the action as recovery.
- **Automatically retry ambiguous calls because generation is read-only.** Provider processing, billing, retained data, and an unobserved response are still external effects.
- **Represent safe-point continuation as retry.** It consumes intentionally changed context and obscures steering provenance.
- **Mark cancellation successful when the local connection closes.** The provider may still complete the request.

## Consequences

Provider adapters need retry control or observability at the boundary where a request may be accepted. Some provider SDK defaults may need to be disabled later, but no SDK is selected here.

Call histories can contain several continuation calls and, after explicit ambiguity recovery, another physical call for one turn. They remain understandable because each records its frontier and target. Alias convenience is retained at turn creation, while the accepted alias definition and pinned target prevent later drift.

Conservative ambiguous-call handling may require owner action and can delay completion. That cost is preferred to undisclosed duplicate provider work.

## Scenario walkthroughs

- **S02:** The initial call completes normally. If safe-point steering later requires another call, it is a continuation with a newer frontier, not a retry.
- **S04:** Restart ends the abandoned attempt `Lost` and classifies the in-flight call. Known failure fails the turn without retry; recovered completion or refusal may complete or refuse it; ambiguity puts a non-cancelled turn in `AwaitingRecoveryDecision`. Owner-authorized recovery is explicit and retains all evidence and previously consumed steering in the replacement frontier.
- **S20:** The requested alias and its definition version or value snapshot freeze at turn acceptance. The exact target is resolved and pinned before the first model call is created; a later alias-definition change does not alter this turn.
- **S21:** A pinned model remains pinned. Provider-reported substitution is recorded separately and handled by ADR-0007 rather than rewritten as the requested target.
- **S22:** Automatic fallback remains unsupported until ADR-0006. If later accepted, each fallback interaction is still a distinct model call with explicit provenance.
- **S23:** A refusal is terminal for that call and makes the turn `Terminal(Refused)` with an explicit refusal outcome in the baseline; it is not successful completion, a retryable availability failure, or an implicit wait for input.

## Extension implications

An accepted fallback ADR may define a frozen target-selection policy whose authorized target transitions remain within one turn. It must update this ADR's baseline same-target rule, expose the reason, and preserve a separate model-call identity for every interaction.

Provider idempotency keys or request-status APIs can reduce ambiguity, but they add evidence; they do not merge calls or determine logical identity. A dedicated provider service may later own physical execution while preserving the same hub-owned call records and policy.

## Open questions

- Whether a future ADR introduces explicit or automatic retries for known failures, and their backoff/resource limits, remains later provider-policy and resource-governance scope; the version-one answer is none.
- The exact evidence thresholds for known failure versus ambiguous outcome are provider-specific contract work.
- ADR-0007 must decide the disposition of a provider-reported target mismatch.
- ADR-0006 must decide whether any automatic fallback exists.
- Streaming checkpoint and final assistant-content commit granularity remain open.

## Explicit non-decisions

This ADR does not choose provider SDKs, fallback targets, automatic fallback, refusal remediation, alias administration, billing limits, wire protocol, persistence schema, or a final provider-error taxonomy. It does not claim exactly-once provider execution.

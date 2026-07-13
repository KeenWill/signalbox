# ADR-0005: Model-call retry and configuration identity

- Status: Proposed
- Date: 2026-07-12
- Owners: Repository owner
- Reviewers: Provider, domain, and reliability reviewers unassigned
- Supersedes: none
- Superseded by: none
- Decision-ledger questions: provider-call retry versus turn retry; ambiguous provider-call disposition; model or configuration change identity

## Context

A turn can require multiple provider interactions because it receives steering, consumes tool results, or encounters a provider failure. Provider libraries may also retry transports invisibly. If those cases share one “call” identity, Signalbox cannot explain billing, context, model provenance, ambiguous acceptance, or which external request produced committed content.

Retry rules must preserve user intent without assuming that one turn has one context. They must also prevent a changed model or configuration from being smuggled into recovery.

## Decision

Every hub authorization to attempt a provider interaction creates a distinct durable **model call** before the request may be sent. A call therefore exists even if preparation later proves that no request reached the provider. A provider-call retry always creates a new `ModelCallId`; no externally issued interaction is overwritten or grouped as though it never occurred.

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
| Prepared | InFlight | Persist requested selection, exact resolved target, context frontier, and send authorization before crossing the provider boundary |
| Prepared | Terminal(KnownFailed or Cancelled) | Evidence proves the request was not accepted |
| InFlight | CancellationRequested | Best-effort cancellation was durably requested |
| InFlight or CancellationRequested | Terminal(any disposition) | Provider evidence or recovery classification is durably recorded |
| Terminal(any) | any state | Prohibited; late evidence is separate audit/reconciliation evidence |

A provider-call retry may remain in the same turn when it is recovery of unfinished logical work under ADR-0004 and all of these additional conditions hold:

- the prior call produced no committed assistant outcome;
- its failure is known, or an owner explicitly authorizes recovery from a recorded ambiguous outcome;
- the new call uses the same complete frozen effective configuration and exact hub-resolved provider/model target;
- the new call records its own immutable context frontier containing every accepted input and committed semantic fact present in the prior call's frontier, including steering already marked consumed, plus any newly classified failure evidence or later eligible context; and
- retry policy and resource limits authorize another call.

If the hub process and turn attempt remain valid after a known failure, the new model call may belong to the same turn attempt. If orchestration tenure ended, was fenced, or was lost on process restart, ADR-0004 requires a replacement turn attempt first.

A later model call in the same turn is **continuation**, not retry, when it intentionally consumes a newer context frontier containing safe-point steering, tool results, or other committed turn history. It still gets a new model-call identity and uses the same frozen configuration and exact target. No retry or continuation frontier may silently omit accepted steering already consumed by an earlier call, even if that earlier call failed before send or ended ambiguously.

The turn's complete effective configuration, including requested model selection, freezes when the turn is created. Every field in that value is identity-significant in the baseline; recovery compares the typed value for equality rather than asking whether a difference is “material.” The first model call durably resolves the requested selection to an exact target. All later calls and retries in that turn use that target. Re-resolving an alias to a different target, manually choosing another model, or changing any effective-configuration field creates a new logical turn. A future explicitly frozen fallback policy could permit a target change within a turn only after a separate ADR; version one does not infer such permission.

An execution fingerprint or digest may detect equal request material, but it never determines whether a call, attempt, or turn retains identity.

In the baseline lifecycle, `Refused` is a terminal model-call disposition that becomes an explicit committed refusal outcome for the turn. That conversational outcome makes the turn `Terminal(Completed)` rather than failed or waiting for more input. A future refusal-remediation ADR may add a typed durable wait or another explicit continuation policy, with corresponding progressing-slot and input-delivery rules.

A non-cancelled `Ambiguous` model call deterministically ends its current turn attempt as `Ambiguous` and places the turn in `Active(AwaitingRecoveryDecision)`. The turn retains the session slot. No retry occurs until an explicit owner decision authorizes a new call and accepts the duplicate-provider-effect risk, or separately recorded evidence resolves what happened for turn-level decision-making. The terminal call record remains `Ambiguous`. The owner may instead terminalize the turn as `ReconciliationRequired`. If cancellation was already requested, ADR-0004 terminalizes the turn as `ReconciliationRequired` without entering the wait.

## Terminology

- **Model call:** One durable hub authorization to attempt a provider interaction; it may terminate before send.
- **Provider-call retry:** A new model call intended to recover a prior model call that did not yield a committed conversational outcome.
- **Continuation call:** A new model call in the same turn that intentionally consumes a later context frontier.
- **Known failure:** Evidence adequately establishes that no usable provider response completed; exact provider acceptance or billing may still be recorded separately when observable.
- **Ambiguous model-call outcome:** Evidence cannot establish whether the provider accepted or completed the request. It is not automatically retryable.
- **Effective-configuration equality:** Equality over the complete frozen typed configuration value. The baseline has no partially material subset; a field is either part of effective configuration and identity-significant or explicitly defined later as operational and late-bound outside it.

## Invariants

- INV-004, INV-006, INV-014–INV-018, INV-025, INV-029, and INV-034 are preserved and made precise.
- Every externally issued provider interaction has one model-call identity and immutable recorded frontier.
- A retry never overwrites the prior call or its cost, timing, provenance, partial evidence, or disposition.
- Every retry or continuation preserves accepted steering already committed into the turn's semantic history.
- No automatic retry follows an ambiguous call outcome.
- A call with a different exact resolved target or any different effective-configuration field cannot remain in the same turn under the baseline policy.
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

Call histories can contain several known failures and continuation calls for one turn. They remain understandable because each records its frontier and target. Alias convenience is retained at turn creation, while recovery cannot silently drift to a later alias target.

Conservative ambiguous-call handling may require owner action and can delay completion. That cost is preferred to undisclosed duplicate provider work.

## Scenario walkthroughs

- **S02:** The initial call completes normally. If safe-point steering later requires another call, it is a continuation with a newer frontier, not a retry.
- **S04:** Restart classifies the in-flight call. A known failure may lead to a new call, usually in a replacement turn attempt. A non-cancelled ambiguous outcome ends the attempt and puts the turn in `AwaitingRecoveryDecision`; owner-authorized recovery is explicit and retains all evidence and previously consumed steering in the replacement frontier.
- **S20:** An alias is frozen as the requested selection and resolved for the first call. The exact target is pinned for the rest of the turn; a later alias-definition change does not alter retries or continuation calls.
- **S21:** A pinned model remains pinned. Provider-reported substitution is recorded separately and handled by ADR-0007 rather than rewritten as the requested target.
- **S22:** Automatic fallback remains unsupported until ADR-0006. If later accepted, each fallback interaction is still a distinct model call with explicit provenance.
- **S23:** A refusal is terminal for that call and makes the turn `Terminal(Completed)` with an explicit refusal outcome in the baseline; it is not a retryable availability failure or an implicit wait for input.

## Extension implications

An accepted fallback ADR may define a frozen target-selection policy whose authorized target transitions remain within one turn. It must update this ADR's baseline same-target rule, expose the reason, and preserve a separate model-call identity for every interaction.

Provider idempotency keys or request-status APIs can reduce ambiguity, but they add evidence; they do not merge calls or determine logical identity. A dedicated provider service may later own physical execution while preserving the same hub-owned call records and policy.

## Open questions

- Maximum automatic retries for known failures and their backoff/resource limits remain part of provider policy and resource governance.
- The exact evidence thresholds for known failure versus ambiguous outcome are provider-specific contract work.
- ADR-0007 must decide the disposition of a provider-reported target mismatch.
- ADR-0006 must decide whether any automatic fallback exists.
- Streaming checkpoint and final assistant-content commit granularity remain open.

## Explicit non-decisions

This ADR does not choose provider SDKs, fallback targets, automatic fallback, refusal remediation, alias administration, billing limits, wire protocol, persistence schema, or a final provider-error taxonomy. It does not claim exactly-once provider execution.

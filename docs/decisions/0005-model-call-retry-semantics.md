# ADR-0005: Model-call retry and configuration identity

- Date: 2026-07-13
- Amended: 2026-07-17 — [ADR-0031](0031-direct-fatal-terminalization.md) clarifies closed-boundary fatal terminalization
- Owners: Repository owner
- Reviewers: Codex (independent adversarial architecture review); no specialist human reviewer assigned
- Supersedes: none
- Superseded by: none
- Accepted with: ADR-0001, ADR-0003, ADR-0004, and ADR-0027 as one atomic foundation set
- Refined by: [ADR-0031](0031-direct-fatal-terminalization.md)
- Decision questions: target-before-call creation; provider-reported target mismatch; no automatic known-failure retry; ambiguous provider-call disposition; acceptance-time alias meaning; model or configuration change identity; turn disposition after provider refusal

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

ProviderOutcomeAuthorityTransfer = {
    decision_command: DurableCommandId,
    from_call: ModelCallId,
    to_replacement_call: ModelCallId
}

ProviderTargetObservation =
    MatchesResolvedTarget { reported: ProviderModelIdentity }
  | Mismatch {
        reported: ProviderModelIdentity
    }

ProviderTargetEvidence = {
    id: ProviderTargetEvidenceId,
    call: ModelCallId,
    observation: ProviderTargetObservation
}

ProviderTargetMismatchInvalidation = {
    invalidated_call: ModelCallId,
    first_mismatch_evidence: ProviderTargetEvidenceId
}

ProviderTargetMismatchFailureRef =
    NonterminalCallObservation { evidence: ProviderTargetEvidenceId }
  | TerminalAmbiguityResolution { evidence: ProviderTargetEvidenceId }
  | TerminalCallInvalidation { invalidated_call: ModelCallId }
```

The initial call for a provider interaction is outcome-eligible. An owner command that chooses duplicate-risk provider replacement closes the exact recovery wait only by atomically recording `DuplicateRiskAccepted`, applying ADR-0027's guarded consumption of every eligible pending steering input into the replacement frontier, creating the new `Prepared` turn attempt and fully targeted `Prepared` replacement call, and recording `ProviderOutcomeAuthorityTransfer`. The replacement becomes the sole call whose completion, refusal, failure, or cancellation may determine the turn's conversational outcome for that interaction. There is no durable state in which the replacement attempt exists while the prior call remains outcome-authoritative or eligible steering remains pending. The prior call and every later fact learned about it remain durable physical and audit/reconciliation evidence, but none of its outcome evidence can change turn disposition, authoritative assistant content, or a successor frontier. A further replacement transfers authority again along the explicit call chain. This relation changes no call identity or terminal physical disposition.

| From | To | Rule |
| --- | --- | --- |
| Prepared | InFlight | Persist send authorization before crossing the provider boundary; requested selection, exact resolved target, and context frontier already exist on the prepared call |
| Prepared | Terminal(KnownFailed) | Preparation cannot proceed or evidence proves the unsent request failed without an applied interrupt proof |
| Prepared | Terminal(Cancelled) | The aggregate has an exact `AppliedInterruptProof` for this predecessor and evidence proves the request was not accepted; the prepared turn attempt ends atomically under ADR-0004 |
| InFlight | CancellationRequested | Best-effort cancellation was durably requested |
| InFlight or CancellationRequested | Terminal(KnownFailed) | A trusted provider-target observation reports a mismatch; record it, make all response material non-authoritative, and request best-effort cancellation of remaining provider work |
| InFlight or CancellationRequested | Terminal(any disposition) | Provider evidence or recovery classification is durably recorded; physical `Cancelled` without an applied interrupt proof for the turn supplies turn failure, not `TurnDisposition::Cancelled` |
| Terminal(any) | any state | Prohibited; late evidence is separate audit/reconciliation evidence |

`ProviderTargetObservation` is the typed payload of a `ProviderTargetEvidence` record adjacent to call state, not another call disposition. Evidence-identifier lookup precedes current-state validation: replay of the same identifier and structurally equal typed payload returns the recorded result, while reuse with a different call or payload is rejected. A trusted mismatch may arrive in headers or streaming metadata before a final response. The first such observation on an outcome-eligible nonterminal call atomically records the evidence, makes every draft/response/refusal from that execution non-authoritative, moves the call to `Terminal(KnownFailed)`, prohibits new semantic effects from that turn attempt, and durably requests best-effort cancellation of remaining provider work without claiming it stopped. Unless aggregate terminal guards already permit [immediate atomic fatal terminalization](0031-direct-fatal-terminalization.md), the current turn attempt enters ADR-0004's typed `StopRequested(FatalMismatch)` state with `NonterminalCallObservation(evidence)` in its nonempty failure set and preserves any applied interrupt; it never remains effect-authorizing `Running`. Once every other issued operation is classified, the live attempt ends `AfterFatalMismatch(KnownFailure)` and the turn becomes `Terminal(Failed)` when no unacknowledged ambiguity remains; otherwise it ends `AfterFatalMismatch(Ambiguous)` and the turn becomes `Terminal(ReconciliationRequired)` with the exact remaining ambiguity set and `FatalMismatchRequiresReconciliation` carrying the complete fatal stop causes. Later chunks, completion, refusal, or cancellation for the mismatched call are audit evidence only. On startup, newly discovered mismatch evidence for a nonterminal call applies the same call transition and ends the abandoned attempt `AfterFatalMismatch(Lost)` with that observation and any applied interrupt; a durable mismatch observation paired with a nonterminal call is an invalid split state because live persistence is atomic.

If the call had already ended `Ambiguous`, resolving mismatch evidence leaves that physical disposition unchanged, removes the call from the turn-level blocking set, and supplies `TerminalAmbiguityResolution(evidence)` as the typed fatal failure. The operation may have become ambiguous before every other operation issued by its attempt was classified, so attempt handling depends on its current durable state. A live `Running` attempt moves to `StopRequested(FatalMismatch)` with that failure; `StopRequested(CancellationOnly)` upgrades to `FatalMismatch` while preserving its applied interrupt; and an existing `FatalMismatch` value gains the failure by set union. If every terminal guard already holds, the matching transition may instead end the live attempt immediately as `AfterFatalMismatch(KnownFailure or Ambiguous)`. No new semantic effect is permitted on any of these paths. Once remaining operations are classified, the turn becomes `Failed` when no other unacknowledged ambiguity remains or `ReconciliationRequired` with the exact remaining set and complete fatal stop causes otherwise. If the attempt had already ended in its live `...Ambiguous` or startup `...Lost` branch and the turn was waiting, later evidence rewrites neither the call nor that attempt; it closes or refines the wait and applies the same failure/reconciliation outcome. On startup, the scan derives `TerminalAmbiguityResolution` with every other recovered cause, ends a still-nonterminal abandoned attempt `AfterFatalMismatch(Lost)`, and applies the same turn precedence. If the current-authority call instead already ended `Completed` but its turn remains nonterminal, the call and committed history remain immutable, but the hub appends the typed `ProviderTargetMismatchInvalidation` value. Its `invalidated_call` must belong to that turn and match the evidence's call, the evidence's reported identity must mismatch the exact target on that canonical call record, and the call must still be outcome-authoritative when the aggregate transition commits. The evidence does not copy the exact target, and the invalidation does not copy an authority generation: both are derived from the canonical call and transfer chain inside the serialized transition. The value is unique by `invalidated_call`; the first valid mismatch fixes it, structurally equal evidence replay is idempotent, and later observations cannot duplicate or replace it. Cross-wired evidence and races lost to authority transfer are rejected rather than relabeled.

That invalidation makes the call's material unusable for any new semantic effect, closes an approval or authorized-but-undispatched request, and requests best-effort cancellation of already-issued continuation/tool work. A running attempt atomically enters `StopRequested(FatalMismatch)` with `TerminalCallInvalidation(invalidated_call)` in its nonempty failure set unless it can terminalize immediately; any existing fatal failures and applied interrupt are retained. The turn retains its slot until every operation is classified and then becomes `Failed`. Ambiguity produced while stopping follows ADR-0004's fatal-stop precedence and constructs `FatalMismatchRequiresReconciliation` from the exact remaining set and complete fatal stop causes; if the turn was already awaiting an unrelated ambiguity, the invalidation prohibits owner continuation and closes that wait with the same marker while preserving both facts. The typed invalidation, prior committed material, effect evidence, and eventual failure/reconciliation marker all remain visible to successor context rather than erasing history.

Mismatch learned after a call already ended `KnownFailed` or `Cancelled` does not change its physical disposition or existing turn-outcome precedence. An ordinary outcome-authoritative `Refused` call without fatal stop and its attempt/turn refusal terminalize in one aggregate transition in the baseline, so mismatch delivered with that provider outcome is classified first and selects known failure, while evidence first learned afterward is post-turn evidence and cannot rewrite refusal. A continuation refusal raced under fatal stop is the physical exception below and remains non-authoritative. If outcome authority already transferred, the observation is non-authoritative audit/reconciliation evidence for the prior call. Evidence first learned after any valid turn terminalization is likewise late evidence: it appends a reconciliation fact but cannot retroactively demote committed assistant/refusal content, change disposition, or rewrite an already-fixed successor frontier. Absence of a provider-reported identity is not fabricated into either a match or mismatch.

An explicitly authorized fallback, if a future ADR introduces one, creates a separate call with its own exact resolved fallback target. It never converts a provider-reported mismatch on an existing call into an allowed substitution; mismatch against the fallback call's own target follows the same observation rule.

A provider-call retry may remain in the same turn only when an owner has explicitly authorized continuation from `AwaitingRecoveryDecision` under ADR-0004 and all of these additional conditions hold:

- the prior call produced no committed assistant outcome;
- the prior call is terminally `Ambiguous`, and the exact-set owner decision accepts the duplicate-provider-effect risk;
- the new call uses the same complete frozen effective configuration and exact hub-resolved provider/model target;
- the replacement transition applies ADR-0027's safe-point guards and consumes every pending input for the turn in acceptance order, recording each against the new call; the call records its own immutable context frontier containing those inputs, every accepted input and committed semantic fact present in the prior call's frontier, the accepted-risk marker, and any other eligible committed turn history; outcome facts learned from a call after authority leaves it remain non-semantic audit/reconciliation evidence; and
- retry policy and resource limits authorize another call; and
- closing the recovery wait atomically creates the replacement attempt and call and transfers outcome authority from the prior call to that replacement.

Version one performs no automatic retry after a known provider failure. While orchestration tenure is still live and no issued operation remains ambiguous, a known failure ends the current attempt as `KnownFailure` and the turn as `Failed`, including when an interrupt was previously applied: it does not rewrite known failure as cancellation. A physical provider cancellation with no applied interrupt proof follows the same turn-failure path while retaining `ModelCallDisposition::Cancelled`; only that proof plus evidence that the interrupt prevented all remaining work permits ADR-0004's turn `Cancelled { cause }`. An unrelated unacknowledged ambiguity follows ADR-0004 first, entering recovery wait for ordinary failure or proof-bearing reconciliation for fatal mismatch. If startup recovery discovers the call failure or a cause-free physical cancellation, the abandoned attempt has disposition `Lost` in the terminal variant matching the complete recovered stop-cause set, and the turn likewise becomes `Failed` only when no unacknowledged ambiguity requires waiting or reconciliation. A future ADR may introduce an explicit or automatic known-failure replacement policy, but every replacement call must use the same outcome-authority transfer rule unless that ADR explicitly introduces and fully defines a typed multi-result lifecycle.

A later model call in the same turn is **continuation**, not retry, when it intentionally consumes a newer context frontier containing safe-point steering, tool results, or other committed turn history. It still gets a new model-call identity and uses the same frozen configuration and exact target. No retry or continuation frontier may silently omit accepted steering already consumed by an earlier call, even if that earlier call failed before send or ended ambiguously.

The turn's complete baseline effective configuration—one direct selection or alias with immutable definition, provider defaults, and disabled known-failure retry/fallback—freezes when the turn is created. `DirectModelSelection` is a canonical hub-owned key with immutable semantic meaning for exactly one configured provider/model selection, while a frozen alias definition selects exactly one such key; deployment may make it unavailable but cannot retarget it. Neither is provider-reported identity or a fallback policy. Recovery compares immutable typed values for semantic value equality rather than record identifiers or a judgment about whether a difference is “material.” Before creating the first `ModelCallId`, the hub validates and resolves that frozen selection to one exact target and pins the target as a durable turn fact without consulting mutable alias policy or availability. All calls in that turn use that target. Failure to resolve produces no targetless model call and fails the attempt and turn. Re-resolving to a different target, manually choosing another model, or changing any baseline effective-configuration field creates a new logical turn. A future subsystem ADR may add other configuration categories; a future explicitly frozen fallback policy could permit a target change within a turn only after a separate ADR. Version one infers neither.

An execution fingerprint or digest may detect equal request material, but it never determines whether a call, attempt, or turn retains identity.

While orchestration tenure is live and no fatal mismatch stop exists, `Refused` on the outcome-eligible call with no reported target mismatch becomes an explicit committed refusal outcome and atomically ends the current attempt `TurnRefused` and turn `Terminal(Refused)`. This transition is authorized only after every earlier operation and logical dependency is already closed under the same serial orchestration guard required before the call; refusal creates no tool request, continuation, or other owned work. There is therefore no ordinary `Running` or waiting `Refused`-call/nonterminal-turn state. A target observation delivered with the refusal is classified in that same transaction before refusal can commit.

One physical exception does not create a settlement phase: a continuation already issued before a different completed call is invalidated may honestly return `Refused` while its attempt is `StopRequested(FatalMismatch)`. The continuation call becomes physically `Refused`, but its refusal content is non-authoritative because the pre-existing fatal cause prohibits completed/refused turn dispositions. The attempt stays stop-requested until every issued operation is classified, then ends `AfterFatalMismatch(KnownFailure)` or `AfterFatalMismatch(Ambiguous)` and the turn becomes `Failed` or `ReconciliationRequired` with the exact remaining ambiguity set and fatal reason. No new effect, recovery wait, or owner continuation is authorized.

If startup recovery first observes ordinary refusal for an outcome-eligible in-flight call, the call becomes `Refused`, the abandoned attempt remains `WithoutStop(Lost)`, and the turn becomes `Refused` in the same classification transaction only when the complete startup evidence set establishes no fatal mismatch and no unrelated owned work. Startup classification derives every call outcome, target observation, invalidation, and stop cause before selecting turn precedence. Therefore a prior durable fatal stop and a fatal mismatch first established in that same serialized scan are equivalent for outcome selection: a newly observed continuation refusal remains physically `Refused` and non-authoritative, the abandoned attempt ends `AfterFatalMismatch(Lost)` with the reconstructed or newly established causes, and fatal failure or ambiguity—not refusal—determines the turn. Finding unrelated owned work with no fatal cause in the complete evidence set is corrupt baseline state rather than an implicit settlement phase. If an outcome-eligible call already ended `Ambiguous`, later resolving non-mismatched refusal evidence leaves that call unchanged; after every issued operation is classified it allows the turn to become `Refused`, preserving an already-ended attempt or ending a still-live attempt under the ordinary cause-free path. Mismatch evidence instead adds `TerminalAmbiguityResolution` to a still-live attempt's fatal causes or preserves already-ended history, then produces `Failed` only when no other unacknowledged ambiguity remains and otherwise constructs `ReconciliationRequired` with the exact remaining set and complete fatal causes. After authority transfers to a replacement, either fact for the prior call is audit/reconciliation evidence only and cannot compete with the replacement's outcome. These paths remain distinct from successful completion, infrastructure failure, cancellation, and ambiguity. A future refusal-remediation ADR may add a typed durable wait or another explicit continuation policy, with corresponding progressing-slot and input-delivery rules.

An `Ambiguous` model call classified while orchestration is live ends its current turn attempt in the matching `...Ambiguous` branch only after every other issued operation is terminally classified. Until then, the exact call disposition is durable but ADR-0004's aggregate dispatch guard prohibits new semantic effects and permits the unchanged attempt to progress only by classifying or resolving already-issued work. Nonfatal evidence may remove the call from the blocking set without reopening it; once every other issued operation is classified, unfinished work may continue under that same attempt identity with the evidence in later context. If startup first classifies the in-flight call `Ambiguous`, the call receives the same physical disposition but the abandoned prior-process attempt ends in the matching `...Lost` branch. Only when neither an applied interrupt nor fatal mismatch prohibits continuation does a completed live or startup classification with remaining blocking ambiguity place the turn in `Active(AwaitingRecoveryDecision)` carrying that call as an exact wait subject; the turn retains the session slot. No retry occurs until an explicit owner decision authorizes a new call and accepts the duplicate-provider-effect risk, or separately recorded evidence resolves what happened for turn-level decision-making. Owner-authorized continuation preserves the terminal `Ambiguous` call and ended attempt, adds a durable `DuplicateRiskAccepted` marker, consumes eligible steering into the replacement frontier, and atomically creates the replacement attempt and prepared call while transferring outcome authority. The replacement outcome may then terminalize the turn normally; every later completion, refusal, known-failure, or cancellation fact from the prior call remains inspectable but non-authoritative. The owner may instead terminalize the turn with `OwnerChoseReconciliation` and that exact wait set. An applied interrupt or fatal mismatch on a running turn instead constructs the matching stop state and eventual proof-bearing reconciliation marker without entering the wait; interruption or fatal invalidation of an existing recovery wait closes that wait and reaches the same terminal disposition atomically.

## Terminology

- **Model call:** One durable hub authorization to attempt a provider interaction; it may terminate before send.
- **Provider-call retry:** A new model call intended to recover a prior model call that did not yield a committed conversational outcome.
- **Continuation call:** A new model call in the same turn that intentionally consumes a later context frontier.
- **Known failure:** Evidence adequately establishes that no usable provider response completed, including a response from a provider-reported target that mismatches the call's exact resolved target; exact provider acceptance or billing may still be recorded separately when observable.
- **Ambiguous model-call outcome:** Evidence cannot establish whether the provider accepted or completed the request. It is not automatically retryable.
- **Outcome-authority transfer:** A durable relation designating a replacement call as the sole provider call eligible to determine that interaction's conversational outcome while preserving every prior call as physical and audit evidence.
- **Effective-configuration equality:** Structural semantic equality over ADR-0027's model-selection-only baseline. Direct and alias selections remain unequal even when they resolve to the same target; future semantic categories do not exist until their ADR extends the typed algebra.

## Invariants

- INV-004, INV-006, INV-014–INV-018, INV-025, INV-029, and INV-034 are preserved and made precise.
- Every externally issued provider interaction has one model-call identity and immutable recorded frontier.
- A retry never overwrites the prior call or its cost, timing, provenance, partial evidence, or disposition.
- Creating a replacement call atomically transfers provider-outcome authority to it; later evidence from the prior call cannot change the turn's outcome or successor context.
- A duplicate-risk provider decision cannot expose a replacement turn attempt before its prepared replacement call and authority transfer; resolving prior-call evidence and the decision are serialized as competing aggregate transitions.
- That replacement transition consumes every eligible pending steering input under ADR-0027 before preparing the call; the consumed relation references the call rather than duplicating its turn or frontier.
- Every retry or continuation preserves accepted steering already committed into the turn's semantic history.
- No automatic retry follows an ambiguous call outcome.
- No automatic retry follows a known provider failure in version one; it fails the attempt and turn.
- Every model call has an exact target at creation; target-resolution failure creates no model call.
- A call with a different exact resolved target or any different effective-configuration field cannot remain in the same turn under the baseline policy.
- Provider-reported mismatch against a call's exact resolved target is typed evidence. Its first trusted observation while the outcome-eligible call is nonterminal immediately selects `KnownFailed`, requests best-effort cancellation, and prevents response/refusal material from becoming authoritative. When it resolves `Terminal(Ambiguous)`, that physical disposition remains unchanged and the evidence becomes `TerminalAmbiguityResolution`; a still-live attempt gains that fatal cause while preserving any prior failures and interrupt, whereas an already-ended attempt remains unchanged. The turn becomes `Failed` when no other unacknowledged ambiguity remains and otherwise gets exact fatal reconciliation. When first learned after current-authority `Completed` but before turn terminalization, it preserves call/history, appends `ProviderTargetMismatchInvalidation`, stops future effects, and fails or constructs the corresponding fatal marker after outstanding work is classified. `KnownFailed`/`Cancelled`, transferred authority, and terminal turns—including atomically refused turns—remain monotonic under the rules above. Allowed fallback requires another explicitly authorized call.
- A live outcome-authoritative refused call without target mismatch or fatal stop ends its attempt as `TurnRefused` and turn as `Refused` atomically; startup-observed ordinary refusal leaves the abandoned attempt `WithoutStop(Lost)` while terminalizing the turn in the same classification transaction only when the complete recovered evidence set has no fatal cause. Evidence arriving after terminal call ambiguity leaves the call `Ambiguous`; an already-ended attempt remains in its live `...Ambiguous` or startup `...Lost` branch, while a still-live attempt follows the classification and fatal-cause rules above. A continuation that refuses after another call caused fatal stop remains physically `Refused` but non-authoritative, preserves `AfterFatalMismatch`, and cannot make the turn refused; startup applies the same rule when mismatch and refusal are first learned together. Mismatch observed with or before ordinary refusal commit selects known failure, while mismatch resolving ambiguity preserves physical call state and produces failure only with no other unacknowledged ambiguity, otherwise reconciliation. Evidence from a replaced or already-terminal refused turn remains audit/reconciliation evidence without rewriting it.
- Cancellation request is nonterminal until outcome evidence is classified.
- Turn cancellation additionally requires the exact applied interrupt proof for that predecessor; a model call may remain physically `Cancelled` without one, but that cause-free outcome supplies turn failure rather than constructing an unstopped cancelled attempt or turn.
- An ambiguous call retains the turn's active slot in `AwaitingRecoveryDecision` only when neither an applied interrupt nor fatal mismatch prohibits continuation; either stop cause yields a proof-bearing reconciliation marker with the exact set, and scheduler timing alone never chooses between wait and terminal disposition.

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
- **S04:** Restart derives the complete recovered evidence set before ending the abandoned attempt with disposition `Lost` in its matching terminal variant and classifying the in-flight call. Known failure, including reported target mismatch or a physical provider cancellation without an applied interrupt proof, fails the turn without retry when no ambiguity remains; recovered non-mismatched completion may complete it while that call remains outcome-eligible, no fatal cause exists, and aggregate guards hold; recovered ordinary non-mismatched refusal may terminalize call and turn together while leaving the attempt `WithoutStop(Lost)` under the same guards; ambiguity enters `AwaitingRecoveryDecision` only without interrupt/fatal stop. Only an applied-and-confirmed interrupt can make the turn `Cancelled`. Mismatch first discovered after completion while the turn remains active creates one typed invalidation and stops or constructs exact fatal reconciliation exactly as live handling while the abandoned attempt becomes `AfterFatalMismatch(Lost)`. A continuation refusal discovered under a prior or same-scan fatal cause is physical non-authoritative evidence and cannot override failure/reconciliation. Owner-authorized recovery consumes pending steering, retains all evidence and previously consumed steering, and atomically creates the replacement attempt and prepared call while transferring outcome authority, but is invalid after fatal mismatch invalidation. Every later outcome fact from the prior call is audit/reconciliation evidence only.
- **S20:** The requested alias and its immutable definition selecting exactly one canonical direct model selection freeze at turn acceptance. The exact target is validated, resolved, and pinned before the first model call is created; a later alias-definition change does not alter this turn. Provider-reported mismatch against the pinned target follows the timing-sensitive failure rule without making substituted content authoritative.
- **S21:** A pinned model remains pinned. Provider-reported substitution is recorded separately rather than rewritten as the requested target and follows the exhaustive timing matrix: immediate known failure while the call is nonterminal; preserved physical ambiguity with turn failure when no other unacknowledged ambiguity remains, otherwise reconciliation; invalidation/stop after completion during an active turn; unchanged terminal known failure/cancellation with existing precedence; non-authoritative evidence after transfer; or non-rewriting evidence after turn terminality, including an atomically refused turn.
- **S22:** Automatic fallback remains unsupported until ADR-0006. If later accepted, each fallback interaction is still a distinct model call with explicit provenance.
- **S23:** An ordinary outcome-authoritative non-mismatched refusal without fatal stop is terminal for that call and makes the turn `Terminal(Refused)` with an explicit refusal outcome in the baseline. Refusal raced under fatal stop remains only physical evidence, while target mismatch keeps its content non-authoritative and follows the timing-sensitive failure/reconciliation rule without reopening a terminal call. Neither path authorizes implicit fallback or an undefined wait.

## Extension implications

An accepted fallback or automatic-retry ADR may define a frozen target-selection policy whose authorized target transitions remain within one turn. It must update this ADR's baseline same-target rule, expose the reason, preserve a separate model-call identity for every interaction, and transfer outcome authority to each replacement unless it defines a complete typed multi-result lifecycle.

Provider idempotency keys or request-status APIs can reduce ambiguity, but they add evidence; they do not merge calls or determine logical identity. A dedicated provider service may later own physical execution while preserving the same hub-owned call records and policy.

## Open questions

- Whether a future ADR introduces explicit or automatic retries for known failures, and their backoff/resource limits, remains later provider-policy and resource-governance scope; the version-one answer is none.
- The exact evidence thresholds for known failure versus ambiguous outcome are provider-specific contract work.
- ADR-0007 must define provider-identity normalization and provenance representation beyond the typed baseline observation; the mismatch disposition is decided here.
- ADR-0006 must decide whether any automatic fallback exists.
- Streaming checkpoint and final assistant-content commit granularity remain open.

## Explicit non-decisions

This ADR does not choose provider SDKs, fallback targets, automatic fallback, refusal remediation, alias administration, billing limits, wire protocol, persistence schema, or a final provider-error taxonomy. It does not claim exactly-once provider execution.

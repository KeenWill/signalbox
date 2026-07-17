# ADR-0004: Logical turn and physical attempt lifecycle

- Status: Accepted
- Date: 2026-07-13
- Amended: 2026-07-17 — [ADR-0031](0031-direct-fatal-terminalization.md) adds the closed-boundary direct fatal-reconciliation edge
- Owners: Repository owner
- Reviewers: Codex (independent adversarial architecture review); no specialist human reviewer assigned
- Supersedes: none
- Superseded by: none
- Accepted with: ADR-0001, ADR-0003, ADR-0005, and ADR-0027 as one atomic foundation set
- Refined by: [ADR-0031](0031-direct-fatal-terminalization.md)
- Decision questions: startup scan versus recovery attempt identity; manual-regeneration identity boundary and later scope; actively progressing state set; aggregate running-attempt and terminalization guards; state-specific cancellation; exact-set ambiguity decisions; refusal attempt/turn disposition with ADR-0005

## Context

Signalbox must serialize conversational progress per session while allowing physical orchestration to stop at approvals, child waits, process loss, and recovery boundaries. “One active turn” is not enforceable if active means only “currently using CPU,” and “same turn on retry” is unsafe if a retry may silently change user intent, configuration, or already-observed effects.

The domain needs a testable logical-work boundary, a physical-attempt boundary, terminal states, and an explicit definition of which states retain the session's single-progressing-turn slot.

## Decision

A **turn** is one durable logical request for Signalbox to produce one conversational outcome from one typed origin under one frozen effective configuration. It owns an ordered history of orchestration decisions and committed semantic effects. It may use several context frontiers and survive zero or more turn attempts.

A **turn attempt** is one exclusive physical orchestration tenure for one active turn. An attempt begins in `Prepared` when orchestration is durably authorized to advance that turn. Initial turn activation creates the current attempt; closure of a durable wait creates one only when unfinished work remains and the applicable policy permits continuation. A closure supported by a terminal outcome creates none, and there is no intermediate `Active(Running)` state without a current attempt. An attempt ends when the turn becomes terminal, orchestration reaches a durable external wait, the attempt fails or is lost, cancellation finishes, or ambiguity blocks continuation.

The turn aggregate owns the `Active(Running)` phase together with the complete state of its current attempt and applies their transitions atomically. Persistence may use separate records, but the domain transition never loads or mutates them as independent aggregates and never permits a turn/attempt pair that the state machine cannot construct.

### Turn states and the progressing slot

```text
AcceptedInputTurnLifecycle = {
    order: AcceptedInputQueueOrder,
    state: TurnState
}

TurnState =
    Queued
  | Active {
        start: AcceptedInputTurnStart,
        phase: ActivePhase
    }
  | Terminal {
        start: AcceptedInputTurnStart,
        disposition: TurnDisposition
    }

ActivePhase =
    Running {
        current_attempt: CurrentTurnAttempt
    }
  | AwaitingApproval { request: ToolRequestId }
  | AwaitingRecoveryDecision { ambiguous_operations: NonEmptyIssuedOperationRefs }

TurnDisposition =
    Completed
  | Refused
  | Failed
  | Cancelled { cause: AppliedInterruptProof }
  | ReconciliationRequired { marker: ReconciliationMarker }

CurrentTurnAttempt =
    Prepared { id: TurnAttemptId }
  | Running { id: TurnAttemptId }
  | StopRequested {
        id: TurnAttemptId,
        causes: TurnAttemptStopCauses
    }

AppliedInterruptProof = {
    command: DurableCommandId,
    predecessor: TurnId
}

AppliedInterruptState =
    NoAppliedInterrupt
  | Applied { proof: AppliedInterruptProof }

FatalMismatchStopCauses = {
    failures: NonEmptySet<ProviderTargetMismatchFailureRef>,
    interrupt: AppliedInterruptState
}

TurnAttemptStopCauses =
    CancellationOnly {
        interrupt: AppliedInterruptProof
    }
  | FatalMismatch(FatalMismatchStopCauses)

ReconciliationMarker = {
    ambiguous_operations: NonEmptyIssuedOperationRefs,
    reason: ReconciliationReason
}

ReconciliationReason =
    OwnerChoseReconciliation {
        decision: AppliedStopForReconciliationProof
    }
  | InterruptRequiresReconciliation {
        interrupt: AppliedInterruptProof
    }
  | FatalMismatchRequiresReconciliation {
        causes: FatalMismatchStopCauses
    }
```

The names are typed pseudocode, not a final Rust API. ADR-0027 defines `AcceptedInputQueueOrder` and `AcceptedInputTurnStart`. The baseline accepted-input turn retains immutable `order` outside its lifecycle state in every variant. `Queued` carries no start payload, while every `Active` or `Terminal` state carries nonoptional starting lineage and frontier. The transition table uses `Active(X)` and `Terminal(Y)` as shorthand and elides both the aggregate's preserved `order` and the `start` value that every transition after eligibility preserves unchanged; `Terminal(Cancelled)` likewise elides its required applied-interrupt proof, and `Terminal(ReconciliationRequired)` elides its required marker. Future non-input origins must introduce their own constructible order/start payloads with their origin lifecycle.

`NonEmptyIssuedOperationRefs` is a nonempty mathematical set whose baseline element is `ModelCall(ModelCallId)` or `ToolAttempt(ToolAttemptId)`. Duplicate or empty caller collections cannot construct this value and fail at the nonclaiming command boundary defined by ADR-0001. Valid reorderings construct the same canonical set and compare equal for both wait comparison and structural command deduplication. It is not a stringly generic identifier; a future physical-operation kind must add an explicit tagged case. Carrying the exact references lets recovery and cancellation close the right wait without reconstructing a hidden prior phase.

`AppliedInterruptProof` is not an arbitrary command reference. It is constructible only from the committed applied result of ADR-0027's `SubmitInput::Interrupt` for the exact `predecessor`; the same transaction creates that interrupting accepted input and its immediate-successor turn. A rejected, non-interrupt, cross-session, or differently targeted command cannot construct it. Version one has no standalone command for cancelling an active turn without creating the interrupt successor; adding one requires a future ADR and a distinct proof variant.

Every `ReconciliationMarker` carries the exact canonical nonempty set that remains physically `Ambiguous` and unacknowledged when terminalization commits. Resolved operations and operations with `DuplicateRiskAccepted` are excluded. `AppliedStopForReconciliationProof` is likewise constructible only from an applied exact-set `ResolveAmbiguity::StopForReconciliation` command for that turn and set. The other reasons preserve either the applied interrupt proof or the complete `FatalMismatchStopCauses` value established for terminalization. When an attempt is still live, that value is exactly the stop value carried into its terminal history; when fatal evidence arrives after an attempt already ended in a wait, the same type is constructed from every applicable fatal failure and the exact applied-interrupt state without rewriting the attempt. Empty, cross-turn, resolved, accepted-risk, cause-free, incomplete, or cross-wired markers are unconstructible.

Additional durable waits, including delegated-child waits, require explicit typed variants and transition review; there is no catch-all wait state. A future wait retains the progressing slot unless the ADR that introduces it also defines branching or rebasing semantics that justify an exception.

For the one-active-turn rule, **actively progressing means `TurnState::Active` in any phase**. It describes ownership of the session's ordered-progress slot, not continuous execution. A turn awaiting approval or recovery authorization, and a turn carrying `CurrentTurnAttempt::StopRequested`, retain the slot. Queued and terminal turns do not.

At most one turn per session may be `Active`. Activating a queued successor is prohibited until the current active turn reaches a terminal disposition. The enforcement mechanism is left to scheduler and persistence design, but process memory alone is insufficient.

`Eligible` is a derived scheduling predicate, not another durable `TurnState`. A queued turn is eligible only when every turn earlier in ADR-0027's durable total queue order is terminal and the session has no active turn. Immutable acceptance positions, typed priority relations, and the active-slot owner are durable, so restart recomputes the same predicate without persisting a second lifecycle state. The eligibility transition fixes the exact immediate-predecessor starting lineage and context frontier once; queued turns do not carry direct predecessor pointers that priority insertion would have to rewrite.

When a live active turn reaches a durable approval or recovery-decision wait, every physical operation already issued by the attempt must first have a durable terminal classification. The current attempt then ends with the cause-specific disposition: approval yields to the wait, while live ambiguity ends it `Ambiguous`. Startup classification is different only for the orchestration tenure: it may classify an issued operation `Ambiguous`, but it ends the abandoned prior-process attempt `Lost` in the matching terminal variant. In both cases the turn remains active and retains the session slot without retaining a live attempt. When resolving evidence or a valid owner decision closes the wait and unfinished work remains, a new turn attempt continues the same turn. Evidence-resolved continuation does not repeat an ambiguous operation and is not an ambiguity retry. A hub restart while the turn is already waiting reconstructs that wait and its already-ended attempt; it does not pretend that a process-local attempt remains alive.

Individual operation classification may precede attempt end. A **blocking ambiguity** is a physically `Ambiguous` owned operation for which neither resolving evidence nor `DuplicateRiskAccepted` supplies a turn-level disposition. If a blocking ambiguity exists while another issued operation remains unclassified, the current attempt retains its existing `Running` identity only to classify already-issued work and accept resolving evidence; an aggregate dispatch guard prohibits every new semantic effect. This is a relation over the attempt's durable owned-operation evidence, not another attempt variant or an optional side flag. When all other issued work is classified, the same aggregate transition either ends the attempt `Ambiguous` and enters the exact recovery wait, or applies the stop/failure precedence below. Nonfatal evidence may remove the last blocking ambiguity without changing the physical disposition; after every other issued operation is classified, the guard lifts and unfinished work may continue under the same still-live attempt identity with the resolving evidence in later context. A fatal mismatch instead moves the attempt to `StopRequested(FatalMismatch)` or ends it immediately in `AfterFatalMismatch` when all terminal guards already hold.

### Allowed turn transitions

| From | To | Allowed reason |
| --- | --- | --- |
| Queued | Active(Running) | Once eligible, the scheduler preserves immutable `order`, atomically constructs nonoptional `AcceptedInputTurnStart` with the exact starting lineage and frontier, acquires the session slot, and creates the initial `Prepared` attempt |
| Queued | Terminal(Failed) | Once eligible, the same transition preserves immutable `order`, constructs the exact nonoptional starting lineage/frontier payload, and records why work cannot execute; a queued turn cannot terminalize ahead of an earlier ordered turn |
| Active(Running with current attempt `Prepared`) | Active(Running with the same attempt `Running`) | Preparation succeeds and the aggregate authorizes external orchestration at the durable boundary |
| Active(Running with current attempt `Running`) | Active(Running with the same attempt `Running`) | An issued operation becomes `Ambiguous` while another issued operation remains unclassified; the exact physical outcome is durable, the aggregate dispatch guard prohibits new semantic effects, and the unchanged attempt remains live only for classification or resolving evidence |
| Active(Running with current attempt `Running`) | Active(Running with the same attempt `Running`) | Nonfatal evidence removes one or more blocking ambiguities without changing their physical dispositions. The guard remains while another blocking ambiguity or unclassified issued operation exists; otherwise it lifts and the same attempt may continue unfinished work without repeating the resolved operation |
| Active(Running with current attempt `Running`) | Active(AwaitingApproval) | Every issued operation is terminally classified, the current attempt ends `YieldedToDurableWait`, and the exact approval request becomes durable |
| Active(Running with current attempt `Running`) | Active(AwaitingRecoveryDecision) | One or more issued operations are `Ambiguous`, every other issued operation is terminally classified, and the current attempt ends `Ambiguous`; startup classification instead ends the abandoned attempt `Lost` while making the same turn-state transition |
| Active(Running with current attempt `Running`) | Active(Running with the same attempt `StopRequested { causes }`) | Unless terminal guards permit an immediate atomic end, the hub durably requests best-effort stop because it applied an interrupt for this predecessor or recorded a fatal current-authority provider-target mismatch, including `TerminalAmbiguityResolution` for an already-ambiguous call; `CancellationOnly` or `FatalMismatch` is persisted in the state that prohibits new effects |
| Active(Running with current attempt `StopRequested { causes: S }`) | Active(Running with the same attempt `StopRequested { causes: S' }`) | A distinct valid stop cause arrives, `S'` is the typed set union, and no cause is lost; replay of an existing cause is idempotent |
| Active(Running with current attempt `Prepared`) | Terminal(Cancelled) | Applying `SubmitInput::Interrupt` for this exact predecessor atomically ends the unsent attempt `AfterCancellation` with its proof, cancels any prepared physical operations, and makes every owned authorized-but-undispatched logical request non-dispatchable with an exact cancellation outcome; no durable stop phase is needed |
| Active(AwaitingApproval) | Active(Running) | Approval closes the exact approval wait/dependency, leaves the owned tool request authorized and blocking until its durable outcome, and creates a new turn attempt atomically |
| Active(AwaitingApproval) | Active(Running) | Denial closes the exact approval wait/dependency, terminally denies the owned tool request, commits that outcome to turn history, and creates a new turn attempt atomically; denial cannot create a tool attempt |
| Active(AwaitingRecoveryDecision) | Active(Running) | No fatal current-authority mismatch invalidation exists; unfinished work remains; the applicable operation-specific policy permits continuation; and either an exact-set owner decision accepts duplicate risk or new evidence resolves every blocking ambiguity. The wait closes and a new attempt is created atomically. For an owner-authorized replacement of an ambiguous model call, that same transaction first applies ADR-0027's safe-point guards and consumes every eligible pending steering input into the replacement frontier, then creates the fully targeted `Prepared` replacement call and transfers outcome authority under ADR-0005; no durable intermediate state exposes the new attempt while the prior call remains outcome-authoritative or steering remains eligible-but-pending |
| Active(AwaitingRecoveryDecision with set S) | Active(AwaitingRecoveryDecision with set S') | A separate evidence record resolves the blocking uncertainty for one or more references without changing their terminal physical dispositions; `S'` is the exact nonempty strict subset still blocking, and no attempt is created |
| Active(AwaitingApproval) | Terminal(Cancelled) | An applied interrupt for this exact predecessor supplies the required proof, atomically closes the approval wait, and terminally cancels the owned tool request so it is non-dispatchable; no physical attempt exists to cancel |
| Active(AwaitingApproval) | Terminal(Failed) | A late current-authority provider-target mismatch atomically appends its invalidation/failure marker, closes the approval wait, terminally cancels the owned tool request so it is non-dispatchable, and fails the turn; no physical attempt exists |
| Active(AwaitingRecoveryDecision) | Terminal(ReconciliationRequired) | An applied interrupt, or a fatal mismatch invalidation on a different completed call that prohibits continuing past the still-unresolved wait, atomically closes the recovery wait and constructs a marker from the wait's exact remaining set plus the matching interrupt or fatal reason; the mismatch path also retains its invalidation marker and prior call history |
| Active(Running with current attempt `Prepared`) | Terminal(Failed) | Preparation fails before running, a late current-authority provider-target mismatch invalidates the work before issue, or startup loses the prepared tenure without ambiguity; ordinary live failure ends `WithoutStop(KnownFailure)`, live fatal mismatch ends `AfterFatalMismatch(KnownFailure)` with its exact cause, startup abandonment without a fatal cause ends `WithoutStop(Lost)`, and a fatal cause first established by that startup scan ends `AfterFatalMismatch(Lost)` |
| Active(Running with current attempt `Running`) | Terminal(Completed, Refused, or Failed) | The terminalization preconditions below hold and durable evidence permits the exact disposition. Completion/refusal require no fatal stop. A fatal mismatch may end directly as `AfterFatalMismatch(KnownFailure)` only when no other work requires an observable stop phase and the exact unacknowledged ambiguity set is empty; a nonempty unacknowledged ambiguity set instead takes the direct `ReconciliationRequired` row below. Startup recovery ends the abandoned attempt with disposition `Lost` in `WithoutStop`, `AfterCancellation`, or `AfterFatalMismatch` according to the complete recovered cause set while making the supported turn-state transition |
| Active(Running with current attempt `Running`) | Terminal(ReconciliationRequired) | Under [ADR-0031](0031-direct-fatal-terminalization.md), trusted fatal-mismatch evidence may close the aggregate directly only when every terminal guard can be satisfied atomically and the exact unacknowledged ambiguity set is nonempty. The same transaction ends the attempt `AfterFatalMismatch(Ambiguous)` and commits a marker carrying that exact set and `FatalMismatchRequiresReconciliation` with the same complete fatal causes; no intermediate `StopRequested` is persisted. |
| Active(Running with current attempt `Running`) | Terminal(Cancelled) | An interrupt is applied for this exact predecessor when the aggregate can already prove that it prevented all remaining work and every terminal guard holds; the same atomic transition records the proof and ends the attempt `AfterCancellation(Cancelled)` without a durable intermediate `StopRequested` state |
| Active(AwaitingRecoveryDecision) | Terminal(Completed, Refused, Failed, or ReconciliationRequired) | Resolving evidence supports an ordinary outcome; a physical cancellation learned without an applied interrupt proof supplies failure rather than turn cancellation; or an exact-set owner decision either accepts risk when existing evidence already supports an outcome, or stops for reconciliation. The stop choice constructs `OwnerChoseReconciliation` from its applied command and the wait's exact set. Physical ambiguous records remain terminal and accepted risk is marked separately |
| Active(Running with current attempt `StopRequested { causes: CancellationOnly }`) | Terminal(Completed, Refused, Cancelled, Failed, or ReconciliationRequired) | Issued work reaches honest terminal classification after best-effort cancellation; ambiguity constructs `InterruptRequiresReconciliation` from the exact unacknowledged set and applied interrupt proof, while cancellation alone does not rewrite raced completion/refusal or known failure |
| Active(Running with current attempt `StopRequested { causes: FatalMismatch }`) | Terminal(Failed or ReconciliationRequired) | Issued work reaches honest terminal classification after fatal stop; ambiguity constructs `FatalMismatchRequiresReconciliation` from the exact unacknowledged set and the complete `FatalMismatchStopCauses` value. The typed branch makes completion, refusal, and cancellation unrepresentable |
| Terminal(any) | any state | Prohibited |

Transitions between different wait variants are prohibited; orchestration must resume through a new attempt before it can reach another kind of wait. The one exception is an evidence-only refinement of `AwaitingRecoveryDecision` to the exact nonempty subset that remains ambiguous. No transition enters a wait while stop is requested, and no transition enters any wait until every operation already issued by the attempt is terminally classified. An interrupt never replaces a durable wait with a generic state: approval interruption closes the exact approval wait, terminally cancels its owned tool request, and terminalizes the turn as `Cancelled { cause }`, while recovery-wait interruption closes that exact wait and terminalizes with an `InterruptRequiresReconciliation` marker over the wait set. A fatal current-authority mismatch discovered during a recovery wait either resolves the waited call's own ambiguity and applies the ordinary failure transition, or, when unrelated ambiguity still blocks failure, closes the wait with a `FatalMismatchRequiresReconciliation` marker preserving both facts. No owner continuation is valid after that invalidation. Queued-input mutation and cancellation and standalone active-turn cancellation are not baseline features, so no user-driven `Queued -> Cancelled` transition is defined here outside applied interrupt handling.

Before any active turn becomes terminal and releases the progressing slot, one atomic transition must:

1. durably close every owned logical tool request and approval dependency or give it an exact terminal outcome that makes it non-dispatchable, including an authorized request for which no tool attempt was created;
2. durably classify every model call, tool attempt, or other issued physical operation owned by the turn;
3. end the current turn attempt, if one exists;
4. close or terminally dispose any outstanding durable wait so a late decision cannot resume the turn;
5. commit the conversational outcome or explicit refusal, failure, applied-interrupt cancellation proof, or complete reconciliation marker supporting the turn disposition; and
6. reclassify pending safe-point input as required by ADR-0027.

An unresolved logical tool/approval dependency or unclassified issued operation prohibits terminalization even if local orchestration has stopped. An authorized-but-undispatched request must be closed explicitly so a scheduler cannot dispatch it after the turn releases its slot. An `Ambiguous` operation also blocks ordinary terminalization until a separate evidence record resolves its uncertainty for turn-level decision-making or an exact-set owner command durably records `DuplicateRiskAccepted` for that operation. Neither path reopens or changes the terminal physical disposition; it changes whether that known uncertainty still blocks the turn. Owner acknowledgement also ensures every later terminal outcome and successor frontier includes an explicit accepted-risk marker. A late result received after valid terminalization is audit or reconciliation evidence only and cannot advance the terminal turn or overwrite the successor's already-fixed context.

### Attempt lifecycle

```text
TurnAttemptState =
    Prepared
  | Running
  | StopRequested { causes: TurnAttemptStopCauses }
  | Ended(AttemptEnd)

AttemptEnd =
    WithoutStop { disposition: UnstoppedAttemptDisposition }
  | AfterCancellation {
        cause: AppliedInterruptProof,
        disposition: CancellationStopDisposition
    }
  | AfterFatalMismatch {
        causes: FatalMismatchStopCauses,
        disposition: FatalMismatchStopDisposition
    }

UnstoppedAttemptDisposition =
    TurnCompleted
  | TurnRefused
  | YieldedToDurableWait
  | KnownFailure
  | Lost
  | Ambiguous

CancellationStopDisposition =
    TurnCompleted
  | TurnRefused
  | KnownFailure
  | Lost
  | Cancelled
  | Ambiguous

FatalMismatchStopDisposition =
    KnownFailure
  | Lost
  | Ambiguous
```

| From | To | Rule |
| --- | --- | --- |
| Prepared | Running | External orchestration is authorized after the durable preparation boundary |
| Prepared | Ended(AfterCancellation(Cancelled)), Ended(AfterFatalMismatch(KnownFailure or Lost)), or Ended(WithoutStop(KnownFailure or Lost)) | A live applied interrupt or fatal mismatch records its typed cause as cancelled or known failure; startup uses `AfterFatalMismatch(Lost)` when its complete scan first establishes a fatal cause for the abandoned prepared tenure, while unrelated preparation failure or startup loss has no stop cause |
| Running | StopRequested | An applied interrupt or fatal mismatch, including mismatch evidence that resolves an already-ambiguous call for turn-level decision-making, records a nonempty singleton cause unless terminal guards permit an immediate end carrying that same cause |
| Running | Ended(AfterCancellation(Cancelled)) | An applied interrupt for the exact predecessor proves prevention of all remaining work while every terminal guard permits an immediate atomic end; the terminal variant carries that proof without a durable intermediate `StopRequested` phase. |
| Running | Ended(AfterFatalMismatch(KnownFailure or Ambiguous)) | During live handling, a fatal cause arrives when every terminal guard already permits a complete atomic end. [ADR-0031](0031-direct-fatal-terminalization.md) requires `KnownFailure` when no unacknowledged ambiguity remains and `Ambiguous` when the same transaction commits exact fatal reconciliation. |
| Running | Ended(AfterFatalMismatch(Lost)) | The startup scan first establishes or reconstructs a fatal cause while abandoning the prior-process tenure; startup retains its distinct `Lost` disposition. |
| StopRequested(CancellationOnly) | StopRequested(FatalMismatch) | Fatal mismatch adds its nonempty failure set while preserving the applied interrupt proof |
| StopRequested(FatalMismatch) | StopRequested(FatalMismatch) | Another fatal failure is added by set union, or the first interrupt adds its applied proof; replay is idempotent and either event order constructs the same value |
| Running | Ended(WithoutStop(TurnCompleted, TurnRefused, YieldedToDurableWait, KnownFailure, Lost, or Ambiguous)) | Evidence is classified, the turn completes, startup loses the tenure, or orchestration yields to a durable wait |
| StopRequested(CancellationOnly(C)) | Ended(AfterCancellation { cause: C, disposition }) | Evidence is classified without claiming rollback; raced completion/refusal remains possible, while yielding to a durable wait is not |
| StopRequested(FatalMismatch(F)) | Ended(AfterFatalMismatch { causes: F, disposition }) | The exact fatal causes and optional applied interrupt are preserved; only known failure, loss, or ambiguity is representable, so completion/refusal/cancellation and durable-wait yield cannot be constructed |
| Ended | any state | Prohibited |

Exactly one `CurrentTurnAttempt` is carried by `Active(Running)`. Its variant is the sole nonterminal attempt state, so no independent turn-level cancellation flag can disagree with it. `StopRequested` is nonempty by construction: `CancellationOnly` carries the applied interrupt proof, while `FatalMismatch` contains a nonempty set of exact mismatch failures and may also retain one applied interrupt. Adding mismatch then interrupt or interrupt then mismatch constructs the same fatal value, and replay is idempotent. Every stop-requested state prohibits new semantic effects except classification of work already issued. The same prohibition applies relationally to `Running` while any blocking ambiguity or other unclassified issued operation exists; only classification or resolving evidence may then advance it. Resolving evidence leaves the physical disposition immutable but can remove the operation from the blocking set, and the same attempt regains dispatch authority only after that set is empty and every other issued operation is classified. Terminal attempt history preserves the matching typed value: `AfterCancellation` permits honest raced completion/refusal, whereas `AfterFatalMismatch` can represent only known failure, loss, or ambiguity. Neither can yield to a durable wait, and `WithoutStop` cannot claim `Cancelled`; restart therefore cannot construct fatal-stop completion/refusal/cancellation or infer an optional side flag. Applying an interrupt to an unsent `Prepared` attempt ends it atomically with the turn and records the proof. A live late mismatch during `Prepared` records its fatal cause and ends as known failure; one first established while startup abandons that prepared tenure records the same cause with disposition `Lost`. Mismatch evidence that resolves an already-ambiguous call while the attempt is still live adds `TerminalAmbiguityResolution` by moving `Running` to `FatalMismatch`, upgrading `CancellationOnly` while preserving its interrupt, or unioning into existing fatal causes. A waiting active turn has no attempt and carries the exact request or nonempty ambiguous-operation references on which it waits. An interrupt against a wait closes that wait and terminalizes in one transition, so there is no stop state with an optional attempt or a hidden prior wait. A new attempt must reference the ended attempt whose wait it continues, and stale attempts cannot advance turn state.

If the current attempt ends while the turn remains nonterminal, the same transaction must move the turn into a typed durable wait. Later resolving evidence or a valid owner decision may atomically close that wait and create a new attempt. The domain never leaves the turn in `Active(Running)` without a current attempt, even briefly in durable state.

### Recovery, replacement, and new logical work

A **recovery retry** is an owner-authorized decision to repeat or continue past the same nonterminal turn after accepting unresolved duplicate risk. An **evidence-resolved continuation** resumes unfinished work only after evidence has resolved every blocking ambiguity and does not repeat an operation whose outcome remains unknown. Both remain subject to the operation-specific retry policy: in particular, ADR-0005's version-one ban on known-provider-failure retry cannot be bypassed by calling it continuation. A **physical attempt replacement** is the new turn attempt created after either permitted path closes the recovery wait.

On startup, the hub scans every nonterminal attempt owned by an earlier process incarnation. It classifies each recorded physical operation from durable evidence as completed, known failed, refused where applicable, cancelled, or ambiguous; derives every stop cause established by that complete evidence set; and ends the abandoned attempt with disposition `Lost` in the matching `WithoutStop`, `AfterCancellation`, or `AfterFatalMismatch` terminal variant. A fatal cause first learned during this scan is therefore retained just like one durable before the crash. The scan is one serialized recovery-bookkeeping transition, not a turn attempt, and it cannot issue new semantic provider or tool effects.

Recovery applies one precedence rule whether classification happens live or at startup. Startup first derives the complete physical-evidence set, target invalidations, and typed stop causes in its single transaction; it does not let whichever recovered fact happened to be examined first choose the turn outcome. First, any remaining unacknowledged ambiguity blocks an ordinary terminal outcome: if any fatal mismatch exists it constructs `FatalMismatchRequiresReconciliation` with the complete `FatalMismatchStopCauses` value, including any applied interrupt; only when no fatal cause exists does an applied interrupt construct `InterruptRequiresReconciliation`; without either it enters `AwaitingRecoveryDecision`. Thus fatal reason always dominates interrupt reason when both are present, independent of event order. Each marker carries the exact canonical set still ambiguous in that transaction. Once no blocking ambiguity remains, sufficient conversational completion evidence from the currently outcome-eligible provider call yields `Completed` only if no fatal current-authority mismatch cause or invalidation exists. A non-mismatched refusal without fatal stop terminalizes call and turn atomically under ADR-0005; a continuation refusal raced under a fatal cause already durable or first established in that same startup transaction remains a physical call disposition while the attempt/turn follow failure precedence. A trusted mismatch observed while a call is nonterminal selects call `KnownFailed`; one first learned after its `Completed` disposition while the turn remains active preserves that call state but appends `ProviderTargetMismatchInvalidation`. Either supplies turn-level failure evidence. Otherwise a known failure that prevents completion yields `Failed`. Only when an `AppliedInterruptProof` exists and effect-specific evidence proves that the interrupt prevented all remaining work does the turn become `Cancelled { cause }`; a physical operation reported `Cancelled` without that proof is honest physical evidence but supplies turn-level failure instead. Evidence from a provider call whose authority transferred to a replacement is retained but does not participate in this precedence or semantic-content selection. An applied interrupt alone proves none of those outcomes; a fatal-mismatch cause references the separate exact failure evidence that does. A prior-process tenure abandoned with no operation ambiguity and no sufficient authoritative completion, refusal, or applied-and-confirmed interrupt evidence fails under the version-one no-automatic-retry policy. The abandoned attempt has disposition `Lost` regardless of the turn disposition, in the terminal variant matching the complete stop-cause set. An already durable wait is simply reconstructed. Startup never creates a cancellation-only or classification-only replacement attempt. A new attempt exists only after resolving evidence or a valid owner decision closes `AwaitingRecoveryDecision`, and fatal mismatch invalidation makes continuation invalid.

Closure of a provider ambiguity wait serializes the owner decision against newly resolving provider evidence. If completion, refusal, known-failure, or confirmed-cancellation evidence for the still-authoritative prior call commits first, the common precedence applies and a now-stale duplicate-risk command cannot create a replacement. If `ContinueAcceptingDuplicateRisk` commits first, its single transaction closes the exact wait, records accepted risk, consumes pending steering under ADR-0027, creates the new `Prepared` turn attempt and fully targeted `Prepared` replacement call with that frontier, and transfers outcome authority; every later fact about the prior call is audit/reconciliation evidence only. Restart reconstructs whichever transaction won and never restores authority to a replaced call or a consumed input to pending state.

Recovery remains in the same turn only when all of the following hold:

- the exact typed turn origin and its immutable origin content are unchanged;
- the frozen effective configuration is unchanged;
- all previously committed semantic content and effect evidence remain part of the turn history rather than being overwritten;
- no already-issued effect would be blindly repeated; and
- no terminal conversational outcome has been committed.

Context is not required to be byte-for-byte immutable. The replacement continues from the turn's latest durable context, including pending or already-consumed steering and committed tool or failure outcomes. It may not silently discard those facts to recreate an earlier prompt.

Turn identity is selected by typed domain transitions, not by comparing free-form intent. Input explicitly accepted as safe-point steering remains in the current turn. Input accepted as a no-active-turn, interrupt, or after-current origin creates a new turn. Recovery commands may reference an unfinished turn but cannot replace its origin content. No transition asks an implementation to decide whether two natural-language objectives have the “same intent” or are “materially different.”

The following create **new logical work** and therefore a new turn identity:

- a new accepted input used as a turn origin;
- an owner-requested model or effective-configuration change for a conversational outcome;
- an explicit future regeneration command requesting another alternative outcome; or
- any future typed origin-creation command rather than a recovery command referencing unfinished work.

Manual regeneration, if introduced, always creates a new turn and never reopens, overwrites, or adds another attempt to the original turn, even when input and configuration are identical. Version one does not yet expose that command or include a regeneration origin variant in the implementable state machine. A future ADR must define its immutable source frontier, queue placement, configuration-freeze boundary, and typed relation before adding it.

Updating session configuration defaults under ADR-0027 is not itself conversational work and creates no turn. It changes only how later origin input is resolved; any already accepted turn keeps its frozen value.

### Cancellation and ambiguity

Cancellation is a forward-only request to stop future progress. In the baseline it is authorized only by applying `SubmitInput::Interrupt`, which creates the successor and an `AppliedInterruptProof` for the exact predecessor in the same transaction. For a running turn with no prior stop cause, application directly ends the attempt `AfterCancellation(Cancelled)` and turn `Cancelled { cause }` only when the aggregate already has effect-specific proof that the interrupt prevented all remaining work and every terminal guard holds in that same transaction. Otherwise it moves the exact current attempt to `StopRequested(CancellationOnly)`, sends best-effort cancellation to current model calls and tool attempts, makes authorized-but-undispatched requests non-dispatchable, and prevents new effects unless needed to classify already-issued work. If a fatal-mismatch stop is already present, application populates its interrupt field without authorizing a cancelled turn. The proof or a locally closed connection is not evidence that an external operation stopped: turn `Cancelled { cause }` requires both that applied proof and effect-specific terminal evidence, while uncertainty is `Ambiguous`. A provider- or runner-originated physical `Cancelled` outcome without an applied interrupt does not fabricate that proof; it makes the unmet turn `Failed` unless stronger precedence applies. Provider or runner work may still finish after the request; raced evidence is classified honestly, and evidence arriving after terminalization follows the late-result rule above. For an approval wait, the interrupt proof closes the exact wait, terminally cancels its owned tool request, and terminalizes the turn as `Cancelled { cause }`. For a recovery-decision wait, it closes the exact wait and constructs `InterruptRequiresReconciliation` over that wait's exact operations. Cancellation does not roll back, compensate, or declare an external effect absent.

The turn cannot become `Cancelled`, `Failed`, `Completed`, or `Refused` while an issued effect has a blocking ambiguous outcome. When every other issued operation is classified, live classification ends the current turn attempt `Ambiguous`; while another remains unclassified, the attempt stays live under the no-new-effects dispatch guard. Startup classification leaves the physical operation `Ambiguous` but ends the abandoned turn attempt `Lost` in its matching terminal variant. When no stop cause or fatal invalidation requires terminal reconciliation, either completed classification path moves the turn to `Active(AwaitingRecoveryDecision)` and retains the session slot. The ambiguity is never resolved merely by scheduler or effect-policy timing. Separate evidence may remove it from the blocking set without reopening its physical state; if the attempt is still live and unfinished work remains, dispatch resumes only after every other issued operation is classified. An exact-set `ContinueAcceptingDuplicateRisk` decision may instead mark the ambiguity acknowledged without changing its physical disposition, but it is invalid after fatal mismatch invalidation; ordinary terminalization is permitted only with that accepted-risk marker in semantic history.

An explicit owner recovery decision is a typed command rather than an unbound boolean:

```text
ResolveAmbiguity = {
    command_id: DurableCommandId,
    turn: TurnId,
    expected_operations: NonEmptyIssuedOperationRefs,
    choice: ContinueAcceptingDuplicateRisk | StopForReconciliation
}

AppliedStopForReconciliationProof = {
    decision_command: DurableCommandId,
    turn: TurnId
}

AmbiguityAcknowledgement =
    DuplicateRiskAccepted {
        decision_command: DurableCommandId,
        operations: NonEmptyIssuedOperationRefs
    }
```

The acknowledgement is a typed semantic-history value, not a string status and not a replacement physical disposition. The command is valid only when `turn` is currently awaiting exactly `expected_operations`; stale, partial, or expanded sets are rejected. `AppliedStopForReconciliationProof` is constructible only from the applied result of this exact command with `choice: StopForReconciliation`; a rejected command, another choice or command kind, or a command for another turn/set cannot construct it. If `ContinueAcceptingDuplicateRisk` would create another attempt, the planned continuation must satisfy every applicable provider and tool-effect policy; owner acknowledgement does not bypass a prohibition on repeating a write. Direct terminalization supported by other durable evidence issues no repeat and therefore requires no retry permission. Durable command deduplication is checked before current-state validation: replaying the same command identifier with the same payload returns its recorded result, while reusing that identifier with a different payload is rejected. Newly discovered provider or runner evidence is a separate typed evidence transition, not an implicit owner choice.

An accepted decision or newly recorded evidence may then do exactly one of the following:

- record separate resolving evidence, remove exactly the resolved references from the wait, and—when none remain—either continue unfinished work in a new attempt if the operation-specific policy permits it, or terminalize according to the resulting evidence;
- accept duplicate risk while preserving the ambiguous record and durably marking each exact referenced operation `DuplicateRiskAccepted`, then either continue unfinished work in a new attempt or terminalize directly when other durable evidence already supports an ordinary outcome; or
- stop the turn as `Terminal(ReconciliationRequired)` with `OwnerChoseReconciliation`, the applied decision proof, and the wait's exact operation set.

No option reopens or overwrites the ambiguous physical operation, and no duplicate-risk retry is automatic. Before acknowledgement, an applied interrupt plus ambiguity leads directly to `Terminal(ReconciliationRequired)` with the exact set and `InterruptRequiresReconciliation`. When a replacement provider call is created, outcome authority transfers to it under ADR-0005; later completion, refusal, failure, or cancellation of the prior call is classified and retained as evidence but cannot compete with the replacement's conversational outcome. The replacement's outcome is classified normally and retains the accepted-risk marker; an interrupt does not retroactively revoke that acknowledgement. If an interrupt is applied while the turn is still in `AwaitingRecoveryDecision`, the transition closes that wait and constructs the same marker atomically. Later reconciliation of a terminal turn records new evidence separately; it does not return a terminal attempt or turn to `Running` or rewrite its marker.

## Terminology

- **Effective configuration:** The durable, immutable configuration governing a turn's semantic execution choices. Every field in this value is identity-significant in the baseline. ADR-0027 fixes its creation boundary; ADR-0005 defines model-selection implications.
- **Progressing slot:** The per-session exclusivity right held by an active turn, including while durably waiting.
- **Durable wait:** A typed state whose continuation depends on separately arriving evidence or a decision, such as approval or a future child result.
- **Recovery retry:** Continuation of unfinished logical work without changing its semantic identity.
- **Physical attempt replacement:** A new attempt identity created after resolving evidence or a valid owner decision closes a durable wait left by an earlier attempt.
- **Manual regeneration:** A future typed command for new logical work requesting another outcome related to a prior turn; its identity rule is decided here, while its command and context lifecycle remain unimplemented.
- **Ambiguous outcome:** Evidence is insufficient to establish whether an external effect occurred; it is not a known failure.

## Invariants

- INV-004, INV-006, INV-009–INV-011, INV-025, INV-026, INV-029, and INV-034 are preserved and made precise.
- INV-009 changes from provisional state membership to the exact rule that every `Active` phase retains the slot.
- A running turn carries exactly one `CurrentTurnAttempt`; its `Prepared`, `Running`, or `StopRequested` variant is the single nonterminal attempt state. `StopRequested` is either `CancellationOnly` with an applied interrupt proof or `FatalMismatch` with a nonempty failure set and optional applied interrupt; additions are idempotent and event-order independent, and matching terminal variants make fatal-stop completion/refusal/cancellation unrepresentable. Applying an interrupt to a `Prepared` attempt ends it and the turn atomically. A live fatal mismatch ends a prepared attempt `AfterFatalMismatch(KnownFailure)`, while a fatal cause first established as startup abandons that tenure ends it `AfterFatalMismatch(Lost)`. Each waiting active turn carries its exact wait subject and no attempt.
- Ending a current attempt for a nonterminal turn atomically moves the turn to a typed wait; only later resolving evidence or a valid owner decision may close that wait and create a replacement.
- No terminal turn or attempt returns to a nonterminal state.
- `TurnDisposition::Cancelled` carries one exact `AppliedInterruptProof` for that predecessor and is constructible only when effect-specific evidence proves the applied interrupt prevented all remaining work. A physical `Cancelled` operation without that proof supplies failure evidence; the unstopped attempt algebra therefore needs no cancellation disposition.
- A turn cannot terminalize or release its slot until every owned logical tool/approval dependency is durably closed or terminally non-dispatchable, every issued physical operation is durably classified, its current attempt is ended, and any durable wait is closed.
- A live provider refusal without reported target mismatch and without fatal stop uses call, attempt, and turn dispositions `Refused`, `TurnRefused`, and `Refused` in one aggregate transition; serial orchestration makes a refused-call/effect-authorizing-turn state invalid. A continuation that races refusal under `StopRequested(FatalMismatch)` remains physically refused and non-authoritative while the attempt ends `AfterFatalMismatch` and the turn fails or reconciles. Startup applies the same distinction from its complete evidence set: a fatal cause already durable or first established in that scan makes the abandoned attempt `AfterFatalMismatch(Lost)` and keeps refusal non-authoritative; without a stop cause it is `WithoutStop(Lost)` and ordinary refusal may terminalize the turn. Evidence after terminal call ambiguity always leaves the call `Ambiguous`. If its attempt already ended, that terminal history remains unchanged; if other issued work keeps the attempt live, mismatch evidence adds `TerminalAmbiguityResolution` to its complete fatal causes and prohibits any new effect until classification ends it. Mismatch delivered with or before ordinary refusal commit selects call known failure; mismatch resolving terminal ambiguity fails the turn when no other ambiguity remains and requires reconciliation otherwise; evidence after an atomically refused turn cannot rewrite it.
- No recovery retry changes origin, effective configuration, committed semantic history, or known effect evidence.
- Every provider replacement call atomically becomes outcome-authoritative; prior-call evidence remains durable but cannot create a second conversational outcome or change successor context.
- Ambiguity is never coerced to cancellation or known failure to free the session slot.
- An unacknowledged ambiguous issued effect enters `AwaitingRecoveryDecision` only when neither an applied interrupt nor fatal mismatch/invalidation requires terminal reconciliation; only a typed owner decision or new evidence may then continue or terminalize it. Owner-authorized continuation preserves the physical ambiguity and adds an explicit accepted-risk marker that later terminal outcomes must retain.
- No applied-interrupt or fatal-mismatch stop enters `AwaitingRecoveryDecision`; either cause plus unresolved ambiguity closes any existing recovery wait and terminalizes with the exact proof-bearing reconciliation marker, and fatal invalidation prohibits owner continuation.

## Strongest alternative

Release the progressing slot whenever a turn is not executing a provider or tool call, allowing queued turns to run while the earlier turn awaits approval or another future durable dependency. This improves apparent concurrency and avoids blocking a session on a slow decision.

It is rejected for the initial architecture because later turns could advance the transcript and external state before the earlier turn resumes, changing its context and making approval or child results apply across interleaved logical work. Supporting that behavior would require explicit branching or rebase semantics, not an “inactive” label on waits.

## Rejected alternatives

- **One immutable context per turn.** Safe-point steering and committed tool results require later model calls to observe a later frontier.
- **One physical attempt per turn.** Explicit continuation after a durable wait needs a new physical tenure without changing logical intent.
- **Keep attempts live while waiting.** This invents process ownership across restart and ties durable waits to leases.
- **Treat every retry as a new turn.** Known recovery would fragment one unfinished request and complicate idempotent restart.
- **Treat regeneration as another attempt.** It would overwrite or multiply outcomes under one logical identity after the owner asked for an alternative.
- **Use a digest as the retry boundary.** Equal bytes do not establish equal intent, effect evidence, or user-visible history.
- **Free the slot on cancellation request.** Issued effects could still complete while replacement work begins against a false rollback assumption.

## Consequences

An approval or recovery-decision wait blocks later turns in the same session in version one. A future delegated-result wait will do the same unless its defining ADR introduces explicit branching or rebasing. Independent work can use another session. This is deliberately conservative and keeps transcript ordering testable.

Attempt records become more numerous around resumed waits, but each describes an actual physical tenure. Startup scans create no attempt records. Recovery code must classify evidence before an owner can authorize replacement and cannot use “retry” as a generic state reset.

Terminal reconciliation-required turns release the progressing slot while preserving the complete immutable exact-set/reason marker for successor context. Reconciliation is a separate lifecycle and may affect later work only through a new durable fact; it never rewrites that terminal marker.

Ambiguity without an applied interrupt or fatal mismatch can therefore block later turns until the owner decides. This is intentionally stronger than selecting terminal reconciliation from scheduler timing: the owner must explicitly choose when unresolved evidence is allowed to release the ordered-progress slot.

## Scenario walkthroughs

- **S03:** Restart reconstructs a queued turn with `AcceptedInputQueueOrder` or an active wait with nonoptional `AcceptedInputTurnStart`. Eligibility is derived from durable total queue order and slot ownership; the activation or eligible-failure transition constructs starting lineage and frontier together. If running orchestration was lost, the startup scan ends its current attempt, classifies every issued operation, and never creates a replacement by itself.
- **S04:** A lost provider stream never changes turn identity by itself. Startup ends the abandoned turn attempt in the matching `...Lost` branch while separately classifying the call; a recovered ambiguous call puts the turn in `AwaitingRecoveryDecision` only when neither an applied interrupt nor fatal mismatch prohibits continuation. Live ambiguity ends the current attempt in the matching `...Ambiguous` branch before entering the same wait only after every other issued operation is classified; until then it preserves the attempt under the classification-only dispatch guard. Either stop cause yields a reconciliation marker containing the exact ambiguous set and matching reason, and a known provider failure without ambiguity fails the turn. Resolving target-mismatch evidence after terminal ambiguity leaves the call `Ambiguous`; it preserves an already-ended attempt, but a still-live attempt gains `TerminalAmbiguityResolution` in its complete fatal causes before ending as known failure or ambiguity. The turn fails when no other unacknowledged ambiguity remains and constructs fatal reconciliation for the exact remaining set otherwise. Mismatch first discovered after call completion while the turn remains active creates typed invalidation and applies the same stop/failure-or-reconciliation rule as live handling; the abandoned attempt ends `AfterFatalMismatch(Lost)` whether that cause was durable before the crash or first established by the scan. A recovered ordinary non-mismatched refusal terminalizes the turn atomically only when the complete evidence set has no fatal cause; refusal from a continuation under a prior or same-scan fatal cause remains physical evidence while the turn fails or reconciles.
- **S06:** A tool write classified ambiguous while orchestration is live ends its attempt in the matching `...Ambiguous` branch and retains the turn slot in `AwaitingRecoveryDecision`; if startup first classifies it, the write remains `Ambiguous` while the abandoned attempt ends in the matching `...Lost` branch. Either stop cause instead preserves the physical ambiguity and makes the turn `ReconciliationRequired` with the exact set and matching interrupt or fatal reason. Later tool policy may prohibit continuation, but scheduler timing cannot silently choose it or release the slot.
- **S07:** Interrupting an unsent `Prepared` attempt ends it and the turn atomically. If accepting an interrupt against `Running` work already proves every terminal guard, the same transaction ends the attempt `AfterCancellation(Cancelled)`, ends the turn `Cancelled { cause }`, reclassifies pending steering, and releases the slot; otherwise it records `StopRequested(CancellationOnly)` and retains the slot until honest classification. Interrupting an approval or recovery wait closes that exact wait and terminalizes atomically. A restart performs the startup scan without creating cancellation-only work. The interrupt-created successor remains queued.
- **S08:** Pending safe-point steering belongs to the active turn. The turn retains its slot through waits, and later calls may consume a newer frontier.
- **S10:** Entering `AwaitingApproval` ends the current attempt with `YieldedToDurableWait`; approval creates a new attempt for the same active turn.
- **S11:** Denial closes the exact approval wait, commits the denial to turn history, and creates a new attempt for conversational continuation without creating a tool attempt.
- **S18:** A future typed child wait must retain the parent session slot and end the current attempt, but ADR-0002 must define that variant and its cancellation/result transitions before implementation.
- **S26:** Manual regeneration is new logical work and leaves the original terminal state and attempts unchanged, but it remains outside the implementable state machine until its command, queue, and source-frontier rules are accepted.

## Extension implications

Future interleaving could add branching or explicitly rebased turns, but cannot redefine a version-one wait as non-progressing without revisiting context and approval semantics. Additional typed waits may be added and retain the slot under this rule unless a later ADR defines a different concurrency model.

Attempt lineage supports explicit continuation after a durable wait without selecting a workflow engine. Startup recovery is a scan and classification transition rather than a fabricated orchestration tenure. Reconciliation can later gain richer typed evidence while preserving terminal attempt facts.

## Open questions

- Scheduler locking, wake-up, leases, and Postgres coordination remain under ADR-0010.
- Approval expiry and child-result delivery remain in their respective future ADRs; ADR-0002 must add any child-wait phase and parent-cancellation transitions.
- The evidence threshold for a physical operation's `KnownFailed` versus `Ambiguous` outcome is effect-specific and remains with provider and tool policies; `Lost` is reserved for the abandoned turn attempt. Once an operation is classified as ambiguous, its turn disposition follows the deterministic rule above.
- Resource limits may constrain how long a turn can retain a slot, but timeout disposition requires a later policy.
- Manual-regeneration command acceptance, queue placement, configuration freeze, and exact historical frontier remain open and block that feature, but not the initial accepted-input-origin turn state machine.

## Explicit non-decisions

This ADR does not choose scheduler infrastructure, persistence schema, tool retry eligibility, tool-risk taxonomy, delegation waits or cancellation, model fallback, archive behavior, manual-regeneration command/context semantics, or any process protocol. It does not define compensation for external effects or promise that cancellation succeeds.

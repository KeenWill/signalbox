# ADR-0027: Accepted-input delivery and context frontiers

- Date: 2026-07-13
- Owners: Repository owner
- Reviewers: Codex (independent adversarial architecture review); no specialist human reviewer assigned
- Supersedes: none
- Superseded by: none
- Accepted with: ADR-0001, ADR-0003, ADR-0004, and ADR-0005 as one atomic foundation set
- Refined by: [ADR-0030](0030-context-frontier-snapshots.md) and [ADR-0034](0034-durable-command-storage-and-equality.md) for typed durable-command storage and structural replay equality
- Decision questions: no-active-turn input; durable-command deduplication; versioned session defaults; safe points, inherited configuration provenance, and terminal disposition; queued work creation, constructible effective configuration, and configuration freeze; state-specific interrupt and successor order; context frontiers; baseline queue mutation scope

## Context

Input may arrive while a turn is executing a provider call, performing a tool attempt, waiting for approval or child work, or stopping. Signalbox accepts three active-work intents: interrupt, next safe point, and after current turn. Durable acknowledgement is insufficient if the input can later disappear, inherit a changed configuration silently, or consume an implicit “latest” transcript whose contents depend on recovery timing.

The first domain state machines also need an explicit no-active-turn command, queue creation boundary, safe-point definition, and terminal disposition for steering that never reaches another model call.

## Decision

### Submission and atomic acceptance

The conceptual command distinguishes input submitted with no active turn from the three active-work delivery modes:

```text
DeliveryRequest =
    StartWhenNoActiveTurn { configuration: PerInputConfigurationChoices }
  | Interrupt { expected_active_turn: TurnId, configuration: PerInputConfigurationChoices }
  | NextSafePoint { expected_active_turn: TurnId }
  | AfterCurrentTurn { expected_active_turn: TurnId, configuration: PerInputConfigurationChoices }

PerInputConfigurationChoices = {
    expected_session_defaults_version: SessionConfigurationDefaultsVersion,
    model: ModelSelectionOverride
}

ModelSelectionOverride =
    UseSessionDefault
  | ReplaceWith(ModelSelectionRequest)

SessionConfigurationDefaults = {
    model: ModelSelectionRequest
}

ConfigurationRequest = {
    model: ModelSelectionRequest
}

OriginConfiguration = {
    requested: ConfigurationRequest,
    session_defaults_version: SessionConfigurationDefaultsVersion,
    effective: EffectiveConfiguration
}

SubmitInput = {
    command_id: DurableCommandId,
    session_id: SessionId,
    content: UserContent,
    delivery: DeliveryRequest
}
```

This is typed pseudocode, not a protocol or final Rust API. `SubmitInput` is one discriminated caller-command variant used for idempotency; comparison excludes `command_id` itself but includes the `SubmitInput` discriminator and the canonical typed value of every other caller-supplied semantic field, never server-derived `OriginConfiguration`. `UseSessionDefault` and `ReplaceWith(X)` remain structurally distinct even when the current default is `X`, because canonical construction cannot consult mutable aggregate state before command lookup. A form that cannot construct `SubmitInput` fails before that lookup. Each immutable `SessionConfigurationDefaults` version contains one complete normalized model-selection request. Session creation establishes version one, and a separate idempotent owner command may install a complete replacement.

For an unseen command, atomic input acceptance requires the caller's expected defaults version still to be current, derives one complete `ConfigurationRequest` from the explicit model override or named default, selects the current immutable alias definition when an alias was requested, resolves that request to `EffectiveConfiguration`, and persists the derived `OriginConfiguration`. Application orchestration may calculate a candidate outside the transaction, but the commit must compare-and-set the expected defaults version and the version or immutable value of every alias definition it read. A defaults mismatch is an authoritative acceptance rejection, is recorded as the command result, and cannot silently adopt a newer version for the same caller payload; the client must submit a new command to select it. An alias-definition race may be recomputed before commit or abort handling as a retryable pre-commit transaction conflict that claims no command identifier. Thus all derived semantic meaning is linearized at successful acceptance rather than at an earlier read.

Updating session defaults creates a new immutable model-selection defaults version and affects only origin inputs accepted afterward. It does not create a turn, mutate queued or active work, alter pending steering, or change recovery. The conceptual update command carries a durable command identifier, the session identifier, the expected current defaults version, and the replacement typed model request. Its deduplication and compare-and-set behavior follow the command rule below.

The first implementable `EffectiveConfiguration` is deliberately model-selection-only. Its complete constructible algebra is:

```text
ModelSelectionRequest =
    Direct(DirectModelSelection)
  | Alias(ModelAlias)

FrozenModelSelection =
    Direct(DirectModelSelection)
  | FrozenAlias {
        alias: ModelAlias,
        definition: FrozenAliasDefinition
    }

EffectiveConfiguration = {
    model: FrozenModelSelection,
    parameters: ProviderDefaults,
    known_provider_failure_retry: Disabled,
    model_fallback: Disabled
}
```

`DirectModelSelection` is a canonical domain-owned key with immutable semantic meaning that names exactly one configured provider/model selection. Deployment may make that selection unavailable, causing resolution failure, but cannot retarget the same key. It is never an alias, policy, fallback set, provider-native unnormalized identifier, or provider-reported identity. `FrozenAliasDefinition` is an immutable definition version or value selecting exactly one `DirectModelSelection`; its current version is selected by the atomic acceptance transition. Resolution later validates that frozen selection and pins one exact target or fails—it cannot reread mutable alias policy, availability, or fallback to choose another selection. Concrete storage encodings and provider-reported identifier normalization remain outside this ADR.

`ProviderDefaults` is one unit choice meaning Signalbox supplies no model-parameter overrides. Custom parameters, instructions, tool enablement/configuration, placement constraints, per-turn resource choices, and interpreting-policy selections are unavailable baseline capabilities, not latent optional fields or values filled from mutable state. The disabled known-provider-failure retry and model-fallback fields make the two lifecycle prohibitions explicit. A future subsystem ADR must extend the request, session-default, override, and effective-value algebras together before exposing another semantic category.

The exact provider/model target is resolved from the frozen selection meaning and pinned as a separate turn fact under ADR-0005 before the first model call is created; it is not an `EffectiveConfiguration` field. Also outside this boundary are provider credentials, live endpoints and connections, scheduler locks and leases, process or runner connection identity, telemetry, transient availability, and transport timing. Those late-bound operational facts must be recorded when relevant but may not change the requested model selection or authorize retry/fallback. A future value that changes semantic execution belongs in an explicitly extended `EffectiveConfiguration` and requires new logical work when changed.

Configuration equality is structural semantic value equality over `FrozenModelSelection` and the unit policy values. Direct and alias selection remain unequal even when they resolve to the same exact target because requested selection and alias provenance differ. The defaults version belongs to `OriginConfiguration` provenance, not effective-value equality. Equality of database identifiers alone is insufficient unless an identifier names one immutable version with stable meaning.

For any successfully constructed durable command, the hub first looks up owner-global `command_id` before validating current session state or deriving server-owned values. Transport decoding, establishment of owner authority, and purpose-specific canonical construction of caller fields precede that lookup; none consults mutable aggregate state or server-derived configuration. The lookup spans every command kind, session, and client under that owner. A recorded identifier with the same canonical discriminated typed command payload returns its terminal applied-or-rejected result even if session state, defaults, or aliases have since changed. Reusing the identifier for a different command variant, session, or canonical payload is rejected. Equality is structural domain equality over the discriminator and canonical caller fields, not equality of serialized bytes. Only an unseen owner-global identifier proceeds to state validation and configuration derivation. The first handling transaction that commits atomically stores the comparison payload and either the applied result or a typed authoritative domain rejection. A malformed transport request, failure to establish owner authority, caller fields that cannot construct the typed command, or infrastructure/transaction failure before this commit stores no durable command result and does not claim the identifier. The hub validates the delivery request against authoritative session state inside that handling transaction:

- `StartWhenNoActiveTurn` is valid only when the session has no active turn. If earlier queued turns exist during a scheduler gap, the new turn joins their FIFO tail rather than bypassing them.
- The three active-work modes are valid only when `expected_active_turn` is the session's current active turn.
- `NextSafePoint` is rejected if that turn carries `CurrentTurnAttempt::StopRequested`. `Interrupt` is rejected when the stop value is `CancellationOnly` or its `FatalMismatch.interrupt` is already `Applied`; a fatal-mismatch-only stop may still accept the first interrupt, create its immediate successor, and populate that proof without reauthorizing predecessor work.
- `AfterCurrentTurn` remains valid while stop is pending because it makes no promise of steering or initiating cancellation.

If authoritative validation rejects stale state or loses a state race, the transaction records that typed rejection under the command identifier; no `AcceptedInputId` is created or acknowledged. Replaying the same identifier and payload returns the recorded rejection rather than revalidating against newer state. The client may refresh and submit a new explicit choice with a new command identifier. The hub never silently normalizes a stale active-work request into no-active-turn or queued behavior.

On success, the hub atomically persists the applied command result together with the accepted input identity, content, requested delivery mode and target, session ordering position, applicable origin or fallback configuration provenance required below, and its initial durable disposition before acknowledgement.

### Work creation and configuration freeze

```text
AcceptedInputDisposition =
    OriginOf(TurnId)
  | PendingSteering { binding: SteeringBinding }
  | ConsumedAsSteering { call: ModelCallId }
  | ReclassifiedAsTurnOrigin { turn: TurnId, reason: NoSafePointBeforeTerminal }

SteeringBinding = {
    source_turn: TurnId
}

TurnConfigurationProvenance =
    ExplicitOrigin(OriginConfiguration)
  | InheritedForReclassifiedSteering(SteeringBinding)
```

`StartWhenNoActiveTurn`, `Interrupt`, and `AfterCurrentTurn` each create a turn and freeze its effective configuration in the same transaction that accepts the input. The accepted input immediately becomes that turn's origin. Input submitted with no active turn is eligible immediately only if no earlier queued turn exists; otherwise it joins the FIFO tail. `Interrupt` and `AfterCurrentTurn` create queued turns.

Effective configuration freezes the complete baseline algebra above. Its typed equality is total: any model-selection difference requires new logical work, while changes to explicitly excluded operational facts do not. The alias definition is already frozen, and exact target resolution occurs during the initial prepared attempt but before model-call creation as defined by ADR-0005. If static pre-activation validation proves a frozen configuration structurally unsupported, the already-created turn waits until its predecessors terminate, fixes its starting frontier, and fails without an attempt. If target resolution then fails after activation, the prepared attempt ends `KnownFailure` and the turn fails. Neither path lets input disappear, adopt newer session defaults, or terminalize ahead of its lineage.

`NextSafePoint` initially creates no turn. Its command variant contains no independent configuration request. It captures one `SteeringBinding` containing only the exact source turn; that turn already owns the canonical immutable effective configuration. If terminalization later reclassifies the input, the new origin reuses that binding as `InheritedForReclassifiedSteering` and atomically sets its own required effective configuration equal to the referenced source turn's canonical value. It does not store a second source configuration in the binding, duplicate a source-turn field, invent a request, or claim that current session defaults produced the value. A missing source turn or any attempt to supply a different configuration is invalid. Any model or configuration change must instead be submitted as origin input for new logical work. The explicit delivery request determines identity: the hub does not inspect natural-language content to decide whether steering expresses the “same” or a “materially different” objective.

```text
AcceptedInputQueueOrder = {
    acceptance_position: SessionInputPosition,
    priority: Ordinary | InterruptImmediatelyAfter { predecessor: TurnId }
}
```

Accepted origin turns persist `AcceptedInputQueueOrder`, not a direct predecessor pointer. `Ordinary` work is ordered by immutable acceptance position, while the interrupt relation places its new turn immediately after the active predecessor and ahead of all then-unstarted ordinary work; nested interrupts compose the same rule. Reclassified steering receives `Ordinary` order using its original accepted-input position. Together those facts form one durable total order for currently known work. A queued turn's direct starting predecessor is fixed only when it becomes eligible, after every priority insertion ahead of it has become durable. The lifecycle therefore does not represent an unfixed predecessor as an optional or sentinel identifier.

### Delivery-mode behavior

| Mode | Durable acceptance effect | Logical work | Interaction with issued work | Restart behavior |
| --- | --- | --- | --- | --- |
| Start with no active turn | Persist input, origin turn, configuration, and acceptance position; derived eligibility holds if no predecessor is queued | New turn immediately | None is active | Reconstruct queued or active work and derive eligibility from durable queue order and slot ownership |
| Interrupt | Persist input and successor configuration plus an `AppliedInterruptProof` and the exact predecessor transition atomically | New turn, designated as the active turn's immediate successor | End an unsent `Prepared` attempt and turn; directly end a `Running` attempt only when the interrupt is already proven to have prevented remaining work and every terminal guard holds, otherwise request best-effort cancellation; or close the exact durable wait; never roll back issued work | Reconstruct the applied proof, cancellation or reconciliation marker, issued-effect evidence, and queued successor; startup scans abandoned attempts without creating recovery work |
| Next safe point | Persist input as pending steering with a canonical source-turn reference | No new turn unless reclassified | Does not mutate an issued provider call, tool request, approval, or tool attempt | Reconstruct pending steering and derive immutable inherited configuration from its source turn; consume or reclassify durably |
| After current turn | Persist input, queued turn, immutable acceptance position, and configuration | New queued turn | Does not cancel or alter current work | Reconstruct exact queue order and frozen configuration |

The first applied interrupt records a typed priority relation designating its origin turn as the immediate successor of the active turn, ahead of every other successor candidate. This includes ordinary after-current turns already queued and any pending safe-point input that may be reclassified when the interrupted turn terminalizes. It does not rewrite a direct predecessor because queued turns do not fix one before eligibility. The applied command result constructs `AppliedInterruptProof { command: command_id, predecessor: expected_active_turn }`; construction validates the command is the applied `SubmitInput::Interrupt` for that exact predecessor and that the same transaction created the accepted input and successor. A rejected, non-interrupt, cross-session, or differently targeted command cannot construct the proof. If the current attempt is `Prepared`, application atomically ends the unsent attempt and predecessor as `Cancelled` with that proof in both attempt and turn disposition. If it is `Running` and every terminal guard already holds with proof that the interrupt prevented all remaining work, the same transaction ends the attempt `AfterCancellation(Cancelled)`, terminalizes the predecessor `Cancelled { cause }`, reclassifies pending steering in the required order, and releases the slot without persisting `StopRequested`. Otherwise a `Running` attempt atomically changes to `StopRequested(CancellationOnly)` with that proof and retains the slot through honest classification. If it is already `StopRequested(FatalMismatch)` without an interrupt, application populates its interrupt field and creates the successor without reauthorizing any predecessor effect. From approval it closes that exact wait and cancels the turn; from recovery it closes the exact wait and constructs `InterruptRequiresReconciliation` with the wait's exact operation set. Another interrupt is rejected once a proof is present, and a later request must target the new authoritative active state. After the interrupt-created successor, reclassified steering and ordinary queued turns retain their original accepted-input order. This defined priority insertion is part of interrupt semantics, not a general queue-reordering command.

### Safe points

A version-one **safe point** exists only immediately before the hub prepares a new model call for the target turn, after every earlier model call, tool attempt, or other issued physical operation for that turn has reached a durable classified state and every earlier logical tool request or approval dependency has a durable outcome. An approved tool request remains blocking while authorized-but-undispatched; scheduling delay cannot make it invisible merely because no `ToolAttemptId` exists yet. It is not a point inside a provider stream, tool execution, or unresolved approval/tool dependency. Version one has no implicit “not needed for this call” exception; a future concurrency ADR may add explicit durable dependency edges without weakening this baseline.

At that boundary, the hub atomically:

1. selects all pending safe-point inputs for the turn in session acceptance order;
2. commits those semantic inputs into the turn's ordered semantic history and extends the call's context frontier with them;
3. marks each input `ConsumedAsSteering` by reference to the prepared model call, from which its owning turn and immutable frontier are derived; and
4. prepares the model call under the turn's unchanged effective configuration.

`ConsumedAsSteering` means that the input became durable semantic history and was included in the identified call's frontier; it does not redundantly store a possibly conflicting turn or frontier beside the immutable call. The consumption transaction validates that the call belongs to the binding's source turn. It does not claim that the provider accepted or observed the prepared request. If that call fails before send, fails after send, or ends ambiguously, every later retry or continuation in the same turn must retain the consumed input in its own frontier. The input never becomes pending again and is not reclassified merely because one physical call failed.

An ADR-0005 duplicate-risk replacement is one such safe-point boundary. Its single transaction first validates the safe-point guards, consumes every pending steering input in acceptance order into semantic history and the replacement frontier, records their `ConsumedAsSteering` dispositions, and includes the accepted-risk marker. It then creates the replacement attempt and prepared call and transfers outcome authority. A replacement call cannot be prepared while eligible steering remains pending.

Pending steering does not change an already-issued model call. It also does not change a tool request, its normalized arguments, an approval, or an in-flight tool attempt. If orchestration later reaches another model call after the tool outcome is durable, that call consumes the steering.

If the turn becomes terminal without another safe point, the same terminal transaction reclassifies every pending steering input as the origin of a new after-current queued turn using its captured binding as `InheritedForReclassifiedSteering` provenance. An interrupt-created successor, if one exists, is first. After it, reclassified turns and ordinary after-current turns are ordered together by their original accepted-input positions. Reclassification adds those new turns and their original acceptance positions to the durable total order before the active slot is released; no affected queued turn has fixed a direct predecessor yet. Each input receives a durable `ReclassifiedAsTurnOrigin` disposition. Reclassification is visible; it is never described as having steered the terminal predecessor or as having been resolved from session defaults.

### Interrupt completion and successor eligibility

Interrupt frees the progressing-turn slot immediately only when its application transaction terminalizes an unsent `Prepared` attempt, a `Running` attempt for which prevention of all remaining work is already proven and every terminal guard holds, or a durable wait. The direct running path ends the attempt `AfterCancellation(Cancelled)`, records turn `Cancelled { cause: AppliedInterruptProof }`, and completes pending-steering reclassification before release. Every other already-running attempt constructs `StopRequested(CancellationOnly)` or populates the interrupt field of existing `StopRequested(FatalMismatch)`, retaining the exact attempt and every earlier fatal failure while the hub attempts to stop current provider and tool work and classifies every issued outcome honestly. Interruption from approval atomically closes that wait and yields `Cancelled { cause }`; interruption from recovery atomically closes that wait and yields `ReconciliationRequired` with `InterruptRequiresReconciliation` and the wait's exact set. The interrupted turn becomes `Completed`, `Refused`, `Cancelled`, `Failed`, or proof-bearing `ReconciliationRequired` under ADR-0004's common precedence; interrupt-only stop does not relabel raced completion, refusal, or known failure, while fatal mismatch still forces failure or reconciliation. If the hub restarts before classification, the startup scan ends the abandoned attempt as `Lost`, preserves its typed stop value, and applies the same precedence without creating a cancellation-only attempt. Only after predecessor terminalization may the interrupt-created successor activate.

An unacknowledged ambiguous prior external effect remains terminally `Ambiguous` on its physical operation while the predecessor uses `ReconciliationRequired` with a complete marker containing the exact unacknowledged set and typed reason. If an owner had already accepted its duplicate risk and orchestration resumed, interruption is classified normally while preserving both the physical `Ambiguous` outcome and its `DuplicateRiskAccepted` marker. The successor observes either form of uncertainty in context; interrupt never asserts rollback.

### Successor context frontier

Every accepted-input-origin turn has an immutable **starting context frontier** and **starting lineage**, selected together when the turn becomes eligible, not when queued input was accepted. Eligibility is the derived predicate defined by ADR-0004: every earlier turn in the durable total queue order is terminal and the session has no active turn. The selection uses that order rather than wall-clock “latest” state:

```text
AcceptedInputStartingLineage =
    FirstInSession
  | After { immediate_predecessor: TurnId }

AcceptedInputTurnStart = {
    lineage: AcceptedInputStartingLineage,
    frontier: ContextFrontier
}

starting_frontier(turn) =
    match fix_starting_lineage_from_durable_order(turn) {
        FirstInSession
            if session_has_no_prior_turn(turn.session)
            => session_ancestry_frontier_or_empty(turn.session),
        After { immediate_predecessor }
            if immediate_predecessor.is_terminal
               and no_turn_is_ordered_between(immediate_predecessor, turn)
            => semantic_frontier_through(immediate_predecessor),
        _ => invalid_lineage
    }
  + turn.origin_accepted_input
```

For an accepted-input-origin turn, `FirstInSession` is valid if and only if the session has no earlier turn. Every such turn made eligible after an earlier turn, including `StartWhenNoActiveTurn` accepted after all earlier work became terminal, fixes `After` to the terminal turn immediately before it in the durable total order. It cannot restart from session ancestry merely because no turn is currently active. The accepted-input lifecycle retains immutable order facts outside state, while `Queued` carries no starting-lineage value; activation or eligible failure constructs that value exactly once. This algebra does not decide lineage or context rules for future non-input origins.

The predecessor terminal frontier contains, in order:

- all semantic entries committed before and by the predecessor;
- every `DuplicateRiskAccepted` marker for a physically ambiguous operation, regardless of the predecessor's later terminal disposition;
- every typed `ProviderTargetMismatchInvalidation` together with the exact committed material and effect evidence of its `invalidated_call`, without treating that material as a usable assistant outcome;
- committed assistant and tool-result content plus an explicit completion marker for a completed predecessor;
- explicit refusal content and a refusal marker for a refused predecessor;
- an explicit failure marker for a failed predecessor;
- committed effects plus an explicit cancellation marker for a cancelled predecessor; or
- the complete immutable `ReconciliationMarker`, including its exact nonempty operation set and typed reason, for a reconciliation-required predecessor.

It excludes transient provider drafts, uncommitted partial tool output, later queued accepted inputs, assumptions about an ambiguous effect's result, and every completion, refusal, known-failure, or cancellation fact learned from a provider call after outcome authority transferred to its replacement. Those prior-call facts remain audit/reconciliation evidence and may be referenced as such without determining turn disposition or becoming authoritative conversational content.

Thus queued and interrupt-created work observe the same outcome-aware rule. They do not freeze a prematurely incomplete predecessor or transcript at acceptance, and later activity cannot rewrite the starting lineage or frontier after eligibility. Priority insertions are durable before the slot can be released, so each successor fixes a frontier through every turn ordered ahead of it. A first turn begins from the session's immutable transcript ancestry frontier, if any, plus its origin input. Later input submitted with no active turn joins the durable order; its frontier is fixed through its then-immediate predecessor after every earlier turn terminates.

Activation atomically fixes starting lineage and this frontier, acquires the session slot, creates the initial `Prepared` turn attempt, and moves the turn to `Active(Running)`. Static pre-activation rejection atomically fixes the same lineage, frontier, and terminal failure marker without creating an attempt. Dynamic preparation, including exact target resolution, occurs only after activation and ends that prepared attempt `KnownFailure` if it cannot proceed. A queued turn cannot fail or otherwise terminalize before earlier ordered turns, so every successor frontier remains a complete prefix of durable queue order.

Every individual model call still records its own context frontier. Within a turn, retry and continuation frontiers retain all previously committed accepted inputs and may extend the starting frontier with later committed turn history, tool results, outcome markers, and newly consumed safe-point steering.

This ADR defines starting frontiers only for turns whose origin is an accepted input. Future typed origins such as manual regeneration must define their own immutable context contribution, acceptance boundary, and queue interaction before becoming implementable; they do not substitute a missing `origin_accepted_input` into this formula.

### Baseline queue scope

Version one does not support editing accepted input, reordering queued turns, changing delivery policy, changing frozen configuration, cancelling queued input, or cancelling an active turn without the interrupt-created immediate successor. A future ADR must define identity, client convergence, proof, and disposition rules before adding any of these commands.

## Terminology

- **Delivery request:** The explicit treatment requested when input is submitted relative to authoritative session state.
- **Accepted input:** Durable user content with one recoverable disposition; transport acceptance alone is insufficient.
- **Safe point:** The version-one provider-call preparation boundary at which pending steering can enter a new immutable context frontier.
- **Context frontier:** An immutable reference to the exact ordered semantic content consumed by one model call, including applicable user inputs, consumed steering, committed assistant or tool content, and explicit terminal-outcome markers.
- **Session configuration defaults:** Mutable, explicitly versioned user/session-level model-selection defaults used only while accepting future origin input.
- **Reconciliation marker:** The immutable terminal proof carrying the exact nonempty set of still-unacknowledged ambiguous operations and the applied owner-choice, interrupt, or fatal-mismatch reason that released the active slot.
- **Inherited reclassification provenance:** The exact source-turn reference captured for pending steering, from which reclassification derives that turn's canonical immutable effective configuration when the input must become new logical work without an explicit configuration request.
- **Starting context frontier:** The immutable outcome-aware semantic frontier fixed with starting lineage when an accepted-input-origin turn becomes eligible.
- **Queue order:** Immutable accepted-input positions plus typed priority relations that form the durable total order of known work before eligibility.
- **Starting lineage:** The first-in-session or exact immediate-predecessor relation fixed once from durable queue order when an accepted-input-origin turn becomes eligible.
- **Eligibility:** A derived predicate for a queued turn after every turn earlier in durable queue order is terminal while no active turn owns the session slot; it is not a separate durable turn state.

## Invariants

- INV-007–INV-010, INV-012, INV-015, INV-016, INV-028–INV-030, and INV-034 are preserved and made precise.
- INV-007 no longer has a provisional no-active-turn treatment: such input explicitly uses `StartWhenNoActiveTurn`.
- INV-008 fixes turn creation and configuration freeze atomically with accepted origin input.
- INV-016 fixes version-one safe points at model-call preparation boundaries.
- Every acknowledged input is either a turn origin, pending steering, consumed steering, or visibly reclassified as a turn origin; no state permits silent disappearance.
- Every origin turn carries either explicit request/default-version/effective provenance or inherited source-turn provenance. Reclassification constructs the new turn's effective configuration from that canonical source turn in the same transition; no reclassified steering input invents a configuration request or carries a competing copied value.
- Updating session defaults affects only origin input accepted after the new version becomes current.
- Every queued or interrupt-created turn fixes one explicit starting lineage and frontier before activation.
- For accepted-input-origin turns, `FirstInSession` starting lineage is constructible if and only if the session has no prior turn; every such turn made eligible later fixes the exact terminal predecessor immediately before it in durable queue order. Future non-input origins retain their own explicit extension boundary.
- A queued turn cannot terminalize before every turn earlier in durable queue order; activation or eligible failure fixes starting lineage and frontier atomically.
- Consuming steering commits it to turn semantic history; later calls in that turn cannot silently omit it because the first prepared call failed or was ambiguous.
- Issued provider calls, tool requests, approvals, and tool attempts are immutable with respect to later steering.
- A safe point requires every earlier issued physical operation for the turn to have a durable classified outcome and every earlier logical tool/approval dependency to have a durable outcome; an authorized-but-undispatched tool request still blocks it.
- Preparing a duplicate-risk replacement call applies the same safe-point transition and cannot leave eligible steering pending or duplicate turn/frontier identities in its disposition.

## Strongest alternative

Persist only accepted input and delivery policy, then create the turn, choose current configuration, and use the latest transcript when the scheduler eventually starts it. This keeps queues lightweight and lets queued work benefit automatically from configuration updates.

It is rejected because restart timing or administrator changes could silently alter model choice after acknowledgement; future configuration categories would create the same problem. Two identical accepted queues could execute differently without a durable domain decision explaining why.

## Rejected alternatives

- **Reuse `AfterCurrentTurn` when no turn is active.** It invents an active target and makes stale active-work commands ambiguous.
- **Create queued turns only after the predecessor terminates.** Acknowledged intent would lack durable logical-work identity and configuration provenance during recovery.
- **Freeze queued context at acceptance.** It would omit the very predecessor outcome the user asked to follow.
- **Inject steering into an in-flight provider call or tool attempt.** Those external requests are already issued and immutable.
- **Drop steering if no later call occurs.** Durable accepted content would disappear without disposition.
- **Append unconsumed steering to a completed turn retroactively.** It falsely claims the completed work consumed content it never observed.
- **Let interrupt activation overlap cancellation.** It violates the one-progressing-turn rule and implies effects stopped before evidence says so.
- **Use “latest transcript” at scheduler wake-up.** It makes context depend on unrelated timing rather than durable queue order and eligibility-fixed starting lineage.

## Consequences

Origin turns and their effective configurations exist while queued, increasing durable state but making acknowledgement and recovery explainable. Starting context is intentionally fixed later than configuration: configuration expresses accepted execution intent, while context must include the predecessor's eventual outcome.

Safe-point steering has a narrow, testable meaning. Once consumed, it remains in every later call frontier for the turn even if the consuming physical call fails. If it is never consumed, it may become a visibly separate turn when no later provider call exists, which is more explicit than pretending it was consumed.

Interrupt is responsive in cancellation signaling but conservative in activation. Ambiguous effects can delay termination classification, and successors must see uncertainty rather than a fabricated rollback.

## Scenario walkthroughs

- **S01:** The client submits `StartWhenNoActiveTurn`; with no earlier queued work, acceptance creates a queued turn for which eligibility is immediately derivable. Activation atomically fixes a frontier based on no ancestry or one immutable ancestry source and creates its initial attempt.
- **S03:** Restart finds the accepted input, already-created queued turn, configuration provenance, acceptance/priority order facts, and disposition, then derives eligibility. An unexecutable queued turn waits for every earlier ordered turn and fixes its exact starting lineage and frontier before failing. No session default is re-read to reconstruct intent.
- **S07:** Interrupt atomically creates the immediate successor with a typed priority relation. It ends an unsent prepared predecessor; directly ends a running predecessor, reclassifies pending steering, and releases the slot only when acceptance already proves every terminal guard; otherwise it requests cancellation with `StopRequested(CancellationOnly)` and retains the slot; or it closes the exact wait. No queued successor has fixed a direct predecessor; after terminalization adds any reclassified steering, each turn later fixes lineage and a frontier through the exact terminal turn before it. Startup classification creates no cancellation-only attempt.
- **S08:** Steering accepted during a provider call remains pending with one binding to its canonical source turn and no copied configuration. After every earlier issued physical operation is classified and every earlier tool/approval dependency has a durable outcome, the next provider-call boundary—including duplicate-risk replacement—consumes and commits it by reference to that call; an authorized-but-undispatched tool request cannot be bypassed. Every future authorized call retains the steering, and if no such boundary occurs, terminalization reclassifies it into visible queued work whose configuration is derived from that source turn and whose order uses the original acceptance position.
- **S09:** After-current input creates a queued turn immediately with frozen configuration and acceptance position but no direct predecessor. At eligibility it fixes context and starting lineage through the terminal turn immediately before it after all priority insertions.
- **S10:** Steering can remain pending through an approval wait. It neither alters the approved request nor releases the active turn's slot.
- **S24:** Reconnecting clients reconstruct accepted-input dispositions and replace any transient draft; pending steering is not inferred from client state.

## Extension implications

Future safe-point kinds can be added as typed boundaries after defining what may consume steering and how each affects tool or approval identity. Future non-input origins, including regeneration, must add origin-specific starting-frontier rules rather than weakening queue-lineage semantics for accepted input. Queue-management commands can add new dispositions without rewriting historical acceptance records.

A future subsystem ADR may add instructions, custom parameters, tools, placement, resources, or another semantic category only by extending the request, default, override, and effective-value algebras together and defining canonical construction and equality. An operational category may remain late-bound only by showing it cannot change any semantic choice. Neither kind may be introduced as an implicit mutable-default lookup.

The outcome-aware frontier rule supports later reconciliation entries and richer semantic projections without making raw operational logs the transcript.

## Open questions

- Concrete encodings for canonical model-selection keys, alias identifiers, and immutable alias-definition references remain for the provider-provenance ADR. Other semantic configuration categories are absent until their subsystem ADRs define and add them; implementation may not invent generic maps or stringly placeholders meanwhile.
- Raw audit evidence selection and semantic outcome-marker rendering remain open, although their required presence and ordering are decided here.
- Queue size, admission limits, and resource governance remain open.
- UI defaults may be chosen later, but the submitted command must carry an explicit resulting delivery request.
- Whether future queue cancellation, editing, reordering, or policy change is supported remains later scope.

## Explicit non-decisions

This ADR does not choose a process protocol, client technology, storage schema, any enabled model-fallback policy, tool-risk taxonomy, approval policy, detailed runner capability schema, authentication, archive/restore behavior, destructive retention, or implementation layout. It does not define a delegated-child wait or any delegation-result or delegation-cancellation transition; ADR-0002 must add those while preserving accepted-input dispositions and progressing-slot compatibility.

# ADR-0027: Accepted-input delivery and context frontiers

- Status: Proposed
- Date: 2026-07-12
- Owners: Repository owner
- Reviewers: Domain, lifecycle, configuration, protocol, and reliability reviewers unassigned
- Supersedes: none
- Superseded by: none
- Decision-ledger questions: no-active-turn input; durable-command deduplication; versioned session defaults; safe points, inherited configuration provenance, and terminal disposition; queued work creation, constructible effective configuration, and configuration freeze; state-specific interrupt and successor order; context frontiers; baseline queue mutation scope

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
    overrides: TypedConfigurationOverrides
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

This is typed pseudocode, not a protocol or final Rust API. `SubmitInput` is the caller command payload used for idempotency; it contains only caller-supplied choices, never server-derived `OriginConfiguration`. Each immutable `SessionConfigurationDefaults` version contains a complete normalized `ConfigurationRequest` template over the same semantic categories below. Session creation establishes version one, and a separate idempotent owner command may install a complete replacement template.

For an unseen command, atomic input acceptance requires the caller's expected defaults version still to be current, selects every current mutable resolver input needed by those choices—including alias definitions and interpreting-policy snapshots—applies the overrides to produce a complete `ConfigurationRequest`, resolves it to `EffectiveConfiguration`, and persists the derived `OriginConfiguration`. Application orchestration may calculate a candidate outside the transaction, but the commit must compare-and-set the expected defaults version and the version or immutable value of every mutable resolver input it read. A defaults mismatch is a retryable acceptance conflict and cannot silently adopt a newer version for the same caller payload; the client must submit a new command to select it. A race in another resolver input may be recomputed before commit or returned as a retryable conflict. Thus all derived semantic meaning is linearized at successful acceptance rather than at an earlier read.

Updating session defaults creates a new immutable defaults version and affects only origin inputs accepted afterward. It does not create a turn, mutate queued or active work, alter pending steering, or change recovery. The conceptual update command carries a durable command identifier, the session identifier, the expected current defaults version, and the replacement typed defaults. Its deduplication and compare-and-set behavior follow the command rule below.

The version-one `EffectiveConfiguration` boundary is closed over these semantic categories:

- requested model selection, model parameters, and any semantic instruction or prompt-policy snapshot applied to the turn;
- enabled tools, their turn-visible semantic configuration, and requested placement or execution constraints;
- owner-visible recovery, fallback, cost, or resource policy choices that authorize or prohibit semantic execution paths; and
- immutable versions or value snapshots needed to interpret those choices later.

The minimum constructible algebra is:

```text
EffectiveConfiguration = {
    model: ModelConfiguration {
        requested_selection: DirectModel | FrozenAlias {
            alias: ModelAlias,
            definition: FrozenAliasDefinition
        },
        parameters: ProviderDefaults | FrozenModelParameters
    },
    instructions: None | FrozenInstructionSnapshot,
    tools: Disabled | Enabled(FrozenToolConfiguration),
    placement: Unconstrained | FrozenPlacementConstraints,
    recovery: RecoveryConfiguration {
        known_provider_failure_retry: Disabled,
        model_fallback: Disabled,
        turn_resource_policy: NoTurnSpecificLimit | FrozenResourcePolicy
    },
    interpreting_policies: FrozenPolicyVersions
}
```

`FrozenAliasDefinition` is the immutable definition version or value snapshot used to interpret the alias; its current version is selected by the atomic acceptance transition. `ProviderDefaults` means provider-defined parameter defaults selected as part of this frozen semantic value, not a later Signalbox lookup. `FrozenPolicyVersions` may be empty only when no interpreting policy applies. Each snapshot is a purpose-specific immutable domain value rather than JSON or a generic key-value map. Later subsystem ADRs may refine its internals without changing the variants or moving the category across the identity boundary.

`TypedConfigurationOverrides`, `ConfigurationRequest`, and `SessionConfigurationDefaults` use corresponding purpose-specific typed variants rather than generic maps. Overrides express only caller-selected differences from the named defaults version; the normalized request itself contains no optional “fill this later” fields and is complete inside the domain acceptance transition. `EffectiveConfiguration` differs by capturing immutable alias meaning and the policy snapshots required to execute that request.

An unimplemented capability is represented explicitly as disabled or absent inside this algebra; it is not omitted and later filled from mutable defaults. In particular, version one has no known-provider-failure retry and no model fallback.

The exact provider/model target is resolved from the frozen selection meaning and pinned as a separate turn fact under ADR-0005 before the first model call is created; it is not an `EffectiveConfiguration` field. Also outside this boundary are provider credentials, live endpoints and connections, scheduler locks and leases, process or runner connection identity, telemetry, transient availability, and transport timing. Those late-bound operational facts must be recorded when relevant but may not change requested model selection, semantic context, enabled tool behavior, placement constraints, approval binding, or authorized retry/fallback paths. A value that can change one of those semantic choices belongs inside `EffectiveConfiguration` and requires new logical work when changed.

Configuration equality is semantic value equality over this complete immutable typed value. Equality of database identifiers alone is insufficient unless an identifier names one immutable version with stable meaning; two separately stored values with the same normalized semantic content compare equal.

For any durable command, the hub first looks up `command_id` before validating current session state or deriving server-owned values. An already accepted identifier with the same caller-supplied typed command payload returns its recorded result even if defaults or aliases have since changed. Reusing the identifier with a different caller payload is rejected. Equality is structural domain equality, not equality of serialized bytes. Only an unseen identifier proceeds to state validation and configuration derivation. The hub then validates the delivery request against authoritative session state in the same transaction that would accept it:

- `StartWhenNoActiveTurn` is valid only when the session has no active turn. If earlier queued turns exist during a scheduler gap, the new turn joins their FIFO tail rather than bypassing them.
- The three active-work modes are valid only when `expected_active_turn` is the session's current active turn.
- `Interrupt` and `NextSafePoint` are rejected if that turn carries `CurrentTurnAttempt::CancellationRequested`.
- `AfterCurrentTurn` remains valid while cancellation is pending because it makes no promise of steering or initiating cancellation.

If validation loses a race, the command fails before acceptance and no `AcceptedInputId` is acknowledged. The client may refresh and submit a new explicit choice. The hub never silently normalizes a stale active-work request into no-active-turn or queued behavior.

On success, the hub atomically persists the accepted input identity, content, requested delivery mode and target, session ordering position, applicable origin or fallback configuration provenance required below, and its initial durable disposition before acknowledgement.

### Work creation and configuration freeze

```text
AcceptedInputDisposition =
    OriginOf(TurnId)
  | PendingSteering { turn: TurnId, fallback_provenance: ReclassifiedSteeringConfiguration }
  | ConsumedAsSteering { turn: TurnId, call: ModelCallId, frontier: ContextFrontier }
  | ReclassifiedAsTurnOrigin { turn: TurnId, reason: NoSafePointBeforeTerminal }

ReclassifiedSteeringConfiguration = {
    source_turn: TurnId,
    source_effective_configuration: EffectiveConfiguration
}

TurnConfigurationProvenance =
    ExplicitOrigin(OriginConfiguration)
  | InheritedForReclassifiedSteering(ReclassifiedSteeringConfiguration)
```

`StartWhenNoActiveTurn`, `Interrupt`, and `AfterCurrentTurn` each create a turn and freeze its effective configuration in the same transaction that accepts the input. The accepted input immediately becomes that turn's origin. Input submitted with no active turn is eligible immediately only if no earlier queued turn exists; otherwise it joins the FIFO tail. `Interrupt` and `AfterCurrentTurn` create queued turns.

Effective configuration freezes every semantic category defined above. Its typed equality is total: any field difference requires new logical work, while changes to explicitly excluded operational facts do not. The alias definition is already frozen, and exact target resolution occurs during the initial prepared attempt but before model-call creation as defined by ADR-0005. If static pre-activation validation proves a frozen configuration structurally unsupported, the already-created turn waits until its predecessors terminate, fixes its starting frontier, and fails without an attempt. If target resolution then fails after activation, the prepared attempt ends `KnownFailure` and the turn fails. Neither path lets input disappear, adopt newer session defaults, or terminalize ahead of its lineage.

`NextSafePoint` initially creates no turn. Its command variant contains no independent configuration request. It binds the accepted input to the active turn and captures `InheritedForReclassifiedSteering` with the exact source turn and source effective configuration. If terminalization later reclassifies the input, the new origin uses that provenance; it does not invent a request or claim that current session defaults produced the value. Any model or configuration change must instead be submitted as origin input for new logical work. The explicit delivery request determines identity: the hub does not inspect natural-language content to decide whether steering expresses the “same” or a “materially different” objective.

### Delivery-mode behavior

| Mode | Durable acceptance effect | Logical work | Interaction with issued work | Restart behavior |
| --- | --- | --- | --- | --- |
| Start with no active turn | Persist input, origin turn, configuration, and FIFO position; derived eligibility holds if no predecessor is queued | New turn immediately | None is active | Reconstruct queued or active work and derive eligibility from durable lineage and slot ownership |
| Interrupt | Persist input and successor configuration plus the exact predecessor transition atomically | New turn, designated as the active turn's immediate successor | End an unsent `Prepared` attempt and turn; request best-effort cancellation of a `Running` attempt; or close the exact durable wait; never roll back issued work | Reconstruct cancellation or terminal disposition, issued-effect evidence, and queued successor; startup scans abandoned attempts without creating recovery work |
| Next safe point | Persist input as pending steering with source-turn configuration provenance | No new turn unless reclassified | Does not mutate an issued provider call, tool request, approval, or tool attempt | Reconstruct pending steering, target, and inherited fallback provenance; consume or reclassify durably |
| After current turn | Persist input, FIFO successor turn, and configuration | New queued turn | Does not cancel or alter current work | Reconstruct exact queue order and frozen configuration |

The first accepted interrupt becomes the designated immediate successor ahead of every other successor candidate for the active turn. This includes ordinary after-current turns already queued and any pending safe-point input that may be reclassified when the interrupted turn terminalizes. If the current attempt is `Prepared`, acceptance atomically ends the unsent attempt and predecessor as `Cancelled`. If it is `Running`, acceptance atomically changes that sole current-attempt state to `CancellationRequested`. Cancellation from an approval or recovery wait closes that exact wait and terminalizes atomically under ADR-0004. Another interrupt is rejected once running cancellation is pending, and a later request must target the new authoritative active state. After the interrupt-created successor, reclassified steering and ordinary queued turns retain their original accepted-input order. This defined priority insertion is part of interrupt semantics, not a general queue-reordering command.

### Safe points

A version-one **safe point** exists only immediately before the hub prepares a new model call for the target turn, after every earlier model call, tool attempt, or other issued physical operation for that turn has reached a durable classified state. It is not a point inside a provider stream or tool execution. Version one has no implicit “not needed for this call” exception; a future concurrency ADR may add explicit durable dependency edges without weakening this baseline.

At that boundary, the hub atomically:

1. selects all pending safe-point inputs for the turn in session acceptance order;
2. commits those semantic inputs into the turn's ordered semantic history and extends the call's context frontier with them;
3. marks each input consumed with the target turn, model call, and frontier; and
4. prepares the model call under the turn's unchanged effective configuration.

`ConsumedAsSteering` means that the input became durable semantic history and was included in the identified call frontier; it does not claim that the provider accepted or observed the prepared request. If that call fails before send, fails after send, or ends ambiguously, every later retry or continuation in the same turn must retain the consumed input in its own frontier. The input never becomes pending again and is not reclassified merely because one physical call failed.

Pending steering does not change an already-issued model call. It also does not change a tool request, its normalized arguments, an approval, or an in-flight tool attempt. If orchestration later reaches another model call after the tool outcome is durable, that call consumes the steering.

If the turn becomes terminal without another safe point, the same terminal transaction reclassifies every pending steering input as the origin of a new after-current queued turn using its captured `InheritedForReclassifiedSteering` provenance. An interrupt-created successor, if one exists, is first. After it, reclassified turns and ordinary after-current turns are ordered together by their original accepted-input positions. Each input receives a durable `ReclassifiedAsTurnOrigin` disposition. Reclassification is visible; it is never described as having steered the terminal predecessor or as having been resolved from session defaults.

### Interrupt completion and successor eligibility

Interrupt does not free the progressing-turn slot unless it atomically terminalizes an unsent `Prepared` attempt or a durable wait. For an already `Running` attempt it records cancellation while retaining the exact current attempt, then the hub attempts to stop current provider and tool work and classifies every issued outcome honestly. Cancellation from approval atomically closes that wait and yields `Cancelled`; cancellation from recovery atomically closes that wait and yields `ReconciliationRequired`. The interrupted turn becomes `Completed`, `Refused`, `Cancelled`, `Failed`, or `ReconciliationRequired` under ADR-0004's common precedence; a cancellation request does not relabel raced completion, refusal, or known failure. If the hub restarts before classification, the startup scan ends the abandoned attempt as `Lost` and applies the same precedence without creating a cancellation-only attempt. Only after predecessor terminalization may the interrupt-created successor activate.

An unacknowledged ambiguous prior external effect remains terminally `Ambiguous` on its physical operation while the predecessor uses `ReconciliationRequired` and an explicit semantic outcome marker. If an owner had already accepted its duplicate risk and orchestration resumed, interruption is classified normally while preserving both the physical `Ambiguous` outcome and its `DuplicateRiskAccepted` marker. The successor observes either form of uncertainty in context; interrupt never asserts rollback.

### Successor context frontier

Every accepted-input-origin turn has an immutable **starting context frontier**, selected when the turn becomes eligible, not when queued input was accepted. Eligibility is the derived predicate defined by ADR-0004: every queue predecessor is terminal and the session has no active turn. The selection uses queue lineage rather than wall-clock “latest” state:

```text
starting_frontier(turn) =
    match turn.immediate_predecessor_terminal {
        None => session_ancestry_frontier_or_empty(turn.session),
        Some(predecessor) => semantic_frontier_through(predecessor)
    }
  + turn.origin_accepted_input
```

The predecessor terminal frontier contains, in order:

- all semantic entries committed before and by the predecessor;
- every `DuplicateRiskAccepted` marker for a physically ambiguous operation, regardless of the predecessor's later terminal disposition;
- committed assistant and tool-result content for a completed predecessor;
- explicit refusal content and a refusal marker for a refused predecessor;
- an explicit failure marker for a failed predecessor;
- committed effects plus an explicit cancellation marker for a cancelled predecessor; or
- an explicit ambiguity/reconciliation-required marker for that disposition.

It excludes transient provider drafts, uncommitted partial tool output, later queued accepted inputs, and assumptions about an ambiguous effect's result. Raw audit evidence may be referenced without copying it wholesale into model context.

Thus queued and interrupt-created work observe the same outcome-aware rule. They do not freeze a prematurely incomplete transcript at acceptance, and later activity cannot rewrite the frontier after eligibility. A first turn begins from the session's immutable transcript ancestry frontier, if any, plus its origin input. Later input submitted with no active turn joins any queued lineage; its frontier is fixed through its immediate predecessor after that predecessor terminates.

Activation atomically fixes this frontier, acquires the session slot, creates the initial `Prepared` turn attempt, and moves the turn to `Active(Running)`. Static pre-activation rejection atomically fixes the same frontier and terminal failure marker without creating an attempt. Dynamic preparation, including exact target resolution, occurs only after activation and ends that prepared attempt `KnownFailure` if it cannot proceed. A queued turn cannot fail or otherwise terminalize before its predecessors, so every successor frontier remains a complete prefix of queue lineage.

Every individual model call still records its own context frontier. Within a turn, retry and continuation frontiers retain all previously committed accepted inputs and may extend the starting frontier with later committed turn history, tool results, outcome markers, and newly consumed safe-point steering.

This ADR defines starting frontiers only for turns whose origin is an accepted input. Future typed origins such as manual regeneration must define their own immutable context contribution, acceptance boundary, and queue interaction before becoming implementable; they do not substitute a missing `origin_accepted_input` into this formula.

### Baseline queue scope

Version one does not support editing accepted input, reordering queued turns, changing delivery policy, changing frozen configuration, or cancelling queued input. A future ADR must define identity, client convergence, and disposition rules before adding any of these commands.

## Terminology

- **Delivery request:** The explicit treatment requested when input is submitted relative to authoritative session state.
- **Accepted input:** Durable user content with one recoverable disposition; transport acceptance alone is insufficient.
- **Safe point:** The version-one provider-call preparation boundary at which pending steering can enter a new immutable context frontier.
- **Context frontier:** An immutable reference to the exact ordered semantic content consumed by one model call, including applicable user inputs, consumed steering, committed assistant or tool content, and explicit terminal-outcome markers.
- **Session configuration defaults:** Mutable, explicitly versioned user/session-level defaults used only while accepting future origin input.
- **Inherited reclassification provenance:** The source turn and frozen effective configuration captured for pending steering in case that input must become new logical work without an explicit configuration request.
- **Starting context frontier:** The immutable outcome-aware semantic frontier fixed when an origin turn becomes eligible.
- **Queue lineage:** Durable predecessor ordering used to select a starting frontier independently of recovery timing.
- **Eligibility:** A derived predicate for a queued turn whose predecessors are terminal while no active turn owns the session slot; it is not a separate durable turn state.

## Invariants

- INV-007–INV-010, INV-012, INV-015, INV-016, INV-028–INV-030, and INV-034 are preserved and made precise.
- INV-007 no longer has a provisional no-active-turn treatment: such input explicitly uses `StartWhenNoActiveTurn`.
- INV-008 fixes turn creation and configuration freeze atomically with accepted origin input.
- INV-016 fixes version-one safe points at model-call preparation boundaries.
- Every acknowledged input is either a turn origin, pending steering, consumed steering, or visibly reclassified as a turn origin; no state permits silent disappearance.
- Every origin turn carries either explicit request/default-version/effective provenance or inherited reclassification provenance; no reclassified steering input invents a configuration request.
- Updating session defaults affects only origin input accepted after the new version becomes current.
- Every queued or interrupt-created turn fixes one explicit starting frontier before activation.
- A queued turn cannot terminalize before its lineage predecessors; activation or eligible failure fixes its starting frontier atomically.
- Consuming steering commits it to turn semantic history; later calls in that turn cannot silently omit it because the first prepared call failed or was ambiguous.
- Issued provider calls, tool requests, approvals, and tool attempts are immutable with respect to later steering.
- A safe point requires every earlier issued physical operation for the turn to have a durable classified outcome.

## Strongest alternative

Persist only accepted input and delivery policy, then create the turn, choose current configuration, and use the latest transcript when the scheduler eventually starts it. This keeps queues lightweight and lets queued work benefit automatically from configuration updates.

It is rejected because restart timing or administrator changes could silently alter model choice, tools, policy, and context after acknowledgement. Two identical accepted queues could execute differently without a durable domain decision explaining why.

## Rejected alternatives

- **Reuse `AfterCurrentTurn` when no turn is active.** It invents an active target and makes stale active-work commands ambiguous.
- **Create queued turns only after the predecessor terminates.** Acknowledged intent would lack durable logical-work identity and configuration provenance during recovery.
- **Freeze queued context at acceptance.** It would omit the very predecessor outcome the user asked to follow.
- **Inject steering into an in-flight provider call or tool attempt.** Those external requests are already issued and immutable.
- **Drop steering if no later call occurs.** Durable accepted content would disappear without disposition.
- **Append unconsumed steering to a completed turn retroactively.** It falsely claims the completed work consumed content it never observed.
- **Let interrupt activation overlap cancellation.** It violates the one-progressing-turn rule and implies effects stopped before evidence says so.
- **Use “latest transcript” at scheduler wake-up.** It makes context depend on unrelated timing rather than queue lineage.

## Consequences

Origin turns and their effective configurations exist while queued, increasing durable state but making acknowledgement and recovery explainable. Starting context is intentionally fixed later than configuration: configuration expresses accepted execution intent, while context must include the predecessor's eventual outcome.

Safe-point steering has a narrow, testable meaning. Once consumed, it remains in every later call frontier for the turn even if the consuming physical call fails. If it is never consumed, it may become a visibly separate turn when no later provider call exists, which is more explicit than pretending it was consumed.

Interrupt is responsive in cancellation signaling but conservative in activation. Ambiguous effects can delay termination classification, and successors must see uncertainty rather than a fabricated rollback.

## Scenario walkthroughs

- **S01:** The client submits `StartWhenNoActiveTurn`; with no earlier queued work, acceptance creates a queued turn for which eligibility is immediately derivable. Activation atomically fixes a frontier based on no ancestry or one immutable ancestry source and creates its initial attempt.
- **S03:** Restart finds the accepted input, already-created queued turn, configuration provenance, queue lineage, and disposition, then derives eligibility. An unexecutable queued turn waits for its predecessors and fixes its frontier before failing. No session default is re-read to reconstruct intent.
- **S07:** Interrupt atomically creates the immediate successor and ends an unsent prepared predecessor, requests cancellation of an already-running predecessor, or closes the exact wait. The successor waits when necessary, then fixes a frontier containing the predecessor's completion, refusal, cancellation, failure, or ambiguity outcome. Startup classification creates no cancellation-only attempt.
- **S08:** Steering accepted during a provider call remains pending and captures inherited configuration provenance from the target turn. After every earlier issued physical operation is classified, the next provider-call boundary consumes and commits it, after which every future authorized call retains it; if no such boundary occurs, terminalization reclassifies it into visible queued work with that provenance.
- **S09:** After-current input creates a FIFO queued turn immediately. Its configuration is fixed at acceptance; its context is fixed after its immediate predecessor terminates.
- **S10:** Steering can remain pending through an approval wait. It neither alters the approved request nor releases the active turn's slot.
- **S24:** Reconnecting clients reconstruct accepted-input dispositions and replace any transient draft; pending steering is not inferred from client state.

## Extension implications

Future safe-point kinds can be added as typed boundaries after defining what may consume steering and how each affects tool or approval identity. Future non-input origins, including regeneration, must add origin-specific starting-frontier rules rather than weakening queue-lineage semantics for accepted input. Queue-management commands can add new dispositions without rewriting historical acceptance records.

A future ADR may add a new explicitly late-bound operational category only by showing that it cannot change any semantic choice assigned above to `EffectiveConfiguration`; each such value must still be named, policy-governed, and durably resolved when relevant. A late-bound value cannot be introduced as an implicit default lookup or excluded ad hoc from effective-configuration equality.

The outcome-aware frontier rule supports later reconciliation entries and richer semantic projections without making raw operational logs the transcript.

## Open questions

- Concrete nested field spellings and policy-reference encodings remain for the relevant subsystem ADRs, but the semantic categories included in `EffectiveConfiguration`, the operational exclusions, and semantic value-equality rule are fixed above and may not be reclassified by implementation.
- Raw audit evidence selection and semantic outcome-marker rendering remain open, although their required presence and ordering are decided here.
- Queue size, admission limits, and resource governance remain open.
- UI defaults may be chosen later, but the submitted command must carry an explicit resulting delivery request.
- Whether future queue cancellation, editing, reordering, or policy change is supported remains later scope.

## Explicit non-decisions

This ADR does not choose a process protocol, client technology, storage schema, any enabled model-fallback policy, tool-risk taxonomy, approval policy, detailed runner capability schema, authentication, archive/restore behavior, destructive retention, or implementation layout. It does not define a delegated-child wait or any delegation-result or delegation-cancellation transition; ADR-0002 must add those while preserving accepted-input dispositions and progressing-slot compatibility.

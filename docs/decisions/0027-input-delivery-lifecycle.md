# ADR-0027: Accepted-input delivery and context frontiers

- Status: Proposed
- Date: 2026-07-12
- Owners: Repository owner
- Reviewers: Domain, lifecycle, configuration, protocol, and reliability reviewers unassigned
- Supersedes: none
- Superseded by: none
- Decision-ledger questions: no-active-turn input; safe points, configuration inheritance, and terminal disposition; queued work creation, derived eligibility, and configuration freeze; successor context frontiers; baseline queue mutation scope

## Context

Input may arrive while a turn is executing a provider call, performing a tool attempt, waiting for approval or child work, or stopping. Signalbox accepts three active-work intents: interrupt, next safe point, and after current turn. Durable acknowledgement is insufficient if the input can later disappear, inherit a changed configuration silently, or consume an implicit “latest” transcript whose contents depend on recovery timing.

The first domain state machines also need an explicit no-active-turn command, queue creation boundary, safe-point definition, and terminal disposition for steering that never reaches another model call.

## Decision

### Submission and atomic acceptance

The conceptual command distinguishes input submitted with no active turn from the three active-work delivery modes:

```text
DeliveryRequest =
    StartWhenNoActiveTurn { configuration: OriginConfiguration }
  | Interrupt { expected_active_turn: TurnId, configuration: OriginConfiguration }
  | NextSafePoint { expected_active_turn: TurnId }
  | AfterCurrentTurn { expected_active_turn: TurnId, configuration: OriginConfiguration }

OriginConfiguration = {
    requested: ConfigurationRequest,
    effective: EffectiveConfiguration
}

SubmitInput = {
    command_id: DurableCommandId,
    session_id: SessionId,
    content: UserContent,
    delivery: DeliveryRequest
}
```

This is typed pseudocode, not a protocol or final Rust API. Application orchestration resolves the requested configuration to a complete typed `EffectiveConfiguration` before invoking the atomic domain acceptance transition. The transition persists both requested and effective provenance. Every field inside `EffectiveConfiguration` is identity-significant in the baseline; a later ADR must place any explicitly operational late-bound value outside it.

The hub validates the delivery request against authoritative session state in the same transaction that would accept it:

- `StartWhenNoActiveTurn` is valid only when the session has no active turn. If earlier queued turns exist during a scheduler gap, the new turn joins their FIFO tail rather than bypassing them.
- The three active-work modes are valid only when `expected_active_turn` is the session's current active turn.
- `Interrupt` and `NextSafePoint` are rejected if that turn is already in `CancellationRequested`.
- `AfterCurrentTurn` remains valid while cancellation is pending because it makes no promise of steering or initiating cancellation.

If validation loses a race, the command fails before acceptance and no `AcceptedInputId` is acknowledged. The client may refresh and submit a new explicit choice. The hub never silently normalizes a stale active-work request into no-active-turn or queued behavior.

On success, the hub atomically persists the accepted input identity, content, requested delivery mode and target, session ordering position, applicable origin or fallback configuration material required below, and its initial durable disposition before acknowledgement. Command deduplication returns the same accepted result.

### Work creation and configuration freeze

```text
AcceptedInputDisposition =
    OriginOf(TurnId)
  | PendingSteering { turn: TurnId, fallback_configuration: EffectiveConfiguration }
  | ConsumedAsSteering { turn: TurnId, call: ModelCallId, frontier: ContextFrontier }
  | ReclassifiedAsTurnOrigin { turn: TurnId, reason: NoSafePointBeforeTerminal }
```

`StartWhenNoActiveTurn`, `Interrupt`, and `AfterCurrentTurn` each create a turn and freeze its effective configuration in the same transaction that accepts the input. The accepted input immediately becomes that turn's origin. Input submitted with no active turn is eligible immediately only if no earlier queued turn exists; otherwise it joins the FIFO tail. `Interrupt` and `AfterCurrentTurn` create queued turns.

Effective configuration freezes requested model selection, model parameters, tool availability/configuration, and the policy references or versions needed to explain later decisions. Its typed equality is total: any field difference requires new logical work. Exact provider/model resolution still occurs as defined by ADR-0005. If a frozen configuration later cannot execute, the already-created turn waits in queue until its predecessors terminate, fixes its starting frontier when it becomes eligible, and then fails explicitly; the input does not disappear, adopt newer defaults, or terminalize ahead of its lineage.

`NextSafePoint` initially creates no turn. Its command variant contains no independent configuration request. It binds the accepted input to the active turn, inherits that turn's effective configuration, and captures the same value as the immutable fallback configuration. Any model or configuration change must instead be submitted as origin input for new logical work. The explicit delivery request determines identity: the hub does not inspect natural-language content to decide whether steering expresses the “same” or a “materially different” objective.

### Delivery-mode behavior

| Mode | Durable acceptance effect | Logical work | Interaction with issued work | Restart behavior |
| --- | --- | --- | --- | --- |
| Start with no active turn | Persist input, origin turn, configuration, and FIFO position; derived eligibility holds if no predecessor is queued | New turn immediately | None is active | Reconstruct queued or active work and derive eligibility from durable lineage and slot ownership |
| Interrupt | Persist input, successor turn/configuration, and cancellation request atomically | New turn, designated as the active turn's next successor | Best-effort cancel current calls/attempts; never roll back or mutate them | Reconstruct cancellation, issued-effect evidence, and queued successor |
| Next safe point | Persist input as pending steering with fallback configuration | No new turn unless reclassified | Does not mutate an issued provider call, tool request, approval, or tool attempt | Reconstruct pending steering and its target; consume or reclassify durably |
| After current turn | Persist input, FIFO successor turn, and configuration | New queued turn | Does not cancel or alter current work | Reconstruct exact queue order and frozen configuration |

The first accepted interrupt becomes the designated successor ahead of ordinary after-current turns already queued behind the active turn. Because the active turn immediately enters `CancellationRequested`, another interrupt is rejected until authoritative state changes. Existing queued turns retain their relative FIFO order after the interrupt-created successor and consequently observe its terminal semantic outcome before they run. This defined priority insertion is part of interrupt semantics, not a general queue-reordering command.

### Safe points

A version-one **safe point** exists only immediately before the hub prepares a new model call for the target turn, after every earlier model call, tool attempt, or other issued physical operation for that turn has reached a durable classified state. It is not a point inside a provider stream or tool execution. Version one has no implicit “not needed for this call” exception; a future concurrency ADR may add explicit durable dependency edges without weakening this baseline.

At that boundary, the hub atomically:

1. selects all pending safe-point inputs for the turn in session acceptance order;
2. commits those semantic inputs into the turn's ordered semantic history and extends the call's context frontier with them;
3. marks each input consumed with the target turn, model call, and frontier; and
4. prepares the model call under the turn's unchanged effective configuration.

`ConsumedAsSteering` means that the input became durable semantic history and was included in the identified call frontier; it does not claim that the provider accepted or observed the prepared request. If that call fails before send, fails after send, or ends ambiguously, every later retry or continuation in the same turn must retain the consumed input in its own frontier. The input never becomes pending again and is not reclassified merely because one physical call failed.

Pending steering does not change an already-issued model call. It also does not change a tool request, its normalized arguments, an approval, or an in-flight tool attempt. If orchestration later reaches another model call after the tool outcome is durable, that call consumes the steering.

If the turn becomes terminal without another safe point, the same terminal transaction reclassifies every pending steering input as the origin of a new after-current queued turn using its captured fallback configuration. The reclassified turns and ordinary after-current turns are ordered by their original accepted-input positions; interrupt priority remains the one exception described above. Each input receives a durable `ReclassifiedAsTurnOrigin` disposition. Reclassification is visible; it is never described as having steered the completed turn.

### Interrupt completion and successor eligibility

Interrupt records a cancellation request but does not free the progressing-turn slot. The hub attempts to stop current provider and tool work and classifies every issued outcome honestly. The interrupted turn then becomes `Completed`, `Cancelled`, `Failed`, or `ReconciliationRequired` as required by evidence; a completion that races cancellation is not relabeled. Only then may the interrupt-created successor activate.

An ambiguous prior external effect is represented in the predecessor's terminal disposition and semantic outcome marker. The successor may proceed with that uncertainty in context; interrupt never asserts rollback.

### Successor context frontier

Every accepted-input-origin turn has an immutable **starting context frontier**, selected when the turn becomes eligible, not when queued input was accepted. Eligibility is the derived predicate defined by ADR-0004: every queue predecessor is terminal and the session has no active turn. The selection uses queue lineage rather than wall-clock “latest” state:

```text
starting_frontier(turn) =
    semantic_frontier_through(turn.immediate_predecessor_terminal)
  + turn.origin_accepted_input
```

The predecessor terminal frontier contains, in order:

- all semantic entries committed before and by the predecessor;
- committed assistant, tool-result, and explicit refusal content for a completed predecessor;
- an explicit failure marker for a failed predecessor;
- committed effects plus an explicit cancellation marker for a cancelled predecessor; or
- an explicit ambiguity/reconciliation-required marker for that disposition.

It excludes transient provider drafts, uncommitted partial tool output, later queued accepted inputs, and assumptions about an ambiguous effect's result. Raw audit evidence may be referenced without copying it wholesale into model context.

Thus queued and interrupt-created work observe the same outcome-aware rule. They do not freeze a prematurely incomplete transcript at acceptance, and later activity cannot rewrite the frontier after eligibility. A first turn begins from the session's immutable transcript ancestry frontier, if any, plus its origin input. Later input submitted with no active turn joins any queued lineage; its frontier is fixed through its immediate predecessor after that predecessor terminates.

Activation atomically fixes this frontier, acquires the session slot, creates the initial turn attempt, and moves the turn to `Active(Running)`. If an eligible turn cannot execute, its failure transition atomically fixes the same frontier and terminal failure marker without creating an attempt. A queued turn cannot fail or otherwise terminalize before its predecessors, so every successor frontier remains a complete prefix of queue lineage.

Every individual model call still records its own context frontier. Within a turn, retry and continuation frontiers retain all previously committed accepted inputs and may extend the starting frontier with later committed turn history, tool results, outcome markers, and newly consumed safe-point steering.

This ADR defines starting frontiers only for turns whose origin is an accepted input. Future typed origins such as manual regeneration must define their own immutable context contribution, acceptance boundary, and queue interaction before becoming implementable; they do not substitute a missing `origin_accepted_input` into this formula.

### Baseline queue scope

Version one does not support editing accepted input, reordering queued turns, changing delivery policy, changing frozen configuration, or cancelling queued input. A future ADR must define identity, client convergence, and disposition rules before adding any of these commands.

## Terminology

- **Delivery request:** The explicit treatment requested when input is submitted relative to authoritative session state.
- **Accepted input:** Durable user content with one recoverable disposition; transport acceptance alone is insufficient.
- **Safe point:** The version-one provider-call preparation boundary at which pending steering can enter a new immutable context frontier.
- **Context frontier:** An immutable reference to the exact ordered semantic content consumed by one model call, including applicable user inputs, consumed steering, committed assistant or tool content, and explicit terminal-outcome markers.
- **Fallback configuration:** The active turn configuration captured for pending steering in case that input must become new logical work.
- **Starting context frontier:** The immutable outcome-aware semantic frontier fixed when an origin turn becomes eligible.
- **Queue lineage:** Durable predecessor ordering used to select a starting frontier independently of recovery timing.
- **Eligibility:** A derived predicate for a queued turn whose predecessors are terminal while no active turn owns the session slot; it is not a separate durable turn state.

## Invariants

- INV-007–INV-010, INV-012, INV-015, INV-016, INV-028–INV-030, and INV-034 are preserved and made precise.
- INV-007 no longer has a provisional no-active-turn treatment: such input explicitly uses `StartWhenNoActiveTurn`.
- INV-008 fixes turn creation and configuration freeze atomically with accepted origin input.
- INV-016 fixes version-one safe points at model-call preparation boundaries.
- Every acknowledged input is either a turn origin, pending steering, consumed steering, or visibly reclassified as a turn origin; no state permits silent disappearance.
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
- **S03:** Restart finds the accepted input, already-created queued turn, configuration, queue lineage, and disposition, then derives eligibility. An unexecutable queued turn waits for its predecessors and fixes its frontier before failing. No default is re-read to reconstruct intent.
- **S07:** Interrupt atomically creates the successor and requests cancellation. The successor waits, then fixes a frontier containing the predecessor's cancellation, failure, or ambiguity outcome.
- **S08:** Steering accepted during a provider call remains pending and inherits the target turn's configuration. After every earlier issued physical operation is classified, the next provider-call boundary consumes and commits it, after which retries and continuations retain it; if no such boundary occurs, terminalization reclassifies it into visible queued work.
- **S09:** After-current input creates a FIFO queued turn immediately. Its configuration is fixed at acceptance; its context is fixed after its immediate predecessor terminates.
- **S10:** Steering can remain pending through an approval wait. It neither alters the approved request nor releases the active turn's slot.
- **S24:** Reconnecting clients reconstruct accepted-input dispositions and replace any transient draft; pending steering is not inferred from client state.

## Extension implications

Future safe-point kinds can be added as typed boundaries after defining what may consume steering and how each affects tool or approval identity. Future non-input origins, including regeneration, must add origin-specific starting-frontier rules rather than weakening queue-lineage semantics for accepted input. Queue-management commands can add new dispositions without rewriting historical acceptance records.

Future configuration design may define explicitly late-bound operational values outside `EffectiveConfiguration`, but each must be named, policy-governed, and durably resolved. A late-bound value cannot be introduced as an implicit default lookup or excluded ad hoc from effective-configuration equality.

The outcome-aware frontier rule supports later reconciliation entries and richer semantic projections without making raw operational logs the transcript.

## Open questions

- Exact configuration fields and policy-version representations remain for a configuration ADR or the first relevant domain design; whatever fields enter `EffectiveConfiguration` participate in total equality.
- Raw audit evidence selection and semantic outcome-marker rendering remain open, although their required presence and ordering are decided here.
- Queue size, admission limits, and resource governance remain open.
- UI defaults may be chosen later, but the submitted command must carry an explicit resulting delivery request.
- Whether future queue cancellation, editing, reordering, or policy change is supported remains later scope.

## Explicit non-decisions

This ADR does not choose a process protocol, client technology, storage schema, model fallback, tool-risk taxonomy, approval policy, detailed runner capability schema, authentication, archive/restore behavior, destructive retention, or implementation layout. It does not define a delegated-child wait or any delegation-result or delegation-cancellation transition; ADR-0002 must add those while preserving accepted-input dispositions and progressing-slot compatibility.

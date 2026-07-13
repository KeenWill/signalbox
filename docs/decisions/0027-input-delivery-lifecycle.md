# ADR-0027: Accepted-input delivery and context frontiers

- Status: Proposed
- Date: 2026-07-12
- Owners: Repository owner
- Reviewers: Domain, lifecycle, configuration, protocol, and reliability reviewers unassigned
- Supersedes: none
- Superseded by: none
- Decision-ledger questions: no-active-turn input; safe points and terminal disposition; queued work creation and configuration freeze; successor context frontiers; baseline queue mutation scope

## Context

Input may arrive while a turn is executing a provider call, performing a tool attempt, waiting for approval or child work, or stopping. Signalbox accepts three active-work intents: interrupt, next safe point, and after current turn. Durable acknowledgement is insufficient if the input can later disappear, inherit a changed configuration silently, or consume an implicit “latest” transcript whose contents depend on recovery timing.

The first domain state machines also need an explicit no-active-turn command, queue creation boundary, safe-point definition, and terminal disposition for steering that never reaches another model call.

## Decision

### Submission and atomic acceptance

The conceptual command distinguishes input submitted with no active turn from the three active-work delivery modes:

```text
DeliveryRequest =
    StartWhenNoActiveTurn
  | Interrupt { expected_active_turn: TurnId }
  | NextSafePoint { expected_active_turn: TurnId }
  | AfterCurrentTurn { expected_active_turn: TurnId }

SubmitInput = {
    command_id: DurableCommandId,
    session_id: SessionId,
    content: UserContent,
    delivery: DeliveryRequest,
    requested_configuration: ConfigurationRequest
}
```

This is typed pseudocode, not a protocol or final Rust API.

The hub validates the delivery request against authoritative session state in the same transaction that would accept it:

- `StartWhenNoActiveTurn` is valid only when the session has no active turn. If earlier queued turns exist during a scheduler gap, the new turn joins their FIFO tail rather than bypassing them.
- The three active-work modes are valid only when `expected_active_turn` is the session's current active turn.
- `Interrupt` and `NextSafePoint` are rejected if that turn is already in `CancellationRequested`.
- `AfterCurrentTurn` remains valid while cancellation is pending because it makes no promise of steering or initiating cancellation.

If validation loses a race, the command fails before acceptance and no `AcceptedInputId` is acknowledged. The client may refresh and submit a new explicit choice. The hub never silently normalizes a stale active-work request into no-active-turn or queued behavior.

On success, the hub atomically persists the accepted input identity, content, requested delivery mode and target, session ordering position, configuration material required below, and its initial durable disposition before acknowledgement. Command deduplication returns the same accepted result.

### Work creation and configuration freeze

```text
AcceptedInputDisposition =
    OriginOf(TurnId)
  | PendingSteering { turn: TurnId, fallback_configuration: EffectiveConfiguration }
  | ConsumedAsSteering { turn: TurnId, call: ModelCallId, frontier: ContextFrontier }
  | ReclassifiedAsTurnOrigin { turn: TurnId, reason: NoSafePointBeforeTerminal }
```

`StartWhenNoActiveTurn`, `Interrupt`, and `AfterCurrentTurn` each create a turn and freeze its effective configuration in the same transaction that accepts the input. The accepted input immediately becomes that turn's origin. Input submitted with no active turn is eligible immediately only if no earlier queued turn exists; otherwise it joins the FIFO tail. `Interrupt` and `AfterCurrentTurn` create queued turns.

Effective configuration freezes requested model selection, material model parameters, tool availability/configuration, and the policy references or versions needed to explain later decisions. Exact provider/model resolution still occurs as defined by ADR-0005. If a frozen configuration later cannot execute, the already-created turn fails explicitly; the input does not disappear or adopt newer defaults.

`NextSafePoint` initially creates no turn. It binds the accepted input to the active turn and captures that turn's effective configuration as the immutable fallback configuration. Steering cannot alter the active turn's effective configuration. A submission that requests a material configuration or model change is invalid as safe-point steering and must use new logical work.

### Delivery-mode behavior

| Mode | Durable acceptance effect | Logical work | Interaction with issued work | Restart behavior |
| --- | --- | --- | --- | --- |
| Start with no active turn | Persist input, origin turn, configuration, and FIFO position; make it eligible if no predecessor is queued | New turn immediately | None is active | Reconstruct queued, eligible, or active work from Postgres |
| Interrupt | Persist input, successor turn/configuration, and cancellation request atomically | New turn, designated as the active turn's next successor | Best-effort cancel current calls/attempts; never roll back or mutate them | Reconstruct cancellation, issued-effect evidence, and queued successor |
| Next safe point | Persist input as pending steering with fallback configuration | No new turn unless reclassified | Does not mutate an issued provider call, tool request, approval, or tool attempt | Reconstruct pending steering and its target; consume or reclassify durably |
| After current turn | Persist input, FIFO successor turn, and configuration | New queued turn | Does not cancel or alter current work | Reconstruct exact queue order and frozen configuration |

The first accepted interrupt becomes the designated successor ahead of ordinary after-current turns already queued behind the active turn. Because the active turn immediately enters `CancellationRequested`, another interrupt is rejected until authoritative state changes. Existing queued turns retain their relative FIFO order after the interrupt-created successor and consequently observe its terminal semantic outcome before they run. This defined priority insertion is part of interrupt semantics, not a general queue-reordering command.

### Safe points

A version-one **safe point** exists only immediately before the hub prepares a new model call for the target turn, after every earlier call or tool attempt whose outcome is needed for that call has reached a durable classified state. It is not a point inside a provider stream or tool execution.

At that boundary, the hub atomically:

1. selects all pending safe-point inputs for the turn in session acceptance order;
2. extends the call's context frontier with those semantic inputs;
3. marks each input consumed with the target turn, model call, and frontier; and
4. prepares the model call under the turn's unchanged effective configuration.

Pending steering does not change an already-issued model call. It also does not change a tool request, its normalized arguments, an approval, or an in-flight tool attempt. If orchestration later reaches another model call after the tool outcome is durable, that call consumes the steering.

If the turn becomes terminal without another safe point, the same terminal transaction reclassifies every pending steering input as the origin of a new after-current queued turn using its captured fallback configuration. The reclassified turns and ordinary after-current turns are ordered by their original accepted-input positions; interrupt priority remains the one exception described above. Each input receives a durable `ReclassifiedAsTurnOrigin` disposition. Reclassification is visible; it is never described as having steered the completed turn.

### Interrupt completion and successor eligibility

Interrupt records a cancellation request but does not free the progressing-turn slot. The hub attempts to stop current provider and tool work and classifies every issued outcome honestly. The interrupted turn then becomes `Completed`, `Cancelled`, `Failed`, or `ReconciliationRequired` as required by evidence; a completion that races cancellation is not relabeled. Only then may the interrupt-created successor activate.

An ambiguous prior external effect is represented in the predecessor's terminal disposition and semantic outcome marker. The successor may proceed with that uncertainty in context; interrupt never asserts rollback.

### Successor context frontier

Every origin turn has an immutable **starting context frontier**, selected when the turn becomes eligible, not when queued input was accepted. The selection uses queue lineage rather than wall-clock “latest” state:

```text
starting_frontier(turn) =
    semantic_frontier_through(turn.immediate_predecessor_terminal)
  + turn.origin_accepted_input
```

The predecessor terminal frontier contains, in order:

- all semantic entries committed before and by the predecessor;
- committed assistant and tool-result content for a completed predecessor;
- an explicit failure marker for a failed predecessor;
- committed effects plus an explicit cancellation marker for a cancelled predecessor; or
- an explicit ambiguity/reconciliation-required marker for that disposition.

It excludes transient provider drafts, uncommitted partial tool output, later queued accepted inputs, and assumptions about an ambiguous effect's result. Raw audit evidence may be referenced without copying it wholesale into model context.

Thus queued and interrupt-created work observe the same outcome-aware rule. They do not freeze a prematurely incomplete transcript at acceptance, and later activity cannot rewrite the frontier after eligibility. A first turn begins from the session's immutable transcript ancestry frontier, if any, plus its origin input. Later input submitted with no active turn joins any queued lineage; its frontier is fixed through its immediate predecessor after that predecessor terminates.

Every individual model call still records its own context frontier. Within a turn, continuation calls may extend the starting frontier with committed turn history and consumed safe-point steering.

### Baseline queue scope

Version one does not support editing accepted input, reordering queued turns, changing delivery policy, changing frozen configuration, or cancelling queued input. A future ADR must define identity, client convergence, and disposition rules before adding any of these commands.

## Terminology

- **Delivery request:** The explicit treatment requested when input is submitted relative to authoritative session state.
- **Accepted input:** Durable user content with one recoverable disposition; transport acceptance alone is insufficient.
- **Safe point:** The version-one provider-call preparation boundary at which pending steering can enter a new immutable context frontier.
- **Fallback configuration:** The active turn configuration captured for pending steering in case that input must become new logical work.
- **Starting context frontier:** The immutable outcome-aware semantic frontier fixed when an origin turn becomes eligible.
- **Queue lineage:** Durable predecessor ordering used to select a starting frontier independently of recovery timing.

## Invariants

- INV-007–INV-010, INV-012, INV-015, INV-016, INV-028–INV-030, and INV-034 are preserved and made precise.
- INV-007 no longer has a provisional no-active-turn treatment: such input explicitly uses `StartWhenNoActiveTurn`.
- INV-008 fixes turn creation and configuration freeze atomically with accepted origin input.
- INV-016 fixes version-one safe points at model-call preparation boundaries.
- Every acknowledged input is either a turn origin, pending steering, consumed steering, or visibly reclassified as a turn origin; no state permits silent disappearance.
- Every queued or interrupt-created turn fixes one explicit starting frontier before activation.
- Issued provider calls, tool requests, approvals, and tool attempts are immutable with respect to later steering.

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

Safe-point steering has a narrow, testable meaning. It may become a visibly separate turn when no later provider call exists, which is more explicit than pretending it was consumed.

Interrupt is responsive in cancellation signaling but conservative in activation. Ambiguous effects can delay termination classification, and successors must see uncertainty rather than a fabricated rollback.

## Scenario walkthroughs

- **S01:** The client submits `StartWhenNoActiveTurn`; with no earlier queued work, acceptance creates an eligible turn with frozen configuration and a frontier based on no ancestry or one immutable ancestry source.
- **S03:** Restart finds the accepted input, already-created queued turn, configuration, queue lineage, and disposition. No default is re-read to reconstruct intent.
- **S07:** Interrupt atomically creates the successor and requests cancellation. The successor waits, then fixes a frontier containing the predecessor's cancellation, failure, or ambiguity outcome.
- **S08:** Steering accepted during a provider call remains pending. The next provider-call boundary consumes it, or terminalization reclassifies it into visible queued work.
- **S09:** After-current input creates a FIFO queued turn immediately. Its configuration is fixed at acceptance; its context is fixed after its immediate predecessor terminates.
- **S10:** Steering can remain pending through an approval wait. It neither alters the approved request nor releases the active turn's slot.
- **S24:** Reconnecting clients reconstruct accepted-input dispositions and replace any transient draft; pending steering is not inferred from client state.

## Extension implications

Future safe-point kinds can be added as typed boundaries after defining what may consume steering and how each affects tool or approval identity. Queue-management commands can add new dispositions without rewriting historical acceptance records.

Future configuration may distinguish fields frozen at acceptance from explicitly late-bound operational values, but every late-bound field must be named, policy-governed, and durably resolved. It cannot be introduced as an implicit default lookup.

The outcome-aware frontier rule supports later reconciliation entries and richer semantic projections without making raw operational logs the transcript.

## Open questions

- Exact configuration fields and policy-version representations remain for a configuration ADR or the first relevant domain design.
- Raw audit evidence selection and semantic outcome-marker rendering remain open, although their required presence and ordering are decided here.
- Queue size, admission limits, and resource governance remain open.
- UI defaults may be chosen later, but the submitted command must carry an explicit resulting delivery request.
- Whether future queue cancellation, editing, reordering, or policy change is supported remains later scope.

## Explicit non-decisions

This ADR does not choose a process protocol, client technology, storage schema, model fallback, tool-risk taxonomy, approval policy, detailed runner capability schema, authentication, archive/restore behavior, destructive retention, or implementation layout. It does not decide delegation-result or delegation-cancellation semantics beyond preserving an active wait and future compatibility.

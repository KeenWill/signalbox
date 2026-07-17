# ADR-0031: Direct fatal-mismatch terminalization at a closed aggregate boundary

- Status: Proposed
- Date: 2026-07-16
- Owners: Repository owner
- Reviewers: none yet; this record is authoritative only if the owner accepts it
- Supersedes: none
- Superseded by: none
- Depends on: [ADR-0004](0004-turn-and-attempt-lifecycle.md), [ADR-0005](0005-model-call-retry-semantics.md), and [ADR-0027](0027-input-delivery-lifecycle.md)
- Refines if accepted: ADR-0004's exhaustive live turn-transition matrix and ADR-0005's fatal-mismatch handling; neither is amended while this record remains Proposed
- Decision questions: when fatal mismatch requires `StopRequested`; direct `Failed` versus `ReconciliationRequired`; complete cause and ambiguity derivation; audit without an intermediate stop state; best-effort cancellation and restart behavior

## Context

ADR-0004 already permits a running attempt to end directly as `AfterFatalMismatch` when the aggregate's terminal guards can be satisfied in the same transaction. ADR-0005 applies that rule to provider-target mismatch and says the turn then fails when no other unacknowledged ambiguity remains or requires reconciliation with the exact remaining set otherwise.

The accepted records nevertheless leave one structural tension. ADR-0004 calls its turn-transition table exhaustive, but that table lists direct `Running` to `Failed` and omits direct `Running` to `ReconciliationRequired`. Its attempt table and ADR-0005's narrative allow the omitted case. An implementation could therefore treat `StopRequested(FatalMismatch)` as a mandatory historical waypoint even when every operation is already classified and nothing remains to stop.

`StopRequested` has an important operational purpose while shutdown or classification is unfinished: it retains the session slot, prohibits new semantic effects, accumulates causes, and gives restart an honest state from which to finish. Persisting it only to create an observable “stopping” phase adds a commit and crash boundary without improving those guarantees.

This ADR makes the closed-boundary rule exhaustive. It decides when the fatal mismatch transition ends directly and when `StopRequested` remains mandatory.

## Decision

### One missing live edge

If accepted, the live turn algebra gains the explicit edge:

```text
Active(Running {
    current_attempt: Running
})
    -> Terminal(ReconciliationRequired)
```

The edge is available only for fatal mismatch under the complete rule below. It does not add direct interrupt-only reconciliation, change startup recovery, or make `ReconciliationRequired` available without an exact nonempty ambiguity set and typed reason.

### Complete post-evidence derivation

The serialized aggregate transition first applies one trusted fatal-mismatch fact under ADR-0005. Depending on timing, that fact may classify a nonterminal call `KnownFailed`, resolve a terminally ambiguous call for turn-level decision-making without rewriting its physical `Ambiguous` disposition, or invalidate a completed current-authority call while preserving its immutable history.

From the complete authoritative aggregate after applying that fact, the transition derives:

```text
F = complete FatalMismatchStopCauses established at commit
U = exact canonical set of issued operations that remain physically
    Ambiguous and turn-blocking at commit because neither resolving
    evidence nor DuplicateRiskAccepted supplies their disposition
```

`F` is nonempty and contains every already-durable fatal cause plus the new cause and the aggregate's exact applied-interrupt state. The same `F` must appear in the ended attempt and, when reconciliation is required, in `FatalMismatchRequiresReconciliation`.

`U` is computed after the new evidence takes effect. If mismatch evidence resolves an already-ambiguous call, that call keeps its physical `Ambiguous` disposition but is removed from `U`; its `TerminalAmbiguityResolution` remains in `F`. An operation classified `Ambiguous` counts as physically classified. While it remains in `U`, its uncertainty prevents an ordinary terminal disposition, but it can appear honestly in an exact reconciliation marker.

The transition derives `F`, `U`, and guard satisfaction itself. A public or application caller cannot supply an `all_done`, `all_guards_hold`, ambiguity set, or desired disposition flag as authority. If the complete aggregate projection is unavailable or stale, no fatal transition commits; persistence reloads or rejects it rather than recording an incomplete cause or marker.

### Direct closure versus stop coordination

Given the complete post-evidence aggregate, the outcome is:

| Aggregate state at commit | Attempt transition | Turn transition |
| --- | --- | --- |
| Any owned logical dependency remains open or cannot become terminally non-dispatchable in this transaction; any issued physical operation remains unclassified; or another ADR-0004 terminal guard cannot be satisfied atomically | `Running -> StopRequested(FatalMismatch(F))` | Remain `Active(Running)`, retain the session slot, and authorize no new semantic effects while still allowing already-issued work to be classified or resolved and the input delivery ADR-0027 keeps valid under a pending stop (queued `AfterCurrentTurn`, and the first `Interrupt` against a fatal-mismatch-only stop) |
| Every ADR-0004 terminal guard can be satisfied atomically and `U` is empty | `Running -> Ended(AfterFatalMismatch { causes: F, disposition: KnownFailure })` | `Terminal(Failed)` |
| Every ADR-0004 terminal guard can be satisfied atomically and `U` is nonempty | `Running -> Ended(AfterFatalMismatch { causes: F, disposition: Ambiguous })` | `Terminal(ReconciliationRequired { marker: { ambiguous_operations: U, reason: FatalMismatchRequiresReconciliation(F) } })` |

The two direct branches commit no intermediate `StopRequested`. Their single aggregate transaction records the mismatch evidence and physical classification or invalidation, closes every logical dependency, makes authorized-but-undispatched work terminally non-dispatchable, ends the current attempt, commits the failure outcome or exact reconciliation marker, reclassifies pending steering under ADR-0027, and releases the session slot.

`StopRequested(FatalMismatch)` remains mandatory whenever any of that work cannot finish in the mismatch transaction. It is not optional merely because direct terminalization would be operationally convenient. While it exists, the accepted no-new-effects guard, cause-union rules, slot retention, later classification, and terminal precedence remain unchanged.

### Best-effort cancellation is not a terminal guard

ADR-0005 requires the first trusted nonterminal mismatch to durably request best-effort cancellation of remaining provider work, and active-turn invalidation of a completed current-authority call to durably request best-effort cancellation of already-issued continuation/tool work. The direct transaction preserves each case's exact cancellation target rather than narrowing it to provider work, so an issued tool attempt (the algebra's representation of runner-local execution) is not left without durable cancellation intent. Whenever the mismatch timing requires that request, the direct transaction still records it. Delivery, provider acknowledgement, or local connection closure is operational cleanup; none proves that the provider stopped, and none is an ADR-0004 terminal guard.

Once the mismatched call is durably classified and non-authoritative, pending delivery or acknowledgement of that cancellation request does not by itself require `StopRequested`. A successor may become eligible while cleanup continues. Late chunks or provider outcomes remain audit/reconciliation evidence and cannot authorize new effects, restore provider-outcome authority, or rewrite the terminal turn.

An actually unclassified issued operation is different: it requires `StopRequested` until honest evidence classifies it. The distinction is between unfinished aggregate work and unfinished delivery of an already-durable best-effort containment request.

### Audit and restart

The absence of a `StopRequested` record on a direct path is intentional. Durable audit reads the complete fatal causes from `AfterFatalMismatch` and, for reconciliation, from the matching marker. Monitoring must not infer that every fatal incident passed through a durable “stopping” state.

The direct transaction is atomic:

- a crash before commit leaves the prior running state and none of the proposed transition's facts;
- a crash after commit reconstructs the complete terminal attempt, turn outcome or marker, steering dispositions, and released slot; and
- replay of the mismatch evidence cannot insert a synthetic stop state or a second terminal result.

Startup classification remains governed by ADR-0004 and ADR-0005. A prior-process nonterminal attempt ends `AfterFatalMismatch(Lost)` with the complete recovered causes; this ADR does not let startup describe an abandoned tenure as a live `KnownFailure` or `Ambiguous` end.

## Invariants

If accepted, this record refines the enforcement boundary of:

- INV-006: the exhaustive live matrix includes direct fatal `ReconciliationRequired` only when every terminal guard is satisfied and the exact marker is committed atomically.
- INV-009: direct closure releases the session slot only in the complete terminal transaction; otherwise `StopRequested` retains it.
- INV-014: trusted provider mismatch remains typed evidence and makes mismatched material non-authoritative before either direct or stop-requested turn handling.
- INV-025 and INV-026: physical ambiguity remains immutable and is neither coerced to failure nor made retryable; direct reconciliation carries the exact unacknowledged remainder.
- INV-034: startup recovery and live handling share fatal precedence while retaining their distinct `Lost` versus live attempt dispositions.

These catalog rows remain unchanged while this ADR is Proposed. Acceptance must add their links without copying this record's complete decision rule into the catalog.

## Strongest alternative

Require every fatal mismatch observed during `Running` to persist `StopRequested(FatalMismatch)` before any terminal result, even when the same transition already has complete classification and can satisfy every terminal guard.

That creates one uniform observable path and may simplify monitoring that treats a stop state as the audit event. It is rejected because the terminal attempt already carries the complete fatal causes, the reconciliation marker carries the exact remaining ambiguity and reason, and the extra state coordinates no unfinished work. It adds a write, scheduling pass, and crash-recovery state without strengthening correctness.

## Rejected alternatives

- **Terminalize while a dependency or issued operation remains open or unclassified.** This releases the slot before Signalbox can classify raced effects honestly.
- **Convert every closed fatal case to `Failed`.** That fabricates certainty when `U` is nonempty and discards the exact reconciliation subject.
- **Enter `AwaitingRecoveryDecision` after fatal mismatch.** Fatal invalidation prohibits the continuation that this wait is designed to authorize.
- **Wait for cancellation delivery or acknowledgement.** Delivery is not proof of no external effect and could retain the slot indefinitely after the aggregate already has a terminal classification.
- **Let the caller choose the direct path.** A boolean or caller-provided ambiguity set can be stale, partial, or cross-wired; only the serialized aggregate transition derives the result.
- **Treat the missing table row as implementation discretion.** Exhaustive state machines cannot depend on choosing one of two contradictory readings of accepted prose.

## Consequences

The aggregate API must receive enough authoritative state to derive every fatal cause, every issued-operation classification, the exact unacknowledged ambiguity set, and all terminal guards. The persistence boundary must compare-and-set that complete transition so stale derivations do not commit.

The common fully classified path has one fewer durable state and cannot be stranded in a ceremonial stop phase after a crash. The cost is that operators cannot use the presence of a `StopRequested` row as the universal record of fatal mismatch; terminal attempt history and reconciliation markers are the source of truth.

`StopRequested` retains its stronger meaning: some aggregate work genuinely remains to be stopped, closed, or classified. That makes the state more useful for recovery and monitoring than a mandatory event-log waypoint.

## Scenario walkthrough

**Proposed S27 — fatal mismatch with a separately classified ambiguity.** A running attempt already owns provider call X and tool attempt Y. Y has become physically `Ambiguous`, but X is still unclassified, so the attempt remains live under the classification-only dispatch guard. Trusted target-mismatch evidence is then the last outstanding classification: X becomes `KnownFailed`, `F` gains its exact mismatch reference, every dependency is closed, and `U` is exactly `{Y}`.

The same transaction ends the attempt `AfterFatalMismatch(Ambiguous)`, terminalizes the turn as `ReconciliationRequired` with `{Y}` and `FatalMismatchRequiresReconciliation(F)`, reclassifies pending steering, and releases the slot. It does not persist `StopRequested`.

Countercase: if another issued operation Z remains unclassified when X mismatches, the transition persists `StopRequested(FatalMismatch(F))` and retains the slot. After Z is classified, the existing stop-requested path ends in `Failed` when the final `U` is empty or exact fatal reconciliation when it is nonempty.

This fixture combines S21's provider-target mismatch with S06's independently ambiguous operation without changing either operation's physical truth.

## Acceptance follow-up

If the owner accepts this record, the same acceptance change must:

- add the missing distinct `Running -> Terminal(ReconciliationRequired)` row to ADR-0004's turn table and link it to this decision;
- make ADR-0004's immediate fatal attempt row explicitly name `KnownFailure or Ambiguous`;
- replace ADR-0005's ambiguous phrase “immediate atomic failure” with “immediate atomic fatal terminalization” while preserving its timing matrix;
- add the focused S27 fixture above to [scenarios.md](../scenarios.md);
- link the affected invariant rows and add this record under an accepted-refinements section without rewriting the original five-record atomic foundation statement.

No accepted record or derived catalog is changed while this ADR remains Proposed.

## Open questions

- Provider-specific evidence thresholds for mismatch, known failure, and ambiguity remain with provider contracts.
- Persistence layout and the delivery mechanism for best-effort cancellation remain separate implementation decisions.
- Direct interrupt-only reconciliation from a running attempt is not decided here.
- The exact aggregate API and transaction representation remain implementation work constrained by this rule.

## Explicit non-decisions

This ADR does not change provider-call dispositions, mismatch evidence variants, terminal guard membership, fatal-cause union, ambiguity acknowledgement, recovery waits, startup precedence, interrupt authority, steering order, or terminal monotonicity. It does not choose a database schema, outbox, scheduler, provider SDK, evidence threshold, cancellation protocol, or monitoring system.

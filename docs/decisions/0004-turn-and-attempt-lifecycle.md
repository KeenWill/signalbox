# ADR-0004: Logical turn and physical attempt lifecycle

- Status: Proposed
- Date: 2026-07-12
- Owners: Repository owner
- Reviewers: Domain, lifecycle, and reliability reviewers unassigned
- Supersedes: none
- Superseded by: none
- Decision-ledger questions: recovery attempt identity; manual-regeneration identity boundary and later scope; actively progressing state set; typed cancellation subject; running-attempt and terminalization guards; ambiguous-outcome slot ownership; refusal attempt/turn disposition with ADR-0005

## Context

Signalbox must serialize conversational progress per session while allowing physical orchestration to stop at approvals, child waits, process loss, and recovery boundaries. “One active turn” is not enforceable if active means only “currently using CPU,” and “same turn on retry” is unsafe if a retry may silently change user intent, configuration, or already-observed effects.

The domain needs a testable logical-work boundary, a physical-attempt boundary, terminal states, and an explicit definition of which states retain the session's single-progressing-turn slot.

## Decision

A **turn** is one durable logical request for Signalbox to produce one conversational outcome from one typed origin under one frozen effective configuration. It owns an ordered history of orchestration decisions and committed semantic effects. It may use several context frontiers and survive zero or more turn attempts.

A **turn attempt** is one exclusive physical orchestration tenure for one active turn. An attempt begins in `Prepared` when orchestration is durably authorized to advance that turn. Initial turn activation and resolution of a durable wait atomically create the new current attempt; there is no intermediate `Active(Running)` state without one. An attempt ends when the turn becomes terminal, orchestration reaches a durable external wait, the attempt fails or is lost, cancellation finishes, ambiguity blocks continuation, or recovery fences it in favor of a replacement.

### Turn states and the progressing slot

```text
TurnState =
    Queued
  | Active(ActivePhase)
  | Terminal(TurnDisposition)

ActivePhase =
    Running {
        current_attempt: TurnAttemptId,
        cancellation: NotRequested | Requested
    }
  | AwaitingApproval { request: ToolRequestId }
  | AwaitingRecoveryDecision { ambiguous_operations: NonEmptyIssuedOperationRefs }

TurnDisposition =
    Completed
  | Refused
  | Failed
  | Cancelled
  | ReconciliationRequired
```

The names are typed pseudocode, not a final Rust API.

`NonEmptyIssuedOperationRefs` is a nonempty typed collection whose baseline element is `ModelCall(ModelCallId)` or `ToolAttempt(ToolAttemptId)`. It is not a stringly generic identifier; a future physical-operation kind must add an explicit tagged case. Carrying the exact references lets recovery and cancellation close the right wait without reconstructing a hidden prior phase.

Additional durable waits, including delegated-child waits, require explicit typed variants and transition review; there is no catch-all wait state. A future wait retains the progressing slot unless the ADR that introduces it also defines branching or rebasing semantics that justify an exception.

For the one-active-turn rule, **actively progressing means `TurnState::Active` in any phase**. It describes ownership of the session's ordered-progress slot, not continuous execution. A turn awaiting approval or recovery authorization, and a running turn whose cancellation has been requested, retain the slot. Queued and terminal turns do not.

At most one turn per session may be `Active`. Activating a queued successor is prohibited until the current active turn reaches a terminal disposition. The enforcement mechanism is left to scheduler and persistence design, but process memory alone is insufficient.

`Eligible` is a derived scheduling predicate, not another durable `TurnState`. A queued turn is eligible only when every predecessor in its queue lineage is terminal and the session has no active turn. Queue lineage and the active-slot owner are durable, so restart recomputes the same predicate without persisting a second lifecycle state.

When an active turn reaches a durable approval or recovery-decision wait, its current physical attempt ends with the cause-specific disposition: approval yields to the wait, while ambiguity, loss, or known failure retains that classified outcome. The turn remains active and retains the session slot without retaining a live attempt. When the wait resolves, a new turn attempt continues the same turn. A hub restart while the turn is waiting therefore reconstructs the wait; it does not pretend that a process-local attempt remains alive.

### Allowed turn transitions

| From | To | Allowed reason |
| --- | --- | --- |
| Queued | Active(Running) | Once eligible, the scheduler atomically fixes the starting frontier, acquires the session slot, and creates the initial `Prepared` attempt |
| Queued | Terminal(Failed) | Once eligible, the same transition fixes the starting frontier and records why work cannot execute; a queued turn cannot terminalize ahead of a nonterminal predecessor |
| Active(Running) | Active(AwaitingApproval or AwaitingRecoveryDecision) | The current attempt ends with the cause-specific terminal disposition and the wait becomes durable |
| Active(Running with current attempt `Running`, cancellation not requested) | Active(Running with the same attempt `CancellationRequested`, cancellation requested) | The hub durably accepts cancellation and updates both states atomically |
| Active(Running with current attempt `Prepared`, cancellation not requested) | Terminal(Cancelled) | Cancellation atomically ends the unsent attempt and any prepared physical operations as cancelled; no durable cancellation phase is needed |
| Active(AwaitingApproval or AwaitingRecoveryDecision) | Active(Running) | The wait resolves and a new attempt is created atomically |
| Active(AwaitingApproval) | Terminal(Cancelled) | Cancellation atomically closes the approval wait; no physical attempt exists to cancel |
| Active(AwaitingRecoveryDecision) | Terminal(ReconciliationRequired) | Cancellation atomically closes the recovery wait while preserving every ambiguous physical outcome |
| Active(Running, cancellation not requested) | Terminal(Completed, Refused, Failed, or ReconciliationRequired) | The terminalization preconditions below hold and durable evidence permits the exact disposition |
| Active(AwaitingApproval) | Terminal(Completed or Failed) | The approval wait is closed by denial or another final decision and durable evidence permits the exact disposition |
| Active(AwaitingRecoveryDecision) | Terminal(Completed, Failed, or ReconciliationRequired) | The recovery wait is closed by resolving evidence or explicit owner choice; provider refusal cannot originate from the wait |
| Active(Running, cancellation requested) | Terminal(Completed, Refused, Cancelled, Failed, or ReconciliationRequired) | Issued work reaches honest terminal classification after cancellation; a raced completion or refusal is not rewritten as cancellation |
| Terminal(any) | any state | Prohibited |

Direct wait-to-wait transitions are prohibited. Orchestration must resume through a new attempt before it can reach a different wait. Cancellation never replaces a durable wait with a generic state: approval cancellation closes the exact approval request and terminalizes as `Cancelled`, while recovery-wait cancellation closes that exact wait and terminalizes as `ReconciliationRequired`. Queued-input mutation and cancellation are not baseline features, so no user-driven `Queued -> Cancelled` transition is defined here.

Before any active turn becomes terminal and releases the progressing slot, one atomic transition must:

1. durably classify every model call, tool attempt, or other issued physical operation owned by the turn;
2. end the current turn attempt, if one exists;
3. close or terminally dispose any outstanding durable wait so a late decision cannot resume the turn;
4. commit the conversational outcome or explicit refusal, failure, cancellation, or ambiguity marker supporting the turn disposition; and
5. reclassify pending safe-point input as required by ADR-0027.

An unclassified issued operation prohibits terminalization even if local orchestration has stopped. A late result received after valid terminalization is audit or reconciliation evidence only and cannot advance the terminal turn or overwrite the successor's already-fixed context.

### Attempt lifecycle

```text
TurnAttemptState =
    Prepared
  | Running
  | CancellationRequested
  | Ended(AttemptDisposition)

AttemptDisposition =
    TurnCompleted
  | TurnRefused
  | YieldedToDurableWait
  | KnownFailure
  | Lost
  | Cancelled
  | Ambiguous
  | Replaced
```

| From | To | Rule |
| --- | --- | --- |
| Prepared | Running | External orchestration is authorized after the durable preparation boundary |
| Prepared | Ended(Cancelled, KnownFailure, or Replaced) | Cancellation, preparation failure, or fencing occurs before running |
| Running | CancellationRequested | The hub durably requests best-effort cancellation |
| Running | Ended(any disposition) | Evidence is classified, the turn completes, or orchestration yields to a durable wait |
| CancellationRequested | Ended(TurnCompleted, TurnRefused, Cancelled, KnownFailure, Lost, Ambiguous, or Replaced) | Cancellation evidence is classified without claiming rollback; a raced completion or refusal retains its actual disposition |
| Ended | any state | Prohibited |

Exactly one nonterminal attempt is carried by `Active(Running)`, whether or not cancellation has been requested. When the turn's cancellation flag is `Requested`, that attempt is `CancellationRequested`; an unsent `Prepared` attempt instead ends atomically with the turn. A waiting active turn has no attempt and carries the exact request or nonempty ambiguous-operation references on which it waits. Cancellation from a wait closes that wait and terminalizes in one transition, so there is no cancellation state with an optional attempt or a hidden prior wait. A new attempt must reference the ended attempt it continues or replaces, and stale attempts cannot advance turn state.

If the current attempt ends while the turn remains nonterminal, the same transaction must either move the turn into a typed durable wait or create its replacement attempt. It cannot leave the turn in `Active(Running)` without a current attempt, even briefly in durable state.

### Recovery, replacement, and new logical work

A **recovery retry** is a hub or owner-authorized decision to continue the same nonterminal turn after a known failure, loss, or restart. A **physical attempt replacement** is the new turn attempt created to carry out that decision.

Process restart always ends or fences a nonterminal attempt before orchestration continues. A restart reconstructs a durable wait without inventing a live attempt; continuing running work requires a replacement attempt that satisfies the rules below.

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
- an owner-requested model or effective-configuration change;
- an explicit future regeneration command requesting another alternative outcome; or
- any future typed origin-creation command rather than a recovery command referencing unfinished work.

Manual regeneration, if introduced, always creates a new turn and never reopens, overwrites, or adds another attempt to the original turn, even when input and configuration are identical. Version one does not yet expose that command or include a regeneration origin variant in the implementable state machine. A future ADR must define its immutable source frontier, queue placement, configuration-freeze boundary, and typed relation before adding it.

### Cancellation and ambiguity

Cancellation is a forward-only request to stop future progress. For a running turn, the request is recorded on the `Running` state that retains the exact current attempt; it sends best-effort cancellation to current model calls and tool attempts and prevents new effects unless needed to classify already-issued work. For an approval wait, cancellation closes the exact wait and terminalizes as `Cancelled`. For a recovery-decision wait, cancellation closes the exact wait and terminalizes as `ReconciliationRequired`. It does not roll back, compensate, or declare an external effect absent.

The turn cannot become `Cancelled`, `Failed`, `Completed`, or `Refused` while an issued effect's outcome is still ambiguous. The physical attempt ends `Ambiguous`. When no cancellation request is active, the turn enters `Active(AwaitingRecoveryDecision)` and retains the session slot. The ambiguity is therefore never resolved merely by scheduler or effect-policy timing.

An explicit owner recovery decision or newly recorded evidence may then do exactly one of the following:

- record separate resolving evidence and continue or terminalize according to it without reopening the ambiguous operation;
- authorize continuation in a new attempt while preserving the ambiguous record and accepting any effect-specific duplicate risk; or
- stop the turn as `Terminal(ReconciliationRequired)` with an explicit ambiguity marker.

No option reopens or overwrites the ambiguous physical operation, and no continuation is automatic. If cancellation was requested on the running turn, an ambiguous issued effect leads directly to `Terminal(ReconciliationRequired)`, not to a recovery wait. If cancellation is requested after the turn has already entered `AwaitingRecoveryDecision`, the transition closes that wait and reaches the same terminal disposition atomically. Later reconciliation of a terminal turn records new evidence separately; it does not return a terminal attempt or turn to `Running`.

## Terminology

- **Effective configuration:** The durable, immutable configuration governing a turn's semantic execution choices. Every field in this value is identity-significant in the baseline. ADR-0027 fixes its creation boundary; ADR-0005 defines model-selection implications.
- **Progressing slot:** The per-session exclusivity right held by an active turn, including while durably waiting.
- **Durable wait:** A typed state whose continuation depends on separately arriving evidence or a decision, such as approval or a future child result.
- **Recovery retry:** Continuation of unfinished logical work without changing its semantic identity.
- **Physical attempt replacement:** A new attempt identity authorized after an earlier attempt ended or was fenced.
- **Manual regeneration:** A future typed command for new logical work requesting another outcome related to a prior turn; its identity rule is decided here, while its command and context lifecycle remain unimplemented.
- **Ambiguous outcome:** Evidence is insufficient to establish whether an external effect occurred; it is not a known failure.

## Invariants

- INV-004, INV-006, INV-009–INV-011, INV-025, INV-026, INV-029, and INV-034 are preserved and made precise.
- INV-009 changes from provisional state membership to the exact rule that every `Active` phase retains the slot.
- A running turn carries exactly one current nonterminal attempt and an explicit cancellation flag; `Requested` pairs atomically with that attempt's `CancellationRequested` state, while cancellation of a `Prepared` attempt ends it and the turn atomically. Each waiting active turn carries its exact wait subject and no attempt.
- Ending a current attempt for a nonterminal turn atomically creates its replacement or moves the turn to a typed wait.
- No terminal turn or attempt returns to a nonterminal state.
- A turn cannot terminalize or release its slot until every issued physical operation is durably classified, its current attempt is ended, and any durable wait is closed.
- Provider refusal uses the distinct call, attempt, and turn dispositions `Refused`, `TurnRefused`, and `Refused`; it is neither successful completion nor infrastructure failure.
- No recovery retry changes origin, effective configuration, committed semantic history, or known effect evidence.
- Ambiguity is never coerced to cancellation or known failure to free the session slot.
- A non-cancelled ambiguous issued effect always enters `AwaitingRecoveryDecision`; only a typed owner decision or new evidence may continue or terminalize it.
- No cancellation transition enters `AwaitingRecoveryDecision`; cancellation plus unresolved ambiguity closes any existing recovery wait and terminalizes as reconciliation required.

## Strongest alternative

Release the progressing slot whenever a turn is not executing a provider or tool call, allowing queued turns to run while the earlier turn awaits approval or another future durable dependency. This improves apparent concurrency and avoids blocking a session on a slow decision.

It is rejected for the initial architecture because later turns could advance the transcript and external state before the earlier turn resumes, changing its context and making approval or child results apply across interleaved logical work. Supporting that behavior would require explicit branching or rebase semantics, not an “inactive” label on waits.

## Rejected alternatives

- **One immutable context per turn.** Safe-point steering and committed tool results require later model calls to observe a later frontier.
- **One physical attempt per turn.** Durable waits and process recovery need new physical tenure without changing logical intent.
- **Keep attempts live while waiting.** This invents process ownership across restart and ties durable waits to leases.
- **Treat every retry as a new turn.** Known recovery would fragment one unfinished request and complicate idempotent restart.
- **Treat regeneration as another attempt.** It would overwrite or multiply outcomes under one logical identity after the owner asked for an alternative.
- **Use a digest as the retry boundary.** Equal bytes do not establish equal intent, effect evidence, or user-visible history.
- **Free the slot on cancellation request.** Issued effects could still complete while replacement work begins against a false rollback assumption.

## Consequences

An approval or recovery-decision wait blocks later turns in the same session in version one. A future delegated-result wait will do the same unless its defining ADR introduces explicit branching or rebasing. Independent work can use another session. This is deliberately conservative and keeps transcript ordering testable.

Attempt records become more numerous around waits and restarts, but each describes an actual physical tenure. Recovery code must classify evidence before replacement and cannot use “retry” as a generic state reset.

Terminal reconciliation-required turns release the progressing slot while preserving explicit ambiguity for successor context. Reconciliation is a separate lifecycle and may affect later work only through a new durable fact.

Non-cancelled ambiguity can therefore block later turns until the owner decides. This is intentionally stronger than selecting terminal reconciliation from scheduler timing: the owner must explicitly choose when unresolved evidence is allowed to release the ordered-progress slot.

## Scenario walkthroughs

- **S03:** Restart reconstructs a queued turn or an active wait. Eligibility is derived from durable lineage and slot ownership. If running orchestration was lost, recovery ends or fences its required current attempt and may create a replacement under the same turn only after the recovery criteria pass.
- **S04:** A lost provider stream ends or blocks the physical attempt; it never changes turn identity by itself. A non-cancelled ambiguous result puts the turn in `AwaitingRecoveryDecision` and prevents automatic replacement.
- **S06:** A non-cancelled ambiguous tool write ends its attempt and retains the turn slot in `AwaitingRecoveryDecision`. Later tool policy may prohibit continuation, but scheduler timing cannot silently choose it or release the slot.
- **S07:** A running interrupted turn records cancellation on its `Running` phase and retains its exact attempt and slot until `Completed`, `Refused`, `Cancelled`, `Failed`, or `ReconciliationRequired`; cancellation from approval or recovery wait closes that exact wait and terminalizes atomically. The interrupt-created successor remains queued.
- **S08:** Pending safe-point steering belongs to the active turn. The turn retains its slot through waits, and later calls may consume a newer frontier.
- **S10:** Entering `AwaitingApproval` ends the current attempt with `YieldedToDurableWait`; approval creates a new attempt for the same active turn.
- **S18:** A future typed child wait must retain the parent session slot and end the current attempt, but ADR-0002 must define that variant and its cancellation/result transitions before implementation.
- **S26:** Manual regeneration is new logical work and leaves the original terminal state and attempts unchanged, but it remains outside the implementable state machine until its command, queue, and source-frontier rules are accepted.

## Extension implications

Future interleaving could add branching or explicitly rebased turns, but cannot redefine a version-one wait as non-progressing without revisiting context and approval semantics. Additional typed waits may be added and retain the slot under this rule unless a later ADR defines a different concurrency model.

Attempt lineage supports recovery across process or scheduler changes without selecting a workflow engine. Reconciliation can later gain its own typed records and owner actions while preserving terminal attempt evidence.

## Open questions

- Scheduler locking, wake-up, leases, and Postgres coordination remain under ADR-0010.
- Approval expiry and child-result delivery remain in their respective future ADRs; ADR-0002 must add any child-wait phase and parent-cancellation transitions.
- The evidence threshold for `Lost` versus `Ambiguous` is effect-specific and remains with provider and tool policies; once evidence is classified as ambiguous, its turn disposition follows the deterministic rule above.
- Resource limits may constrain how long a turn can retain a slot, but timeout disposition requires a later policy.
- Manual-regeneration command acceptance, queue placement, configuration freeze, and exact historical frontier remain open and block that feature, but not the initial accepted-input-origin turn state machine.

## Explicit non-decisions

This ADR does not choose scheduler infrastructure, persistence schema, tool retry eligibility, tool-risk taxonomy, delegation waits or cancellation, model fallback, archive behavior, manual-regeneration command/context semantics, or any process protocol. It does not define compensation for external effects or promise that cancellation succeeds.

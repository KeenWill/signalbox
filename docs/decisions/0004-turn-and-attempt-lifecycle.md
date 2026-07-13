# ADR-0004: Logical turn and physical attempt lifecycle

- Status: Proposed
- Date: 2026-07-12
- Owners: Repository owner
- Reviewers: Domain, lifecycle, and reliability reviewers unassigned
- Supersedes: none
- Superseded by: none
- Decision-ledger questions: recovery attempt identity; manual regeneration; actively progressing state set; waiting-state slot ownership

## Context

Signalbox must serialize conversational progress per session while allowing physical orchestration to stop at approvals, child waits, process loss, and recovery boundaries. “One active turn” is not enforceable if active means only “currently using CPU,” and “same turn on retry” is unsafe if a retry may silently change user intent, configuration, or already-observed effects.

The domain needs a testable logical-work boundary, a physical-attempt boundary, terminal states, and an explicit definition of which states retain the session's single-progressing-turn slot.

## Decision

A **turn** is one durable logical request for Signalbox to produce one conversational outcome from one typed origin under one frozen effective configuration. It owns an ordered history of orchestration decisions and committed semantic effects. It may use several context frontiers and survive zero or more turn attempts.

A **turn attempt** is one exclusive physical orchestration tenure for one active turn. An attempt begins when orchestration is durably authorized to advance that turn. It ends when the turn becomes terminal, orchestration reaches a durable external wait, the attempt fails or is lost, cancellation finishes, ambiguity blocks continuation, or recovery fences it in favor of a replacement.

### Turn states and the progressing slot

```text
TurnState =
    Queued
  | Active(ActivePhase)
  | Terminal(TurnDisposition)

ActivePhase =
    Running
  | AwaitingApproval
  | AwaitingChild
  | AwaitingRecoveryDecision
  | CancellationRequested

TurnDisposition =
    Completed
  | Failed
  | Cancelled
  | ReconciliationRequired
```

The names are typed pseudocode, not a final Rust API.

Additional durable waits require explicit typed variants and transition review; there is no catch-all wait state.

For the one-active-turn rule, **actively progressing means `TurnState::Active` in any phase**. It describes ownership of the session's ordered-progress slot, not continuous execution. A turn awaiting approval, child work, recovery authorization, or cancellation retains the slot. Queued and terminal turns do not.

At most one turn per session may be `Active`. Activating a queued successor is prohibited until the current active turn reaches a terminal disposition. The enforcement mechanism is left to scheduler and persistence design, but process memory alone is insufficient.

When an active turn reaches a durable approval or child-result wait, its current physical attempt ends with a typed suspended/yielded disposition. The turn remains active and retains the session slot without retaining a live attempt. When the wait resolves, a new turn attempt continues the same turn. A hub restart while the turn is waiting therefore reconstructs the wait; it does not pretend that a process-local attempt remains alive.

### Allowed turn transitions

| From | To | Allowed reason |
| --- | --- | --- |
| Queued | Active(Running) | Scheduler atomically acquires the session slot |
| Queued | Terminal(Failed) | Work cannot become executable and records an explicit failure |
| Active(Running) | Active(AwaitingApproval or AwaitingChild or AwaitingRecoveryDecision) | The current attempt ends with a durable typed wait |
| Active(Running or a waiting phase) | Active(CancellationRequested) | The hub durably accepts a cancellation request |
| Active(AwaitingApproval or AwaitingChild or AwaitingRecoveryDecision) | Active(Running) | The wait resolves and a new attempt is created atomically |
| Active(Running or a waiting phase) | Terminal(Completed or Failed or ReconciliationRequired) | Durable outcome evidence permits the exact disposition |
| Active(CancellationRequested) | Terminal(Completed, Cancelled, Failed, or ReconciliationRequired) | Issued work reaches honest terminal classification after cancellation; a raced completion is not rewritten as cancellation |
| Terminal(any) | any state | Prohibited |

Direct wait-to-wait transitions and `CancellationRequested -> Running` are prohibited. Orchestration must resume through a new attempt before it can reach a different wait. Queued-input mutation and cancellation are not baseline features, so no user-driven `Queued -> Cancelled` transition is defined here.

### Attempt lifecycle

```text
TurnAttemptState =
    Prepared
  | Running
  | CancellationRequested
  | Ended(AttemptDisposition)

AttemptDisposition =
    TurnCompleted
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
| CancellationRequested | Ended(TurnCompleted, Cancelled, KnownFailure, Lost, Ambiguous, or Replaced) | Cancellation evidence is classified without claiming rollback; a raced completion remains a completion |
| Ended | any state | Prohibited |

Only one nonterminal attempt may be current for a turn. A new attempt must reference the ended attempt it continues or replaces, and stale attempts cannot advance turn state.

### Recovery, replacement, and new logical work

A **recovery retry** is a hub or owner-authorized decision to continue the same nonterminal turn after a known failure, loss, or restart. A **physical attempt replacement** is the new turn attempt created to carry out that decision.

Recovery remains in the same turn only when all of the following hold:

- the typed turn origin and requested objective are unchanged;
- the frozen effective configuration is unchanged;
- all previously committed semantic content and effect evidence remain part of the turn history rather than being overwritten;
- no already-issued effect would be blindly repeated; and
- no terminal conversational outcome has been committed.

Context is not required to be byte-for-byte immutable. The replacement continues from the turn's latest durable context, including eligible steering and committed tool or failure outcomes. It may not silently discard those facts to recreate an earlier prompt.

The following create **new logical work** and therefore a new turn identity:

- a new accepted input used as a turn origin;
- manual regeneration of a terminal or already-answered turn;
- an owner-requested model or material configuration change;
- a request to pursue a materially different objective; or
- an explicit request to produce another alternative outcome rather than recover an unfinished one.

Manual regeneration always creates a new turn linked by `RegenerationOf(original_turn)`. It never reopens, overwrites, or adds another attempt to the original turn, even when input and configuration are identical.

### Cancellation and ambiguity

Cancellation is a forward-only request to stop future progress. It sends best-effort cancellation to current model calls and tool attempts and prevents new effects unless needed to classify already-issued work. It does not roll back, compensate, or declare an external effect absent.

The turn cannot become `Cancelled` while an issued effect's outcome is still ambiguous. The physical attempt ends `Ambiguous`. If an applicable effect policy permits an explicit owner recovery decision, the turn may remain `Active(AwaitingRecoveryDecision)` and retain the session slot; authorization creates a new attempt without changing or repeating the ambiguous attempt record. Otherwise, including interruption whose requested goal is to stop the old work, the turn reaches `Terminal(ReconciliationRequired)`. Later reconciliation of a terminal turn records new evidence separately; it does not return a terminal attempt or turn to `Running`.

## Terminology

- **Effective configuration:** The durable, immutable configuration governing a turn's semantic execution choices. ADR-0027 fixes its creation boundary; ADR-0005 defines model-selection implications.
- **Progressing slot:** The per-session exclusivity right held by an active turn, including while durably waiting.
- **Durable wait:** A typed state whose continuation depends on separately arriving evidence or a decision, such as approval or a child result.
- **Recovery retry:** Continuation of unfinished logical work without changing its semantic identity.
- **Physical attempt replacement:** A new attempt identity authorized after an earlier attempt ended or was fenced.
- **Manual regeneration:** New logical work requesting another outcome related to a prior turn.
- **Ambiguous outcome:** Evidence is insufficient to establish whether an external effect occurred; it is not a known failure.

## Invariants

- INV-004, INV-006, INV-009–INV-011, INV-025, INV-026, INV-029, and INV-034 are preserved and made precise.
- INV-009 changes from provisional state membership to the exact rule that every `Active` phase retains the slot.
- A turn has at most one current nonterminal attempt; a waiting active turn normally has none.
- No terminal turn or attempt returns to a nonterminal state.
- No recovery retry changes origin, effective configuration, committed semantic history, or known effect evidence.
- Ambiguity is never coerced to cancellation or known failure to free the session slot.

## Strongest alternative

Release the progressing slot whenever a turn is not executing a provider or tool call, allowing queued turns to run while the earlier turn awaits approval or a child. This improves apparent concurrency and avoids blocking a session on a slow decision.

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

An approval or delegated-result wait blocks later turns in the same session in version one. Independent work can use another session. This is deliberately conservative and keeps transcript ordering testable.

Attempt records become more numerous around waits and restarts, but each describes an actual physical tenure. Recovery code must classify evidence before replacement and cannot use “retry” as a generic state reset.

Terminal reconciliation-required turns release the progressing slot while preserving explicit ambiguity for successor context. Reconciliation is a separate lifecycle and may affect later work only through a new durable fact.

## Scenario walkthroughs

- **S03:** Restart reconstructs a queued turn or an active wait. If running orchestration was lost, recovery ends/fences that attempt and may create a replacement under the same turn only after the recovery criteria pass.
- **S04:** A lost provider stream ends or blocks the physical attempt; it never changes turn identity by itself. Ambiguous evidence prevents automatic replacement.
- **S07:** The interrupted turn enters `CancellationRequested` and retains the slot until `Cancelled`, `Failed`, or `ReconciliationRequired`; the interrupt-created successor remains queued.
- **S08:** Pending safe-point steering belongs to the active turn. The turn retains its slot through waits, and later calls may consume a newer frontier.
- **S10:** Entering `AwaitingApproval` ends the current attempt with `YieldedToDurableWait`; approval creates a new attempt for the same active turn.
- **S18:** `AwaitingChild` behaves like approval waiting for slot and attempt purposes without deciding child-result or cancellation policy.
- **S26:** Manual regeneration creates a linked queued turn and leaves the original terminal state and attempts unchanged.

## Extension implications

Future interleaving could add branching or explicitly rebased turns, but cannot redefine a version-one wait as non-progressing without revisiting context and approval semantics. Additional typed waits may be added and automatically retain the slot under this rule unless a later ADR defines a different concurrency model.

Attempt lineage supports recovery across process or scheduler changes without selecting a workflow engine. Reconciliation can later gain its own typed records and owner actions while preserving terminal attempt evidence.

## Open questions

- Scheduler locking, wake-up, leases, and Postgres coordination remain under ADR-0010.
- Approval expiry and child-result delivery remain in their respective future ADRs.
- The evidence threshold for `Lost` versus `Ambiguous` is effect-specific and remains with provider and tool policies.
- Resource limits may constrain how long a turn can retain a slot, but timeout disposition requires a later policy.
- The exact historical frontier chosen for manual regeneration remains open and must be explicit when the feature is designed.

## Explicit non-decisions

This ADR does not choose scheduler infrastructure, persistence schema, tool retry eligibility, tool-risk taxonomy, delegation cancellation, model fallback, archive behavior, or any process protocol. It does not define compensation for external effects or promise that cancellation succeeds.

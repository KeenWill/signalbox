# ADR-0004: Logical turn and physical attempt lifecycle

- Status: Proposed
- Date: 2026-07-12
- Owners: Repository owner
- Reviewers: Domain, lifecycle, and reliability reviewers unassigned
- Supersedes: none
- Superseded by: none
- Acceptance dependency: must be accepted atomically with ADR-0001, ADR-0003, ADR-0005, and ADR-0027 in the current foundation set
- Decision-ledger questions: startup scan versus recovery attempt identity; manual-regeneration identity boundary and later scope; actively progressing state set; aggregate running-attempt and terminalization guards; state-specific cancellation; exact-set ambiguity decisions; refusal attempt/turn disposition with ADR-0005

## Context

Signalbox must serialize conversational progress per session while allowing physical orchestration to stop at approvals, child waits, process loss, and recovery boundaries. “One active turn” is not enforceable if active means only “currently using CPU,” and “same turn on retry” is unsafe if a retry may silently change user intent, configuration, or already-observed effects.

The domain needs a testable logical-work boundary, a physical-attempt boundary, terminal states, and an explicit definition of which states retain the session's single-progressing-turn slot.

## Decision

A **turn** is one durable logical request for Signalbox to produce one conversational outcome from one typed origin under one frozen effective configuration. It owns an ordered history of orchestration decisions and committed semantic effects. It may use several context frontiers and survive zero or more turn attempts.

A **turn attempt** is one exclusive physical orchestration tenure for one active turn. An attempt begins in `Prepared` when orchestration is durably authorized to advance that turn. Initial turn activation creates the current attempt; closure of a durable wait creates one only when unfinished work remains and the applicable policy permits continuation. A closure supported by a terminal outcome creates none, and there is no intermediate `Active(Running)` state without a current attempt. An attempt ends when the turn becomes terminal, orchestration reaches a durable external wait, the attempt fails or is lost, cancellation finishes, or ambiguity blocks continuation.

The turn aggregate owns the `Active(Running)` phase together with the complete state of its current attempt and applies their transitions atomically. Persistence may use separate records, but the domain transition never loads or mutates them as independent aggregates and never permits a turn/attempt pair that the state machine cannot construct.

### Turn states and the progressing slot

```text
TurnState =
    Queued
  | Active(ActivePhase)
  | Terminal(TurnDisposition)

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
  | Cancelled
  | ReconciliationRequired

CurrentTurnAttempt =
    Prepared { id: TurnAttemptId }
  | Running { id: TurnAttemptId }
  | CancellationRequested { id: TurnAttemptId }
```

The names are typed pseudocode, not a final Rust API.

`NonEmptyIssuedOperationRefs` is a nonempty mathematical set whose baseline element is `ModelCall(ModelCallId)` or `ToolAttempt(ToolAttemptId)`. Duplicate elements are invalid, and equality is order-insensitive typed set equality for both wait comparison and structural command deduplication. It is not a stringly generic identifier; a future physical-operation kind must add an explicit tagged case. Carrying the exact references lets recovery and cancellation close the right wait without reconstructing a hidden prior phase.

Additional durable waits, including delegated-child waits, require explicit typed variants and transition review; there is no catch-all wait state. A future wait retains the progressing slot unless the ADR that introduces it also defines branching or rebasing semantics that justify an exception.

For the one-active-turn rule, **actively progressing means `TurnState::Active` in any phase**. It describes ownership of the session's ordered-progress slot, not continuous execution. A turn awaiting approval or recovery authorization, and a turn carrying `CurrentTurnAttempt::CancellationRequested`, retain the slot. Queued and terminal turns do not.

At most one turn per session may be `Active`. Activating a queued successor is prohibited until the current active turn reaches a terminal disposition. The enforcement mechanism is left to scheduler and persistence design, but process memory alone is insufficient.

`Eligible` is a derived scheduling predicate, not another durable `TurnState`. A queued turn is eligible only when every predecessor in its queue lineage is terminal and the session has no active turn. Queue lineage and the active-slot owner are durable, so restart recomputes the same predicate without persisting a second lifecycle state.

When an active turn reaches a durable approval or recovery-decision wait, every physical operation already issued by the attempt must first have a durable terminal classification. The current attempt then ends with the cause-specific disposition: approval yields to the wait, while ambiguity retains that classified outcome. The turn remains active and retains the session slot without retaining a live attempt. When resolving evidence or a valid owner decision closes the wait and unfinished work remains, a new turn attempt continues the same turn. Evidence-resolved continuation does not repeat an ambiguous operation and is not an ambiguity retry. A hub restart while the turn is waiting reconstructs the wait; it does not pretend that a process-local attempt remains alive.

### Allowed turn transitions

| From | To | Allowed reason |
| --- | --- | --- |
| Queued | Active(Running) | Once eligible, the scheduler atomically fixes the starting frontier, acquires the session slot, and creates the initial `Prepared` attempt |
| Queued | Terminal(Failed) | Once eligible, the same transition fixes the starting frontier and records why work cannot execute; a queued turn cannot terminalize ahead of a nonterminal predecessor |
| Active(Running with current attempt `Prepared`) | Active(Running with the same attempt `Running`) | Preparation succeeds and the aggregate authorizes external orchestration at the durable boundary |
| Active(Running with current attempt `Running`) | Active(AwaitingApproval) | Every issued operation is terminally classified, the current attempt ends `YieldedToDurableWait`, and the exact approval request becomes durable |
| Active(Running with current attempt `Running`) | Active(AwaitingRecoveryDecision) | One or more issued operations are `Ambiguous`, every other issued operation is terminally classified, and the current attempt ends `Ambiguous`; startup classification instead ends the abandoned attempt `Lost` while making the same turn-state transition |
| Active(Running with current attempt `Running`) | Active(Running with the same attempt `CancellationRequested`) | The hub durably accepts cancellation; the current-attempt variant is the single cancellation state |
| Active(Running with current attempt `Prepared`) | Terminal(Cancelled) | Cancellation atomically ends the unsent attempt and any prepared physical operations as cancelled, and makes every owned authorized-but-undispatched logical request non-dispatchable with an exact cancellation outcome; no durable cancellation phase is needed |
| Active(AwaitingApproval) | Active(Running) | Approval closes the exact approval wait/dependency, leaves the owned tool request authorized and blocking until its durable outcome, and creates a new turn attempt atomically |
| Active(AwaitingApproval) | Active(Running) | Denial closes the exact approval wait/dependency, terminally denies the owned tool request, commits that outcome to turn history, and creates a new turn attempt atomically; denial cannot create a tool attempt |
| Active(AwaitingRecoveryDecision) | Active(Running) | Unfinished work remains, the applicable operation-specific policy permits continuation, and either an exact-set owner decision accepts duplicate risk or new evidence resolves every blocking ambiguity; the wait closes and a new attempt is created atomically. For an owner-authorized replacement of an ambiguous model call, that same transaction also creates the fully targeted `Prepared` replacement call and transfers outcome authority under ADR-0005; no durable intermediate state exposes the new attempt while the prior call remains outcome-authoritative |
| Active(AwaitingRecoveryDecision with set S) | Active(AwaitingRecoveryDecision with set S') | A separate evidence record resolves the blocking uncertainty for one or more references without changing their terminal physical dispositions; `S'` is the exact nonempty strict subset still blocking, and no attempt is created |
| Active(AwaitingApproval) | Terminal(Cancelled) | Cancellation atomically closes the approval wait and terminally cancels the owned tool request so it is non-dispatchable; no physical attempt exists to cancel |
| Active(AwaitingRecoveryDecision) | Terminal(ReconciliationRequired) | Cancellation atomically closes the recovery wait while preserving every ambiguous physical outcome |
| Active(Running with current attempt `Prepared`) | Terminal(Failed) | Preparation fails before running, or startup loses the prepared tenure without ambiguity; the attempt ends `KnownFailure` in the former case and `Lost` in the latter, and neither has a model result that could support completion or refusal |
| Active(Running with current attempt `Running`) | Terminal(Completed, Refused, or Failed) | The terminalization preconditions below hold and durable evidence permits the exact disposition; startup recovery may leave the abandoned attempt `Lost` while making the same turn-state transition |
| Active(AwaitingRecoveryDecision) | Terminal(Completed, Refused, Failed, Cancelled, or ReconciliationRequired) | Resolving evidence supports an ordinary outcome; or an exact-set owner decision either accepts risk when existing evidence already supports that outcome, or stops for reconciliation. Physical ambiguous records remain terminal and accepted risk is marked separately |
| Active(Running with current attempt `CancellationRequested`) | Terminal(Completed, Refused, Cancelled, Failed, or ReconciliationRequired) | Issued work reaches honest terminal classification after cancellation; a raced completion or refusal is not rewritten as cancellation |
| Terminal(any) | any state | Prohibited |

Transitions between different wait variants are prohibited; orchestration must resume through a new attempt before it can reach another kind of wait. The one exception is an evidence-only refinement of `AwaitingRecoveryDecision` to the exact nonempty subset that remains ambiguous. No transition enters a wait while cancellation is requested, and no transition enters any wait until every operation already issued by the attempt is terminally classified. Cancellation never replaces a durable wait with a generic state: approval cancellation closes the exact approval wait, terminally cancels its owned tool request, and terminalizes the turn as `Cancelled`, while recovery-wait cancellation closes that exact wait and terminalizes as `ReconciliationRequired`. Queued-input mutation and cancellation are not baseline features, so no user-driven `Queued -> Cancelled` transition is defined here.

Before any active turn becomes terminal and releases the progressing slot, one atomic transition must:

1. durably close every owned logical tool request and approval dependency or give it an exact terminal outcome that makes it non-dispatchable, including an authorized request for which no tool attempt was created;
2. durably classify every model call, tool attempt, or other issued physical operation owned by the turn;
3. end the current turn attempt, if one exists;
4. close or terminally dispose any outstanding durable wait so a late decision cannot resume the turn;
5. commit the conversational outcome or explicit refusal, failure, cancellation, or ambiguity marker supporting the turn disposition; and
6. reclassify pending safe-point input as required by ADR-0027.

An unresolved logical tool/approval dependency or unclassified issued operation prohibits terminalization even if local orchestration has stopped. An authorized-but-undispatched request must be closed explicitly so a scheduler cannot dispatch it after the turn releases its slot. An `Ambiguous` operation also blocks ordinary terminalization until a separate evidence record resolves its uncertainty for turn-level decision-making or an exact-set owner command durably records `DuplicateRiskAccepted` for that operation. Neither path reopens or changes the terminal physical disposition; it changes whether that known uncertainty still blocks the turn. Owner acknowledgement also ensures every later terminal outcome and successor frontier includes an explicit accepted-risk marker. A late result received after valid terminalization is audit or reconciliation evidence only and cannot advance the terminal turn or overwrite the successor's already-fixed context.

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
```

| From | To | Rule |
| --- | --- | --- |
| Prepared | Running | External orchestration is authorized after the durable preparation boundary |
| Prepared | Ended(Cancelled, KnownFailure, or Lost) | Cancellation or preparation failure occurs before running, or startup recovery proves the creating process was lost |
| Running | CancellationRequested | The hub durably requests best-effort cancellation |
| Running | Ended(TurnCompleted, TurnRefused, YieldedToDurableWait, KnownFailure, Lost, or Ambiguous) | Evidence is classified, the turn completes, startup loses the tenure, or orchestration yields to a durable wait; `Cancelled` requires the aggregate's prior `Running -> CancellationRequested` transition |
| CancellationRequested | Ended(TurnCompleted, TurnRefused, Cancelled, KnownFailure, Lost, or Ambiguous) | Cancellation evidence is classified without claiming rollback; a raced completion or refusal retains its actual disposition |
| Ended | any state | Prohibited |

Exactly one `CurrentTurnAttempt` is carried by `Active(Running)`. Its variant is the sole nonterminal attempt state, so no independent turn-level cancellation flag can disagree with it. Cancellation changes an already-running attempt to `CancellationRequested`; an unsent `Prepared` attempt instead ends atomically with the turn. A waiting active turn has no attempt and carries the exact request or nonempty ambiguous-operation references on which it waits. Cancellation from a wait closes that wait and terminalizes in one transition, so there is no cancellation state with an optional attempt or a hidden prior wait. A new attempt must reference the ended attempt whose wait it continues, and stale attempts cannot advance turn state.

If the current attempt ends while the turn remains nonterminal, the same transaction must move the turn into a typed durable wait. Later resolving evidence or a valid owner decision may atomically close that wait and create a new attempt. The domain never leaves the turn in `Active(Running)` without a current attempt, even briefly in durable state.

### Recovery, replacement, and new logical work

A **recovery retry** is an owner-authorized decision to repeat or continue past the same nonterminal turn after accepting unresolved duplicate risk. An **evidence-resolved continuation** resumes unfinished work only after evidence has resolved every blocking ambiguity and does not repeat an operation whose outcome remains unknown. Both remain subject to the operation-specific retry policy: in particular, ADR-0005's version-one ban on known-provider-failure retry cannot be bypassed by calling it continuation. A **physical attempt replacement** is the new turn attempt created after either permitted path closes the recovery wait.

On startup, the hub scans every nonterminal attempt owned by an earlier process incarnation. It ends that attempt as `Lost` and classifies each recorded physical operation from durable evidence as completed, known failed, refused where applicable, cancelled, or ambiguous. The scan is recovery bookkeeping, not a turn attempt, and it cannot issue new semantic provider or tool effects.

Recovery applies one precedence rule whether classification happens live or at startup. First, any unacknowledged ambiguity blocks an ordinary terminal outcome: with cancellation it yields `ReconciliationRequired`, otherwise it enters `AwaitingRecoveryDecision`. Once no blocking ambiguity remains, sufficient conversational completion or refusal evidence from the currently outcome-eligible provider call yields `Completed` or `Refused`; otherwise a known failure that prevents completion yields `Failed`; otherwise evidence that cancellation prevented all remaining work yields `Cancelled`. Evidence from a provider call whose authority transferred to a replacement is retained but does not participate in this precedence or semantic-content selection. A cancellation request alone proves none of those outcomes. A prior-process tenure abandoned with no operation ambiguity and no sufficient authoritative completion, refusal, or confirmed-cancellation evidence fails under the version-one no-automatic-retry policy. The abandoned attempt remains `Lost` regardless of the turn disposition. An already durable wait is simply reconstructed. Startup never creates a cancellation-only or classification-only replacement attempt. A new attempt exists only after resolving evidence or a valid non-cancelled owner decision closes `AwaitingRecoveryDecision`.

Closure of a provider ambiguity wait serializes the owner decision against newly resolving provider evidence. If completion, refusal, known-failure, or confirmed-cancellation evidence for the still-authoritative prior call commits first, the common precedence applies and a now-stale duplicate-risk command cannot create a replacement. If `ContinueAcceptingDuplicateRisk` commits first, its single transaction closes the exact wait, records accepted risk, creates the new `Prepared` turn attempt and fully targeted `Prepared` replacement call, and transfers outcome authority; every later fact about the prior call is audit/reconciliation evidence only. Restart reconstructs whichever transaction won and never restores authority to a replaced call.

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

Cancellation is a forward-only request to stop future progress. For a running turn, the request is recorded on the `Running` state that retains the exact current attempt; it sends best-effort cancellation to current model calls and tool attempts, makes authorized-but-undispatched requests non-dispatchable, and prevents new effects unless needed to classify already-issued work. The request or a locally closed connection is not evidence that an external operation stopped: `Cancelled` requires effect-specific terminal evidence, and uncertainty is `Ambiguous`. Provider or runner work may still finish after the request; raced evidence is classified honestly, and evidence arriving after terminalization follows the late-result rule above. For an approval wait, cancellation closes the exact wait, terminally cancels its owned tool request, and terminalizes the turn as `Cancelled`. For a recovery-decision wait, cancellation closes the exact wait and terminalizes as `ReconciliationRequired`. Cancellation does not roll back, compensate, or declare an external effect absent.

The turn cannot become `Cancelled`, `Failed`, `Completed`, or `Refused` while an issued effect has an unacknowledged ambiguous outcome. The physical attempt ends `Ambiguous`. When no cancellation request is active, the turn enters `Active(AwaitingRecoveryDecision)` and retains the session slot. The ambiguity is never resolved merely by scheduler or effect-policy timing. An exact-set `ContinueAcceptingDuplicateRisk` decision may instead mark the ambiguity acknowledged without changing its physical disposition; ordinary terminalization is then permitted only with that accepted-risk marker in semantic history.

An explicit owner recovery decision is a typed command rather than an unbound boolean:

```text
ResolveAmbiguity = {
    command_id: DurableCommandId,
    turn: TurnId,
    expected_operations: NonEmptyIssuedOperationRefs,
    choice: ContinueAcceptingDuplicateRisk | StopForReconciliation
}

AmbiguityAcknowledgement =
    DuplicateRiskAccepted {
        decision_command: DurableCommandId,
        operations: NonEmptyIssuedOperationRefs
    }
```

The acknowledgement is a typed semantic-history value, not a string status and not a replacement physical disposition. The command is valid only when `turn` is currently awaiting exactly `expected_operations`; stale, partial, or expanded sets are rejected. If `ContinueAcceptingDuplicateRisk` would create another attempt, the planned continuation must satisfy every applicable provider and tool-effect policy; owner acknowledgement does not bypass a prohibition on repeating a write. Direct terminalization supported by other durable evidence issues no repeat and therefore requires no retry permission. Durable command deduplication is checked before current-state validation: replaying the same command identifier with the same payload returns its recorded result, while reusing that identifier with a different payload is rejected. Newly discovered provider or runner evidence is a separate typed evidence transition, not an implicit owner choice.

An accepted decision or newly recorded evidence may then do exactly one of the following:

- record separate resolving evidence, remove exactly the resolved references from the wait, and—when none remain—either continue unfinished work in a new attempt if the operation-specific policy permits it, or terminalize according to the resulting evidence;
- accept duplicate risk while preserving the ambiguous record and durably marking each exact referenced operation `DuplicateRiskAccepted`, then either continue unfinished work in a new attempt or terminalize directly when other durable evidence already supports an ordinary outcome; or
- stop the turn as `Terminal(ReconciliationRequired)` with an explicit ambiguity marker.

No option reopens or overwrites the ambiguous physical operation, and no duplicate-risk retry is automatic. Before acknowledgement, cancellation plus ambiguity leads directly to `Terminal(ReconciliationRequired)`. When a replacement provider call is created, outcome authority transfers to it under ADR-0005; later completion, refusal, failure, or cancellation of the prior call is classified and retained as evidence but cannot compete with the replacement's conversational outcome. The replacement's outcome is classified normally and retains the accepted-risk marker; cancellation does not retroactively revoke that acknowledgement. If cancellation is requested while the turn is still in `AwaitingRecoveryDecision`, the transition closes that wait and reaches `ReconciliationRequired` atomically. Later reconciliation of a terminal turn records new evidence separately; it does not return a terminal attempt or turn to `Running`.

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
- A running turn carries exactly one `CurrentTurnAttempt`; its `Prepared`, `Running`, or `CancellationRequested` variant is the single nonterminal attempt state. Cancellation of a `Prepared` attempt ends it and the turn atomically. Each waiting active turn carries its exact wait subject and no attempt.
- Ending a current attempt for a nonterminal turn atomically moves the turn to a typed wait; only later resolving evidence or a valid owner decision may close that wait and create a replacement.
- No terminal turn or attempt returns to a nonterminal state.
- A turn cannot terminalize or release its slot until every owned logical tool/approval dependency is durably closed or terminally non-dispatchable, every issued physical operation is durably classified, its current attempt is ended, and any durable wait is closed.
- A live provider refusal uses call, attempt, and turn dispositions `Refused`, `TurnRefused`, and `Refused`. Startup-observed refusal leaves the abandoned attempt `Lost`; evidence after terminal ambiguity leaves call and attempt ambiguous. Both recovery paths may still terminalize the turn as `Refused` without reopening physical records.
- No recovery retry changes origin, effective configuration, committed semantic history, or known effect evidence.
- Every provider replacement call atomically becomes outcome-authoritative; prior-call evidence remains durable but cannot create a second conversational outcome or change successor context.
- Ambiguity is never coerced to cancellation or known failure to free the session slot.
- A non-cancelled unacknowledged ambiguous issued effect always enters `AwaitingRecoveryDecision`; only a typed owner decision or new evidence may continue or terminalize it. Owner-authorized continuation preserves the physical ambiguity and adds an explicit accepted-risk marker that later terminal outcomes must retain.
- No cancellation transition enters `AwaitingRecoveryDecision`; cancellation plus unresolved ambiguity closes any existing recovery wait and terminalizes as reconciliation required.

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

Terminal reconciliation-required turns release the progressing slot while preserving explicit ambiguity for successor context. Reconciliation is a separate lifecycle and may affect later work only through a new durable fact.

Non-cancelled ambiguity can therefore block later turns until the owner decides. This is intentionally stronger than selecting terminal reconciliation from scheduler timing: the owner must explicitly choose when unresolved evidence is allowed to release the ordered-progress slot.

## Scenario walkthroughs

- **S03:** Restart reconstructs a queued turn or an active wait. Eligibility is derived from durable lineage and slot ownership. If running orchestration was lost, the startup scan ends its current attempt, classifies every issued operation, and never creates a replacement by itself.
- **S04:** A lost provider stream ends the physical attempt; it never changes turn identity by itself. A non-cancelled ambiguous result puts the turn in `AwaitingRecoveryDecision` and prevents automatic replacement; a known provider failure without ambiguity fails the turn.
- **S06:** A non-cancelled ambiguous tool write ends its attempt and retains the turn slot in `AwaitingRecoveryDecision`. Later tool policy may prohibit continuation, but scheduler timing cannot silently choose it or release the slot.
- **S07:** Interrupting an unsent `Prepared` attempt ends it and the turn atomically; interrupting a `Running` attempt records cancellation and retains the slot until honest classification; interrupting an approval or recovery wait closes that exact wait and terminalizes atomically. A restart performs the startup scan without creating cancellation-only work. The interrupt-created successor remains queued.
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

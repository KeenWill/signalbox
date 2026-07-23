# Turn lifecycle and scheduling

This page specifies the implemented behavior of turns, turn attempts,
eligibility derivation, the scheduler, and startup recovery as verified against
the working tree at commit `bf39f5f` (main). Code homes:
`crates/domain/src/{turn_lifecycle,turn_attempt,turn_eligibility,`
`context_frontier,queue_order}.rs`, `crates/application/src/{scheduler,`
`start_eligible_turn,startup_scan,submit_input}.rs`,
`crates/persistence/src/{start_eligible_turn,startup,scheduler,`
`lock_inventory}.rs` and its migrations, and `apps/hubd/src/main.rs`.
[docs/invariants.md](../invariants.md) remains the law catalog; INV tags below
reference its rows without restating them. Designed lifecycle behavior that has
no committed code path appears only under [Open edges](#open-edges). Sibling
pages named in scope deferrals below (identity-and-commands,
sessions-and-transcript, persistence-protocol, model-call-execution,
configuration-and-credentials, runtime-substrate) are companion pages of this
spec set; each deferral names the owning page rather than restating its
material.

## Turns, states, and the single active slot

A turn is one durable logical request for one conversational outcome from one
accepted-input origin under one frozen effective configuration (configuration
freeze is [identity-and-commands](identity-and-commands.md) scope). The
implemented slice stores three lifecycle states per turn
(`turn_lifecycle.state_kind`): `queued`, `active`, and `terminal`, with the
terminal disposition kind closed to `failed`, `completed`, and `refused`
(migration `202607220001`). The domain `TurnDisposition` algebra carries all
five accepted variants — `Completed`, `Refused`, `Failed`,
`Cancelled { cause }`, `ReconciliationRequired { marker }` — but `Cancelled` is
constructible only from an `AppliedInterruptProof` and `ReconciliationRequired`
only from a sealed `ReconciliationMarker`, and committed transitions now produce
`Completed`, `Refused`, and `Failed` (model-call terminal closure); no committed
transition produces `Cancelled` or `ReconciliationRequired`.

The domain `ActiveTurnPhase` algebra is `Running { current_attempt }`,
`AwaitingApproval { request }`, and
`AwaitingRecoveryDecision { ambiguous_operations }`. Every active phase retains
the session's progressing slot (`retains_progressing_slot()` is unconditionally
true; INV-009). Storage and reconstitution admit the `running` and
`awaiting_model_call_recovery` phases; `AwaitingRecoveryDecision` is
reconstituted from an `ambiguous` terminal model call correlated with its ended
attempt (`ambiguous` from a live loss, `lost` from startup recovery), while
`StopRequested` and `AwaitingApproval` still have no storage rows or production
constructors (see
[Evidence-bearing reconstitution](#evidence-bearing-reconstitution)).

At most one turn per session is `active`. Enforcement is layered:

1. the partial unique index `turn_lifecycle_one_active_per_session`
   (`WHERE state_kind = 'active'`) — sufficient alone;
2. a guarded activation `UPDATE` whose predicates revalidate `queued` state, the
   absence of an active turn, and terminal predecessors;
3. a per-session `session_scheduler` row that every turn-lifecycle writer locks
   `FOR UPDATE` before touching any `turn_lifecycle` row (`lock_inventory.rs`),
   serializing racing passes so the loser re-reads state the winner committed;
   and
4. row triggers (`reject_turn_lifecycle_invalid_change`,
   `reject_turn_attempt_invalid_change`) that reject invalid writes even from a
   defective writer: insert-as-`queued` / insert-as-`prepared`, only monotonic
   transitions (`queued`→`active`|`terminal`, `active`→`terminal`;
   `prepared`→`running`|`ended`, `running`→`ended`), terminal-turn and
   ended-attempt immutability, write-once start fields, no new attempt on a
   terminal turn, and queued terminalization only without attempt history.

Why: process memory carries no authority (INV-009, INV-010), so exclusivity must
hold in durable rows even if every in-process structure is lost. Terminal turns
and ended attempts never return to a nonterminal state: the sealed types expose
no such transition, and the triggers enforce the same monotonicity in the rows
themselves (INV-006).

The scheduler lock is not acquired first by every writer, and it does not make
deadlock unrepresentable by itself. Activation and startup recovery take it as
their only explicit row lock; submit-input locks the session row before it, in
`FOR NO KEY UPDATE` mode. Deadlock freedom rests on two standing constraints,
documented at both lock sites: every turn-lifecycle writer acquires the
scheduler lock before touching `turn_lifecycle` rows, and no production path may
take PostgreSQL's strongest row-lock mode on the session row. The second is
load-bearing: defaults replacement holds the current-defaults pointer row while
its `session_defaults_version` insert requests `FOR KEY SHARE` on the session
row through its foreign key, so a submit-input `FOR UPDATE` there would close
that lock-order cycle into a real deadlock (40P01); `FOR NO KEY UPDATE` does not
conflict with referential `KEY SHARE` locks while remaining self-exclusive.

## Turn attempts

A turn attempt is one exclusive physical orchestration tenure. The implemented
`CurrentTurnAttempt` factors the attempt identity outside its state and closes
the nonterminal states to `Prepared`, `Running`, and `StopRequested { causes }`.
All transitions are crate-private, sealed behind the (future) turn aggregate;
callers cannot forge a running attempt, an ended attempt, or terminal history
(compile-fail-tested).

Stop causes are a canonical union algebra: `CancellationOnly` carries one
applied interrupt proof; `FatalMismatch` carries a nonempty mismatch failure set
plus the retained interrupt state. Adding a fatal cause to a cancellation-only
stop upgrades it without losing the proof; equal replay is idempotent; a
distinct second interrupt proof is rejected without changing state. Ended
attempts carry cause-specific terminal history: `WithoutStop`,
`AfterCancellation`, and `AfterFatalMismatch`, whose disposition enums make
dishonest ends unrepresentable — a fatal-stopped attempt cannot claim
completion, refusal, cancellation, or a wait yield, and `WithoutStop` cannot
claim `Cancelled`. Why: encoding the stop/disposition compatibility matrix in
types means restart cannot construct a state the accepted lifecycle prohibits.

Committed attempt facts today are exactly: the initial `Prepared` attempt
created by activation, a stored `running` state kind admitted by reconstitution,
the startup scan's `Ended(WithoutStop(Lost))`, and the model-call slice's
`Ended(WithoutStop(_))` closures with dispositions `turn_completed`,
`turn_refused`, `known_failure`, and `ambiguous`. `turn_attempt` storage
enforces one initial attempt per turn (`turn_attempt_one_initial_per_turn`), at
most one live attempt per turn (`turn_attempt_one_live_per_turn`,
`WHERE state_kind <> 'ended'` — the durable form of exclusive tenure), and a
unique continuation chain; `StopRequested` has no storage.

## Eligibility derivation

Eligibility is a derived predicate, never a durable state. Why: the immutable
acceptance positions, typed priority relations, and active-slot owner are
already durable, so a second lifecycle state could only diverge from the facts
it is derived from.

The authoritative pass reconstitutes one complete session-scoped scheduling
projection (`AcceptedInputSchedulingReconstitutionInput::reconstitute`) and
fails closed on any omission or cross-wiring: cross-session records, duplicate
accepted inputs, missing origin or failure entries, snapshots that do not
resolve to their exact stored membership, stored starts whose lineage or
frontier disagree with the derived order, and lifecycle states that do not form
a failed-terminal prefix, at most one active slot, and a queued suffix in
durable total order. The total order itself is
`derive_accepted_input_total_order`: ordinary roots by acceptance position, each
followed by its unique recursive interrupt-successor chain, with monotonic
interrupt targets validated. Queued turns store no predecessor pointer; the
immediate predecessor is fixed once, at eligibility.

`prepare_earliest_queued_activation` then applies the predicate in pure domain
code: it rejects when an active turn holds the slot or no queued turn exists
(both map to a `NoEligibleTurn` no-op, not an error), selects the earliest
queued turn, and constructs atomically-committable state:

- lineage `FirstInSession` iff the session has no earlier turn, else
  `After { immediate_predecessor }` naming the exact terminal turn ordered
  immediately before it;
- the starting context frontier: the predecessor's terminal frontier with the
  fresh origin semantic entry appended (prefix-preserving), or a fresh snapshot
  containing only the origin entry for a first-in-session turn;
- the opaque `AcceptedInputTurnStart` binding lineage and frontier, whose
  constructor is private to validated eligibility (INV-009 — a raw identifier or
  list supplied by a caller is not start authority); and
- the initial `Prepared` attempt.

Sessions created with transcript ancestry cannot be scheduled yet;
reconstitution fails with `UnsupportedSessionAncestry` (open edge).

## The activation transaction

`StartEligibleTurnRepository::handle` runs one authoritative pass per hint:

1. Lock the `session_scheduler` row `FOR UPDATE`. A hint for a nonexistent
   session rolls back as `NoEligibleTurn`; a session without its scheduler row
   is fail-closed corruption. Why the lock rule: taking this lock before any
   `turn_lifecycle` write serializes every lifecycle writer's lifecycle access;
   deadlock freedom additionally requires the session-row lock-mode contract
   (previous section), not this lock alone.
2. Load the current session and the complete scheduling projection under that
   lock, through the checked domain seams.
3. Let the domain prepare the activation (previous section). The application
   layer supplies three fresh UUIDv7 identity candidates (origin entry, starting
   frontier, initial attempt) per pass and never selects a target turn.
4. Commit atomically: insert the origin semantic entry, the starting snapshot
   with complete materialized membership, and the prepared attempt row, then run
   the guarded lifecycle `UPDATE` that binds the exact lineage, frontier, and
   attempt and flips `queued` to `active`. The update re-asserts queued state,
   no active turn, all earlier ordered turns terminal, and the exact derived
   predecessor (the SQL re-check orders by raw acceptance position, which
   coincides with the interrupt-aware derived order only while no
   interrupt-priority rows can be committed). Commit only when it affects
   exactly one row; zero rows after in-lock validation is fail-closed
   corruption, and identity-key conflicts map to typed identity-collision errors
   after full rollback.

The committed origin entry, snapshot, start, active slot, and attempt are one
transaction: no durable state exists in which a start references a missing or
partial snapshot.

Both authoritative repositories — activation and startup recovery — classify
commit failures (`commit_failure_is_ambiguous`, tested in each): SQLSTATE
08007/40003 or any non-database error during the commit await surfaces
`Infrastructure { commit_ambiguous: true }`, because the commit may have durably
taken effect despite the error return; failures proven to precede commit are
never marked ambiguous.

## Scheduler loop and eligibility sweep

The durable rows are the only queue. Every in-process structure is a latency
hint that may be lost at any moment. Why: a wake-up is a hint, never authority —
acting on a false hint changes zero rows, and a lost true hint is recovered by
the sweep (INV-007).

- **Nudge (primary).** After a submit-input pass whose recorded result is a turn
  origin (`Recorded(Applied(TurnOrigin))` — including owner-global replay of an
  already-recorded command, whose transaction rolls back and commits nothing
  new), `SubmitInputService` hands the session to the in-process nudge port. The
  buffer is bounded (1024); a full buffer or closed source drops only the hint,
  visibly, and never changes the command result.
- **Sweep (backstop).** `PostgresEligibilitySweep` finds sessions with a queued
  turn and no active turn — the storage shape of the eligibility precondition;
  the `turn_lifecycle_queued_by_session` partial index is created for exactly
  this query shape, though planner adoption is not pinned by any test — paged 16
  sessions per query with a fixed per-cycle bound; continuation pages run
  immediately. The baseline interval is one second; missed ticks are delayed,
  not burst. A failed sweep is logged with its operator classification and
  retried at the next interval.
- **Loop.** `SchedulerLoop::run_until` spawns at most 16 concurrent per-session
  passes, deduplicates hints for a session already in flight (recording one
  rerun), and keeps an in-progress sweep read alive across pass completions. A
  failed or panicked pass is logged and retried by a later hint or sweep;
  nothing is lost because the rows are the queue.

The initial sweep runs as soon as the work source is first polled, seeding the
scheduler after startup recovery. Activation returns the activated turn
(`StartEligibleTurnOutcome::Activated(Box<ActivatedAcceptedInputTurn>)`), and
hubd's `ActivatedTurnPass` hands it to an `ActivatedTurnExecution` —
`ModelCallExecutionService` over the `ModelCallProvider` port — so each pass
activates and then drives the turn's model call. hubd depends on
`model-runtime`/`model-runtime-anthropic` through the `model-provider-runtime`
bridge; application and persistence still declare no runtime-crate dependency.
Tool-attempt storage still does not exist.

## Startup scan and recovery

hubd orders startup strictly: embedded migrations, then the startup scan to
completion, then scheduling. Why: scheduling before the scan could dispatch new
work into a session whose durable state still shows a live-looking prior-process
attempt (INV-034).

`StartupScanService` reads the finite inventory of sessions with an active turn
(deterministic order), then runs one independent transaction per session under
the same scheduler-row lock ordering as every other lifecycle writer. Each
transaction reconstitutes the complete scheduling projection and classifies the
lost tenure by its durable model-call evidence — startup never fabricates a live
end (INV-034):

- an evidence-free turn (no model call) prepares
  `prepare_active_turn_lost_failure`: the current attempt ends
  `WithoutStop(Lost)` and the turn fails;
- a turn holding a `Prepared` model call (`recover_after_restart`) closes the
  call `known_failed` while its abandoned attempt still ends
  `WithoutStop(Lost)`, and the turn fails; and
- a turn holding an in-flight call ends the call `ambiguous` and the attempt
  `WithoutStop(Lost)`, but the turn does not terminalize: it stays active,
  parked in the `awaiting_model_call_recovery` phase naming the ambiguous call
  (`recovery_model_call_id`), with no `TurnFailed` entry, no terminal frontier,
  no terminal disposition, and no `turn_failed` outbox record (a
  `cancellation_requested` call cannot pass the reconstitution seam).

In the two failing branches only: one `TurnFailed` semantic entry is appended,
and the terminal frontier is derived as the starting frontier plus that marker
(entry payloads are [sessions-and-transcript](sessions-and-transcript.md)
scope); the turn terminalizes `Failed`, releasing the slot via one guarded
attempt-end update and one guarded lifecycle update, each required to match
exactly one row; and a `turn_failed` outbox record is appended in the same
transaction (outbox mechanics are
[persistence-protocol](persistence-protocol.md) scope).

Why `Failed`: the evidence-free slice stores no operations, waits, or stop
causes, so an abandoned tenure has no sufficient completion, refusal, or
confirmed-interrupt evidence, and the version-one no-automatic-retry policy
([model-call-execution](model-call-execution.md)) makes the recovered turn fail
rather than silently retry.

Only an evidence-free session (active turn with no model call) whose tail
contains pending steering defers with the blocking accepted input and fails hub
startup; when the lost turn holds a `Prepared` model call, recovery fails the
turn and atomically reclassifies each pending-steering row as a fresh queued
successor turn origin (`reclassified_as_turn_origin`). Identity collisions are
retried with fresh candidates; infrastructure and fail-closed corruption stop
startup visibly. The scan is idempotent — a rerun inventories only work still
active, and a stale observation rolls back as `NoActiveTurn`. There is no
process-incarnation column and no lease: under the single-hub deployment
contract, every nonterminal attempt observed at startup is a prior-process
abandonment (INV-010). That contract is an operational assumption with no code
enforcement — the advisory singleton guard is an unadopted open edge, and a
second concurrent hub would violate the premise undetected.

## Occupied-slot input handling

Command construction, owner-global deduplication, and acceptance atomicity are
[identity-and-commands](identity-and-commands.md) scope. The occupied-slot
delivery outcomes implemented here are:

- `StartWhenNoActiveTurn` while a turn holds the slot records the typed
  rejection `ActiveTurnPresent`; a stale `expected_active_turn` on any
  active-work mode records `ActiveTurnMismatch`. Both are terminal recorded
  command results, replayed as such (INV-028).
- `NextSafePoint` records the input as `PendingSteering` with a
  configuration-free binding to the exact active source turn; its acceptance
  position derives from the validated session acceptance tail. No turn is
  created. A reclassification path now exists: terminalization of the source
  turn reclassifies pending steering into a queued successor origin turn that
  inherits the source turn's configuration
  (`queued_input_origin.source_configuration_turn_id`); in-turn safe-point
  consumption still does not exist, and evidence-free startup recovery still
  defers on pending steering.
- `AfterCurrentTurn` creates an ordinary queued origin turn with frozen
  configuration and an immutable acceptance position; it fixes no predecessor
  until eligibility.
- `Interrupt` targeting the active turn is deliberately a nonclaiming
  preparation failure (`InterruptApplicationUnavailable`): no command identity
  is claimed, no rejection is recorded, and the caller receives a typed error.
  Why: the accepted interrupt application must atomically construct the applied
  proof, immediate-successor priority, and predecessor transition, and none of
  that authority exists before the `StopRequested` slice — the owner ratified
  this deferral
  ([decision ledger, 2026-07-19](../decisions.md#2026-07-19--owner-ratified-matching-interrupt-milestone-deferral))
  rather than let a weaker interrupt claim a result.

## Context frontier snapshots

A context frontier is `{ owning_session, snapshot: ContextFrontierId }`;
`ContextFrontierId` is a distinct domain identity (INV-001). Ordinary equality
is identity equality; exact-content comparison (`same_semantic_content`) is a
separate explicit operation over the complete ordered source-qualified entry
sequence. A resolved snapshot is an ordered, duplicate-free sequence of
`SemanticTranscriptEntryRef` values; the only derivation offered is
prefix-preserving append (`derive_appending_candidate`), so a later snapshot
retains every earlier entry in order (INV-015). Why identity-not-content: two
independently created snapshots may contain equal entries without being the same
fixed frontier, and provenance must survive that coincidence.

Construction authority is sealed: public code cannot assemble a
`ResolvedContextFrontierSnapshot`, `AcceptedInputTurnStart`, or activated turn
from raw identifiers; the producers are the sealed domain transitions and
checked seams — eligibility activation, startup recovery, model-call closure
(completion, refusal, and known failure in
`crates/domain/src/model_execution.rs` derive terminal snapshots), and the
fail-closed reconstitution seams that rebuild a stored snapshot only from its
complete materialized membership. Persistence materializes complete snapshot
membership (`context_frontier` + `context_frontier_member`), inserts only; a
deferred constraint trigger (`context_frontier_requires_complete_membership`)
re-asserts complete contiguous membership — exact declared count, positions
`1..count` — at commit, and reconstitution rejects any stored snapshot whose
resolved membership disagrees with the complete entry set — one identifier can
never resolve differently. Transcript-ancestry resolution into a first frontier
is unimplemented (open edge); `TranscriptFrontier` itself is
[sessions-and-transcript](sessions-and-transcript.md) scope.

## Evidence-bearing reconstitution

Evidence validation is implemented for the scheduling seam: stored active phases
are conclusions derived from complete owner facts, never trusted discriminators.

- `AwaitingRecoveryDecision` now reconstitutes from complete model-call owner
  facts (an `ambiguous` terminal call correlated with its ended attempt —
  `ambiguous` from a live loss, `lost` from startup recovery); `StopRequested`
  and `AwaitingApproval` inputs still have no production constructors
  (compile-fail-tested); a stored proof-shaped payload or bare wait subject
  cannot become a phase until a complete correlated owner projection exists.
- A failed terminal turn that ended through a physical attempt durably names its
  exact ended attempt and optional terminal call
  (`turn_lifecycle.terminal_attempt_id`, `terminal_model_call_id`, backfilled
  and closed by migration `202607220003`). Reconstitution validates that
  provenance fail-closed through the typed
  `FailedTurnExecutionReconstitutionInput` — an ended `known_failure` or `lost`
  attempt, plus a correlated `known_failed`/`cancelled` call when one exists —
  instead of accepting an evidence-free failure record, and the deferred
  `assert_failed_terminal_execution_final_state` assertion re-closes the shape
  at every commit.
- Every active turn's projection must carry a session-scoped acceptance tail
  anchored at the turn's exact origin and extending gap-free through the
  observed last acceptance position, with unique identities, same- session
  membership, and per-entry delivery/disposition correlation. A filtered
  pending-steering list or bare maximum cannot substitute (INV-007, INV-016).
- A tail entry recording an accepted interrupt against the active turn fails
  reconstitution (`ActivePhaseEvidenceMismatch`): applied-interrupt evidence
  would require a proof-bearing phase this seam cannot construct.

Why fail-closed: an omission inside a claimed complete observation is
indistinguishable from acknowledged work disappearing, so the seam rejects
rather than repairs, and no effect is authorized from a failed reconstruction
(general reconstitution boundary:
[persistence-protocol](persistence-protocol.md)).

## Hub runtime: startup order and shutdown

hubd is the composition root. It reads `DATABASE_URL`, `SIGNALBOX_CONFIG_FILE`
(the model-configuration TOML naming provider targets, selections, and aliases),
and `ANTHROPIC_API_KEY_FILE` from the process environment (explicitly
provisional; delivery-channel decision is
[configuration-and-credentials](configuration-and-credentials.md) scope),
connects, then runs migrate → scan → schedule; any phase failure is a failed
startup with a classified, key-bearing log line and a failure exit code.
Observability and the operator failure taxonomy are
[runtime-substrate](runtime-substrate.md) scope.

On SIGINT/SIGTERM the scheduler loop stops admitting new passes and in-flight
passes get a bounded 30-second grace window to let their authoritative
transactions commit or abort. Window expiry abandons the work, warns, and skips
the unbounded pool drain; a clean exit closes the pool. Why shutdown is polish,
not correctness: abrupt exit at any point is safe because durable rows plus the
startup scan recover whatever a window abandoned (INV-034), so the grace window
buys only latency. Repositories and services are cheap per-invocation clones
over the shared pool; no shared locked service instance exists.

## Open edges

- Interrupt application is deferred: `SubmitInput::Interrupt` against the active
  turn is a nonclaiming failure until the `StopRequested` slice adds application
  and its designed recorded rejections.
- `StopRequested` and `AwaitingApproval` have no storage or reconstitution
  (`AwaitingRecoveryDecision` now has both); evidence-bearing reconstitution
  requires complete owner projections before any such phase lands.
- Direct fatal terminalization has sealed domain derivation values
  (`fatal_mismatch` module) but no aggregate transition or commit path.
- Dispatch fencing is partially implemented: `model_call` storage, the
  `AttemptDispatchGate`, and the hubd dispatch path now exist; only tool-attempt
  storage and tool dispatch remain absent.
- The eligible terminal-failure path (queued turn fixes its start and fails
  without an attempt for a structurally unexecutable configuration) is
  unimplemented; activation is the only eligibility outcome.
- Ancestry-derived sessions cannot be scheduled (`UnsupportedSessionAncestry`);
  ancestry-to-first-frontier resolution is unimplemented.
- Pending safe-point steering is reclassified into a queued successor origin at
  source-turn terminalization; in-turn safe-point consumption is still absent,
  and only evidence-free startup recovery defers on pending steering.
- Startup recovery now classifies model-call evidence (a `Prepared` call closes
  as a known failure; an in-flight call parks the turn as ambiguous in
  `awaiting_model_call_recovery`); stop-cause recovery and wait reconstruction
  for the remaining phases still await their slices.
- The single-hub advisory singleton guard and per-session scan gating (designed
  refinements) are not adopted; sweep interval and fairness tuning remain
  operational open questions.
- LISTEN/NOTIFY remains the documented multi-process extension only; the
  baseline is single-process nudge plus sweep.

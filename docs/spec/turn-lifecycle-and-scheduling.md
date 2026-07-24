# Turn lifecycle and scheduling

The baseline turn behavior was verified through PR #175 (`agent/stop-requests`).
This page covers turns, turn attempts, eligibility derivation, the scheduler,
and startup recovery. Code homes:
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
terminal disposition kind closed to `failed`, `completed`, `refused`,
`cancelled`, and `reconciliation_required` (migrations `202607220001` and
`202607220005`). The domain `TurnDisposition` algebra carries all five accepted
variants — `Completed`, `Refused`, `Failed`, `Cancelled { cause }`,
`ReconciliationRequired { marker }` — but `Cancelled` is constructible only from
an `AppliedInterruptProof` and `ReconciliationRequired` only from a sealed
`ReconciliationMarker`. Committed transitions produce every variant: interrupted
physical ambiguity produces proof-bearing `ReconciliationRequired`, while
confirmed interrupted cancellation produces proof-bearing `Cancelled`.

The domain `ActiveTurnPhase` algebra is `Running { current_attempt }`,
`AwaitingApproval { request }`, and
`AwaitingRecoveryDecision { ambiguous_operations }`. Every active phase retains
the session's progressing slot (`retains_progressing_slot()` is unconditionally
true; INV-009). Storage and reconstitution admit the `running` and
`awaiting_model_call_recovery` phases; `AwaitingRecoveryDecision` is
reconstituted from an `ambiguous` terminal model call correlated with its ended
attempt (`ambiguous` from a live loss, `lost` from startup recovery).
`StopRequested` is a stored current-attempt state inside the `running` active
phase and reconstitutes only from its exact applied-interrupt proof;
`AwaitingApproval` has no storage row or production constructor (see
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
   `prepared`→`running`|`ended`, `running`→`stop_requested`|`ended`,
   `stop_requested`→`ended`), terminal-turn immutability, write-once start
   fields, no new attempt on a terminal turn, and queued terminalization only
   without attempt history. Ended attempts are immutable.

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

Committed attempt facts include the initial `Prepared` attempt created by
activation, `running`, and proof-bearing `stop_requested` state kinds, the
startup scan's lost closures, and the model-call slice's cause-specific terminal
histories. `stop_requested` stores the exact applied interrupt command and
predecessor needed to reconstruct `CancellationOnly`; the correlated call is
durably `cancellation_requested`. `turn_attempt` storage enforces one initial
attempt per turn (`turn_attempt_one_initial_per_turn`), at most one live attempt
per turn (`turn_attempt_one_live_per_turn`, `WHERE state_kind <> 'ended'` — the
durable form of exclusive tenure), and a unique continuation chain.

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
a terminal prefix, at most one active slot, and a queued suffix in durable total
order. Every terminal variant contributes its checked terminal frontier to that
prefix, including proof-bearing cancellation and reconciliation-required
predecessors. The total order itself is `derive_accepted_input_total_order`:
ordinary roots by acceptance position, each followed by its unique recursive
interrupt-successor chain, with monotonic interrupt targets validated. Queued
turns store no predecessor pointer; the immediate predecessor is fixed once, at
eligibility.

`prepare_earliest_queued_activation` then applies the predicate in pure domain
code: it rejects when an active turn holds the slot or no queued turn exists
(both map to a `NoEligibleTurn` no-op, not an error), selects the earliest
queued turn, and constructs atomically-committable state:

- lineage `FirstInSession` iff the session has no earlier turn, else
  `After { immediate_predecessor }` naming the exact terminal turn ordered
  immediately before it;
- the starting context frontier: the predecessor's terminal frontier with the
  fresh origin semantic entry appended (prefix-preserving); for a
  first-in-session turn, the exact frontier identity stored by the session's
  `ImportedSessionSeed` followed by the origin entry when ancestry is
  `ImportedConversation`, or only the origin entry when ancestry is `None`;
- the opaque `AcceptedInputTurnStart` binding lineage and frontier, whose
  constructor is private to validated eligibility (INV-009 — a raw identifier or
  list supplied by a caller is not start authority); and
- the initial `Prepared` attempt.

`SingleSource` native-fork ancestry remains unschedulable and fails
reconstitution with `UnsupportedSessionAncestry`. Imported ancestry is admitted
only when its seed satisfies the complete imported-session contract in
[sessions-and-transcript](sessions-and-transcript.md) (INV-038, INV-039).

Imported ancestry does not alter lifecycle order, eligibility, slot ownership,
or lineage. Its resume/fork relationship is immutable creation provenance, not a
scheduler mode. The first native turn is still `FirstInSession`; imported
entries are a context prefix, not a synthetic predecessor turn. Migration
`202607240003_imported_session_first_native_frontier.sql` changes only the
first-frontier lifecycle check: a native session still starts with its one
origin entry, while an imported session must start with its exact stored seed
membership followed by that origin entry. All other lifecycle evidence checks
remain shared.

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
   no active turn, every earlier turn under the interrupt-aware total order
   terminal, and the exact derived predecessor. An `interrupt_immediately_after`
   origin proves its named predecessor and may precede ordinary queued inputs
   with lower raw acceptance positions. Commit only when the update affects
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

After configuration and database connection, hubd acquires the dedicated
single-hub advisory guard specified by [process-protocol](process-protocol.md),
then orders startup strictly: embedded migrations, the startup scan to
completion, process-socket bind, and only then request admission, outbox
dispatch, and scheduling. Why: any lifecycle writer or client read before the
scan could observe or alter a live-looking prior-process attempt (INV-034).

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
- a turn holding an unstopped in-flight call ends the call `ambiguous` and the
  attempt `WithoutStop(Lost)`, but the turn does not terminalize: it stays
  active, parked in the `awaiting_model_call_recovery` phase naming the
  ambiguous call (`recovery_model_call_id`), with no `TurnFailed` entry, no
  terminal frontier, no terminal disposition, and no `turn_failed` outbox
  record; and
- a turn holding a proof-correlated `stop_requested` attempt and
  `cancellation_requested` call ends the call `ambiguous` and the attempt
  `AfterCancellation(Lost)`, then terminalizes `ReconciliationRequired` with the
  call as its exact ambiguity set, an equal-content terminal frontier, and the
  interrupt reason.

In the two failing branches only: one `TurnFailed` semantic entry is appended.
The evidence-free branch extends the starting frontier; the prepared-call branch
extends that call's exact source frontier, which already contains every steering
entry consumed when the call was prepared. The turn terminalizes `Failed`,
releasing the slot via one guarded attempt-end update and one guarded lifecycle
update, each required to match exactly one row; and a `turn_failed` outbox
record is appended in the same transaction (entry payloads are
[sessions-and-transcript](sessions-and-transcript.md) scope; outbox mechanics
are [persistence-protocol](persistence-protocol.md) scope).

Why `Failed`: the evidence-free slice stores no operations, waits, or stop
causes, so an abandoned tenure has no sufficient completion, refusal, or
confirmed-interrupt evidence, and the version-one no-automatic-retry policy
([model-call-execution](model-call-execution.md)) makes the recovered turn fail
rather than silently retry.

Every terminal restart branch atomically reclassifies pending-steering rows as
fresh queued successor origins (`reclassified_as_turn_origin`) in ascending
acceptance position, including evidence-free turns; pending steering therefore
never defers or blocks startup. A persisted `StopRequested` attempt with its
`CancellationRequested` call reconstructs the exact proof, ends the abandoned
attempt through `AfterCancellation(Lost)`, and classifies the unobserved issued
call as ambiguous, terminalizes proof-bearing reconciliation, and releases the
slot without discarding stop intent. Identity collisions are retried with fresh
candidates; infrastructure and fail-closed corruption stop startup visibly. The
scan is idempotent — a rerun inventories only work still active, and a stale
observation rolls back as `NoActiveTurn`. There is no process-incarnation column
and no lease: under the single-hub deployment contract, every nonterminal
attempt observed at startup is a prior-process abandonment (INV-010). The
advisory guard is acquired before this scan and held on its dedicated connection
for the complete process lifetime, so a second hub cannot run the premise
concurrently.

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
  (`queued_input_origin.source_configuration_turn_id`). At the next model-call
  preparation, every pending input is consumed under the atomic boundary in
  [model-call-execution](model-call-execution.md) (INV-036).
- `AfterCurrentTurn` creates an ordinary queued origin turn with frozen
  configuration and an immutable acceptance position; it fixes no predecessor
  until eligibility.
- `Interrupt` targeting the active turn atomically accepts a configured
  immediate-successor origin, constructs the exact `AppliedInterruptProof`, and
  applies the predecessor transition (INV-029, INV-037). Before any terminal
  transition releases the slot, the same transaction reclassifies every pending
  steering input against the interrupted turn as an ordered queued successor
  origin. Call, attempt, and turn terminalization follow
  [model-call-execution](model-call-execution.md#terminal-outcomes). A matching
  interrupt against `AwaitingRecoveryDecision` preserves the already terminal
  ambiguous call and ended attempt, records the new proof on the turn's
  reconciliation marker, and terminalizes `ReconciliationRequired` with the
  wait's exact operation set. A next-safe-point request against a stopping turn
  records `SafePointUnavailableWhileStopping`; equal interrupt replay returns
  the original applied result. A distinct later interrupt records
  `InterruptAlreadyApplied { active_turn, existing_command }` without accepting
  an input or replacing the existing proof.

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
checked seams — imported-frontier session creation (which constructs exactly one
seed frontier from the selected normalized imported prefix), eligibility
activation, startup recovery, model-call closure (completion, refusal, and known
failure in `crates/domain/src/model_execution.rs` derive terminal snapshots),
and the fail-closed reconstitution seams that rebuild a stored snapshot only
from its complete materialized membership. Persistence materializes complete
snapshot membership (`context_frontier` + `context_frontier_member`), inserts
only; a deferred constraint trigger
(`context_frontier_requires_complete_membership`) re-asserts complete contiguous
membership — exact declared count, positions `1..count` — at commit, and
reconstitution rejects any stored snapshot whose resolved membership disagrees
with the complete entry set — one identifier can never resolve differently.
Imported ancestry resolves only through the checked session-creation producer;
its separate one-to-one `ImportedSessionSeed` must name the exact stored
frontier identity whose membership matches the selected imported prefix.
Equal-content reminting fails reconstitution. `SingleSource` ancestry resolution
remains unimplemented. `TranscriptFrontier` itself is
[sessions-and-transcript](sessions-and-transcript.md) scope.

## Evidence-bearing reconstitution

Evidence validation is implemented for the scheduling seam: stored active phases
are conclusions derived from complete owner facts, never trusted discriminators.

- `AwaitingRecoveryDecision` now reconstitutes from complete model-call owner
  facts (an `ambiguous` terminal call correlated with its ended attempt —
  `ambiguous` from a live loss, `lost` from startup recovery). A `StopRequested`
  current attempt reconstructs only when its stored interrupt command,
  predecessor, configured immediate successor, applied result, and
  cancellation-requested call form the exact proof. `AwaitingApproval` still has
  no production constructor (compile-fail-tested); a bare wait subject cannot
  become a phase until a complete correlated owner projection exists.
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
- A cancelled terminal turn reconstructs only from
  `CancelledTurnExecutionReconstitutionInput`: its exact ended attempt carries
  `AfterCancellation(Cancelled)` and the same complete applied-interrupt result
  as the turn disposition. It names either no call, proving direct cancellation
  before any call was prepared, or its one correlated terminal `cancelled` call.
  Its terminal frontier must extend the starting or call frontier by exactly the
  correlated `TurnCancelled` marker.
- A reconciliation-required terminal turn names its exact ended attempt and
  required terminal `ambiguous` call. The attempt end is either
  `WithoutStop(Ambiguous|Lost)` with a later turn-correlated applied interrupt,
  or `AfterCancellation(Ambiguous|Lost)` carrying that same proof. Its terminal
  frontier is an equal-content boundary over the ambiguous call's frontier. The
  checked scheduling input validates those correlations before the turn can
  serve as a terminal predecessor.
- Every active turn's projection must carry a session-scoped acceptance tail
  anchored at the turn's exact origin and extending gap-free through the
  observed last acceptance position, with unique identities, same- session
  membership, and per-entry delivery/disposition correlation. A filtered
  pending-steering list or bare maximum cannot substitute (INV-007, INV-016).
- A tail entry recording an accepted interrupt against the active turn is
  admitted only when the current stop/recovery state carries its exact
  `AppliedInterruptProof`; an evidence-free active phase rejects it as
  `ActivePhaseEvidenceMismatch`.

Why fail-closed: an omission inside a claimed complete observation is
indistinguishable from acknowledged work disappearing, so the seam rejects
rather than repairs, and no effect is authorized from a failed reconstruction
(general reconstitution boundary:
[persistence-protocol](persistence-protocol.md)).

## Hub runtime: startup order and shutdown

hubd is the composition root. It reads exactly `DATABASE_URL`,
`SIGNALBOX_CONFIG_FILE` (the model-configuration TOML naming provider targets,
selections, and aliases), `ANTHROPIC_API_KEY_FILE`, and `SIGNALBOX_SOCKET_PATH`
from the process environment (the provisional configuration channels are
[configuration-and-credentials](configuration-and-credentials.md) scope). It
connects, acquires the single-hub guard, fences the prior pool incarnation,
migrates, completes recovery scan, binds the process socket, then concurrently
admits protocol requests, dispatches the outbox, and schedules eligible work. On
a database without the fence migration, the guarded first migration creates the
fence row before the hub initializes its first fenced pool. No request, dispatch
cursor advance, or scheduler pass occurs before recovery completes. Any phase
failure is a failed startup with a classified, key-bearing log line and a
failure exit code.

The dedicated guard connection is checked once per second while the runtime is
active. Losing that session is a fatal fencing event: admission, dispatch, and
scheduling are cancelled without the graceful-shutdown window, all pooled
connections are terminated, and the process exits instead of reconnecting or
reacquiring in place. A successor can acquire the singleton guard immediately
but cannot pass the exclusive prior-generation fence until those old pooled
sessions are gone, so its migration and recovery never overlap them.
Observability and the operator failure taxonomy are
[runtime-substrate](runtime-substrate.md) scope.

On SIGINT/SIGTERM the listener stops accepting requests, follow streams are
closed, the dispatcher stops starting transactions, and the scheduler stops
admitting passes. Finite request handlers, the current dispatcher transaction,
and in-flight scheduler passes share the bounded 30-second grace window to let
authoritative transactions commit or abort. A clean exit closes the fenced pool,
waits on the guard session's exclusive current-generation fence so even detached
pool sessions have ended, removes only this hub's identity-pinned and
revalidated socket, and releases the advisory locks by closing its dedicated
guard connection. Window expiry abandons remaining tasks, warns, and skips the
unbounded pool drain; process exit releases its sessions. Why signal-driven
shutdown is polish, not correctness: abrupt exit at any point is safe because
durable rows plus the next guarded startup scan recover work and the durable
outbox cursor redelivers an uncommitted offer (INV-032, INV-034), so the grace
window buys only latency. Repositories and services are cheap per-invocation
clones over the shared pool; no shared locked service instance exists.

## Open edges

- `AwaitingApproval` has no storage or reconstitution; evidence-bearing
  reconstitution requires a complete owner projection before that phase lands.
- Direct fatal terminalization has sealed domain derivation values
  (`fatal_mismatch` module) but no aggregate transition or commit path.
- Dispatch fencing is partially implemented: `model_call` storage, the
  `AttemptDispatchGate`, and the hubd dispatch path now exist; only tool-attempt
  storage and tool dispatch remain absent.
- The eligible terminal-failure path (queued turn fixes its start and fails
  without an attempt for a structurally unexecutable configuration) is
  unimplemented; activation is the only eligibility outcome.
- Native `SingleSource` ancestry remains unschedulable
  (`UnsupportedSessionAncestry`); selecting and resolving native fork boundaries
  is unimplemented. Imported-conversation ancestry has its own exact
  selected-prefix frontier path and does not close that fork question.
- Continuation safe points after a call or tool result are not implemented; the
  current execution slice consumes steering only while preparing its one initial
  call. Source terminalization and evidence-free startup recovery reclassify any
  input that remains pending.
- Startup recovery now classifies model-call evidence (a `Prepared` call closes
  as a known failure; an unstopped in-flight call parks the turn as ambiguous in
  `awaiting_model_call_recovery`); wait reconstruction for remaining phases
  still awaits their slices.
- Per-session scan gating, sweep interval, and fairness tuning remain
  operational open questions; the process-wide advisory singleton guard is
  specified by [process-protocol](process-protocol.md).
- LISTEN/NOTIFY remains the documented multi-process extension only; the
  baseline is single-process nudge plus sweep.

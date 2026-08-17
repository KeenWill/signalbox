# Turn lifecycle and scheduling

The runner-recovery active-phase algebra, checked persistence reconstitution,
and preserved interrupt/stop authority were verified against this PR
(`agent/runner-awaiting-recovery-persistence`). The atomic persistence
transition into runner recovery was verified against this PR
(`agent/runner-loss-session-transaction`).

The active-tail predecessor-steering correction was verified against this PR
(`agent/daemon-ops-overnight`).

The cancelled-turn outbox projection for stopped tool responses with completed
producing calls was re-verified against this PR
(`agent/cancelled-outbox-completed-call`).

The user-vocabulary surface on this page was re-verified through PR #378
(`agent/user-vocabulary`).

This page specifies the implemented behavior of turns, turn attempts,
eligibility derivation, the scheduler, and startup recovery, as verified against
the implementing stack through PR #230 (`agent/frontier-scaling`); the
parked-approval interrupt delivery outcome was verified through PR #254
(`agent/fix-parked-approval-interrupt`), model-identity turn-start boundaries
were verified through PR #272 (`agent/mid-session-model`), the tool-round
continuation reconstitution and terminal shapes were verified through PR #292
(`agent/continuation-reconstitution`), and the steering-free continuation shapes
at the refused, reconciliation-required, and model-call recovery gates were
verified through PR #296 (`agent/continuation-reconstitution-siblings`); the
input-delivery surface, queued restart behavior, and protocol-driven
continuation steering were verified through PR #302 (`agent/mid-turn-steering`);
the template-specific home requirement and template-catalog startup order were
verified through PR #311 (`agent/session-templates-spec`); exact start-frontier
reconstitution across a validated compaction boundary was verified through PR
`#312` (`agent/context-compaction-core`); and the corresponding persistent
final-state gate was verified through PR #314
(`agent/context-compaction-protocol`); the delegated foreground-wait termination
path and descendant-cascade ordering were verified through this PR
(`agent/delegation`). Code homes:
`crates/domain/src/{turn_lifecycle,turn_attempt,turn_eligibility,`
`context_frontier,queue_order}.rs`, `crates/application/src/{scheduler,`
`start_eligible_turn,startup_scan,submit_input}.rs`,
`crates/persistence/src/{start_eligible_turn,startup,scheduler,`
`lock_inventory}.rs` and its migrations, and
`apps/signalboxd/src/{main,process_runtime}.rs`. The `signalboxd`
composition-root name and that `apps/signalboxd` code home were verified through
PR #258 (`agent/signalboxd-rename`); the additional daemon-held code-host
credential path is verified through PR #270 (`agent/tool-batch-tier1`), and the
Brave Search credential path is verified through PR #433
(`agent/web-search-wiring`); the user reconciliation decision that releases an
ambiguity wait, together with the startup scan's separate report of sessions
holding their slot for that decision, were verified through PR #281
(`agent/turn-reconciliation-recovery`). The finite startup scan and removal of
the superseded steering blocker were verified through PR #291
(`agent/turn-control-verbs`). INV-tagged tests are the enforcement of record;
tags below resolve through the generated
[invariant test index](../invariants.md). Designed lifecycle behavior that has
no committed code path appears only under [Open edges](#open-edges). The
registration-only runner-socket startup and supervision are verified by PR #376
(`agent/runner-daemon`). Runner-loss recovery and recovery-only startup remain
committed unimplemented functionality as labeled below. Sibling pages named in
scope deferrals below (identity-and-commands, sessions-and-transcript,
persistence-protocol, model-call-execution, configuration-and-credentials,
runtime-substrate) are companion pages of this spec set; each deferral names the
owning page rather than restating its material.

## Turns, states, and the single active slot

A turn is one durable logical request for one conversational outcome from one
accepted-input origin under one frozen effective configuration. Model-selection
freeze is
[configuration-and-credentials](configuration-and-credentials.md#model-selection-validation)
scope; defaults-epoch binding is
[sessions-and-transcript](sessions-and-transcript.md#session-defaults-and-replacement)
scope. The implemented slice stores three lifecycle states per turn
(`turn_lifecycle.state_kind`): `queued`, `active`, and `terminal`, with the
terminal disposition kind closed to `failed`, `completed`, `refused`,
`cancelled`, and `reconciliation_required` (migrations `202607220001` and
`202607220005`). The domain `TurnDisposition` algebra carries all five accepted
variants — `Completed`, `Refused`, `Failed`, `Cancelled { cause }`,
`ReconciliationRequired { marker }` — but `Cancelled` is constructible only from
an `AppliedInterruptProof`. `ReconciliationRequired` remains constructible only
from a sealed `ReconciliationMarker`. Committed transitions produce every
variant: interrupted physical ambiguity produces proof-bearing
`ReconciliationRequired`, and confirmed interrupted cancellation produces
proof-bearing `Cancelled`. Runner abandonment creates no additional turn-ending
authority.

The domain `ActiveTurnPhase` algebra is `Running { current_attempt }`,
`AwaitingApproval { request }`,
`AwaitingRecoveryDecision { ambiguous_operations }`, and
`AwaitingRunnerRecovery { runner, placement_revision, optional_tool_attempt }`.
Every active phase retains the session's progressing slot
(`retains_progressing_slot()` is unconditionally true; INV-009). Storage and
reconstitution admit `running`, `awaiting_tool_approval`,
`awaiting_model_call_recovery`, `awaiting_tool_recovery`, and
`awaiting_runner_recovery`; the domain `AwaitingApproval` phase maps to the
exact stored `awaiting_tool_approval` discriminator. `AwaitingRecoveryDecision`
is reconstituted from either an `ambiguous` terminal model call or an ambiguous
external-effect tool attempt correlated with its exact ended attempt
(`ambiguous` from a live loss, `lost` from startup recovery). `StopRequested` is
a stored current-attempt state inside the `running` active phase and
reconstitutes only from its exact applied-interrupt proof; `AwaitingApproval`
reconstitutes only from the exact earliest undecided request of a complete tool
batch and carries no live turn or tool attempt ([tool-loop](tool-loop.md)).

**Committed unimplemented functionality — credential-availability wait.** No
present `ActiveTurnPhase`, storage discriminator, startup-scan branch, scheduler
path, or process state supplies the pool-availability wait in
[model-call execution](model-call-execution.md#availability-successor-calls),
and no present runtime can enter it. Its implementing child must add a distinct
active phase with a closed `exhausted`/`contended` cause that retains the
session slot and durably binds the frozen pool-policy snapshot. The exhausted
form carries every policy member's exclusion evidence and optional reset, plus
the optional deadline the machine computes from them — this page carries the
field, never its formula, since only the machine decides which exclusion kinds
expire at a reset they report. The contended form carries every durable
exclusion in the selection snapshot and the complete nonempty set of
otherwise-admissible bounded members with exact invocation-reservation
identities. Startup may reconstitute either only from that complete evidence.
Which ending a selection attempt reaches — and therefore whether a wait is
stored at all, and in which form — is owned by
[the credential-availability machine](credential-availability.md#the-credential-availability-machine).
This page owns two of that table's columns, turn phase with attempt disposition
and wake conditions, and states no rule about which row is reached. Startup
reconstitutes the persisted choice without reclassifying it, and release
consumes only a stored wait. Entering either wait form atomically ends the
call-free current attempt as `WithoutStop(YieldedToDurableWait)` and stores the
wait, leaving no live attempt. A reservation is `pending_spawn` from its atomic
acquisition with the `Prepared` call until successful spawn durably attaches the
child process group's reuse-safe host identity as
`spawned { process_group_identity }`. Startup retains live-process `spawned`
reservations, whose observation path this daemon still owns, and must resolve
every fenced prior-process reservation before scheduling — proving that exact
process group absent, or terminating it and then proving absence — before
closing it as lost. It is never retained for a later death notice, since the
observation that would release it died with its daemon. A prior-process
`pending_spawn` reservation is ambiguous and fails startup before scheduling.
The scheduler makes a reached deadline, an exact reservation release, or a
durable member-availability update eligible. Because a capacity bound is a live
configuration value rather than a frozen one, startup additionally re-evaluates
every retained `contended` wait against the current registrations before
enabling scheduling. Each wait names a complete nonempty bounded-member set, so
every member is evaluated and any one of them suffices: a member the current
registration leaves unbounded makes the wait eligible outright, and a member
still bounded makes it eligible when that profile's surviving reservation count
is below the current bound. Without that pass a raised or removed bound would
not admit work until an unrelated old invocation happened to finish, since a
configuration edit produces neither a release nor an availability update.
Release atomically consumes the wait, creates a fresh `Prepared` successor
attempt, and returns the same turn to `Running`, resuming the availability chain
the wait was part of rather than starting a new one — the release origin carries
that chain's predecessor call and its proof, which a new chain could not. Which
exclusions survive that release is owned by
[the credential-availability machine](credential-availability.md#the-credential-availability-machine),
and it splits them in two. A predecessor exclusion earned by a qualifying
failure in this turn is insert-only and turn-local, so nothing readmits that
member within this turn — not a reset passing, not an operator clear of its
exact predecessor correlation, and not any other durable availability update
([model-call execution](model-call-execution.md#availability-successor-calls));
without that, waking a one-member `switch_now` pool on its own reported reset
would call the same profile again without bound. Every other exclusion the wait
recorded — an ordinary reset-aware membership exclusion, an `avoid_new_sessions`
exclusion, a profile quarantine — is re-read from its current active state
instead, because a release that re-applied a reset which had already passed
would re-enter the same wait on the very wake its deadline exists to produce.

Eligibility is permission to retry selection, not a guarantee of a slot. One
release can make several contended waits eligible while admitting only one, so
each release transaction reruns admission under the same capacity locks
preparation takes. The transaction that acquires the freed reservation performs
the release above. A transaction that finds no admissible member does not fail
and does not leave its wait pointing at a reservation that no longer exists:
under those locks it atomically replaces the wait's evidence with the live
reservation identities now holding the bound, and the turn stays parked in the
contended form. If no bounded member remains — every former one is now durably
excluded — the contention is over and the transaction re-runs the pool's
exhaustion policy rather than assuming a wait. Which ending that re-run reaches
is the contended-to-exhausted rule of
[the credential-availability machine](credential-availability.md#the-credential-availability-machine),
and this page adds nothing to it: the turn moves to whichever row that table
names, and releasing a contended wait does not change which one. A `fail` pool
therefore cannot be left parked indefinitely by having entered contention first,
and cannot acquire a pre-call cause it never earned. Releasing a bounded
reservation holds that profile's capacity row across the atomic release-and-wake
commit, and a rewrite holds the capacity rows of every bounded member its
evidence names, so a completion cannot slip between a loser's read and its
commit. A reservation identity therefore never outlives the wait that names it,
and losing a race costs a re-park rather than a failed turn, a missed wake, or
an admission above the bound.

The wait has an exact occupied-slot control matrix. `steer` is accepted as
ordinary pending steering bound to this source turn and remains pending until a
release transaction consumes it with the fresh call. `stop_turn` is admitted:
under the scheduler lock it revalidates the exact wait, accepts the configured
immediate-successor origin, closes the wait, creates the fresh `Prepared`
attempt, records the ordinary applied-interrupt proof on that attempt, ends it
`AfterCancellation(Cancelled)`, appends `TurnCancelled` after the wait's latest
frontier, reclassifies any steering still pending on the source turn as a queued
successor, and terminalizes `Cancelled`, all atomically. No live attempt
remains. The reclassification is not optional here: pending steering accepted
while the turn was parked is ordinary pending steering, and the lifecycle rule
below — with the `turn_lifecycle_pending_steering_closed` constraint that
enforces it — requires every such row to be closed before its turn terminalizes,
so a stop transaction that left it pending would be rejected at commit. Equal
replay returns that receipt; a released or otherwise changed wait returns the
ordinary active-turn mismatch. A goal `stop_goal` or supersede command remains a
goal-state transition and does not manufacture turn-interrupt authority; if the
caller also wants to release this active slot it submits the existing
`stop_turn` command. No approval or reconciliation command applies to the wait.
This compatibility constraint does not add that phase or these branches to the
implemented closed vocabulary above.

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
durable form of exclusive tenure), and a unique continuation chain. A completed
tool-using model round ends the current attempt as a tool-round yield; approval
completion creates the next attempt in that chain, which owns serialized tools
and the next model call without creating a new logical turn.

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
- the starting context frontier: the predecessor's terminal frontier followed by
  a `ModelIdentityChanged` entry exactly when this turn's acceptance-frozen
  direct model differs from the predecessor's, then the fresh origin semantic
  entry (prefix-preserving); for a first-in-session turn, the exact frontier
  identity stored by the session's `ImportedSessionSeed` followed by the origin
  entry when ancestry is `ImportedConversation`, or only the origin entry when
  ancestry is `None`;
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
   layer supplies four fresh UUIDv7 identity candidates (optional model-identity
   entry, origin entry, starting frontier, initial attempt) per pass and never
   selects a target turn.
4. Commit atomically: insert the optional model-identity entry and origin
   semantic entry, the starting snapshot with complete materialized membership,
   and the prepared attempt row, then run the guarded lifecycle `UPDATE` that
   binds the exact lineage, frontier, and attempt and flips `queued` to
   `active`. The update re-asserts queued state, no active turn, every earlier
   turn under the interrupt-aware total order terminal, and the exact derived
   predecessor. An `interrupt_immediately_after` origin proves its named
   predecessor and may precede ordinary queued inputs with lower raw acceptance
   positions. Commit only when the update affects exactly one row; zero rows
   after in-lock validation is fail-closed corruption, and identity-key
   conflicts map to typed identity-collision errors after full rollback.

The committed turn-start entries, snapshot, start, active slot, and attempt are
one transaction: no durable state exists in which a start references a missing
or partial snapshot (INV-040).

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
  origin (`Recorded(Applied(TurnOrigin))` — including user-global replay of an
  already-recorded command, whose transaction rolls back and commits nothing
  new), `SubmitInputService` hands the session to the in-process nudge port. The
  buffer is bounded (1024); a full buffer or closed source drops only the hint,
  visibly, and never changes the command result.
- **Sweep (backstop).** `PostgresEligibilitySweep` finds four durable shapes: a
  queued turn with no active turn (the activation precondition), an active turn
  whose current model call remains `Prepared`, an active tool round in the
  running phase, or a current pursuing goal turn that is terminal and therefore
  still owed its durable continuation-or-blocking disposition. A `Prepared`
  model call covers both an ordinary not-yet-driven call and an attachment check
  retained for retry after temporary store unavailability. The
  `turn_lifecycle_queued_by_session` partial index is created for the queued
  query shape, though planner adoption is not pinned by any test. Results are
  paged 16 sessions per query with a fixed per-cycle bound; continuation pages
  run immediately. The baseline interval is one second; missed ticks are
  delayed, not burst. A failed sweep is logged with its operator classification
  and retried at the next interval.
- **Loop.** `SchedulerLoop::run_until` spawns at most 16 concurrent per-session
  passes, deduplicates hints for a session already in flight (recording one
  rerun), and keeps an in-progress sweep read alive across pass completions. A
  failed or panicked pass is logged and retried by a later hint or sweep;
  nothing is lost because the rows are the queue. A pass about to perform
  attachment store I/O first tries the blob contract's separate
  attachment-preparation permit without waiting. If none is immediately
  available, the pass relinquishes its 16-pass capacity, ends, and leaves only
  the durable `Prepared` row for a later sweep. After acquiring a permit, its
  task remains in flight for per-session deduplication but relinquishes the
  scheduler-pass slot during store I/O; after successful verification it
  reacquires a slot before send authorization and its guarded transaction
  revalidates authority. A model-originated `blob_read` uses the same slot
  handoff after it acquires the blob contract's non-waiting direct-read permit:
  its physical attempt remains in flight during store traversal, and it
  reacquires a slot before committing correlated result evidence or crash-loss
  classification. At most 16 direct reads can wait at that reacquisition point.

The initial sweep runs as soon as the work source is first polled, seeding the
scheduler after startup recovery. This recovers a goal disposition when the
process ended after turn terminalization but before scheduler reconciliation.
Each authoritative pass first asks its execution composition to reconcile any
active running tool round for the hinted session. If the active turn instead
retains a current `Prepared` model call, the pass reloads that call and hands it
to the same `ModelCallExecutionService` used after activation; temporary
attachment unavailability can therefore retain the call for a later sweep
without requiring restart. Only a session with neither shape proceeds to
ordinary queued-turn activation. Failure of either read-only lookup is an
ordinary failed pass for later scheduler retry; only a failure after active-turn
execution begins trips fatal recovery supervision. A parked approval returns
from the pass immediately and therefore retains no scheduler worker capacity.
Activation returns the activated turn
(`StartEligibleTurnOutcome::Activated(Box<ActivatedAcceptedInputTurn>)`), and
signalboxd's `ActivatedTurnPass` hands it to an `ActivatedTurnExecution` —
`ModelCallExecutionService` over the `ModelCallProvider` port — so each pass
activates and then drives the turn's model call. signalboxd depends on
`model-runtime`/`model-runtime-anthropic` through the `model-provider-runtime`
bridge; application and persistence still declare no runtime-crate dependency.
The same execution composition drives approval, tool attempts, and continuation
through the ports owned by [tool-loop](tool-loop.md).

## Startup scan and recovery

After configuration and database connection, signalboxd acquires the dedicated
single-daemon advisory guard specified by
[process-protocol](process-protocol.md). The registration-only startup order is
embedded migrations, the generic startup scan to completion, prior-process
runner connections marked lost, runner-socket bind, process-socket bind, then
concurrent runner enrollment, client request admission, outbox dispatch, and
scheduling. Runner admission cannot begin before the migration that creates
durable request receipts, the generic scan, or connection-loss classification.

**Committed unimplemented functionality.** No present surface performs retained
runner recovery. When recovery is implemented, startup must instead bind the
runner socket in recovery-only mode after migrations, reconcile retained runner
inventory, evidence, and nonterminal replacement commands, complete the generic
startup scan, bind the process socket, and only then enable ordinary runner
enrollment and scheduling. This compatibility constraint prevents generic
recovery from terminalizing authority that retained runner evidence resolves
(INV-034).

The runner recovery phase admits only `resume` for a recorded active or pending
identity and frames needed to reconcile its bounded inventory; it creates no new
enrollment or lease. For each active attempt owned by a durable claimed runner
lease, the phase ends only after retained terminal evidence commits, the local
journal proves execution had not started, or the exact connection passes the
fifteen-second loss bound and effect-class loss commits. An otherwise complete
inventory that omits the claimed lease is itself loss evidence, never permission
to repeat. A durable nonterminal `replace_lost_runner` command resumes its exact
provisioning authorization and receipt in the same phase. The generic startup
scan skips runner-owned attempts until this prior phase has resolved them, then
classifies only remaining daemon-owned tenure. With no retained runner work the
phase completes immediately.

`StartupScanService` reads the finite inventory of sessions with an active turn
(deterministic order), then runs one independent transaction per session under
the same scheduler-row lock ordering as every other lifecycle writer. Each
transaction reconstitutes the complete scheduling projection and classifies the
lost tenure by its durable model-call evidence — startup never fabricates a live
end (INV-034):

- an evidence-free turn (no model call) prepares
  `prepare_active_turn_lost_failure`: the current attempt ends
  `WithoutStop(Lost)` and the turn fails;
- a turn holding a `Prepared` model call proves that no send authorization
  existed. Startup validates its exact stored frontier and leaves the call,
  attempt, and turn unchanged for the ordinary scheduler to retry; and
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
  interrupt reason;
- a turn already parked in `awaiting_model_call_recovery` is not reclassified:
  its physical tenure ended in a prior process and its exact ambiguity set is
  already durable, so the transaction rolls back and reports the session as
  awaiting a recovery decision. The scan does not count it as recovered and does
  not block startup on it;
  `StartupScanOutcome::awaiting_recovery_decision_sessions` carries those
  sessions so the completed-phase log names each one instead of leaving the wait
  indistinguishable from a healed session. The scan that parks a turn itself —
  the in-flight branch below — reports its session the same way, so the wait is
  named on the restart that creates it and not only on a later one. The report
  is scoped to the model-call wait: `AwaitingRecoveryDecision` also carries a
  tool-attempt ambiguity set, which has no operator surface and stays classified
  as before, so a reported session always names a decision an operator can make;
- an approval wait remains parked unchanged, with no fabricated decision or live
  attempt; and
- a running tool attempt follows its stored effect class: prepared or
  effect-free work closes known-failed, appends the exact proposal-ordered
  result suffix plus `TurnFailed`, and fails the turn, while in-flight
  external-effect work closes ambiguous and parks on that exact attempt; and
- a running tool batch whose requests are all already resolved and which has no
  current tool attempt is returned as resumable work — its continuation turn
  attempt remains current, `Prepared` or `Running` ([tool-loop](tool-loop.md)) —
  so a scheduler pass projects its results and prepares the next call without
  relying on a lost local wake.

In the two failing branches only, one `TurnFailed` semantic entry is appended.
The evidence-free branch extends the starting frontier, and the
prepared/effect-free tool branch extends the yielded tool-use frontier by
exactly one correlated result entry per request in proposal order before the
failure marker. The turn terminalizes `Failed`, releasing the slot via one
guarded attempt-end update and one guarded lifecycle update, each required to
match exactly one row; and a `turn_failed` outbox record is appended in the same
transaction (entry payloads are
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
never defers or blocks startup. An `AfterCurrentTurn` input is already an
ordinary queued origin, not pending steering and not part of the active-turn
startup inventory. It survives restart unchanged. When startup terminalizes its
active predecessor and releases the slot, the ordinary eligibility pass selects
that durable queued origin by the same acceptance order it had before restart. A
persisted `StopRequested` attempt with its `CancellationRequested` call
reconstructs the exact proof, ends the abandoned attempt through
`AfterCancellation(Lost)`, and classifies the unobserved issued call as
ambiguous, terminalizes proof-bearing reconciliation, and releases the slot
without discarding stop intent. Identity collisions are retried with fresh
candidates; infrastructure and fail-closed corruption stop startup visibly. The
scan is idempotent — a rerun inventories only work still active, and a stale
observation rolls back as `NoActiveTurn`. There is no process-incarnation column
and no lease: under the single-daemon deployment contract, every nonterminal
attempt observed at startup is a prior-process abandonment (INV-010). The
advisory guard is acquired before this scan and held on its dedicated connection
for the complete process lifetime, so a second daemon cannot run the premise
concurrently.

## Occupied-slot input handling

Command construction, user-global deduplication, and acceptance atomicity are
[identity-and-commands](identity-and-commands.md) scope. The process protocol
exposes the existing delivery algebra as the closed `start_when_idle`, `steer`,
and `queue` intents, mapped respectively to `StartWhenNoActiveTurn`,
`NextSafePoint`, and `AfterCurrentTurn`;
[process-protocol](process-protocol.md#client-requests) owns the wire shapes.
The occupied-slot delivery outcomes implemented here are:

- `StartWhenNoActiveTurn` while a turn holds the slot records the typed
  rejection `ActiveTurnPresent`; an active-work mode against an idle slot
  records `NoActiveTurn`, and a stale `expected_active_turn` records
  `ActiveTurnMismatch`. Both are terminal recorded command results, replayed as
  such (INV-028).
- `NextSafePoint` records the input as `PendingSteering` with a
  configuration-free binding to the exact active source turn; its acceptance
  position derives from the validated session acceptance tail. No turn is
  created; the daemon returns a normal typed receipt carrying that input,
  position, and source turn. A reclassification path now exists: terminalization
  of the source turn reclassifies pending steering into a queued successor
  origin turn that inherits the source turn's configuration
  (`queued_input_origin.source_configuration_turn_id`). At the next model-call
  preparation, every pending input is consumed under the atomic boundary in
  [model-call-execution](model-call-execution.md) (INV-036).
- `AfterCurrentTurn` creates an ordinary queued origin turn with frozen
  configuration and an immutable acceptance position; it fixes no predecessor
  until eligibility. While the source turn holds the slot it cannot activate.
  After that turn terminalizes, multiple queued origins become eligible and
  activate in ascending acceptance order.
- `Interrupt` targeting the active turn atomically accepts a configured
  immediate-successor origin, constructs the exact `AppliedInterruptProof`, and
  applies the predecessor transition (INV-029, INV-037). The `stop_turn` request
  in [process-protocol](process-protocol.md#client-requests) is the client
  surface that submits this delivery; it adds no authority beyond the treatment
  specified here. Before any terminal transition releases the slot, the same
  transaction reclassifies every pending steering input against the interrupted
  turn as an ordered queued successor origin. Call, attempt, and turn
  terminalization follow
  [model-call-execution](model-call-execution.md#terminal-outcomes). A matching
  interrupt against `AwaitingRecoveryDecision` preserves the already terminal
  ambiguous call and ended attempt, records the new proof on the turn's
  reconciliation marker, and terminalizes `ReconciliationRequired` with the
  wait's exact operation set. The `reconcile_turn` request in
  [process-protocol](process-protocol.md) is the operator surface that supplies
  that interrupt for a model-call ambiguity wait, and the only one: it is
  admitted only for a turn the daemon observes parked in
  `awaiting_model_call_recovery`, so it never becomes the standalone active-turn
  cancellation the baseline excludes (INV-029). Because an ended attempt never
  returns to a live phase, an admitted wait can only remain parked or
  terminalize before the authoritative transaction runs; that transaction
  revalidates the exact expected active turn under the scheduler lock and
  records `ActiveTurnMismatch`, or `NoActiveTurn` when the winning decision left
  the slot empty, if a racing decision won. A next-safe-point request against a
  stopping turn records `SafePointUnavailableWhileStopping`; equal interrupt
  replay returns the original applied result. A distinct later interrupt records
  `InterruptAlreadyApplied { active_turn, existing_command }` without accepting
  an input or replacing the existing proof. An interrupt delivered while the
  active turn is parked on a tool-approval wait records
  `InterruptUnavailableWhileAwaitingApproval { active_turn }` without accepting
  an input: the wait remains parked until its canonical decision command
  resolves the approval obligation, and the interrupt is neither a denial nor a
  bypass of the decision command
  ([tool-loop](tool-loop.md#approval-policy-and-decision-sources) owns the
  deny-first caller protocol).

## Runner-loss session recovery

The heartbeat-loss transaction durably records `lost` for the exact current
connection epoch, and stale epochs cannot write after that commit. Transport
closure and protocol failure reach the same terminal connection state; clean
shutdown remains a distinct durable state.

The persistence adapter applies bounded per-session connection-loss propagation.
It marks a pinned placement `RunnerLost` or an unpinned placement whose
exact-identity selector names the lost runner `RunnerLostBeforePin { runner }`,
preserves exact tool-attempt ambiguity, and appends one durable runner state
event. An active turn already at a runner boundary moves to
`AwaitingRunnerRecovery`. A daemon-local model operation that was physically
authorized before loss retains its ordinary completion or ambiguity law; its
observation may complete the turn, but any returned runner-only proposal parks
before authorization because the frozen runner locus is now lost. No provider
call is repeated merely to project runner loss. A queued turn remains queued and
cannot activate while its placement is lost. An unpinned capability-class
request names no selected runner and is unaffected until a live registration can
satisfy it. Locking, page bounds, and crash recovery are owned by
[persistence-protocol](persistence-protocol.md). **Committed unimplemented
functionality.** No present daemon service pages pending losses or invokes that
adapter, and no runner execution surface yet depends on the projected state.

Only two user commands consume that state. `ReplaceLostRunner` requires the
expected current placement revision and either a different live exact runner,
the one pending replacement enrollment it atomically activates, or — for a
registration-triggered loss alone — a checked re-enrollment of the same runner
against its current connection
([runner protocol and placement](runner-protocol.md#identity-enrollment-and-registration)).
For a pinned loss, its transaction installs the checked successor placement and
grant lineage, provisions a new revisioned workspace when the successor request
requires one, appends the reference-only `RunnerPlacementChanged` semantic
entry, extends the next context frontier, and returns the turn to the phase
justified by its retained work. Safe retry proof may be consumed only inside
this command.

Replacement is never refused because a model call is in flight; it is staged
behind that call. The command claims its identity and provisioning authorization
immediately, while the terminal transaction that installs the successor
placement, appends the placement entry, and extends the next context frontier
commits only after any authorized in-flight daemon-local call for that session
has reached its observation boundary. That call's assistant and tool entries
therefore append first, from the frozen source frontier it was prepared against,
and the placement boundary appends after them: the prefix-only frontier law
holds, and the model never meets a placement event ordered ahead of output that
could not have observed it. A call that terminalizes known-failed, refused,
cancelled, or ambiguous reaches an observation boundary too, so staging cannot
wait indefinitely on a call that will never complete. Why: the two rules — a
call keeping its ordinary completion law and a replacement extending the next
context frontier — are jointly satisfiable in exactly this order, and the
persistence triggers that enforce prefix-only extension reject every other order
at commit, so an implementation that appends the boundary early fails loudly in
its own tests rather than writing a mis-ordered transcript. For
`RunnerLostBeforePin`, replacement installs the new exact selector and returns
the placement to `Unpinned` at the successor revision; it creates no semantic
boundary, workspace, grant, or lease. Workspace provisioning and the first pin
remain part of the eventual initial dispatch. `AbandonLostRunner` requires the
same exact lost revision and no active turn, then installs terminal
`RunnerAbandoned` placement state. If a turn is active it records
`ActiveTurnRequiresExistingControl`; the user first uses the existing
`stop_turn`, approval-decision, or reconciliation flow until the slot is empty,
so abandonment never mints cancellation authority. With no active turn,
including an idle session with queued turns, no turn or frontier is fabricated;
queued work remains queued and later runs with the daemon-only executable-tool
snapshot because the terminal placement can issue no runner lease. No case turns
ambiguous effect evidence into known failure.

Equal command replay returns the recorded receipt. Another revision, a live or
ordinary unpinned placement, the same runner outside the registration-triggered
recovery above, a stale connection, or an already replaced or abandoned session
fails closed. These commands are administrative recovery, not input delivery:
they neither widen `Interrupt` nor create a standalone cancellation path
(INV-026, INV-029, INV-037, INV-044).

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
from its complete resolved membership. The
[persistence protocol](persistence-protocol.md#relational-representation) owns
the header-plus-prefix-delta representation; a deferred constraint trigger
(`context_frontier_requires_complete_membership`) re-asserts complete contiguous
resolved membership — exact declared count, positions `1..count` — at commit.
Reconstitution rejects any stored snapshot whose resolved membership disagrees
with the complete entry set — one identifier can never resolve differently.
Before validating any stored turn start, the complete scheduling scan
reconstructs every dedicated compaction call, summary entry, source and result
snapshot, exact summarized range, and predecessor link. Every compaction record
requires its terminal `Completed` call and an exact source-plus-summary result;
the predecessor chain must be single-rooted, linear, and prefix-preserving. A
standalone `Completed` call or any standalone summary fails closed. A standalone
`Prepared` or `InFlight` call blocks ordinary activation until the finite
startup scan recovers its exactly correlated pending command: Prepared
terminalizes `KnownFailed`, InFlight terminalizes `Ambiguous`, the command
becomes failed, and neither branch creates a summary or result frontier. A
terminal non-completed call remains historical recovery evidence without a
summary or result. Live authorization and terminalization serialize on the
session row and exactly replay an already-landed transition after an ambiguous
commit. Unreferenced snapshots and compaction records fail closed.

The exact historical start law remains closed. Its prefix is either the
immediate predecessor's terminal snapshot (or the imported seed for the first
native turn), or the validated chain tip's result when the start follows that
compaction. A historical start committed before the chain tip remains admissible
only when its entire stored frontier is an exact semantic prefix of the tip's
source; this preserves old starts without authorizing a later turn to omit the
summary. In either shape, the only remaining suffix is the already-required
model-identity boundary, when applicable, followed by the turn's exact origin. A
summary entry is never accepted as an arbitrary extra suffix. New eligibility
uses the unique latest result when it preserves the applicable seed or
predecessor terminal prefix. In-memory append derivation structurally shares the
immutable ordered prefix, membership index, and lineage index; complete
iteration and comparison retain the same values and ordering. Imported ancestry
resolves only through the checked session-creation producer; its separate
one-to-one `ImportedSessionSeed` must name the exact stored frontier identity
whose membership matches the selected imported prefix. Substituting an
equal-content reminted identity for that seed fails reconstitution.
`SingleSource` ancestry resolution remains unimplemented. `TranscriptFrontier`
itself is [sessions-and-transcript](sessions-and-transcript.md) scope.

## Evidence-bearing reconstitution

Evidence validation is implemented for the scheduling seam: stored active phases
are conclusions derived from complete owner facts, never trusted discriminators.

- `AwaitingRecoveryDecision` reconstitutes from complete operation-owner facts:
  an `ambiguous` terminal model call or tool attempt correlated with its ended
  turn attempt (`ambiguous` from a live loss, `lost` from startup recovery). An
  ambiguous continuation call — prepared at the continuation boundary of a
  completed tool round and lost in flight — is admitted when its whole frontier
  is that round's batch-correlated result projection, proven by the round's
  durable result evidence; the wait extends it by no entry. A `StopRequested`
  current attempt reconstructs only when its stored interrupt command,
  predecessor, configured immediate successor, applied result, and
  cancellation-requested call form the exact proof. `AwaitingApproval`
  reconstructs only from the complete tool batch proving its earliest undecided
  request and absence of a live attempt; a bare wait subject cannot become a
  phase.
- A failed terminal turn that ended through a physical attempt durably names its
  exact ended attempt and optional terminal call
  (`turn_lifecycle.terminal_attempt_id`, `terminal_model_call_id`, backfilled
  and closed by migration `202607220003`). Reconstitution validates that
  provenance fail-closed through the typed
  `FailedTurnExecutionReconstitutionInput` — an ended `known_failure` or `lost`
  attempt, plus a correlated `known_failed`/`cancelled` call when one exists —
  instead of accepting an evidence-free failure record, and the deferred
  `assert_failed_terminal_execution_final_state` assertion re-closes the shape
  at every commit. When the failure closed a tool round, the same input names
  the complete user-sourced denial resolutions backing every `ToolDenied` result
  entry in the terminal suffix; a `ToolDenied` entry whose request lacks an
  exact `Deny` resolution — including a missing or approving decision — fails
  reconstitution rather than fabricating a user denial. A failed turn may
  instead name the round's own continuation call — created at the continuation
  boundary and later lost or known-failed. Reconstitution accepts that call
  exactly when its whole frontier is the completed round's batch-correlated
  result projection, extended by any steering it consumed, and the terminal
  frontier extends the call frontier by the failure marker alone; a
  round-completed continuation window never contains a turn-end closure.
- A cancelled terminal turn reconstructs only from
  `CancelledTurnExecutionReconstitutionInput`: its exact ended attempt carries
  `AfterCancellation(Cancelled)` and the same complete applied-interrupt result
  as the turn disposition. It names either no call, proving direct cancellation
  before any call was prepared, or its one correlated terminal `cancelled` call.
  Its terminal frontier must extend the starting or call frontier by exactly the
  correlated `TurnCancelled` marker. When the cancellation terminalized a tool
  round, the input instead names the batch's `completed` producing call, and the
  terminal frontier extends that call's yielded frontier by exactly one
  batch-correlated result entry per request in proposal order before the
  correlated `TurnCancelled` marker. Each `ToolDenied` entry in that suffix is
  batch-correlated only against a named user-sourced `Deny` resolution for its
  exact request; a missing or approving decision fails reconstitution. A
  cancelled continuation call — prepared at the continuation boundary and
  interrupted before or during send — is admitted on the same terms as the
  failed form: its whole frontier is the completed round's batch-correlated
  result projection extended by any steering it consumed, and the terminal
  frontier extends it by the cancellation marker alone.
- A refused terminal turn names its exact ended attempt and correlated terminal
  `refused` call, and its terminal frontier is an equal-content boundary over
  that call's frontier. A refused continuation call — prepared at the
  continuation boundary of a completed tool round — is admitted when its whole
  frontier is that round's batch-correlated result projection, proven by the
  round's durable result evidence; a refusal appends no semantic content, and a
  round-completed continuation window never contains a turn-end closure.
- A reconciliation-required terminal turn names its exact ended turn attempt and
  exactly one required terminal `ambiguous` model call or tool attempt. The
  attempt end is either `WithoutStop(Ambiguous|Lost)` with a later
  turn-correlated applied interrupt, or `AfterCancellation(Ambiguous|Lost)`
  carrying that same proof. A model-call reconciliation terminal frontier is an
  equal-content boundary over the ambiguous call's source frontier — for an
  interrupted continuation call, the completed round's batch-correlated result
  projection, proven by the round's durable result evidence. A tool-attempt
  reconciliation terminal frontier extends the producing call's yielded frontier
  by exactly one batch-correlated result entry per request in proposal order,
  with the ambiguous request represented as `ToolClosed`. The checked scheduling
  input validates those correlations before the turn can serve as a terminal
  predecessor.
- A consumed steering input reconstitutes only against its exact consuming call,
  whose frontier is the turn's starting frontier — or, for a call prepared at a
  tool-round continuation boundary, the round's batch-correlated result
  projection, proven by the round's durable result evidence — extended by the
  consumed steering entries in acceptance order. The same evidence is what
  admits the durable continuation pair of a `Running` continuation attempt
  owning a `Prepared` steering-consuming call ([tool-loop](tool-loop.md) owns
  the continuation transaction that commits it). A consumer that completed by
  proposing a tool round stays correlated through its validated assistant
  history for the rest of the turn — later safe points, parked waits, and every
  terminal shape included — rather than through the current phase's attempt or
  the turn's terminal call.
- Every active turn's projection must carry a session-scoped acceptance tail
  anchored at the turn's exact origin and extending gap-free through the
  observed last acceptance position, with unique identities, same- session
  membership, and per-entry delivery/disposition correlation. When an origin was
  queued before a later input was consumed by the then-active predecessor, that
  predecessor-consumed position remains in the complete tail after the queued
  origin activates; only steering consumed by the new active turn enters its
  execution aggregate. A filtered pending-steering list or bare maximum cannot
  substitute (INV-007, INV-016).
- A tail entry recording an accepted interrupt against the active turn is
  admitted only when the current stop/recovery state carries its exact
  `AppliedInterruptProof`; an evidence-free active phase rejects it as
  `ActivePhaseEvidenceMismatch`.

Why fail-closed: an omission inside a claimed complete observation is
indistinguishable from acknowledged work disappearing, so the seam rejects
rather than repairs, and no effect is authorized from a failed reconstruction
(general reconstitution boundary:
[persistence-protocol](persistence-protocol.md)).

## Daemon runtime: startup order and shutdown

signalboxd is the composition root. It reads the six unconditionally required
values—`DATABASE_URL`, `SIGNALBOX_CONFIG_FILE` (the model-configuration TOML
naming provider targets, selections, and aliases),
`SIGNALBOX_TEMPLATE_CONFIG_FILE`, `BRAVE_API_KEY_FILE`, `GITHUB_TOKEN_FILE`, and
`SIGNALBOX_SOCKET_PATH`—from the process environment, plus the optional
`SIGNALBOX_RUNNER_SOCKET_PATH` override and `HOME` as specified below. A
model-provider credential path is not among them: every `file` profile carries
its own path, and composition builds `FileCredentialAccess` from the complete
profile map, as specified by
[configuration and credentials](configuration-and-credentials.md#process-configuration).
The configuration page owns these provisional channels. It validates the model
catalog, then resolves the template catalog and all of its prompt files against
that model catalog, before connecting. It then acquires the single-daemon guard,
fences the prior pool incarnation, migrates, and completes the generic recovery
scan. It then initializes every configured blob store against its recorded
namespace binding, verifies every currently routed S3 namespace marker and
multipart lifecycle rule under the blob contract's aggregate startup deadline,
resolves the one-time imported display-title backfill
([conversation-import](conversation-import.md#derived-display-titles)), and
marks every prior-process nonterminal runner connection lost. Blob
initialization failure stops startup before the backfill, either socket binding,
or scheduling; unrouted historical S3 bindings retain their lazy runtime check.
It then — once the credential-pool child is composed — establishes each
`codex_home` profile's credential-home identity, resolves every prior-process
capacity reservation, resolves every retained OAuth refresh-in-progress marker
to a replacement token or a quarantine, scavenges every crash-left OAuth scratch
home, and runs the legacy family-to-policy backfill
([configuration and credentials](configuration-and-credentials.md#credential-deliveries)).
Those five gates sit after the recovery scan so a failure cannot block recovery
of acknowledged work, and before any socket binding or scheduling so no request
reaches a historical session whose policy is not yet rewritten and no CLI call
runs against an unestablished credential home. The two OAuth gates are here for
the same reason and are not optional cleanup. A retained marker names a
single-flight refresh whose owning daemon is gone, so a preparation that
admitted work first would either join a flight that no longer exists or reuse a
generation the provider may already have rotated, which the pinned client treats
as permanent failure. A crash-left scratch home holds real tokens on disk under
a directory nothing is now watching, so scavenging it before admitting work is
what bounds how long those tokens outlive the process that minted them. No
present composition performs any of the five. It then binds the runner socket,
binds the process socket, then concurrently admits runner enrollment and
protocol requests, dispatches the outbox, and schedules eligible work. On a
database without the fence migration, the guarded first migration creates the
fence row before the daemon initializes its first fenced pool. No request,
dispatch cursor advance, or scheduler pass occurs before recovery completes. Any
phase failure is a failed startup with a classified, key-bearing log line and a
failure exit code. Runner recovery-only binding and reconciliation remain the
committed unimplemented ordering stated under
[startup scan and recovery](#startup-scan-and-recovery).

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
pool sessions have ended, removes only this daemon's identity-pinned and
revalidated socket, and releases the advisory locks by closing its dedicated
guard connection. Window expiry abandons remaining tasks, warns, and skips the
unbounded pool drain; process exit releases its sessions. Why signal-driven
shutdown is polish, not correctness: abrupt exit at any point is safe because
durable rows plus the next guarded startup scan recover work and the durable
outbox cursor redelivers an uncommitted offer (INV-032, INV-034), so the grace
window buys only latency. Repositories and services are cheap per-invocation
clones over the shared pool; no shared locked service instance exists.

## Delegated waits, messages, and wake turns

This section is the session-delegation foundation proposal and becomes verified
only with its implementing child pull requests. A spawned child's first turn has
a closed delegated-task origin naming the exact spawning request; its starting
frontier contains the checked `DelegatedTask` entry and contains no synthetic
accepted input. Every later turn uses the ordinary accepted-input or
delegation-wake origin appropriate to the work that queued it.

`AwaitingChild { wait }` is an active phase with no current attempt. Its wait
names the exact foreground `await_session` request, spawning request, and child;
it retains the parent's sole progressing-turn slot, and survives restart
unchanged until that relationship has one deliverable terminal result or an
applied interrupt terminates the parent turn. Result consumption atomically
appends the parent's delivered-result entry, returns the result as that tool
request's content, and moves the same turn back to running with a fresh
continuation attempt. A parent-only interrupt instead appends ordinary
`ToolClosed` evidence for the await request and terminalizes the parent without
fabricating a child result or a live turn attempt. A descendant-scoped interrupt
materializes its complete cascade first, so a resulting child outcome is
delivered before the parent closes. The scheduler routes a durable result hint
back through the tool loop: the exact parked batch is reconstituted first, then
the fresh attempt and lifecycle transition commit before ordinary tool
continuation runs. A raw child identity or an unrelated result cannot release
the wait.

A delegated-task or delegation-wake turn owns the same session-local active slot
as an accepted-input turn. Input submission therefore uses that exact turn for
active-turn mismatch, vacant-slot rejection, safe-point steering, and interrupt
predecessor checks even though delegated origins do not fabricate an
accepted-input scheduling row.

A background await instead commits a delivery registration and returns its
receipt immediately. It does not create `AwaitingChild` or retain the slot; the
current turn may continue and end normally. Child result commit appends one
parent-scoped `DelegationWake` event and makes the parent eligible for a new
delegation-origin turn. A missing in-process nudge changes only latency: the
eligibility sweep includes both a foreground wait with a deliverable result and
an undelivered background result.

Pending inter-session messages use the same safety pattern. An active recipient
consumes its FIFO inbox at the next model-call safe point. Pending messages and
background results are ordered by their shared, gap-free recipient delivery
sequence, not by relationship-local ordinals. An idle recipient has at most one
queued delegation-origin wake turn; later items coalesce into that turn's
starting frontier until activation. Reconstitution checks delivery sequence,
message ordinal where present, relationship, sender/recipient, and
semantic-entry identity before any content becomes model-visible.

Parent termination evaluates descendants only when its explicit scope is
`ParentAndDescendants`. The transaction locks the relationship frontier before
applying each edge's background/bound policy and records one typed disposition
per evaluated edge. `ParentAlone`, background, and bound `KeepRunning` never
fabricate child stop authority. Parent-driven stop/cancel outcomes carry their
exact spawn request plus opaque authority from the applied parent termination; a
turn command names its exact turn, while a goal-stop command names its exact
goal generation and carries no turn. Raw identities cannot construct either
source. Each edge verifies that authority's parent, command kind, and descendant
scope against its typed outcome before applying the policy. Partial or
unrecorded propagation does not commit. Equal reevaluation of the same edge by
the same command returns the recorded disposition without appending another. An
edge whose child already has a terminal result records an `AlreadyTerminal`
disposition for the evaluating command, creates no second terminal result, and
still traverses the child's outgoing relationships. Child-originated
cancellation instead carries the child's exact proof-bearing cancelled turn. A
reconciliation-required turn supplies no terminal child outcome, and the same
cancelled-turn evidence cannot be selected as a stopped outcome. Detached child
work stays independently schedulable after the parent's turn or goal has
terminalized.

**SPEC PROPOSAL — immediate descendant terminalization.** A bound `Stop` or
`Cancel` policy action commits an authoritative logical terminal proof for the
child in the same transaction as the parent command, relationship disposition,
delivered child result, and wake. That proof carries the exact parent-command
authority and is terminal even when a provider or tool operation was already
physically in flight. The child is no longer scheduler work: an immutable
one-to-one lifecycle projection releases its active or queued index slot, while
the retained turn reads as the typed parent-terminated outcome and remains the
immediate terminal predecessor of later independent work. Its retained terminal
frontier preserves that execution lineage for accepted-input and delegation-wake
activation. Physical cancellation latency cannot revive it, replace its result,
or change the typed stop/cancel provenance. A background or bound `KeepRunning`
edge has no such proof and remains schedulable. This proposal is accepted with
the implementing stack's merge.

Startup and the periodic sweep recognize child waits, pending delegation inbox
content, and undelivered results from durable rows. They neither infer a result
from child transcript state nor depend on process-local wake memory.

## Open edges

- Direct fatal terminalization has sealed domain derivation values
  (`fatal_mismatch` module) but no aggregate transition or commit path.
- Dispatch fencing covers model calls, daemon tools, and local runner leases;
  remote runner transport and result envelopes remain deferred.
- Loss replacement is the only version-one producer of a placement change.
  User-directed relocation of a healthy session, and a working-directory move on
  the same runner, are committed functionality with no command here yet; the
  placement-revision, transcript-boundary, and runner-event mechanisms this page
  drives must stay compatible with a relocation that no loss caused
  ([runner protocol and placement](runner-protocol.md#committed-functionality-beyond-version-one)).
- The eligible terminal-failure path (queued turn fixes its start and fails
  without an attempt for a structurally unexecutable configuration) is
  unimplemented; activation is the only eligibility outcome.
- Native `SingleSource` ancestry remains unschedulable
  (`UnsupportedSessionAncestry`); selecting and resolving native fork boundaries
  is unimplemented. Imported-conversation ancestry has its own exact
  selected-prefix frontier path and does not close that fork question.
- Continuation safe points after tool results consume pending steering through
  the atomic boundary in [tool-loop](tool-loop.md).
- Startup recovery now classifies model-call evidence (a `Prepared` call closes
  as a known failure; an unstopped in-flight call parks the turn as ambiguous in
  `awaiting_model_call_recovery`), tool-loop evidence, and delegated-result
  waits. A user reconciliation decision is the only implemented resolution for
  that park: no automatic resolution exists, because the terminal disposition it
  produces is proof-bearing and the durable evidence supplies no authority to
  construct. Resolving the ambiguity itself from provider evidence remains an
  [open question](../open-questions.md#turn-lifecycle); the tool-attempt
  ambiguity wait keeps its own operator surface deferred with that question.
- Per-session scan gating, sweep interval, and fairness tuning remain
  operational open questions; the process-wide advisory singleton guard is
  specified by [process-protocol](process-protocol.md).
- LISTEN/NOTIFY remains the documented multi-process extension only; the
  baseline is single-process nudge plus sweep.

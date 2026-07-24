# Model-call execution

This page describes the implemented model-call orchestration chain as verified
against the implementing stack rooted at PR #193 (`agent/tool-loop-spec`):
rendering a context frontier into provider messages, the staged prepare /
authorize-send / commit-observation effects, assistant content, intra-turn tool
rounds and final turn completion, provider failure classification into physical
dispositions, and the retry prohibition. Tool requests, approvals, attempts, and
continuation are owned by [tool-loop](tool-loop.md). Turn and attempt lifecycle
law lives in [turn-lifecycle-and-scheduling](turn-lifecycle-and-scheduling.md);
semantic entries and frontiers in
[sessions-and-transcript](sessions-and-transcript.md); storage protocol and the
outbox in [persistence-protocol](persistence-protocol.md); the typed
model-runtime layer and hub runtime in
[runtime-substrate](runtime-substrate.md); model configuration and credentials
in [configuration-and-credentials](configuration-and-credentials.md). Invariant
tags cite [docs/invariants.md](../invariants.md).

## Call records and lifecycle

A model call is one durable hub authorization to attempt a provider interaction
(INV-014). Its record (`crates/domain/src/model_call.rs`) fixes at creation:
`ModelCallId`, owning turn and attempt, the exact frozen model selection, the
turn-pinned resolved target, and the exact ordered context frontier it consumes
(INV-015). Nonterminal states are `Prepared`, `InFlight`, and
`CancellationRequested`; terminal history is a separate `EndedModelCall`
carrying one of five physical dispositions — `Completed`, `KnownFailed`,
`Refused`, `Cancelled`, `Ambiguous` — and exposes no transition back (INV-006).

The predecessor matrix:

- `Prepared -> InFlight` is the only send authorization.
- `Prepared` classifies terminally only as `KnownFailed`; ending an unsent call
  as `Cancelled` requires the exact applied-interrupt proof for the call's own
  turn (INV-029). An unsent call cannot complete, refuse, or become ambiguous.
- `InFlight` and `CancellationRequested` accept every disposition.
- Terminal state never reopens. Why: terminal physical history is the record of
  what was externally done, and rewriting it would let later facts silently
  change that record.

Storage enforces the matrix durably
(`crates/persistence/migrations/202607220001_model_call_execution.sql`): the
`model_call_changes_are_guarded` trigger rejects any insert whose state is not
`Prepared`, any mutation of the eleven authorization-fact columns (the pinned
credential reference joined them in
`202607220002_model_call_credential_reference.sql`), any non-monotonic
transition, any rewrite of a terminal row, any unsent-terminal disposition other
than `KnownFailed`/`Cancelled`, and any delete; `model_call_pinned_target_fk`
forces every call row's resolved target to equal the turn's pinned target. Why:
the schema backstops the aggregate against any buggy or racing writer, not just
the audited one.

The provider target is pinned as a turn-level fact before any call exists: the
turn's frozen selection resolves through an immutable configured
`ModelTargetCatalog` to one exact `ResolvedProviderTarget`, and every call in
the turn must use it (the pin FK above is the durable form of "must").
Resolution failure pins nothing, creates no call, and atomically fails the
attempt and turn. Why: pinning before the first `ModelCallId` prevents a mutable
alias or deployment change from being smuggled into a turn as recovery.

## Aggregate and reconstitution

`ModelCallExecution` (`crates/domain/src/model_execution.rs`) is the
purpose-specific aggregate: one active accepted-input turn in its `Running`
phase plus the one call owned by its current turn attempt. Earlier rounds remain
durable transcript/request/result history. Reconstitution is fail-closed: it
rejects a non-running phase, session or snapshot mismatches, frontier entries
that do not exactly back ordered membership, missing or unreferenced origin
content, a call whose turn/attempt/frontier/selection/target contradict the
checked turn facts, more than one call, and any attempt/call state pair outside
`(Prepared, none)`, `(Prepared, Prepared)`, `(Running, Prepared)`,
`(Running, InFlight)`, or the proof-bearing
`(StopRequested, CancellationRequested)` pair. `(Running, Prepared)` is admitted
only for a continuation attempt whose exact stored call frontier includes the
current tool round's complete result evidence. The stopped pair must reconstruct
the exact applied-interrupt proof retained by the attempt before it can
authorize cancellation observation or restart recovery. Why: acting on a
partially consistent projection could authorize a second provider effect against
stale authority, so every invalid shape refuses rather than repairs. Sealed
constructors (compile-fail-tested) prevent forging call records or terminal
history outside the aggregate (INV-002).

Reconstituting a checkpointed `Prepared` call also reloads the call's exact
stored snapshot, not only the turn's starting snapshot. When steering extended
that frontier, the complete acceptance tail must reconstruct every consumed
input in acceptance order; the corresponding suffix must contain exactly one
`SteeringAcceptedInput` entry per receipt, correlated to the input, source turn,
and current call, and every referenced input's checked content must be present.
The call becomes resumable only when that extended snapshot is a strict
prefix-preserving extension of the starting snapshot and its complete ordered
membership equals those checked semantic entries. Why: checkpointing cannot
erase steering that the durable call was prepared to observe.

Scheduling projection reconstitution independently reloads every consumed
input's stored session, lifecycle, acceptance position, source turn, and
consuming call. Each fact must have exactly one matching
`SteeringAcceptedInput`; the call must belong to that source turn and lifecycle,
and its snapshot must equal the turn's starting snapshot plus the complete
acceptance-ordered steering suffix. Terminal response-frontier validation uses
that checked call snapshot as its prefix. Why: every adapter reaching the domain
seam must reject cross-wired steering history, even when its storage schema has
already performed the same correlation.

## Frontier rendering

`PreparedModelOperation::render` (`crates/application/src/model_execution.rs`)
projects the exact frontier order into provider-neutral messages:

- `OriginAcceptedInput` renders as a user message with its checked accepted
  input content;
- `SteeringAcceptedInput` renders as a user message with the referenced accepted
  input's checked content;
- `AssistantText` renders as an assistant message retaining its producing-call
  provenance;
- `TurnFailed`, `TurnCompleted`, and `TurnCancelled` markers are skipped — they
  delimit history and carry no model-visible content;
- `AssistantToolUse` and its proposal-ordered result entries render as paired
  assistant tool calls and user tool results after resolving their referenced
  request, attempt, and decision records through [tool-loop](tool-loop.md).

The model operation also carries the current registry declarations. The runtime
bridge maps them to provider tool definitions and accepts `ToolCall` completion
parts only with a matching `ToolUse` finish reason. Provider-native tool types
remain inside the bridge.

Every message keeps its source-qualified semantic-entry reference. Why:
inherited entries need not come from a native turn in the current session, so
role and provenance derive from the entry itself, never from turn grouping. The
runtime bridge then maps these to provider wire messages; provider types never
cross the application boundary (INV-002; layering rules in
[runtime-substrate](runtime-substrate.md)).

## Staged execution

`ModelCallExecutionService::execute` runs one linear invocation over five
composed roles (prepare, capability, authorize-send, provider,
commit-observation) plus an id generator and a dispatch gate. No database
transaction is ever open across credential I/O or provider work.

The two off-transaction provider roles share one call-scoped
`CancellationSignal`. It resolves when an authoritative reload finds the exact
call `CancellationRequested` or terminal: direct cancellation of a prepared call
therefore releases blocked capability preparation, while issued-call
cancellation reaches provider invocation. Capability preparation reports this
signal as `Cancelled`, and the application returns `NoWork`; it never converts
authoritative cancellation into the guarded known-failure closure for a call
that may already be terminal (INV-037).

1. **Prepare transaction.** Locks the session, reconstitutes the aggregate, and
   either: reports no runnable work; creates and commits the exact `Prepared`
   call with its pinned non-secret credential reference
   ([configuration-and-credentials](configuration-and-credentials.md)), the
   turn-target pin, and a `ModelCallTransition` (`Prepared`) outbox event. If
   pending steering exists, the same transaction first consumes every eligible
   input in ascending acceptance position, appends its correlated semantic
   entry, and derives the exact extended frontier supplied to the call. It then
   stops the invocation (`Checkpointed`); reloads an already-committed
   `Prepared` call read-only and returns its request material (`Ready`); closes
   target-resolution failure as an atomic no-call attempt-and-turn failure. A
   new `Prepared` call is never advanced to `InFlight` in its creating
   transaction. Why: committing durable call identity before any external step
   means a crash can never produce a provider effect with nothing durable to
   classify.
2. **Capability preparation (no transaction).** The provider adapter resolves
   its credential internally from the call's durably pinned reference (reloading
   a `Prepared` call without one fails closed) and builds an opaque, one-shot,
   call-bound send capability; application and domain code only move the value
   and cannot inspect, persist, or log it (INV-035;
   [configuration-and-credentials](configuration-and-credentials.md)). Why: a
   nonserializable one-shot value makes credential escape and capability reuse
   structurally impossible rather than a review convention. Preparation races
   the shared cancellation signal above. A trustworthy ordinary failure here
   commits the accepted `Prepared -> KnownFailed` closure with attempt and turn
   failure in a separate guarded transaction; an adapter defect is an operator
   failure and commits no provider-failure closure.
3. **Authorize-send transaction.** After acquiring the process-shared
   per-attempt dispatch gate, a distinct transaction reloads authority and
   commits `Prepared -> InFlight`; an initial attempt moves
   `Prepared -> Running`, while a tool-continuation attempt already entered
   `Running` while executing its tool batch and remains there. The same
   transaction appends a `ModelCallTransition` (`InFlight`) outbox event — every
   durable physical transition, not just the terminal one, is externally
   observable atomically with its commit. The gate permit is retained into the
   send and released at the runtime's first report that provider acceptance is
   possible (`SendCommenced`); if no acceptance report ever arrives, it is
   released when the provider interaction returns, and the
   ambiguous-authorization reread paths drop it before returning. Why: holding
   the gate across the authorize commit and send start serializes
   execution-service passes for that attempt across the acceptance-capable
   boundary; it does not serialize interrupt application.
4. **Provider interaction (no transaction).** The provider port is invoked at
   most once per invocation, and exactly once only after the `InFlight` commit
   is known. It consumes the capability exactly once and returns one
   provider-neutral terminal observation bound to the sealed issued correlation
   (session, turn, attempt, call, target, frontier). Its runtime
   `CancellationSignal` is the shared durable signal defined above.
5. **Commit-observation transaction.** A fresh transaction reloads and
   revalidates complete authority — it never trusts the pre-send projection —
   checks the observation's correlation against fresh state, and atomically
   commits the call disposition, attempt and turn transitions, semantic entries,
   terminal frontier, and outbox rows.

Failure keeps its stage: `ModelCallExecutionError` names which of prepare,
render, capability, capability-failure commit, capability-failure reread,
authorization, authorization reread, authorization reconciliation, provider, or
observation commit failed.

### Identity minting and commit ambiguity

The application owns all candidate identity minting (UUIDv7); persistence uses
or discards candidates but never mints its own. Fixed-count call, entry, and
frontier candidates are minted immediately before each port call. Inventories
knowable only under an authoritative lock use application-owned generator
closures: initial preparation draws one steering semantic-entry candidate and
one fallback reclassified-successor candidate per pending input, while terminal
closure and startup recovery draw one reclassified-successor candidate per
pending input. Persistence invokes those closures inside the transaction but
never owns minting. Why: the locked pending count moves into the transaction
without moving identity authority into persistence. A proven hub-minted identity
collision (unique-violation rollback on the call, entry, frontier, or
reclassified-turn key) is the only same-invocation transaction retry, with fresh
candidates and no repeated credential or provider work. Why: a proven
unique-violation rollback is the one failure that guarantees the transaction had
no effect, so retrying it cannot duplicate anything.

Commit ambiguity has an explicit detection rule (`commit_failure_is_ambiguous`,
`crates/persistence/src/model_execution.rs`): a database error with SQLSTATE
08007 or 40003, or any non-database error while awaiting `COMMIT`, is ambiguous;
a server-rejected commit is a plain non-ambiguous failure; and a unique
violation surfacing only at `COMMIT` (the identity constraints are deferred) is
still classified as an identity collision and retried.

Ambiguous commits are never resolved by replay:

- An ambiguous prepare-stage commit fails the invocation; authoritative state
  must be reread before any later action.
- An ambiguous authorize-send commit triggers a read-only reread: if the call is
  still `Prepared`, the capability and permit are discarded and the error
  returned; if `InFlight` committed, the unconsumed capability is proof of
  non-send, and the service commits a `KnownFailed` observation for the issued
  call without ever sending; if an interrupt concurrently committed
  `CancellationRequested`, the same unconsumed capability proves no send, the
  stop remains authoritative, and the service commits the correlated `Cancelled`
  observation instead; if the interrupt already terminalized the unsent call as
  `Cancelled`, the complete proof-bearing closure is authoritative and the
  service returns `NoWork`.
- A failed terminal-observation commit retains the unchanged observation in
  memory. A later pass rereads durable state first: `Pending` recommits the
  identical observation; `AlreadyCommitted` (same disposition and content)
  discards it. Any drift in correlation or content is rejected.

### One call, one physical interaction

Per durable authorization, at most one physical interaction may reach the
provider-acceptance boundary. Storage backstops single-call-ness independently
of the aggregate: `model_call_attempt_once UNIQUE (turn_attempt_id)` admits at
most one call row per attempt against any buggy or racing writer. There is no
automatic retry after a known failure and no automatic retry of an ambiguous
outcome (INV-025, INV-026); a known failure fails the attempt and turn, and
ambiguity parks the turn for recovery. A later scheduler pass never treats an
issued unclassified call as fresh authorization. Why: a lost acknowledgement
cannot prove the provider did not act, so repetition risks undisclosed duplicate
provider effects and spend; honest ambiguity is preferred to an invented
exactly-once claim.

## Provider observation classification

Classification is an adapter contract consuming the full-request-send boundary
([runtime-substrate](runtime-substrate.md)); the hub never reinterprets SDK
errors by retryability or exception type. The runtime bridge
(`crates/model-provider-runtime/src/lib.rs`) maps the runtime's typed terminal
evidence ([runtime-substrate](runtime-substrate.md) owns how evidence is
derived) to exactly one disposition:

| Terminal evidence                                                            | Disposition   |
| ---------------------------------------------------------------------------- | ------------- |
| `Completed` (supported ordered assistant content)                            | `Completed`   |
| `Refused`                                                                    | `Refused`     |
| `ProviderError` (any kind, incl. rate limit, credential rejection, overload) | `KnownFailed` |
| `ProvenUnsent(CancelledBeforeSend)`                                          | `Cancelled`   |
| other `ProvenUnsent` (proof of no acceptance)                                | `KnownFailed` |
| `CancellationConfirmed`                                                      | `Cancelled`   |
| `BoundaryLoss` (loss after possible acceptance, incl. timeouts)              | `Ambiguous`   |

The bridge maps `Refused` evidence unconditionally; that such evidence arises
only from an authenticated complete exchange is the runtime layer's contract
([runtime-substrate](runtime-substrate.md)), not rechecked here. Empty text
blocks are dropped without creating invalid entries. Tool-call parts with a
`ToolUse` finish become the normalized proposals owned by
[tool-loop](tool-loop.md); thinking or redacted-thinking still fail the adapter
stage closed because no durable semantic representation exists. A
provider-reported model differing from the expected exact spelling — in early
observations or terminal evidence — also fails the adapter stage closed rather
than classifying, because provider-target mismatch evidence is not yet
representable durably (see Open edges). Scripted providers declare their exact
terminal observation; nothing is inferred from timing or injected I/O errors.

## Terminal outcomes

`apply_terminal_observation` derives one of six outcomes from fresh state, and
persistence commits it atomically with its outbox rows
([persistence-protocol](persistence-protocol.md)):

- **Completed without tools.** The call ends `Completed`; the attempt ends
  `TurnCompleted`; ordered assistant text is followed by `TurnCompleted`, and
  the turn terminalizes through the existing final-response all-or-nothing
  boundary.
- **Completed with tools.** The call ends `Completed`; ordered assistant text
  and logical tool-use entries plus their request records commit atomically, the
  attempt ends as a tool-round yield, and the turn stays active. Approval,
  execution, result projection, and preparation of the next call follow
  [tool-loop](tool-loop.md). A physical call completion is therefore never
  treated alone as proof that the logical turn completed.
- **KnownFailed.** The call ends `KnownFailed`; an unstopped attempt ends
  `KnownFailure`, and the turn fails with a `TurnFailed` entry and terminal
  frontier. A stop-requested attempt instead ends
  `AfterCancellation(KnownFailure)` and still fails; the physical result has not
  proven cancellation.
- **Cancelled.** Without the exact applied-interrupt proof, a physical
  cancellation is an unstopped known failure. With the exact proof — carried
  directly by the atomic interrupt transition before any call exists or for an
  unsent `Prepared` call, or retained by `StopRequested` for an issued call —
  the attempt ends `AfterCancellation(Cancelled)`, one `TurnCancelled` marker
  extends the starting or call frontier, and the turn ends `Cancelled { cause }`
  rather than failed or ambiguous.
- **Refused.** The call ends `Refused`; the attempt ends `TurnRefused`; the turn
  terminalizes `Refused` atomically with an equal-content terminal frontier. No
  refusal-content entry exists yet (INV-018; open edge).
- **Ambiguous.** The call ends terminally `Ambiguous`; an unstopped attempt ends
  `Ambiguous` (live) or `Lost` (startup), and the turn enters the durable
  `awaiting_model_call_recovery` phase carrying the exact wait set (that one
  call) while retaining the session slot. No semantic entry or frontier is
  created.
- **ReconciliationRequired.** When that same unacknowledged ambiguity has an
  applied-interrupt proof, the attempt instead ends
  `AfterCancellation(Ambiguous)` (live) or `AfterCancellation(Lost)` (startup).
  The turn terminalizes with the exact model-call wait set and
  `InterruptRequiresReconciliation` marker, an equal-content terminal frontier,
  and a typed reconciliation outbox record, releasing the slot. The same result
  applies when an interrupt is accepted after an unstopped ambiguity already
  entered `AwaitingRecoveryDecision`: the terminal call remains unchanged, and
  its ended attempt remains the original `WithoutStop(Ambiguous|Lost)` evidence.
  The exact later interrupt proof is carried by the turn's reconciliation marker
  and correlated accepted successor instead of rewriting that evidence.

Completion and refusal races against `StopRequested` end through their typed
`AfterCancellation` dispositions while retaining their ordinary turn outcomes.

Every terminal turn outcome, including proof-bearing reconciliation, atomically
reclassifies each pending steering input into a fresh queued successor turn at
its original acceptance position (`NoSafePointBeforeTerminal`), inheriting the
source turn's effective configuration; see
[turn-lifecycle-and-scheduling](turn-lifecycle-and-scheduling.md) (INV-016).

## Serialization and locking

Every model-call transaction — prepare, capability-failure closure,
authorize-send, observation commit, both rereads, and startup recovery — issues
the per-session `session_scheduler` row lock (`FOR UPDATE`,
`crates/persistence/src/lock_inventory.rs`) as its first statement. The reviewed
lock statement bundles a session-existence probe (startup's variant also reads
the active turn) into the same SELECT, so lock-before-read is guaranteed at
statement granularity, not within the statement. Why: one lock statement issued
first in every transaction makes per-session serialization total and lock-order
cycles impossible. The in-process per-attempt dispatch gate is the only other
ordering primitive; in this slice the execution service is its sole consumer.
Interrupt application deliberately does not acquire it: once `InFlight` commits,
the call is issued work, so a later interrupt durably requests cancellation and
the runtime signal races any provider progress without claiming that acceptance
was prevented.

## Crash, restart, and supervision

hubd (`apps/hubd/src/lib.rs`, `main.rs`) wraps execution in
`FatalExecutionSupervisor`: a post-activation stage failure — after at most one
same-incarnation reconciliation pass when retained evidence exists — raises a
fatal signal, the scheduler stops (in-flight work bounded by a shutdown grace
window), and the process exits nonzero so the next incarnation's startup scan
regains authority. Why: startup recovery is the one audited path that classifies
an issued call from durable evidence, so a live process that cannot construct a
trustworthy result must stop rather than improvise.

Startup recovery (`crates/persistence/src/startup.rs`), inside the same
per-session locked transaction as the general scan (INV-034):

- an evidence-free turn ends its abandoned attempt `Lost`, fails the turn, and
  reclassifies all pending steering instead of deferring startup;
- a durable `Prepared` call proves no send authorization existed; the call ends
  `KnownFailed`, the abandoned attempt ends `Lost`, and the turn fails,
  reclassifying pending steering. Before closure, reconstitution validates the
  call's exact stored frontier; when preparation consumed steering, that is the
  complete extended snapshot and checked steering suffix described above, not
  the turn's unextended starting snapshot;
- a durable unstopped `InFlight` call with no surviving evidence ends
  `Ambiguous`, the abandoned attempt ends `Lost`, and the turn parks in
  `awaiting_model_call_recovery`;
- a durable `CancellationRequested` call reconstructs its applied interrupt,
  ends the attempt `AfterCancellation(Lost)`, and terminalizes
  `ReconciliationRequired` with that call as the exact ambiguity set.

Recovery is configuration-independent: `require_live_execution_for_restart`
passes no configured catalog and rebuilds target authority from the stored
call's own selection and target facts, so a deployment-configuration change can
never block or alter classification of an issued call. Recovery never resumes an
attempt, redispatches a call, or assumes a request was or was not sent.

## Composition and harness

Production composition wires `PostgresModelCallRepository` (all four transaction
roles), the in-process gate, and `RuntimeModelCallProvider` over the Anthropic
runtime, with the domain target catalog and runtime model catalog built from one
versioned static configuration file and a reread credential file
([configuration-and-credentials](configuration-and-credentials.md)). The
`signalbox-debug` binary (`apps/hubd/src/bin/signalbox-debug.rs`) drives one
session through the real scheduler and PostgreSQL path with either a
deterministic scripted reply or an explicit `--anthropic` smoke mode, then
prints the semantic transcript; it is deliberately not the client protocol.

## Open edges

- Provider-target mismatch evidence (the designed `ProviderTargetEvidence`,
  mismatch-selects-`KnownFailed`, and post-completion invalidation) is
  unimplemented; the adapter fails closed with an operator error, so a
  mismatched call is classified `Ambiguous` by restart rather than `KnownFailed`
  live.
- Unstopped ambiguity recovery is a parked state only: no owner decision,
  `DuplicateRiskAccepted`, replacement call, or outcome-authority transfer is
  implemented. Stop-caused ambiguity terminalizes proof-bearing reconciliation,
  but no later reconciliation workflow is implemented.
- Streaming deltas are collected but never delivered as transient drafts, and
  the designed early-observation pause/commit/resume path is unimplemented.
- The aggregate admits at most one call per turn attempt; the tool loop creates
  continuation attempts and calls in the same logical turn.
- A refused turn commits no refusal-content semantic entry; the variant remains
  an open edge in [sessions-and-transcript](sessions-and-transcript.md).
- Same-incarnation retained-evidence reconciliation gets exactly one production
  pass (`reconcile_retained_once`) before fatal escalation; repeated
  same-incarnation drains are exercised only by tests.
- No system prompt is composed or sent: the bridge always leaves the runtime
  operation's own `ModelOperation::system` field `None`
  (`crates/model-runtime/src/operation.rs`; `ModelSettings` carries no such
  field); system-prompt projection from session configuration remains deferred.

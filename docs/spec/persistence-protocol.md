# Persistence protocol

This page describes the implemented persistence protocol of the Signalbox hub as
verified against the implementing stack through PR #175 (`agent/stop-requests`).
It covers the Postgres representation in `crates/persistence` (source and
migrations), migration discipline, durable command storage and replay equality,
the fail-closed reconstitution boundary, the lock protocol, pending-steering
durable state, the corruption taxonomy, commit-ambiguity handling, and the
transactional outbox. Session aggregate semantics live in
[sessions-and-transcript](sessions-and-transcript.md), turn and attempt
lifecycle in [turn-lifecycle-and-scheduling](turn-lifecycle-and-scheduling.md),
identity kinds and command construction in
[identity-and-commands](identity-and-commands.md), and runtime wiring in
[runtime-substrate](runtime-substrate.md). Invariant text is normative in
[docs/invariants.md](../invariants.md); this page cites rows by tag.

## Stack and boundaries

The persistence crate uses SQLx (Postgres driver, `PgPool`, embedded migrator)
on Tokio. Queries are static SQL through the runtime query API with hand-written
decoding (`Row::try_get`); there are no query macros, `FromRow` derives, or
ORM-generated types. Domain types gain no SQLx or serialization traits; each
adapter module decodes its own rows through explicit fallible functions
(`decode_complete` and kin in `session.rs`, `create_session.rs`,
`submit_input.rs`, `replace_session_defaults.rs`), built on the shared identity
and ordinal scalar conversions in `crates/persistence/src/mapping.rs` (INV-002).
Why: one coherent driver/pool/migration stack minimizes dependency surface while
the module boundary, not the driver, enforces the record/domain split.

Concrete mapping rules:

- Identity newtypes map to native `uuid` columns through kind-specific
  conversion functions; kind is carried by table and column position, never by
  value inspection. `DurableCommandId` decoding rejects the nil and max sentinel
  UUIDs (`DurableCommandIdMappingError::SentinelUuid`); the other identity
  conversions are infallible. Identity supply and encoding semantics:
  [identity-and-commands](identity-and-commands.md).
- `u64` domain ordinals (acceptance position, defaults version) map to
  `numeric(20, 0)`. Decoding rejects non-positive, fractional, and
  out-of-`u64`-range values (`PositiveOrdinalMappingError`). Why: `bigint` is
  signed and silently narrows valid ordinals above `i64::MAX`; `numeric(20, 0)`
  preserves the full range and its ordering.

Connection options are explicit: production parsing forces
`PgSslMode::VerifyFull`; the ephemeral-test helper forces `Disable`. Pool sizing
remains at SQLx defaults until an operational slice selects limits.

## Migrations

Schema change is a forward-only, versioned SQL file set in
`crates/persistence/migrations/` — fifteen files, `202607180001` through
`202607230001` — embedded by `sqlx::migrate!` as the static `MIGRATOR` and
applied through one `migrate(pool)` operation. SQLx's `_sqlx_migrations` ledger
records applied files with checksums (the integration tests read the ledger
directly); serialization of concurrent migration runs is SQLx dependency
behavior, relied on but not demonstrated in this repo. `.gitattributes` pins
migration files to LF so checksums do not vary by platform, and a build script
re-embeds the set whenever a file changes. The production binary holds the
singleton hub guard and fences the prior pool generation, then runs `migrate` as
its first schema phase, followed by the startup scan and runtime (INV-034). The
fence migration's first installation is the sole case without a prior fenced
pool, because no earlier schema can have admitted one. Why: checksummed
forward-only files make every schema change a reviewed, immutable artifact, so a
deployed database's history is never silently edited.

Container-backed integration tests (`postgres-integration` feature, ignored by
default, failing loudly when Docker is absent) exercise the real constraints,
triggers, locks, and races described below against a pinned Postgres image.

## Relational representation

Storage is a normalized, purpose-specific relational schema of current-state
rows and append-only immutable facts. There is no event store: the guarded row
is the durable statement of record, and no state is rebuilt by replaying events
(INV-005). Why: the database-level invariants (INV-009, INV-012) are declarative
constraints over current-state rows; an event log would move them back into
projection code.

Implemented table families (across the fifteen migrations):

- `durable_command` plus typed command records (`create_session_command`,
  `replace_session_defaults_command`, `submit_input_command`,
  `decide_tool_request_command`);
- `session`, `session_defaults_version`, `session_current_defaults`,
  `session_scheduler`;
- `accepted_input`, `queued_input_origin`, `turn_lifecycle`, `turn_attempt`;
- `model_call` (execution state owned by
  [model-call-execution](model-call-execution.md), its turn-level
  provider-target pin on `turn_lifecycle`, and its pinned
  `credential_reference`);
- `semantic_transcript_entry`, `context_frontier`, `context_frontier_member`;
- `tool_round`, `tool_request`, `tool_approval_decision`, and `tool_attempt`;
- the singleton `hub_fence_state`, which supplies the generation used by
  hub-owned session advisory pool fences;
- the outbox family (below).

Representation rules, all enforced in the schema:

- Closed variant sets are `text` discriminators under `CHECK` constraints, with
  variant payload columns constrained present exactly when the discriminator
  requires them (for example `turn_lifecycle_state_payload_shape`). The
  implemented sets are exactly the admitted slices: turn state
  `queued`/`active`/`terminal`, active phase `running`,
  `awaiting_model_call_recovery`, `awaiting_tool_approval`, or
  `awaiting_tool_recovery`, terminal disposition
  `failed`/`completed`/`refused`/`cancelled`/`reconciliation_required`, attempt
  state `prepared`/`running`/`stop_requested`/`ended` with end variants
  `without_stop` and `after_cancellation`, and model-call state
  `prepared`/`in_flight`/`cancellation_requested`/`terminal` with terminal
  dispositions `completed`/`known_failed`/`refused`/`cancelled`/`ambiguous`.
- Immutable fact tables carry `BEFORE UPDATE OR DELETE` triggers that raise
  (`reject_immutable_record_change`), making append-only a database property,
  not a convention. Mutable lifecycle tables carry guard triggers instead:
  `turn_lifecycle` rows must be inserted `queued`, transition only
  monotonically, keep identity/origin/order and written starts write-once, and
  become immutable at `terminal`; `turn_attempt` rows are inserted `prepared`
  and an `ended` attempt is immutable. Why: restart trusts durable rows as
  evidence, so the schema itself must forbid rewriting them (INV-006, INV-007).
- INV-009 is database-level: partial unique indexes
  `turn_lifecycle_one_active_per_session`, `turn_attempt_one_live_per_turn`, and
  `turn_attempt_one_initial_per_turn` reject a second active turn, second live
  attempt, or second initial attempt regardless of process memory.
- Pending steering is durable current state (migration `202607180005`): an
  `accepted_input` row with disposition `pending_steering` records a
  `next_safe_point` delivery and names its expected active source turn, with
  origin and defaults fields constrained absent. Deferred constraint triggers
  correlate it both ways at commit:
  `accepted_input_pending_requires_active_source` requires the named turn to be
  `active`, taking `FOR UPDATE` on that `turn_lifecycle` row, and
  `turn_terminal_requires_closed_pending_steering` rejects a terminal transition
  while pending steering naming the turn remains
  (`turn_lifecycle_pending_steering_closed`). Migration `202607220001` adds the
  reclassification closure: a guard trigger
  (`reject_invalid_accepted_input_change`) replaces plain append-only on
  `accepted_input` and admits only `pending_steering` →
  `reclassified_as_turn_origin`, setting a fresh `origin_turn_id`. Migration
  `202607220004` widens that exact guard for `pending_steering` →
  `consumed_as_steering`, setting the exact `consuming_model_call_id`; both
  admitted changes otherwise preserve the accepted fact. Consumed steering
  additionally requires one correlated `steering_accepted_input` semantic entry
  in that call's frontier, naming the same accepted input and source turn.
  Reclassified steering instead requires its queued origin and terminal source
  proof. Those lifecycle checks preserve the immutable next-safe-point command
  receipt, so equal replay after either transition still returns the original
  applied pending-steering result (INV-012, INV-016).
- Cross-table completeness uses deferrable-initially-deferred foreign keys and
  constraint triggers so rows of one atomic fact can be inserted in any order
  inside a transaction while every commit boundary sees the complete shape: each
  claimed registry row has exactly one typed command record, each
  `submit_input_command` terminal result correlates with exactly its committed
  effects, each `context_frontier` header has complete contiguous ordered
  membership, and turn/attempt/semantic-entry writes re-assert the complete turn
  final state (origin entry, frontier prefix relationships, live-attempt
  cardinality, failure-entry correlation).
- Accepted user text is bounded to 1 MiB of UTF-8 in both the command record and
  `accepted_input` (`octet_length(convert_to(...))` checks), independent of the
  application admission bound.

Some rules are deliberately enforced twice — typed domain transitions and
database constraints — for the database-level invariants; a passing SQL row set
can still fail domain correlation (see reconstitution below). One current-state
row sits below the guarded tier: the mutable `session_current_defaults` pointer
carries no guard trigger, so beyond its range `CHECK` and deferred foreign key
into `session_defaults_version`, pointer discipline rests solely on the
application-side compare-and-set in `replace_session_defaults.rs`.

## Durable command storage and replay equality

The claim protocol, structural replay equality, and conflicting-reuse semantics
are owned by [identity-and-commands](identity-and-commands.md); this section
states only their storage representation and adapter mechanics.

One append-only, owner-global `durable_command` registry claims every command
identifier: `command_id` is the primary key across all kinds and sessions
(INV-012), with a `CHECK`-closed kind set (`create_session`,
`replace_session_defaults`, `submit_input`) and `storage_version` (currently
`1`). Each kind has one typed subordinate record keyed by `command_id` that
stores every caller-supplied semantic field in typed, `CHECK`-constrained
columns, plus the terminal `applied`/`rejected` result and its typed result
fields; result-shape `CHECK` constraints tie each rejection kind to exactly its
fields, and deferred reverse constraints require exactly one typed record per
claimed registry row at commit. Why: typed per-kind records keep replay
semantics reviewable and constraint-checked, where a universal serialized
payload would make the serializer a second semantic authority.

Adapter mechanics behind the shared protocol: registry inspection is the first
durable operation, before any current-state read, and an unseen identifier is
claimed with `INSERT ... ON CONFLICT DO NOTHING`, so duplicate concurrent
submission is a database conflict rather than an application race and a
concurrent loser rereads the winner. First handling commits the registry row,
typed record, terminal result, and every domain effect in one transaction, with
acknowledgement only after commit (INV-007); the stateful commands
(`ReplaceSessionDefaults`, `SubmitInput`) prepare that result against locked
current state inside the claim transaction, while `CreateSession` — which has no
current session state to lock — arrives as an already-prepared
`PreparedCreateSession` value and is inserted after the claim
(`create_session.rs`). Authoritative rejections claim the identifier and commit
their typed record exactly as applied results do. Replay resolution —
reconstruct the recorded command, compare structurally, return the recorded
result or `ConflictingReuse` — follows the owner page's contract.

`load` operations return `None` only for an unseen identifier; a claimed row
that cannot be reconstructed is corruption, never an unclaimed identifier.

## Lock protocol

Every Rust-issued SQL statement that takes an explicit row lock lives in
`crates/persistence/src/lock_inventory.rs`. One explicit lock lives in the
schema instead of the inventory: the deferred pending-steering source-turn
trigger (migration `202607180005`) takes `FOR UPDATE` on the named
`turn_lifecycle` row when a pending-steering `accepted_input` insert reaches
commit. Why: a single reviewed inventory makes lock ordering auditable instead
of scattered through query strings; the trigger-resident lock is recorded here
because it fires outside the inventory's view.

Locks per transaction, in acquisition order:

- **SubmitInput** (`prepare_against_locked_state`): session row
  `FOR NO KEY UPDATE`, then `session_scheduler` row `FOR UPDATE`, then
  `session_current_defaults` row `FOR UPDATE`; only then does it read the
  scheduling projection and assign the next acceptance position. A
  pending-steering acceptance additionally locks the named active
  `turn_lifecycle` row `FOR UPDATE` at commit time, inside the deferred
  source-turn trigger.
- **StartEligibleTurn**, **startup recovery**, and the **model-call execution
  transactions** (prepare, authorize, observation commit, restart recovery — all
  in `model_execution.rs`, reusing the same inventory statement): the
  `session_scheduler` row `FOR UPDATE` is the only explicit lock (session
  existence is checked with a bare `EXISTS`). The session row is locked only
  `KEY SHARE`, implicitly, by the inserts' foreign keys, and the candidate
  `turn_lifecycle` row is locked by the guarded `UPDATE` itself.
- **ReplaceSessionDefaults**: no explicit pre-lock; the compare-and-set `UPDATE`
  on the `session_current_defaults` pointer row is the serialization point, and
  its `session_defaults_version` insert takes `FOR KEY SHARE` on the session row
  through the non-deferrable session foreign key.
- **Outbox dispatch**: `outbox_delivery_state` is locked `FOR UPDATE`, then
  exactly `delivered_through + 1` and its typed record are read. Only an
  accepted synchronous offer advances that same singleton inside the
  transaction.
- **Hub-generation advance**: `hub_fence_state` is locked `FOR UPDATE`, then the
  transaction takes the exclusive transaction-level advisory lock for the prior
  generation, updates the singleton to its successor, and also obtains the same
  exclusive session-level advisory lock before commit. Commit releases the
  transaction-level lock and retains the session-level lock. The advisory key is
  the exact unsigned bit pattern
  `generation XOR ((1396852273 << 32) OR 1396852273)`, where `1396852273` is
  ASCII `SBF1`, reinterpreted unchanged as a two's-complement signed `i64` for
  PostgreSQL.

The guarded hub database keeps its fenced application pool and singleton guard
behind one shutdown boundary. Graceful shutdown globally closes the pool and
waits for every outstanding checkout before closing the guard session. If that
explicit shutdown is omitted, or cancelled before the pool drain completes, the
guard session remains retained until process exit rather than releasing while an
escaped pool clone may still write.

Two standing constraints (recorded beside the code):

1. Every turn-lifecycle writer acquires the scheduler-row lock before touching
   `turn_lifecycle` rows. Why: one session-scoped lock serializes activation,
   recovery, and acceptance against each other so guarded predicates race on
   rows, not on process memory (INV-009, INV-010).
2. No production path may take `FOR UPDATE` (the strongest mode) on the session
   row. Why: submit orders session-then-pointer while defaults replacement holds
   the pointer and requests FK `KEY SHARE` on the session row; `FOR UPDATE`
   conflicts with `KEY SHARE` and closes that cycle into a 40P01 deadlock, while
   `FOR NO KEY UPDATE` stays self-exclusive — still serializing per-session
   position assignment — without conflicting with referential-integrity locks.

## Reconstitution

Reconstitution is domain-owned and fail-closed (implemented for the session,
command-receipt, scheduling, and model-call execution projections). The adapter
performs only the boundary step — decode columns, check discriminators and
ordinals, assemble the complete checked input — and the domain performs pure
validation and returns one canonical value or a typed failure (INV-001, INV-002,
INV-006). Concretely: `SessionReconstitutionInput` for the current session,
`SubmitInputReconstitutionInput` (with turn-origin and acceptance-tail inputs)
for command receipts, `AcceptedInputSchedulingProjection` for the session's
complete queue and lifecycle state, `ModelCallExecutionReconstitutionInput` for
the active turn's pinned provider target and complete call history, and
`FailedTurnExecutionReconstitutionInput` for a failed terminal turn's exact
ended attempt and optional `known_failed`/`cancelled` call provenance
(backfilled and closed by migration `202607220003`). Cancelled and
reconciliation-required terminal turns additionally supply their exact
proof-bearing attempt end, applied-interrupt result, and optional cancelled or
required ambiguous call through the scheduling input described in
[turn-lifecycle-and-scheduling](turn-lifecycle-and-scheduling.md). The
scheduling load proves its own completeness — it counts `queued_input_origin`
against `turn_lifecycle` and fails on mismatch — rather than trusting whichever
rows a filter returned. Active-phase, terminal-evidence, and acceptance-tail
validation semantics are owned by
[turn-lifecycle-and-scheduling](turn-lifecycle-and-scheduling.md).

Persisted data is never normalized into a nearby valid state; malformed durable
rows produce typed corruption errors, authorize no effect, and are not repaired
or dropped on load. Load paths do not panic on durable data; checked interrupt
application produces the exact cancellation-requested or reconciliation-required
transition, while a projection that cannot support that transition fails closed
as typed corruption. Startup recovery operates only on successfully
reconstituted projections (INV-034), and a successful reconstitution does not
waive the guarded compare-and-set when a later transaction commits: every
guarded write that matches zero rows is either benign staleness (reload and
rederive) or, where the transaction's own premises made a match mandatory,
corruption. Why: the dangerous corruption cases are rows that look individually
valid while their cross-record correlations are not, so authority comes only
from complete validated projections, never from raw identifiers.

Startup recovery terminalizes an evidence-free lost active turn as failed and
atomically reclassifies its pending steering to successor origins. A turn
holding a `Prepared` call follows the same logical closure after ending the call
known-failed; an in-flight call recovers into the `awaiting_model_call_recovery`
wait. A persisted `stop_requested` attempt and `cancellation_requested` call
reconstruct through their exact applied interrupt, end the abandoned attempt
`after_cancellation/lost`, and terminalize proof-bearing reconciliation for the
ambiguous call without erasing stop intent. The schema guard
(`turn_lifecycle_pending_steering_closed`) independently requires every pending
row to be consumed or reclassified before terminalization. Why: a pending
steering row is an accepted delivery obligation, so every recovery branch must
account for it rather than block startup or strand it.

An interrupt accepted against an unstopped `awaiting_model_call_recovery` row
does not rewrite its terminal ambiguous call. In the accepting transaction, the
ended attempt remains its original `without_stop/ambiguous|lost` evidence, and
the active lifecycle terminalizes `reconciliation_required` with an
equal-content frontier and typed outbox record. The reconciliation marker and
accepted successor carry the exact interrupt proof. The attempt trigger rejects
every update to an ended attempt.

## Corruption taxonomy

Each adapter has a purpose-specific corruption enum with a shared vocabulary:

- `Missing(record)` — a required row or field is absent;
- `Unsupported { field, value }` — a closed discriminator or storage version has
  no admitted mapping (unknown values fail; they are never coerced);
- `Inconsistent(relationship)` — correlated durable records disagree;
- `InvalidOrdinal` / `InvalidContent` — checked scalar decoding failed;
- nested `CurrentSession(...)`, `Domain(...)`, `Scheduling(...)` — a subordinate
  projection failed its own boundary or domain validation.

Registry inspection has its own closed set (`RegistryCorruption`):
`UnsupportedKind`, `UnsupportedVersion`, `MissingTypedRecord`,
`ConflictingTypedRecords`.

Four error families implement the shared operator taxonomy
(`ClassifyOperatorFailure`, classifying into `OperatorFailureClass`): startup
scan (`StartupScanRepositoryError`), turn activation
(`StartEligibleTurnRepositoryError`), the eligibility sweep
(`PostgresEligibilitySweepError`), and the model-call repository
(`ModelCallRepositoryError`). The classes: `Infrastructure { commit_ambiguous }`
(with `commit_ambiguous: false`, infrastructure prevented the operation from
completing before commit and retrying is safe; with `commit_ambiguous: true`,
the failure struck at the commit boundary and the transaction may or may not
have won, so the caller must reread durable state instead of assuming either
outcome — see Commit-ambiguity handling), `FailClosedCorruption` (committed rows
cannot construct the accepted domain value; nothing proceeds),
`IdentityCollision` (a fresh hub-minted identity collided with a durable one;
detected either by the domain seam or by mapping the violated unique constraint
out of the database error), and `CallerOrHubBug`. The command-handling error
families draw the same corruption/infrastructure distinctions in their variants
but implement no operator classification yet (open edge). Startup-scan
corruption additionally carries the scoped active turn so operational policy can
isolate the affected session while remaining fail-closed.

## Commit-ambiguity handling

Transactions whose effects cannot be re-derived from a caller-held command
identifier classify commit failures. `commit_failure_is_ambiguous` (in
`start_eligible_turn.rs`, `startup.rs`, and `model_execution.rs`) treats a
database-reported error as ambiguous only for SQLSTATE `08007` (transaction
resolution unknown) and `40003` (statement completion unknown); any non-database
failure awaiting the commit response (lost connection, IO error) is ambiguous;
every other database-reported commit rejection is a definite failure. The flag
surfaces as `Infrastructure { commit_ambiguous: true }` so the caller knows
durable state may or may not include the transaction. Why: activation and
recovery mint fresh identities instead of claiming a command identifier, so a
lost commit response cannot be resolved by replay and must be reported as
ambiguous rather than guessed.

Command-handling adapters carry no ambiguity flag: retrying the same
`DurableCommandId` replays through the registry and either returns the recorded
result (the commit won) or handles the command fresh (it never claimed), which
resolves the ambiguity exactly (INV-012).

The model-call repository additionally resolves an ambiguous authorization
commit by rereading exact durable authority (`reread_ambiguous_authorization`)
rather than only surfacing the flag.

## Transactional outbox

Committed client-observable transitions become update events only through the
transactional-outbox family (INV-032 mechanism; observation semantics are
protocol scope). Implemented storage:

- `outbox_event` header (allocator-owned `event_sequence`, closed `event_kind`,
  `storage_version`, `session_id`) plus one typed record table per kind —
  `session_created_outbox_event`, `input_accepted_outbox_event`,
  `turn_activated_outbox_event`, `turn_failed_outbox_event`,
  `model_call_transition_outbox_event`, `turn_completed_outbox_event`,
  `turn_refused_outbox_event`, `turn_cancelled_outbox_event`, and
  `turn_reconciliation_required_outbox_event` — with a deferred trigger
  requiring exactly one typed record per header. The header and typed record
  tables are append-only (`reject_immutable_record_change`), and every outbox
  table rejects `TRUNCATE`.
- `outbox_sequence_state`, a mutable singleton row (deletion rejected): a
  `BEFORE INSERT` trigger on the header allocates `last_sequence + 1` by
  updating the singleton, whose row lock is held to transaction end, and a
  deferred trigger requires the event row for every advance. Why: holding the
  allocator row lock until commit makes committed sequences contiguous and
  commit-ordered, so a delivered prefix can never be discovered to have skipped
  a lower in-flight sequence.
- `outbox_delivery_state`, a mutable singleton delivered-through cursor
  (deletion rejected) whose trigger permits advancing by exactly one committed
  sequence at a time and forbids mixing delivery with event production in one
  transaction (and vice versa).

Appends happen only through the crate-private `outbox::append` on the caller's
existing connection; it never begins or commits a transaction, so the
state-changing adapter owns the atomic boundary and no post-commit publish step
exists in application code. Implemented appends: CreateSession handling appends
`session_created`; an applied SubmitInput that creates a turn origin appends
`input_accepted`, while `PendingSteering` appends nothing until terminal
reclassification mints its successor turn and appends that correlated
`input_accepted`; an applied StartEligibleTurn appends `turn_activated`. Startup
recovery appends `turn_failed` for a failed lost turn and
`turn_reconciliation_required` when stopped issued work becomes ambiguous;
terminal reclassification of pending steering appends its correlated
`input_accepted`. Model-call state transitions append `model_call_transition`,
completion closure appends `turn_completed`, refusal closure appends
`turn_refused`, and known-failure closure appends `turn_failed`;
interrupt-confirmed cancellation appends `turn_cancelled`, and live stopped
ambiguity appends `turn_reconciliation_required`; an interrupt against a parked
ambiguous tool attempt appends the same event kind with that exact tool-attempt
reference. A guarded transition that changes zero rows appends zero events. Why:
writing the event in the committing transaction makes the dual-write failure
(state without event, or event without state) unrepresentable.

The public `OutboxDispatcher` is the storage-side single-consumer seam. It locks
the delivery singleton, decodes exactly the next typed event, invokes a
synchronous consumer while retaining the lock, and advances and commits the
cursor only after consumer acceptance. Consumer retry or exit before the commit
request leaves the prefix unchanged for redelivery. A lost commit response is
resolved by the next locked cursor read: a committed advance proceeds, while a
rolled-back advance redelivers. The injected rolled-back-commit PostgreSQL test
enforces ordered at-least-once behavior. Before offering a record or reporting
idle, the dispatcher proves that no header exceeds the allocator cursor. An
activation must agree with the durable turn's active current attempt or retained
terminal attempt; a model-call transition must be reachable from the
authoritative monotonic call state, with an exact disposition match at terminal;
and failed, completed, refused, cancelled, and reconciliation-required records
must agree with the durable turn, terminal frontier, semantic marker where
present, and terminal model call where present. Historical Prepared and InFlight
transition records remain dispatchable after their call advances. Exhausted
delivery still validates the allocator singleton and cursor. Hub task ownership,
polling, fan-out, and client observation semantics are owned by
[process-protocol](process-protocol.md).

## Open edges

- Deferred outbox retention, pruning, and multiple-hub fan-out are cataloged in
  [open questions](../open-questions.md#protocols-and-persistence).
- Attempt continuation is admitted only for the tool-loop yield/approval path;
  no other producer can construct a predecessor-linked attempt.
- Frontier lineage checks assume `none` ancestry; fork ancestry must replace
  that assumption.
- The aggregate-map rows for model calls and the tool loop have landed; provider
  evidence, authority transfers, and fatal cancellation intent are not yet in
  the schema.
- Command-handling error families implement no `ClassifyOperatorFailure`;
  operator classification covers only startup scan, turn activation, the
  eligibility sweep, and the model-call repository.
- Submit-path scaling: the complete scheduling projection (content included)
  loads under the session lock per submission; a bounded-read representation
  remains undesigned (docs/open-questions.md).
- Database-role separation remains a deployment choice; migration invocation
  itself is wired in `apps/hubd`.

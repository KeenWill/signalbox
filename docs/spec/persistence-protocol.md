# Persistence protocol

The baseline persistence protocol was verified through PR #175
(`agent/stop-requests`); the prefix-reservation discipline was added in PR #235
(`agent/review-process-amendments`); the migration inventory was verified
through PR #254 (`agent/fix-parked-approval-interrupt`) and was verified again
in PR #227 (`agent/review-workflow-persistence`); the metadata command issuer
proof was verified through PR #265 (`agent/tool-batch-tier0`); the
`apps/signalboxd` migration-invocation home was verified through PR #258
(`agent/signalboxd-rename`); the model-identity frontier shape was verified
through PR #272 (`agent/mid-session-model`); the runner lease-admission trigger
lock was verified against PR #267 (`agent/runner-persistence`); the current
classifier names, ambiguity reconstitution facts, and command-adapter boundaries
were verified through PR #288 (`agent/audit-fix-docs-coherence`); the session
system-prompt columns were verified through PR #286
(`agent/session-system-prompt`); the terminal model-call token evidence columns
and transcript reader were verified through this PR (`agent/token-usage`); the
additive provider-failure cause column was verified through PR #330
(`agent/audit-verified-fixes`); the session-template provenance columns and
storage version four were verified through PR #311
(`agent/session-templates-spec`); and the context-compaction transaction and
lock inventory were verified against `agent/context-compaction-protocol`. This
page covers the Postgres representation in `crates/persistence` (source and
migrations), migration discipline, durable command storage and replay equality,
the fail-closed reconstitution boundary, the lock protocol, pending-steering
durable state, the corruption taxonomy, commit-ambiguity handling, and the
transactional outbox. Session aggregate semantics live in
[sessions-and-transcript](sessions-and-transcript.md), turn and attempt
lifecycle in [turn-lifecycle-and-scheduling](turn-lifecycle-and-scheduling.md),
identity kinds and command construction in
[identity-and-commands](identity-and-commands.md), and runtime wiring in
[runtime-substrate](runtime-substrate.md). Invariant enforcement lives in
INV-tagged tests; this page cites tags resolved through the generated
[invariant index](../invariants.md). The runner-orchestration transaction and
lock paragraphs are the foundation proposal at the bottom of their implementing
stack and become verified only with those child pull requests.

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
`crates/persistence/migrations/` — thirty-eight files, `202607180001` through
`202607300101` — embedded by `sqlx::migrate!` as the static `MIGRATOR` and
applied through one `migrate(pool)` operation. SQLx's `_sqlx_migrations` ledger
records applied files with checksums (the integration tests read the ledger
directly); serialization of concurrent migration runs is SQLx dependency
behavior, relied on but not demonstrated in this repo. `.gitattributes` pins
migration files to LF so checksums do not vary by platform, and a build script
re-embeds the set whenever a file changes. The production binary holds the
singleton daemon guard and fences the prior pool generation, then runs `migrate`
as its first schema phase, followed by the startup scan and runtime (INV-034).
The fence migration's first installation is the sole case without a prior fenced
pool, because no earlier schema can have admitted one. Why: checksummed
forward-only files make every schema change a reviewed, immutable artifact, so a
deployed database's history is never silently edited.

Prefix reservation across concurrent stacks: the bottom pull request of any
stack that will add migrations declares a reserved prefix block — a date plus a
slot range — in its description, and sibling stacks pick disjoint blocks. Once a
stack holds a reserved block, renumbering its migrations after a base merges is
forbidden as long as the reserved prefix still exceeds the highest prefix on
`main`; the ordering guarantee — a strictly greater prefix than the stack's
ultimate `main`-merge target — is checked against that target rather than
against a prefix the immediate parent branch carries only because a sibling
merged into it. Within a stack the guarantee still binds against the stack's own
migrations: a prefix a child pull request adds strictly exceeds every prefix its
ancestor branches add, because `_sqlx_migrations` keys applied migrations by
version, so a repeated prefix collides and a lower one applies out of order.
Why: parallel migration-bearing stacks would otherwise collide on the next free
prefix and churn-renumber each time a sibling merges.

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

Implemented table families (across the forward-only migrations):

- `durable_command` plus typed command records (`create_session_command`,
  `create_session_from_imported_frontier_command`,
  `replace_session_defaults_command`, `replace_session_metadata_command`,
  `submit_input_command`, `decide_tool_request_command`,
  `replace_lost_runner_command`, `replace_lost_runner_result`, and
  `abandon_lost_runner_command`);
- `session`, `imported_session_seed`, `session_defaults_version`,
  `session_current_defaults`, `session_scheduler`;
- `session_metadata` plus its current tag and attribute satellites,
  `session_metadata_installation`, and the complete tag and attribute satellites
  of `replace_session_metadata_command`;
- `imported_raw_source_record`, `imported_conversation`,
  `imported_conversation_raw_record`, and `imported_transcript_entry`, whose
  exact append-only representation, idempotency, and completeness rules are
  owned by [conversation-import](conversation-import.md);
- `accepted_input`, `queued_input_origin`, `turn_lifecycle`, `turn_attempt`;
- `model_call` (execution state owned by
  [model-call-execution](model-call-execution.md), its turn-level
  provider-target pin on `turn_lifecycle`, and its pinned
  `credential_reference`); migration `202607290301` adds four independently
  nullable, scale-preserving `numeric` token-usage columns whose explicit
  integrality and full-`u64` range checks reject fractional or out-of-range
  input without rounding. A nonterminal row must keep all four null, and a
  direct Prepared-to-terminal transition likewise requires all four null because
  no send occurred; the `cancelled` terminal disposition also requires all four
  null because cancellation evidence reports no usage. Migration `202607310001`
  adds nullable `terminal_provider_failure_cause`, constrained to the closed
  provider taxonomy and present only on `known_failed` terminal rows. The
  ordinary sent-call terminal update installs the exact provider-reported fields
  and optional classification alongside the disposition before terminal-row
  immutability applies;
- `semantic_transcript_entry`, `context_frontier`, `context_frontier_delta`,
  plus the resolved `context_frontier_member` compatibility projection;
- `tool_round`, `tool_request`, `tool_approval_decision`, and `tool_attempt`;
- the singleton `hub_fence_state`, which supplies the generation used by
  daemon-owned session advisory pool fences;
- the outbox family (below).

Representation rules, all enforced in the schema:

- Migration `202607300101` adds the optional session-template provenance pair
  (`template_name`, `template_content_digest`) to `session` and
  `create_session_command`. Both members are absent or present together; names
  satisfy the domain's 1-through-128-byte lowercase ASCII grammar and digests
  are exactly 32 bytes. The create-command row carries the same pair only at
  storage version 4; versions 1 through 3 require two nulls. A present pair also
  requires a nonnull command system prompt in both the schema and Rust reader.
  Reciprocal foreign keys bind every present pair across the creation command
  and its created session, so command replay and checked reconstitution cannot
  cross-wire provenance. Preexisting, imported, and explicit sessions carry two
  nulls. Both tables retain their append-only guards; no template catalog or
  mutable template object exists in Postgres (INV-047).
- Migration `202607280303` adds the optional bounded `system_prompt` column to
  `session_defaults_version` and the three defaults-bearing command tables, each
  guarded by the 1,048,576-UTF-8-byte and nonempty CHECK constraints and, on
  command tables, a version-three gate. A generated exact-encoding SHA-256
  digest column joins the selection key and the command/defaults agreement
  foreign keys, because megabyte text cannot join a btree key; the empty bytea
  stands for an absent prompt so a `MATCH SIMPLE` member never skips enforcement
  ([sessions-and-transcript](sessions-and-transcript.md)).
- Migration `202607280201` adds the closed `model_identity_changed`
  semantic-entry payload, whose turn, positive defaults epoch, and direct
  selection are total only for that kind. Deferred checks bind it to the named
  turn's frozen epoch and direct selection and, for a turn whose immutable
  boundary-requirement fact is true, require exactly one such entry iff a
  started turn's immediate predecessor froze a different selection. A false fact
  is reserved for active or terminal turns started before the boundary law;
  those turns require no boundary entry and cannot carry one. When present, the
  turn-start frontier ends with the boundary entry immediately followed by the
  turn origin. The lifecycle insertion trigger admits that two-entry suffix
  atomically while preserving the predecessor prefix and exact count.
- A `context_frontier` header records its immutable total member count and an
  optional same-session prefix frontier. `context_frontier_delta` stores only
  the absolute-position suffix beyond that prefix; roots store their complete
  membership. The bounded `resolve_context_frontier_members` function follows
  one requested prefix chain and returns the exact complete ordered membership.
  Migration `202607260300` converts existing complete rows by selecting the
  longest exact stored prefix (with an acyclic physical tie-break for
  equal-content identities) without changing any frontier identity or resolved
  sequence. Deferred completeness checks reject missing prefixes, cycles,
  inherited duplicates, gaps, and a resolved count different from the header.
  Why: append-derived histories store and load each immutable suffix once while
  preserving the complete-snapshot contract.
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
  not a convention. This includes raw-record blobs and occurrences,
  imported-conversation headers and members, imported-frontier command records,
  session seed projections, metadata replacement receipts, and every existing
  historical fact. Metadata receipt satellites must be inserted before their
  deferrable parent record; their `BEFORE INSERT` triggers lock the existing
  owner-global command claim, require that claim, and reject insertion once the
  parent seals the receipt. The parent and both receipt satellites also reject
  `TRUNCATE`, which does not invoke row-level delete triggers. Mutable lifecycle
  tables carry guard triggers instead: `turn_lifecycle` rows must be inserted
  `queued`, transition only monotonically, keep identity/origin/order and
  written starts write-once, and become immutable at `terminal`; `turn_attempt`
  rows are inserted `prepared` and an `ended` attempt is immutable. Why: restart
  trusts durable rows as evidence, so the schema itself must forbid rewriting
  them (INV-006, INV-007).
- The current `session_metadata` root remains mutable by complete replacement
  but rejects deletion and any change to its `session_id`. Once a session has a
  recorded metadata write, root absence can therefore never be reinterpreted as
  the initial unwritten state, and the mutable fields cannot move to another
  session. Its `source_command_id` names the exact immutable applied receipt.
  Deferred constraint triggers compare every current root field and both
  complete satellite sets to that receipt, so a partial direct insert, update,
  or delete cannot commit. Current satellite updates are rejected outright. The
  current root, tag, and attribute tables also reject `TRUNCATE`; complete
  replacement through the adapter is their only admitted mutation. An
  append-only `session_metadata_installation` row records each source receipt
  when its applied receipt parent is sealed. Before admitting that evidence, an
  immediate trigger requires the source to be current and compares the complete
  current root and both satellite sets with the sealed receipt. Each
  installation is therefore authenticated before another write in the same
  transaction can supersede it. Deferred foreign keys bind both the final
  current root and every applied receipt to the evidence. Reinstalling an older
  receipt after a later replacement cannot commit, and installation evidence
  cannot be updated, deleted, or truncated.
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
  effects, each applied metadata replacement receipt has a conditional foreign
  key to the retained current root for its exact target while a rejection has no
  such proof, each imported-conversation and context-frontier header has
  complete contiguous ordered membership, each imported-frontier session names
  its exact aggregate, boundary, and relationship and has exactly one immutable
  `imported_session_seed` naming its exact seed frontier, and
  turn/attempt/semantic-entry writes re-assert the complete turn final state
  (origin entry, frontier prefix relationships, live-attempt cardinality,
  failure-entry correlation). Every invalid-interrupt rejection additionally
  correlates the active phase its receipt claims: the stopping rejections
  through the prior applied interrupt's stopped attempt, and the parked-approval
  rejection directly against its named turn's recorded `awaiting_tool_approval`
  wait, so a receipt naming a running or terminal turn cannot commit and
  therefore never replays as authoritative.
- Accepted user text is bounded to 1 MiB of UTF-8 in both the command record and
  `accepted_input` (`octet_length(convert_to(...))` checks), independent of the
  application admission bound.
- Current and receipt metadata tag and attribute-key columns are bounded to
  1,024 UTF-8 bytes with the same explicit octet-length checks as their domain
  admission boundary.
- Current and applied-receipt metadata timestamps reject PostgreSQL positive and
  negative infinity, so every admitted value reaches the checked
  Unix-microsecond decoder.

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
`create_session_from_imported_frontier`, `replace_session_defaults`,
`replace_session_metadata`, `submit_input`, `decide_tool_request`,
`review_workflow`, `compact_session`, `replace_lost_runner`,
`abandon_lost_runner`) and a kind-scoped `storage_version`. Create-session
records write the provenance-gated version specified above; defaults-bearing
imported-create and replace-defaults records write version 3. Create-session
records reconstitute version 1 with the disabled dangerous-tool posture, and
versions 1 and 2 with no system prompt — a pre-version-three row carrying one
fails closed in both the schema and every Rust reader. A pre-version-four create
row carrying template provenance likewise fails closed; therefore a rollback
reader that supports only versions 1 through 3 rejects every new create record
instead of projecting template creation as explicit creation. Metadata, submit,
decision, review-workflow, compaction, and runner-recovery records use version
1\. Each kind has one typed subordinate request record keyed by `command_id` that
stores every caller-supplied semantic field in typed, `CHECK`-constrained
columns. Every kind except runner replacement also stores the terminal
`applied`/`rejected` result and typed result fields there.
`replace_lost_runner_command` is the immutable request and
provisioning-authorization root; at most one append-only
`replace_lost_runner_result` supplies its terminal result after off-transaction
runner I/O. Result-shape `CHECK` constraints tie each rejection kind to exactly
its fields. Deferred reverse constraints require exactly one typed request per
claimed registry row at commit and forbid a replacement result without its
request; acknowledgement requires the terminal result. Why: typed per-kind
records keep replay semantics reviewable and constraint-checked, where a
universal serialized payload would make the serializer a second semantic
authority.

Adapter mechanics behind the shared protocol: registry inspection is the first
durable operation, before any current-state read, and an unseen identifier is
claimed with `INSERT ... ON CONFLICT DO NOTHING`, so duplicate concurrent
submission is a database conflict rather than an application race and a
concurrent loser rereads the winner. Compaction follows this protocol before its
session-row lock; a losing insert inspects the committed command and returns its
exact replay, conflict, pending, or failed disposition rather than proceeding
with independently selected call identities. Commands whose complete effect is
one transaction commit the registry row, typed record, terminal result, and
every domain effect together, with acknowledgement only after commit (INV-007).
Compaction instead commits its registry row, pending typed command, and Prepared
dedicated call together before provider work; its later session-locked terminal
transaction changes that command exactly once to applied or failed. The stateful
commands (`ReplaceSessionDefaults`, `SubmitInput`) prepare their result against
locked current state inside the claim transaction, while `CreateSession` — which
has no current session state to lock — arrives as an already-prepared
`PreparedCreateSession` value and is inserted after the claim
(`create_session.rs`). Authoritative rejections claim the identifier and commit
their typed record exactly as applied results do. Owner-specified pre-claim
admission errors are different: after registry inspection, a missing
conversation named by the selected imported frontier or a boundary absent from
that conversation returns without inserting a claim or typed record. Replay
resolution — reconstruct the recorded command, compare structurally, return the
recorded result or `ConflictingReuse` — follows the owner page's contract.

`load` operations return `None` only for an unseen identifier; a claimed row
that cannot be reconstructed is corruption, never an unclaimed identifier.

## Lock protocol

Every Rust-issued SQL statement that takes an explicit row lock lives in
`crates/persistence/src/lock_inventory.rs`. Two explicit lock sites live in the
schema instead:

- the deferred pending-steering source-turn trigger (migration `202607180005`)
  takes `FOR UPDATE` on the named `turn_lifecycle` row when a pending-steering
  `accepted_input` insert reaches commit; and
- the metadata receipt-satellite insert trigger (migration `202607260101`) takes
  `FOR UPDATE` on the already-claimed `durable_command` row before it checks
  whether the typed receipt parent has sealed the command.

Why: a single reviewed inventory makes lock ordering auditable instead of
scattered through query strings; trigger-resident locks are recorded here
because they fire outside the Rust inventory's view.

Locks per transaction, in acquisition order:

- **CreateSessionFromImportedFrontier**: no explicit row lock. Registry claim
  insertion and the command/session uniqueness constraints serialize competing
  command identities. The adapter loads the aggregate named by
  `frontier.conversation()`; that selected aggregate is immutable and
  append-only, so complete loading and boundary resolution need no mutable-state
  lock. Semantic-entry candidates are requested only after the resulting checked
  prefix fixes their cardinality.
- **ContextCompaction**: after claiming an unseen owner-global command,
  preparation locks the target `session_scheduler` row `FOR UPDATE` and then the
  current-defaults pointer `FOR UPDATE` before reading defaults, turn, frontier,
  and existing compaction state. Holding the scheduler lock through boundary
  selection and recording makes preparation mutually exclusive with turn
  activation; the loser reconstitutes the winner. Guarded updates and inserts
  serialize the call, command, summary, and result-frontier records. Later
  call-lifecycle transitions use the session row `FOR NO KEY UPDATE`. The
  pending typed command stores its immutable dedicated `model_call_id` from
  creation, so recovery never infers correlation from a result-only field. An
  automatic command additionally stores the immutable queued `turn_id`; a
  partial uniqueness constraint admits at most one automatic compaction command
  for that turn in its session, and preparation recognizes the retained attempt
  before allocating a second call. An equal replay resolves from the command
  registry and receipt without taking a session lifecycle lock or resolving
  current configuration.
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
- **Tool-loop transactions** (owner decision, attempt prepare, attempt
  authorization, preflight failure, result commit, crash classification, result
  projection plus continuation preparation, and their authoritative rereads):
  the `session_scheduler` row `FOR UPDATE` is the first and only explicit lock.
  An unseen decision command first claims the owner-global registry; after
  resolving the request's owning session it takes that scheduler lock before
  reading or mutating the active tool batch. A replay resolves entirely from the
  command registry and receipt and takes no lifecycle lock. Guarded
  `turn_lifecycle`, `turn_attempt`, `tool_attempt`, and model-call updates then
  serialize under the scheduler lock; their foreign keys may take implicit
  `KEY SHARE` locks on parent rows.
- **ReplaceSessionDefaults**: no explicit pre-lock; the compare-and-set `UPDATE`
  on the `session_current_defaults` pointer row is the serialization point, and
  its `session_defaults_version` insert takes `FOR KEY SHARE` on the session row
  through the non-deferrable session foreign key.
- **ReplaceSessionMetadata**: the target session row is locked
  `FOR NO KEY UPDATE` before the complete satellite snapshot is replaced. This
  serializes metadata writers without conflicting with the `KEY SHARE` lock
  taken by foreign-key checks. After the lock is acquired, a separate statement
  samples PostgreSQL statement time at microsecond precision; that exact value
  is written to both the current root and the applied receipt. The unseen
  handler already owns its `durable_command` claim from insertion before this
  session lock; each nonempty receipt satellite later reacquires that same row
  through its trigger before the typed parent seals the receipt. A point read
  and each opened streaming list page use one read-only repeatable-read
  transaction, so their root and satellite values come from one database
  snapshot.
- **Runner total order**: every transaction that takes more than one runner
  authority lock uses the same applicable subsequence, omitting absent rows but
  never reordering them: `session_scheduler` when present; current enrollment or
  pending replacement-request heads in canonical identity order; runner
  connection/loss heads in runner-identity order; current registration head;
  placement; current credential grant; lease; and only then semantic-frontier
  and turn rows. A durable owner-command claim precedes this subsequence.
- **Runner enrollment and registration**: the current enrollment or pending
  replacement-request head is locked first, followed by the relevant runner
  heads in runner-identity order and then the current registration head.
  Activating a pending replacement retires the old enrollment, persists the
  issued successor identities and registration, and installs the owner-command
  effect in one transaction.
- **Runner dispatch and result**: `session_scheduler` is the first lock,
  followed by enrollment, current runner connection/loss, registration,
  placement, current credential grant when present, and lease heads in the total
  order above. The initial dispatch transaction then stores workspace receipt
  consumption, pin, grant, `InFlight` attempt, and offered lease together. Claim
  locks enrollment, runner, registration, and lease in that order and commits
  before acknowledgement. Result admission takes the session scheduler first,
  then the applicable runner and lease rows without acquiring an earlier omitted
  lock, and commits the checked terminal attempt observation and claimed-lease
  completion together.
- **Runner loss**: one short transaction locks only the current connection/loss
  head, advances a positive durable loss epoch, and thereby makes every trigger
  reject new offers or claims from that connection. It never holds that global
  row while waiting for a session lock. A restartable propagation cursor pages
  at most 64 affected session identities in order; each session is updated in
  its own transaction by locking `session_scheduler` first, then the loss head,
  placement, current lease, and guarded turn rows. Offered leases with no
  durable claim acquire exact no-execution proof; claimed leases follow effect
  loss law. A crash resumes at the first uncommitted session, while every
  not-yet-projected placement is already effectively lost through the epoch
  fence.
- **Runner replace, abandon, and release**: an unseen abandonment command owns
  its durable-command claim and terminalizes in one transaction. An unseen
  replacement command first claims its immutable request and provisioning
  authorization in a short transaction, performs no runner I/O under database
  locks, then its terminal transaction follows the runner total order exactly:
  `session_scheduler`, enrollment or pending-request heads, relevant runner
  heads in identity order, registration, placement, grant and lease, then
  guarded semantic-frontier and turn rows. Replacement rechecks and atomically
  activates one pending enrollment, consumes workspace-ready evidence when
  required, installs the placement frontier, and appends the terminal command
  result. A crash before that result leaves the immutable request and
  authorization resumable. Abandon requires an empty active-turn slot and stores
  only terminal placement state. Either transition enqueues the retired
  placement release; release acknowledgement uses the same
  scheduler-then-placement order and never mutates turn lifecycle.
- **Outbox dispatch**: `outbox_delivery_state` is locked `FOR UPDATE`, then
  exactly `delivered_through + 1` and its typed record are read. Only an
  accepted synchronous offer advances that same singleton inside the
  transaction.
- **Daemon-generation advance**: `hub_fence_state` is locked `FOR UPDATE`, then
  the transaction takes the exclusive transaction-level advisory lock for the
  prior generation, updates the singleton to its successor, and also obtains the
  same exclusive session-level advisory lock before commit. Commit releases the
  transaction-level lock and retains the session-level lock. The advisory key is
  the exact unsigned bit pattern
  `generation XOR ((1396852273 << 32) OR 1396852273)`, where `1396852273` is
  ASCII `SBF1`, reinterpreted unchanged as a two's-complement signed `i64` for
  PostgreSQL.

The guarded daemon database keeps its fenced application pool and singleton
guard behind one shutdown boundary. Graceful shutdown globally closes the pool
and waits for every outstanding checkout before closing the guard session. If
that explicit shutdown is omitted, or cancelled before the pool drain completes,
the guard session remains retained until process exit rather than releasing
while an escaped pool clone may still write.

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
proof-bearing attempt end, applied-interrupt result, and optional cancelled call
or required ambiguous model call/tool attempt through the scheduling input
described in [turn-lifecycle-and-scheduling](turn-lifecycle-and-scheduling.md).
The scheduling load proves its own completeness — it counts
`queued_input_origin` against `turn_lifecycle` and fails on mismatch — rather
than trusting whichever rows a filter returned. It also walks the union of the
required frontier prefix chains once, loads each reachable header and delta
once, and reconstitutes shared prefixes without rebuilding their complete
membership. A process transcript read likewise yields acceptance-ordered turns,
then every terminal model call in turn-acceptance and call-identity order, then
opens one database cursor over one resolution of the selected frontier chain. It
validates declared counts and contiguous positions while advancing and decodes
at most one row at a time. The model-call phase decodes every nullable token
field through the full-range ordinal boundary; null remains absence. Failed-turn
projection also decodes the nullable closed provider-failure cause and rejects a
cause attached to any disposition other than `known_failed`. Active-phase,
terminal-evidence, and acceptance-tail validation semantics are owned by
[turn-lifecycle-and-scheduling](turn-lifecycle-and-scheduling.md).

Persisted data is never normalized into a nearby valid state; malformed durable
rows produce typed corruption errors, authorize no effect, and are not repaired
or dropped on load. An applied metadata receipt load also validates the stored
current-root proof against both the command target and result target. A metadata
list load validates the complete stored content, including attributes that its
public summary deliberately omits, before it constructs the summary. Load paths
do not panic on durable data; checked interrupt application produces the exact
cancellation-requested or reconciliation-required transition, while a projection
that cannot support that transition fails closed as typed corruption. Startup
recovery operates only on successfully reconstituted projections (INV-034), and
a successful reconstitution does not waive the guarded compare-and-set when a
later transaction commits: every guarded write that matches zero rows is either
benign staleness (reload and rederive) or, where the transaction's own premises
made a match mandatory, corruption. Why: the dangerous corruption cases are rows
that look individually valid while their cross-record correlations are not, so
authority comes only from complete validated projections, never from raw
identifiers.

Startup recovery terminalizes an evidence-free lost active turn as failed and
atomically reclassifies its pending steering to successor origins. A turn
holding a `Prepared` call follows the same logical closure after ending the call
known-failed; an in-flight call recovers into the `awaiting_model_call_recovery`
wait. A persisted `stop_requested` attempt and `cancellation_requested` call
reconstruct through their exact applied interrupt, end the abandoned attempt
`after_cancellation/lost`, and terminalize proof-bearing reconciliation for the
ambiguous call without erasing stop intent. The schema guard
(`turn_lifecycle_pending_steering_closed`) independently requires every pending
row to be consumed or reclassified before terminalization. The same finite
startup inventory includes every nonterminal dedicated compaction call. Under
the session scheduler lock it requires exactly one matching pending command,
terminalizes Prepared as `known_failed` or InFlight as `ambiguous`, and marks
the command failed in the same transaction; disagreement fails closed and no
summary or result frontier is synthesized. Why: a pending steering row is an
accepted delivery obligation, so every recovery branch must account for it
rather than block startup or strand it.

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

Five error families implement the shared operator taxonomy
(`ClassifyOperatorFailure`, classifying into `OperatorFailureClass`): startup
scan (`StartupScanRepositoryError`), turn activation
(`StartEligibleTurnRepositoryError`), the eligibility sweep
(`PostgresEligibilitySweepError`), the model-call repository
(`ModelCallRepositoryError`), and the tool-loop repository
(`ToolLoopRepositoryError`). The classes: `Infrastructure { commit_ambiguous }`
(with `commit_ambiguous: false`, infrastructure prevented the operation from
completing before commit and retrying is safe; with `commit_ambiguous: true`,
the failure struck at the commit boundary and the transaction may or may not
have won, so the caller must reread durable state instead of assuming either
outcome — see Commit-ambiguity handling), `FailClosedCorruption` (committed rows
cannot construct the accepted domain value; nothing proceeds),
`IdentityCollision` (a fresh daemon-minted identity collided with a durable one;
detected either by the domain seam or by mapping the violated unique constraint
out of the database error), and `CallerOrHubBug`. The remaining command-handling
error families draw the same corruption/infrastructure distinctions in their
variants but implement no operator classification yet (open edge). Startup-scan
corruption additionally carries the scoped active turn so operational policy can
isolate the affected session while remaining fail-closed.

## Commit-ambiguity handling

Transaction boundaries that retain the failing phase classify commit failures
with `commit_failure_is_ambiguous`: the helper is crate-shared in `lib.rs` for
durable-command adapters and repeated locally in `start_eligible_turn.rs`,
`startup.rs`, `model_execution.rs`, and `tool_loop.rs`. This inventory excludes
the phase-insensitive conversation-import repository, whose conservative wire
mapping is owned by [process-protocol](process-protocol.md). A database-reported
error is ambiguous only for SQLSTATE `08007` (transaction resolution unknown)
and `40003` (statement completion unknown); any non-database failure awaiting
the commit response (lost connection, IO error) is ambiguous; every other
database-reported commit rejection is a definite failure.

The activation and recovery families surface ambiguity as
`Infrastructure { commit_ambiguous: true }` because they mint fresh identities
instead of claiming a command identifier: a lost commit response cannot be
resolved by replay and must not be guessed. Command-handling adapters likewise
surface a typed ambiguity variant or flag at the final commit boundary. A caller
can then retry the same `DurableCommandId`: registry replay either returns the
recorded result (the commit won) or handles the command fresh (it never
claimed), which resolves the ambiguity exactly (INV-012).

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
  `model_call_transition_outbox_event`, `tool_batch_transition_outbox_event`,
  `context_compacted_outbox_event`, `turn_completed_outbox_event`,
  `turn_refused_outbox_event`, `turn_cancelled_outbox_event`, and
  `turn_reconciliation_required_outbox_event` — with a deferred trigger
  requiring exactly one typed record per header. Tool-batch transition records
  carry the producing call and exactly one closed state shape: `proposed` names
  the yielded assistant/tool-use frontier, `results_projected` names the
  all-resolved result frontier, and `recovery_required` names the exact
  ambiguous physical attempt. The header and typed record tables are append-only
  (`reject_immutable_record_change`), and every outbox table rejects `TRUNCATE`.
  A context-compacted record names the authoritative compaction, its completed
  dedicated call, exact positive through position, appended summary, and result
  frontier.
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
exists in application code. Implemented appends: `CreateSession` and
`CreateSessionFromImportedFrontier` handling each append `session_created`; an
applied `SubmitInput` that creates a turn origin appends `input_accepted`, while
`PendingSteering` appends nothing until terminal reclassification mints its
successor turn and appends that correlated `input_accepted`; an applied
`StartEligibleTurn` appends `turn_activated`. Startup recovery appends
`turn_failed` for a failed lost turn and `turn_reconciliation_required` when
stopped issued work becomes ambiguous; terminal reclassification of pending
steering appends its correlated `input_accepted`. Model-call state transitions
append `model_call_transition`, tool-round creation appends
`tool_batch_transition { proposed }`, all-resolved result projection appends
`tool_batch_transition { results_projected }`, and an external-effect ambiguity
appends `tool_batch_transition { recovery_required }`. Completion closure
appends `turn_completed`, refusal closure appends `turn_refused`, and
known-failure closure appends `turn_failed`; interrupt-confirmed cancellation
appends `turn_cancelled`, and live stopped ambiguity appends
`turn_reconciliation_required`; completion of a context compaction appends
`context_compacted` in the same transaction as its dedicated call, summary
entry, result frontier, compaction result, and applied command receipt. An
interrupt against a parked ambiguous tool attempt appends the same event kind
with that exact tool-attempt reference. A guarded transition that changes zero
rows appends zero events. Why: writing the event in the committing transaction
makes the dual-write failure (state without event, or event without state)
unrepresentable.

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
present, and terminal model call or tool attempt where present. A
reconciliation-required event carries exactly one of those two operation
references. A context-compacted event must agree with one completed dedicated
call and the authoritative compaction, applied command, exact through position,
summary entry, and result frontier before dispatch. Tool-batch cancellation and
known crash-failure records validate their terminal marker after the earlier
producing model call and ended physical attempts rather than requiring an
otherwise empty call history. Historical Prepared and InFlight transition
records remain dispatchable after their call advances. Exhausted delivery still
validates the allocator singleton and cursor. Daemon task ownership, polling,
fan-out, and client observation semantics are owned by
[process-protocol](process-protocol.md).

## Open edges

- Deferred outbox retention, pruning, and multiple-daemon fan-out are cataloged
  in [open questions](../open-questions.md#protocols-and-persistence).
- Attempt continuation is admitted only for the tool-loop yield/approval path;
  no other producer can construct a predecessor-linked attempt.
- Frontier lineage checks admit `none` and checked imported-frontier ancestry;
  native `SingleSource` fork ancestry remains unimplemented.
- The aggregate-map rows for model calls and the tool loop have landed; provider
  evidence, authority transfers, and fatal cancellation intent are not yet in
  the schema.
- Command-handling operator classification covers the tool-loop repository; the
  other command families do not yet implement `ClassifyOperatorFailure`.
- Database-role separation remains a deployment choice; migration invocation
  itself is wired in `apps/signalboxd`.

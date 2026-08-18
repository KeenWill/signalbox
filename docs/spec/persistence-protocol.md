# Persistence protocol

The runner connection authority head, durable loss epoch, and lease offer/claim
fences were verified against the parent slice (`agent/runner-loss-epoch`).
Placement-relative lease-offer fencing was verified against the parent slice
(`agent/runner-loss-propagation`). The bounded runner-loss propagation cursor
and ordered page read were verified against this PR
(`agent/runner-loss-session-propagation`). The atomic per-session runner-loss
propagation transaction and cursor completion were verified against this PR
(`agent/runner-loss-session-transaction`). Daemon paging after terminal loss and
startup resumption of every pending cursor were verified against this PR
(`agent/runner-loss-daemon-propagation`). The registration-reconciliation
cursor, exact registration-loss cause, and per-session transaction were verified
against this PR (`agent/runner-registration-reconciliation`). Pending-successor
enrollment admission and exact receipt replay were verified against the parent
slice (`agent/runner-pending-successor-promotion`). Creation-command runner
placement and revision-one readback were verified against this PR
(`agent/runner-creation-placement-persistence`). The reference-only
placement-change entry, successor-placement foreign key, and exact final-member
frontier link were verified against this PR
(`agent/runner-placement-semantic-persistence`). The deployment-scoped
pending-successor activation transaction was verified against this PR
(`agent/runner-pending-successor-activation`). The different-live-runner pre-pin
replacement command and result transaction were verified against this PR
(`agent/runner-pre-pin-replacement`). Pending-enrollment activation inside that
transaction was verified against this PR
(`agent/runner-pending-pre-pin-replacement`). The append-only staged
workspace-provisioning authorization representation and checked readback were
verified against this PR
(`agent/runner-workspace-provisioning-authorization-persistence`). The atomic
pinned-replacement command claim and provisioning-stage producer were verified
against this PR (`agent/runner-replacement-provisioning-transaction`). The exact
lease-offer registration and execution-placement reconstitution facts are
verified against this PR (`agent/runner-lease-domain-correlation`). Existing-pin
attempt-and-offer atomicity is verified against this PR
(`agent/runner-pinned-dispatch-transaction`). The pinned-dispatch adapter's
exact runner/registration lookup and returned enrollment routing identity are
verified against this PR (`agent/runner-offer-locus-binding`). The
workspace-free exact-directory initial dispatch transaction and its durable
request-locus fence are verified against this PR
(`agent/runner-initial-dispatch-transaction`). Durable lease-claim admission is
verified against this PR (`agent/runner-lease-claim-transaction`). Atomic
claimed-lease and physical-attempt result admission is verified against this PR
(`agent/runner-lease-result-transaction`). Authenticated pre-resume admission of
retained terminal evidence is verified against this PR
(`agent/daemon-retained-result-resume`). Canonical normalized arguments joined
through every lease's exact tool request are verified against this PR
(`agent/runner-lease-argument-binding`). The closed selected executable locus on
every durable tool request is verified against this PR
(`agent/tool-request-execution-locus`).

The runner-state transition outbox representation, relational source checks, and
dispatch projection were verified against this PR
(`agent/runner-event-outbox-persistence`). The established-successor outbox
source check was re-verified against this PR
(`agent/daemon-runner-health-events`).

The runner-recovery turn-phase representation and read boundary were verified
against this PR (`agent/runner-awaiting-recovery-persistence`).

The user-vocabulary surface on this page was re-verified through PR #378
(`agent/user-vocabulary`).

The baseline persistence protocol was verified through PR #175
(`agent/stop-requests`); the prefix-reservation discipline was added in PR #235
(`agent/review-process-amendments`); the migration inventory was verified
through PR #254 (`agent/fix-parked-approval-interrupt`), was verified again in
PR #227 (`agent/review-workflow-persistence`), through this PR
(`agent/runner-placement-loss-persistence`), and again in this PR
(`agent/git-remote-authority`); the metadata command issuer proof was verified
through PR #265 (`agent/tool-batch-tier0`); the `apps/signalboxd`
migration-invocation home was verified through PR #258
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
lock inventory were verified against PR #314
(`agent/context-compaction-protocol`). The crate-shared commit-ambiguity helper
was verified against this PR (`agent/domain-cleanup`); the session-plan event
sequence was verified through PR #380 (`agent/plan-tool`) and its dependency
extension against PR #385 (`agent/plan-dependencies`); and the goal event
transaction, trigger lock, and goal-turn outbox provenance were verified through
PR #384 (`agent/goal-mode-runtime`); and the approval-judge call, decision, and
posture storage were verified through PR #420 (`agent/approval-judge-storage`);
the approval-judge lifecycle transactions were verified through this PR
(`agent/approval-judge-execution-support`); the approval-decision outbox is
verified against this implementing change; the session-placement event, current
head, and creation transaction were verified through PR #415
(`agent/scoped-visibility-creation`); and the exact stop-command descendant
scopes, delegated transcript origins, foreground-result closure, pre-outbox
cascade locks, typed delegation wake origins, and exact delegation update and
wake obligations were verified through this PR
(`agent/delegation-persistence-schema`); the delegated child-input, await,
peer-message, terminal-observation, and restart-recovery locks plus wait/message
replay satellites and headers were verified through this PR
(`agent/delegation-runtime-persistence-v2`), and the broader child-terminal
endpoint locks were verified through this PR
(`agent/delegation-runtime-daemon-v2`); the model-settings command fields,
immutable evidence, snapshot projection, and typed outbox records were verified
through this PR (`agent/model-settings-persistence`); the defaults-replacement
pointer-lock admission is verified through this PR
(`agent/model-settings-execution`). The runner placement loss-source,
lost-before-pin, pre-pin replacement, and abandonment records are verified
through this PR (`agent/runner-placement-loss-persistence`). This page covers
the Postgres representation in `crates/persistence` (source and migrations),
migration discipline, durable command storage and replay equality, the
fail-closed reconstitution boundary, the lock protocol, pending-steering durable
state, the corruption taxonomy, commit-ambiguity handling, and the transactional
outbox. Session aggregate semantics live in
[sessions-and-transcript](sessions-and-transcript.md), turn and attempt
lifecycle in [turn-lifecycle-and-scheduling](turn-lifecycle-and-scheduling.md),
identity kinds and command construction in
[identity-and-commands](identity-and-commands.md), and runtime wiring in
[runtime-substrate](runtime-substrate.md). Invariant enforcement lives in
INV-tagged tests; this page cites tags resolved through the generated
[invariant index](../invariants.md). The runner-orchestration transaction and
lock paragraphs are the foundation proposal at the bottom of their implementing
stack and become verified only with those child pull requests. The
session-placement update transaction is the foundation proposal at the bottom of
its implementing stack and becomes verified only with its child pull request.

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

Migration `202608020016_session_placement_path.sql` adds the append-only
`session_placement_event` history, its one-row mutable current pointer, and the
typed `update_session_placement_command` record. Every existing session is
backfilled with a pathless version-one creation event. Post-migration legacy
native creation records below storage version 6 and supported imported creation
records below their model-settings version materialize that same pathless event
and head when their typed creation receipt is inserted, so a daemon spanning the
migration cannot create an unreadable session. A deferred reverse check requires
every newly inserted session to end its transaction with a complete selected
placement event. Native creation records at storage version 6 or later store the
optional path and explicit root-global-read-intent bit and append the same event
atomically with the session. Checks make the intent bit true exactly for a
one-segment root path and false for pathless and non-root scoped values. The
current pointer may advance only to the next event; event rows and typed command
records are immutable.

Connection options are explicit: production parsing forces
`PgSslMode::VerifyFull`; the ephemeral-test helper forces `Disable`. Pool sizing
remains at SQLx defaults until an operational slice selects limits.

## Migrations

Schema change is a forward-only, versioned SQL file set in
`crates/persistence/migrations/` — eighty-four files, `202607180001` through
`202608110018` — embedded by `sqlx::migrate!` as the static `MIGRATOR` and
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
rows and append-only immutable facts. There is no general-purpose event store:
outside session plans and commissioned goals, the guarded row is the durable
statement of record and current state is not rebuilt by replaying events
(INV-005). Session plans and commissioned goals are deliberate exceptions: each
has a session-local append-only event sequence as its durable statement of
record, and its current state is the checked fold of that complete history. The
plan exception was verified against this PR (`agent/plan-tool`), and the goal
exception through PR #384 (`agent/goal-mode-runtime`). Why: database-level
invariants (INV-009, INV-012) stay declarative over current-state rows, while
plan and goal history is retained product evidence rather than an implementation
log.

Implemented table families (across the forward-only migrations):

- `durable_command` plus typed command records (`create_session_command`,
  `create_session_from_imported_frontier_command`,
  `replace_session_defaults_command`, `replace_session_metadata_command`,
  `submit_input_command`, `decide_tool_request_command`,
  `replace_lost_runner_command`, `replace_lost_runner_result`,
  `abandon_lost_runner_command`, `promote_pending_runner_command`, and
  `goal_command`);
- `session`, `imported_session_seed`, `session_defaults_version`,
  `session_current_defaults`, `session_scheduler`, plus the immutable
  `session_model_settings_changed` and `turn_model_settings_resolved` evidence
  records;
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
  null because cancellation evidence reports no usage. Migration `202607310002`
  adds nullable `terminal_provider_failure_cause`, constrained to the closed
  provider taxonomy and present only on `known_failed` terminal rows. Migration
  `202607310003` rejects a cause on a direct Prepared-to-terminal transition
  because no provider send occurred. The ordinary sent-call terminal update
  installs the exact provider-reported fields and optional classification
  alongside the disposition before terminal-row immutability applies;
- `semantic_transcript_entry`, `context_frontier`, `context_frontier_delta`,
  plus the resolved `context_frontier_member` compatibility projection;
- `tool_round`, `tool_request`, `tool_approval_decision`,
  `tool_approval_judge_model_call`, and `tool_attempt`;
- the singleton `hub_fence_state`, which supplies the generation used by
  daemon-owned session advisory pool fences;
- `goal_event`, whose session-local positive ordinal sequence retains the
  complete commissioned-goal lineage and state-transition provenance, plus
  `goal_turn`, which correlates each pursuit-starting event or successful
  predecessor with its accepted input and turn;
- `session_plan_event` retains every exact-provenance event. On access, the
  trigger-only first-distinct-edge projection (max 32/entry) rejects headless,
  duplicate, nonchronological, over-limit, or cyclic state; `session_plan_head`
  certifies both tips;
- migration `202608020015` freezes `approval_posture` on each tool request,
  records dedicated approval-judge calls in the global model-call identity
  namespace only while their request is the current active approval wait,
  correlates delegate decisions to their completed call, selection,
  recommendation, and rationale;
- migration `202608110019` freezes one closed execution locus on each tool
  request. Daemon rows carry no runner facts; exact-runner rows carry one runner
  and positive historical registration revision backed by the append-only
  registration relation; capability-class rows carry only one checked catalog
  class. Existing rows and compatibility writers that omit the additive columns
  receive the daemon locus; production model-call persistence writes every
  selected locus explicitly;
- migration `202608030001` adds the typed `tool_approval_decided_outbox_event`
  family, appends one migration-boundary event for each explicit decision that
  already exists, and requires every later explicit decision to install exactly
  one ordered lifecycle effect and outbox event atomically; and
- the outbox family (below).

Representation rules, all enforced in the schema:

- Migration `202607300101` adds the optional session-template provenance pair
  (`template_name`, `template_content_digest`) to `session` and
  `create_session_command`. Both members are absent or present together; names
  satisfy the domain's 1-through-128-byte lowercase ASCII grammar and digests
  are exactly 32 bytes. The create-command row carries the same pair only at
  storage version 4 or a later version; versions 1 through 3 require two nulls,
  and the placement bump below neither removes nor re-gates the pair. A present
  pair also requires a nonnull command system prompt in both the schema and Rust
  reader. Reciprocal foreign keys bind every present pair across the creation
  command and its created session, so command replay and checked reconstitution
  cannot cross-wire provenance. Preexisting, imported, and explicit sessions
  carry two nulls. Both tables retain their append-only guards; no template
  catalog or mutable template object exists in Postgres (INV-047).
- Migration `202607280303` adds the optional bounded `system_prompt` column to
  `session_defaults_version` and the three defaults-bearing command tables, each
  guarded by the 1,048,576-UTF-8-byte and nonempty CHECK constraints and, on
  command tables, a version-three gate. A generated exact-encoding SHA-256
  digest column joins the selection key and the command/defaults agreement
  foreign keys, because megabyte text cannot join a btree key; the empty bytea
  stands for an absent prompt so a `MATCH SIMPLE` member never skips enforcement
  ([sessions-and-transcript](sessions-and-transcript.md)).
- Migration `202608030003` advances native creation to storage version 7,
  imported creation to version 5, defaults replacement to version 4, and
  submit-input to version 2 for their settings-bearing command payloads. Rust
  decoders require provider-default full settings or an inherit-all overlay on
  every earlier supported version. Imported-creation version 4 remains
  unsupported and reserved for its committed runner-placement shape. Existing
  queued configuration roots are marked as predating settings evidence; every
  root inserted after the migration requires a correlated
  `turn_model_settings_resolved` row, and transcript reconstruction treats its
  absence as corruption rather than legacy null.
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
  `awaiting_model_call_recovery`, `awaiting_tool_approval`,
  `awaiting_tool_recovery`, or `awaiting_runner_recovery`, terminal disposition
  `failed`/`completed`/`refused`/`cancelled`/`reconciliation_required`, attempt
  state `prepared`/`running`/`stop_requested`/`ended` with end variants
  `without_stop` and `after_cancellation`, and model-call state
  `prepared`/`in_flight`/`cancellation_requested`/`terminal` with terminal
  dispositions `completed`/`known_failed`/`refused`/`cancelled`/`ambiguous`.
- Migration `202608080100` closes runner placement history over
  `runner_lost_before_pin`, `pre_pin_replaced`, sourced `runner_lost`, and
  `abandoned` records. Each event retains the complete facts required by its
  state shape: pre-pin records retain exact request history without pinned or
  registration facts, while pinned loss and abandonment retain the complete
  pinned snapshot. Pre-pin reconstitution authenticates the revision-one request
  against the exact `created` record and reads every later replacement and lost
  predecessor instead of inferring history from a revision. The generic
  placement snapshot writer refuses loss, either replacement, and abandonment
  because those transitions require connection/loss, durable-command, scheduler,
  and outbox authority outside the placement aggregate. The connection-loss
  propagation adapter installs only loss transitions under those authorities;
  the abandonment transaction installs terminal abandonment after proving the
  active-turn slot empty, and the pre-pin replacement transaction installs an
  ordinary unpinned successor only for a different exact active runner whose
  current registration is connected and advertises every retained request axis.
  Its applied receipt retains and reads back that exact enrollment, registration
  revision, connection epoch, and connected event rather than trusting mutable
  current heads on replay. The pending-target arm additionally locks the exact
  provisioning-only request and candidate, requires its connected registration
  to advertise every retained request axis, then activates the candidate,
  revokes its recorded predecessor, and installs the same unpinned successor in
  one terminal commit. A mismatch or disconnected candidate is a typed durable
  rejection and leaves the pending authority intact. **Committed unimplemented
  functionality.** Pending-enrollment activation for a pinned placement,
  same-runner recovery, and pinned replacement remain for later dedicated
  transactions. Direct snapshot storage cannot stand in for any of them.
- Migration `202608110005` records the connection-loss epoch observed when each
  placement selects a known enrollment and carries that baseline through later
  loss or abandonment records. The value is derived while holding scheduler,
  enrollment, and connection/loss authority in the runner total order; callers
  cannot supply it. Initial pin carries forward an exact-identity selection's
  baseline, while a capability-class request records its first selected runner
  at pin. A loss that wins after exact selection cannot be hidden by
  reconnecting. Lease insertion compares its pinned placement with the
  enrollment's latest loss and remains fenced across successor physical
  connections until a checked replacement installs a fresh baseline. This is the
  implemented placement fence consumed by the bounded session-propagation
  transaction described below.
- Migration `202608110006` gives every new durable connection-loss epoch a
  pending propagation cursor in the same transaction. Migration backfill marks a
  loss completed only when no affected current placement remains: losses already
  absorbed into `202608110005`'s compatibility baseline complete, while a loss
  committed after that migration with an older placement baseline stays pending.
  A repeatable-read page authenticates the exact loss source and returns at most
  64 current pinned or exact-identity unpinned placements whose baselines
  precede that loss, ordered strictly after the durable session-identity cursor.
  An exact-identity selection stored before enrollment remains affected by a
  later loss for its selected runner despite having no enrollment baseline; the
  page and both cursor guards associate it through the runner identity. Cursor
  advancement is monotonic, cannot pass an affected current placement, and
  cannot complete while one remains. A per-session transaction locks the
  scheduler, authenticates the exact loss and cursor, then atomically changes
  placement, any current lease and physical attempt, an active runner-boundary
  turn, the runner-state outbox, and the cursor. An offered lease records no
  execution; a claimed pure or idempotent lease remains retryable in flight; a
  claimed side-effecting lease becomes terminal ambiguous. A separate checked
  operation completes an exhausted cursor. After an applied terminal connection
  transition or an exact replay of its current lost state, the daemon pages
  every pending loss, invokes the per-session transaction in page order, and
  completes each exhausted cursor. Startup performs the same scan after marking
  prior-process nonterminal connections lost, so a crash after the short loss
  transaction cannot strand session projection. **Committed unimplemented
  functionality.** No present daemon transaction retires an unacknowledged
  workspace release.
- Migration `202608110007` gives every changed registration beyond revision one
  a pending reconciliation cursor in its registration transaction. An ordered
  page returns at most 64 still-pinned sessions whose pinned registration is
  older, excluding sessions with an exact immutable observation. The
  scheduler-first per-session transaction locks enrollment, connection/loss,
  registration, cursor, placement, and any current lease in runner order. It
  applies domain availability reconciliation and records `preserved`,
  `runner_lost`, or `superseded`; registration loss stores an exact foreign key
  to the incompatible registration and a deferred exact loss observation. A
  current lease and active turn use the same attempt-loss and
  `awaiting_runner_recovery` transition as connection loss. SQL independently
  checks selector, sandbox, runner-required tools, credential profile, and
  repository/worktree availability against the named cause revision, and loads
  repeat that authentication instead of trusting a loss-source label. Cursor
  advancement cannot skip a current candidate or complete while one remains, and
  a newer registration cannot start while the current cursor retains a
  candidate. The daemon drains the cursor before acknowledging a changed
  registration and drains any crash-retained cursor before startup classifies
  old physical connections lost.
- Migration `202608110008` distinguishes active and replacement-pending
  enrollment receipts and stores each pending candidate's exact active
  predecessor and durable connection-loss epoch. The version-one adapter
  serializes pristine admission, returns an equal request's original receipt
  before inspecting the occupied slot, admits one pending candidate only while
  the current active predecessor is durably lost, and rejects another request
  while that slot remains occupied. Deferred checks require the pending state,
  receipt authority, predecessor enrollment, and loss source to agree. Pending
  authority may create only registration revision one and may open or resume a
  physical connection; later advertisement mutation remains refused. Existing
  request receipts migrate as active authority. Promotion is not part of this
  migration and remains a separate command-authorized transaction.
- Migration `202608110012` adds the typed `promote_pending_runner` command and
  atomically activates one connected pending candidate only while its exact
  predecessor remains durably lost. A reconnect and later loss leaves the
  immutable admission relation intact; activation checks and records the
  predecessor's current loss rather than requiring the admission loss to remain
  current. The durable command claim precedes the runner lock subsequence; the
  transaction then locks both enrollments in canonical identity order, both
  connection heads in runner-identity order, and the pending candidate's
  registration head. Success appends the predecessor revocation and candidate
  activation audit rows, advances both enrollment heads, and records the
  complete promoted-runner receipt together. The applied receipt retains the
  exact pending-request registration, candidate connected event, and predecessor
  loss epoch and source event checked by the transaction; deferred checks
  require the candidate's revision-one pending and revision-two active audit
  facts. Every refusal is a terminal typed command result, so equal replay is
  independent of later connection changes. The immutable pending relation
  remains as admission history, while an applied command is required for every
  pending-to-active state. No session scheduler, placement, grant, lease,
  workspace, turn, or frontier row participates.
- Immutable fact tables carry `BEFORE UPDATE OR DELETE` triggers that raise
  (`reject_immutable_record_change`), making append-only a database property,
  not a convention. This includes raw-record blobs and occurrences,
  imported-conversation headers and members, imported-frontier command records,
  session seed projections, metadata replacement receipts, and every existing
  historical fact. Metadata receipt satellites must be inserted before their
  deferrable parent record; their `BEFORE INSERT` triggers lock the existing
  user-global command claim, require that claim, and reject insertion once the
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
- Migration `202608080101` adds the `awaiting_runner_recovery` active phase to
  `turn_lifecycle` with payload columns total only for that discriminator: the
  exact lost runner, the positive placement revision the loss was projected
  against, and a nullable tool attempt naming the physical attempt the loss
  interrupted. Deferred checks require that runner and revision to name the
  session's current lost placement. A present tool attempt must be either the
  in-flight source retained by an exact retryable lease loss or the terminal
  ambiguous source of a side-effecting execution-possible loss. It must also be
  the current physical attempt for its request, be the attempt the loss
  recorded, and carry runner-lease lineage to that exact runner and placement
  revision; its issuing turn attempt must be the same yielded chain-tip that
  authorizes the wait, and its producing call must be the exact active
  tool-round boundary retained by that wait. A nullable interrupted-attempt arm
  admits a retained continuing tool round only when its current attempt
  inventory contains no prepared, in-flight, or ambiguous physical attempt;
  retired claimed-retry predecessors are historical inventory and do not block
  that arm. A present interrupted attempt must be the round's sole current
  prepared, in-flight, or ambiguous attempt. Lifecycle-side checks and reverse
  checks from placement heads, physical and turn attempts, lease events, and
  lease heads lock the shared session-scheduler row before evaluating the
  relationship, so a concurrent placement, attempt, or lease advance cannot
  leave a wait validated against stale loss evidence. The lifecycle transition
  matrix admits the phase from an already-active running boundary only after
  that exact live attempt has ended by yielding to a durable wait, never
  directly from queued work, and restart reconstitutes it from those correlated
  facts rather than from the stored discriminator. An interrupt closing the wait
  extends the retained active tool round's exact yielded frontier, or the turn's
  starting frontier when no tool round exists; the authenticated
  interrupt-effect record rejects any other same-session frontier. A retained
  round with no interrupted physical attempt appends its proposal-ordered tool
  closures before the cancellation entry. When loss interrupted an ambiguous
  physical attempt, the same stop instead commits the existing
  tool-reconciliation terminal shape, so cancellation never erases or
  reclassifies the ambiguity. Without this shape the loss transaction has
  nowhere to store the phase and restart cannot rebuild it. The same migration
  adds the optional interrupted-attempt fact to the exact placement-loss record,
  and the runner persistence read boundary round-trips both nullable arms. The
  runner-loss propagation transaction produces this phase under the lock order
  below. Independently of that writer, a present interrupted-attempt fact on the
  placement-loss record is admitted only for one of two exact lease-derived
  shapes: an in-flight retryable attempt whose loss proves no execution or whose
  pure/idempotent effect permits successor reissuance, or a terminal ambiguous
  side-effecting attempt whose execution may have occurred. Both carry physical
  runner-lease lineage to the record's exact lost runner and placement revision,
  and the same active runner-recovery tool-round boundary names the attempt.
  Stopping the wait retires retryable authority before releasing the active
  slot. The claimed-retry reservation writer takes that same scheduler lock and
  rechecks that the exact lease-derived source attempt remains in flight, so
  stale authority loaded before the stop cannot be reserved afterward.
  No-execution and pure work become known crash loss and cancel, while
  execution-possible idempotent work becomes ambiguous and requires
  reconciliation. A same-session foreign or older same-placement attempt
  therefore cannot survive placement readback.
- The same slice adds the closed `runner_placement_changed` semantic-entry
  payload: one positive placement revision, total only for that kind, with a
  foreign key to the same session's placement record at exactly that revision.
  At most one such entry exists per session and revision, the session
  placement-frontier pointer names the exact entry and revision, and a deferred
  check requires the entry to be the final member of the frontier that installed
  it. That frontier is exactly a one-entry extension of its physical semantic
  prefix, or a one-entry root only while the owning session has no earlier
  nonempty frontier; inherited entries retain their ancestor source session and
  are protected by that frontier-level check. Reconstitution resolves the
  referenced placement record and rejects a missing, cross-session,
  non-successor, or duplicated reference rather than rendering the entry from
  its own payload.
- Migration `202608110016` adds one append-only
  `runner_workspace_provisioning_authorization` row for a repository-backed
  pinned replacement command. It retains the command and session, exact lost
  placement event and revision, successor revision, selected enrollment, runner,
  current registration, connected event, repository, sandbox, and optional
  credential profile as typed columns. Composite foreign keys bind the command,
  historical placement, registration, connection, sandbox, and optional profile.
  A deferred relational check additionally requires the command target, current
  lost placement, current selected registration and connection, registered
  repository entry and workspace capability to agree at insertion. The
  same-runner arm requires registration-loss lineage; every other arm requires a
  distinct successor. The row rejects update, delete, and truncate, and its
  adapter readback rechecks the immutable joins before returning the stored
  facts. Migration `202608110017` extends the closed replacement-result refusal
  predicates to pinned loss and adds the production command-claim transaction.
  That transaction claims the immutable command request, takes the session
  scheduler before the selected runner and placement authority, and inserts
  either this nonterminal authorization or an exact durable refusal atomically.
  Equal replay returns the first authorization or refusal; unequal reuse returns
  a command conflict. A workspace-free or pre-pin placement rolls the claim back
  and returns `NotApplicable`, leaving its terminal replacement transaction as
  the only command finalizer. No transaction remains open across runner I/O.
- Migration `202608110018` separates the registration revision retained by an
  immutable pinned placement from the then-current registration revision that
  authorizes each lease offer. Existing lease generations preserve their
  historical revision during backfill. New offers must name the exact current
  registration under its guarded head, while their placement foreign key still
  names the immutable pin. Lease readback reconstructs placement revision,
  concrete execution directory, and sandbox from that exact placement record,
  and reconstructs immutable normalized arguments through the exact physical
  attempt and tool request. Both first insert and readback reject a
  caller-supplied disagreement.
- The runner-orchestration foundation adds one append-only
  `runner_operation_failure` record for every durably admitted
  `operation_failed` frame. It stores the exact runner, one closed
  `operation_kind` (`workspace_provision`, `workspace_release`, or
  `lease_offer`), the runner protocol's closed category, and the complete
  runner-authored detail as separate code, message, and exact JSON-object
  payload fields. The discriminator makes exactly one correlation arm total: the
  workspace-provisioning authorization identity; the retired session, positive
  placement revision, and workspace-manifest identity of a release; or the
  offered lease identity and positive generation. Composite foreign keys bind
  that arm to its typed authorization, release, or offered-lease record and to
  the same runner; a unique constraint permits only one retained failure for an
  exact operation correlation. Each arm admits exactly the category/correlation
  pairs owned by
  [runner protocol](runner-protocol.md#local-transport-and-connection-protocol);
  every other pair is rejected. Code, message, payload, aggregate detail size,
  JSON member-name grammar, container cardinality, and depth carry the exact
  checks owned by
  [runner protocol](runner-protocol.md#local-transport-and-connection-protocol);
  none is stored in a generic payload column or normalized on admission. The
  record and its JSON text reject update, delete, and truncate. Admission
  inserts that evidence in the same transaction that resolves the correlated
  operation as refused. Provisioning can then produce no `workspace_ready`
  receipt; a release can produce neither `workspace_released` nor a second
  refusal and is retired as refused; and an offered lease can produce no
  `lease_claim` and terminalizes with exact no-execution evidence. Deferred
  checks require exactly one of the operation's success and refusal proofs and
  preserve the failure after the mutable operation head retires. Equal
  retransmission rereads the equal record and returns
  `operation_failure_recorded`; unequal reuse is a correlation error. Why:
  acknowledging volatile detail would let a restart forget evidence operator
  inspection must reproduce, while delaying the operation transition until after
  acknowledgement would leave the runner resending a failure the daemon had
  already acted on.
- Both creation command families store the caller's optional placement.
  `create_session_command` and `create_session_from_imported_frontier_command`
  carry the complete request — selector kind with its runner identity or class
  name, working-directory selection, credential-profile name, workspace
  requirement with its repository key, and sandbox profile — under
  `CHECK`-constrained variant shape, with every member absent together for a
  daemon-only session, plus one append-only tool-override satellite bounded at
  64 rows per command. Each family advances one kind-scoped storage version for
  these columns, so a reader that supports only earlier versions rejects the new
  records instead of projecting a runner-backed creation as daemon-only. A
  present placement additionally requires the created session's revision-one
  placement record to carry the equal request, so replay and session state
  cannot disagree about what was requested.
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

One append-only, user-global `durable_command` registry claims every command
identifier: `command_id` is the primary key across all kinds and sessions
(INV-012), with a `CHECK`-closed kind set (`create_session`,
`create_session_from_imported_frontier`, `replace_session_defaults`,
`replace_session_metadata`, `submit_input`, `decide_tool_request`,
`review_workflow`, `review_orchestration`, `compact_session`, `goal`,
`update_session_placement`, `register_workspace`, `mint_git_remote`,
`withdraw_git_remote`, `promote_pending_runner`, `abandon_lost_runner`,
`replace_lost_runner`) and a kind-scoped `storage_version`. The gates above fix
the current numbers: create-session records write version 8, imported-create
records write version 6, replace-defaults records write version 4, and
submit-input records write version 2; every other closed kind writes version 1.
The four settings-bearing families require the migration's provider-default full
settings or inherit-all overlay on every earlier supported version.
Create-session records reconstitute version 1 with the disabled dangerous-tool
posture, and versions 1 and 2 with no system prompt — a pre-version-three row
carrying one fails closed in both the schema and every Rust reader. A
pre-version-four create row carrying template provenance and a pre-version-six
create row carrying path placement likewise fail closed. Imported-create version
4 remains unsupported compatibility space for committed runner placement, so the
model-settings writer skips it. Metadata, decision, review-workflow, compaction,
and runner-recovery records use version 1. Each kind has one typed subordinate
request record keyed by `command_id` that stores every caller-supplied semantic
field in typed, `CHECK`-constrained columns. Every kind except runner
replacement also stores the terminal `applied`/`rejected` result and typed
result fields there. `replace_lost_runner_command` is the immutable request and
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
their typed record exactly as applied results do. User-specified pre-claim
admission errors are different: after registry inspection, a missing
conversation named by the selected imported frontier or a boundary absent from
that conversation returns without inserting a claim or typed record. Replay
resolution — reconstruct the recorded command, compare structurally, return the
recorded result or `ConflictingReuse` — follows the owning page's contract.

`load` operations return `None` only for an unseen identifier; a claimed row
that cannot be reconstructed is corruption, never an unclaimed identifier.

## Lock protocol

Every Rust-issued SQL statement that takes an explicit row lock lives in
`crates/persistence/src/lock_inventory.rs`. Twenty-seven explicit lock
statements live in the schema instead:

- the deferred pending-steering source-turn trigger (migration `202607180005`)
  takes `FOR UPDATE` on the named `turn_lifecycle` row when a pending-steering
  `accepted_input` insert reaches commit;
- the metadata receipt-satellite insert trigger (migration `202607260101`) takes
  `FOR UPDATE` on the already-claimed `durable_command` row before it checks
  whether the typed receipt parent has sealed the command;
- `next_session_plan_event_ordinal` (migration `202608020011`) takes
  `FOR NO KEY UPDATE` on the plan's session before reading its certified head;
- the session-plan append trigger in that migration reacquires the session
  `FOR NO KEY UPDATE`, then takes `FOR SHARE` on the exact active `plan_write`
  attempt while authenticating its request payload;
- the goal-event current-turn helper (migration `202608020013`) takes
  `FOR NO KEY UPDATE` on the event's session row before reading the latest goal
  turn;
- the scheduler-failure correlation trigger in that migration takes `FOR SHARE`
  on the named `turn_lifecycle` row while checking its unsuccessful terminal
  disposition; and
- the goal-event continuity trigger in that migration takes `FOR NO KEY UPDATE`
  on the event's session row before reading the preceding event, serializing
  ordinal and generation assignment even when the Rust transaction reached that
  row first with `FOR NO KEY UPDATE`;
- the approval-judge insert guard (migration `202608020015`) first takes
  `FOR UPDATE` on the `tool_request` row and then on the request's active
  `turn_lifecycle` row before it admits a prepared judge call; and
- the deferred approval-decision authority trigger in that migration takes
  `FOR UPDATE` on the `tool_request` row before it checks for a nonterminal
  judge call and validates the decision's frozen-posture authority; and
- the runner-recovery completeness checker and its placement-, attempt-, and
  lease-side rechecks in migration `202608080101` each take `FOR UPDATE` on the
  session scheduler row before re-reading the active recovery lifecycle,
  placement, and execution-loss relationship; and
- the turn-attempt and tool-round before-insert guards in that migration share
  one `FOR UPDATE` helper that serializes new continuation evidence against the
  same session scheduler before either immutable row becomes visible; and
- the lease-offer connection-loss fence in migration `202608110004` takes
  `FOR SHARE` on the selected enrollment and connection authority head, then on
  the optional current loss head when the connection is terminal; and
- the lease-claim connection-loss fence in migration `202608110004` takes
  `FOR SHARE` on the selected enrollment and connection authority head before
  admitting the claim event; and
- the placement-loss baseline trigger in migration `202608110005` takes
  `FOR UPDATE` on the session scheduler, then `FOR SHARE` on the selected
  enrollment, connection authority head, and optional current loss head before
  deriving the immutable baseline and before the placement row becomes visible.
- the pending-successor registration and connection guards in migration
  `202608110008` take `FOR SHARE` or `FOR UPDATE`, respectively, on the
  candidate enrollment before admitting its first registration or a physical
  connection; and
- the pending-successor admission guard in migration `202608110012` checks the
  predecessor's exact current lost connection head under `FOR SHARE` when
  inserting the immutable relation. Later activation uses the reviewed Rust lock
  subsequence above; the relation remains historical if the predecessor
  reconnects; and
- the lease-offer registration fence in migration `202608110018` takes
  `FOR SHARE` on the selected enrollment's current registration head after the
  connection-loss offer fence has acquired enrollment and connection authority.

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

- **ContextCompaction**: after claiming an unseen user-global command,
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

- **Goal commands and transitions**: an unseen user command first claims the
  user-global registry, then every user, model, scheduler, and continuation
  transaction locks the session row `FOR NO KEY UPDATE` before reading the event
  stream. An applied user transition next locks `session_scheduler` `FOR UPDATE`
  before recording its receipt or event, so stop and queued-turn activation
  share one serialization point. Deferred provenance correlation first
  reacquires the session-row lock before checking the current goal turn and, for
  scheduler failure, holds the named lifecycle row `FOR SHARE` while checking
  its unsuccessful terminal disposition. The continuity trigger reacquires the
  session-row lock before validating the predecessor. Pursuing user transitions
  then read current defaults and insert their queued goal turn; rejected
  commands commit without firing the trigger, and exact user-command replay
  takes no row lock.

- **StartEligibleTurn** and nonterminal **model-call execution transactions**
  (prepare and authorize): the `session_scheduler` row `FOR UPDATE` is the only
  explicit lock (session existence is checked with a bare `EXISTS`). The session
  row is locked only `KEY SHARE`, implicitly, by the inserts' foreign keys, and
  the candidate `turn_lifecycle` row is locked by the guarded `UPDATE` itself.
  Terminal observation commit and reread, restart recovery, startup recovery,
  and submit-input interruption first discover whether the target is a delegated
  child. When it is, they lock the immutable parent/child session pair
  `FOR NO KEY UPDATE` in canonical session-ID order before taking the child
  scheduler lock. This is the shared prefix for any path that can record a child
  result. **Committed unimplemented functionality.** No present migration or
  repository operation stores pool state, capacity reservations, or availability
  waits, so the credential-pool locks described in the rest of this bullet are
  the protocol its implementing child must follow, not a guarantee this build
  provides. This bullet is the whole of that protocol: which objects each
  credential-pool transaction takes, in what order, and in which mode is stated
  here and nowhere else.
  [The credential-availability machine](credential-availability.md#the-credential-availability-machine)
  names the transaction that commits each selection ending, which is how a
  reader arrives at the right sentence below; it states no locks of its own and
  must not be consulted for any. A transaction that admits a wait or proves
  exhaustion without preparing a call takes the scheduler lock and the action
  heads below and no cursor row, because it selects nothing; the admission that
  creates a contended wait additionally holds capacity rows, as stated further
  down. Credential-pool call preparation additionally locks the action head of
  every member of the pinned policy it may select, in profile-reference byte
  order, immediately after the scheduler lock and before it reads any exclusion
  state. The mode follows what the transaction does to that member and not which
  path reached it: `FOR SHARE` for a member whose exclusion state it only reads,
  and `FOR UPDATE` for one it writes — including the member whose pending
  `switch_next_turn` displacement a successful preparation consumes, which is a
  write that reads like a read. This holds for every preparation alike,
  including the one that releases a credential-availability wait, so no path
  acquires a weaker mode by arriving through a wait. It then locks every
  potentially selected bounded profile's shared capacity row `FOR UPDATE`, in
  the same order, before reading reservation counts. When `round_robin` decides
  among the first admitted priority's members, preparation next locks that
  immutable-policy-and-priority cursor row `FOR UPDATE` before reading the
  cursor, choosing a member, or advancing it with `Prepared`. It rereads the
  protected selection facts after acquiring each lock and holds all of them
  through the `Prepared` insert. A transaction that mints, activates, or clears
  a credential exclusion — a terminal observation applying a pool trigger, a
  delivery-layer quarantine, or an operator clear — takes those same action
  heads `FOR UPDATE` at that same ordering position. Share and exclusive modes
  conflict, so one of the two transactions waits: a `Prepared` insert either
  precedes the exclusion commit or reads the member as already excluded. This is
  what a selection needs when it takes no other lock the writer takes — an
  unbounded `first_listed` member acquires neither a capacity row nor a cursor
  row. Releasing a bounded profile's reservation takes that profile's capacity
  row `FOR UPDATE` at the same position and holds it through the atomic
  release-and-wake commit, and a woken waiter rewriting its own wait evidence
  takes the capacity rows of every bounded member that evidence names, in the
  same byte order. A release and a snapshot rewrite therefore cannot interleave:
  no wait can commit naming a reservation another transaction has already
  released, and no release can publish its wake between a loser's read and its
  rewrite. No other path may take a scheduler lock while holding an action-head,
  capacity-, or cursor-row lock; take an action-head lock while holding a
  capacity-row or cursor-row lock; or take a capacity-row lock while holding a
  cursor-row lock.

- **Tool-loop transactions** (user decision, attempt prepare, attempt
  authorization, preflight failure, result commit, crash classification, result
  projection plus continuation preparation, and their authoritative rereads):
  the `session_scheduler` row `FOR UPDATE` is the first and only Rust-issued
  explicit lock. An unseen decision command first claims the user-global
  registry; after resolving the request's owning session it takes that scheduler
  lock before reading or mutating the active tool batch. A replay resolves
  entirely from the command registry and receipt and takes no lifecycle lock.
  Guarded `turn_lifecycle`, `turn_attempt`, `tool_attempt`, and model-call
  updates then serialize under the scheduler lock; their foreign keys may take
  implicit `KEY SHARE` locks on parent rows. At decision commit, the deferred
  authority trigger takes the `tool_request` row `FOR UPDATE` after the
  scheduler lock and before checking that no nonterminal judge remains.

- **Approval-judge transactions** (prepare, authorize, complete, and fail): the
  `session_scheduler` row `FOR UPDATE` is always the first Rust-issued explicit
  lock. Preparation then inserts the call; its schema guard takes the exact
  `tool_request` row `FOR UPDATE`, followed by the active `turn_lifecycle` row
  `FOR UPDATE`, before checking for an existing decision and validating the
  prepared call. Completion performs its guarded lifecycle transition under the
  scheduler lock; at commit, the deferred decision-authority trigger then takes
  the `tool_request` row `FOR UPDATE`. Authorization and failure need no
  additional explicit lock. The shared scheduler-first prefix prevents
  approval-judge, tool-loop, and lifecycle-transition transactions from holding
  these rows in reverse order.

- **Delegated terminal-observation transactions**: after nonlocking reads of the
  call's turn and delegation identity, observation commit and authoritative
  reread lock both endpoint session rows `FOR NO KEY UPDATE` in ascending
  session-identity order, then both endpoint `session_scheduler` rows
  `FOR UPDATE` in that same order, and only then the exact `session_delegation`
  row `FOR UPDATE`. They revalidate the immutable relationship after taking that
  prefix. A nondelegated observation retains the ordinary scheduler-only model-
  execution order. Sharing the delegated prefix with peer-message transactions
  prevents either side from holding an endpoint session while waiting for a
  scheduler held by the other.

- **Delegated restart-recovery transactions**: a nonlocking read selects the
  candidate active child turn. Recovery then takes the same canonical endpoint
  sessions, endpoint schedulers, and relationship prefix as terminal observation
  before it rechecks the active turn and classifies any model-call or
  tool-attempt loss. If the candidate changed while the prefix was acquired, the
  transaction rolls back for a later scheduler pass. Known tool-crash failure
  and its typed parent result therefore commit under the same acyclic endpoint
  order as peer messages.

- **Delegated await transactions**: await first locks its issuing delivery
  session row `FOR NO KEY UPDATE`, then that session's `session_scheduler` row
  `FOR UPDATE`, and only then the exact `session_delegation` row `FOR UPDATE`.
  This session-before-scheduler prefix matches input transitions that can race
  with await registration.

- **Input submitted to a delegated child**: after a nonlocking immutable
  relationship read, submission locks both endpoint session rows
  `FOR NO KEY UPDATE` in ascending session-identity order, then the child's
  `session_scheduler` row `FOR UPDATE`. Nondelegated input retains its single-
  session prefix. The endpoint prefix precedes the scheduler because processing
  the input can terminalize the delegated turn and publish its parent result.

- **Descendant-scoped stop and interrupt transactions**: after registry
  inspection and an unseen command claim, but before the ordinary root-session
  or scheduler locks, the repository locks the complete reachable session
  frontier in ascending session-identity order. When the root is itself a
  delegated child, that same ordered set includes its immutable parent endpoint,
  so the later child-terminal prefix reacquires only rows already held. It
  re-evaluates the descendant frontier after waits and then locks its
  relationships in spawning-request order. The goal-stop and input-interrupt
  writers share this prefix; parent-alone commands take no descendant locks.

- **Delegated peer-message transactions**: after a nonlocking peer-existence
  read, message recording locks both endpoint session rows `FOR NO KEY UPDATE`
  in ascending session-identity order, then both endpoint `session_scheduler`
  rows `FOR UPDATE` in that same order, and only then the exact
  `session_delegation` row `FOR UPDATE`. An absent peer instead locks the
  issuing session and scheduler before returning the typed rejection. This
  common endpoint order is acyclic with delegated-child input and opposite-
  direction message transactions. Child terminalization uses the same canonical
  endpoint-session prefix before taking the child scheduler, so message,
  completion, and input submission never hold those row classes in reverse
  order. After locking the relationship, a fresh message reads only its
  immutable endpoints and bounded lifecycle/event frontier; it does not
  reconstruct prior messages. Delivery-sequence allocation runs while the
  recipient session lock is held. Message recording claims the global
  `message_id` before inserting the relationship event; a concurrent claim loser
  returns the typed message-identity collision without leaving a partial event.

- **ReplaceSessionDefaults**: an unseen command locks its
  `session_current_defaults` pointer row `FOR UPDATE` before loading and
  preparing against the current epoch. The compare-and-set `UPDATE` on that
  already-locked row remains the applying check. Rejection-only admission uses
  the same lock: a current expected version rolls back the command claim and
  applies nothing, while a mismatch records the typed rejection. The
  `session_defaults_version` insert takes `FOR KEY SHARE` on the session row
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

- **UpdateSessionPlacement**: an unseen command locks the target's
  `session_current_placement` head `FOR UPDATE` before checking the expected
  version, appending the next immutable placement event, and advancing the head.
  Exact replay and conflicting reuse resolve from the command registry without
  taking that lock.

- **SessionPlan append**: ordinal allocation locks the session row
  `FOR NO KEY UPDATE` before reading the trigger-maintained head. The adapter
  uses the inventory's `PLAN_APPEND_ATTEMPT` statement to lock the exact active
  tool attempt `FOR SHARE` while authenticating its request. The insert trigger
  reacquires locks session-then-attempt, caps distinct edges, and rejects cycles
  with node-deduplicated reachability. It projects first occurrences while
  advancing both heads. Reads fetch at most 32 direct dependencies per returned
  entry after verifying both heads; they never load transitive closure.

- **Runner total order**: every transaction that takes more than one runner
  authority lock uses the same applicable subsequence, omitting absent rows but
  never reordering them: `session_scheduler` when present; current enrollment or
  pending replacement-request heads in canonical identity order; runner
  connection/loss heads in runner-identity order; current registration head;
  placement; current credential grant; lease; operation-failure evidence after
  its correlated operation; and only then semantic-frontier and turn rows. A
  durable user-command claim precedes this subsequence.

- **Runner enrollment and registration**: the current enrollment or pending
  replacement-request head is locked first, followed by the relevant runner
  heads in runner-identity order and then the current registration head.
  Activating a pending replacement retires the old enrollment, persists the
  issued successor identities and registration, and installs the user-command
  effect in one transaction. Deployment-scoped promotion
  (`promote_pending_runner`) uses that same subsequence, takes no
  `session_scheduler`, placement, grant, or lease lock because it changes none
  of them, and commits its claim, activation, and terminal result together.

- **Runner registration reconciliation**: the registration append locks the
  enrollment and current registration head, refuses a newer revision while the
  prior cursor still owns a candidate, and creates the next cursor before
  advancing the registration head. It does not take a session lock. Each
  restartable session projection takes `session_scheduler` first, then
  enrollment, current connection/loss, current registration, the exact cursor,
  placement, and a current lease when present. It records the exact registration
  observation and cursor advance in that transaction. Completion takes the same
  enrollment-through-registration prefix and the cursor but no session lock,
  after proving that no unobserved candidate remains.

- **Runner dispatch and result**: `session_scheduler` is the first lock,
  followed by enrollment, current runner connection/loss, registration,
  placement, current credential grant when present, and lease heads in the total
  order above. Every offer authenticates the canonical approved request's
  selected runner locus. Dispatch through an existing pin revalidates the
  caller's frozen enrollment and registration revision, derives the tool only
  from that request, and commits its `InFlight` attempt and offered lease in one
  transaction. The workspace-free exact-directory initial transaction follows
  the same order and atomically installs the first pin and optional grant before
  the attempt and lease, with the `Pinned` runner-state outbox event sourced
  from that exact placement record. It rejects `RunnerDefault` and every
  workspace requirement because neither has an authenticated working-directory
  receipt at this boundary. A stale registration or unsupported composition
  rolls back all facts. Claim locks the session scheduler first, followed by
  enrollment, runner connection/loss, current registration, and lease. It loads
  the canonical offer, checks the complete immutable correlation, commits the
  claimed lease, and returns that canonical value for acknowledgement. A
  compatible later registration may preserve an offer, so claim serializes on
  the current registration head without replacing the offer's immutable
  registration revision. Claim and connection loss therefore serialize, and the
  winner determines whether an execution capability exists. Result admission
  locks the session scheduler and current lease head, loads the canonical
  claimed lease and active tool batch, derives result-only authority from the
  exact in-flight attempt, and commits the lease completion and terminal attempt
  together. Duplicate, stale, or cross-wired evidence advances neither
  aggregate. Ambiguous external-effect evidence also ends the issuing turn
  attempt and enters the exact `awaiting_tool_recovery` wait in that
  transaction. Authenticated retained-result admission takes the same scheduler
  lock before it verifies the immutable enrollment request, all issued
  identities, a non-revoked enrollment, and the caller's prior registration
  revision against the current advertisement. It then invokes that same result
  boundary before any later registration change or availability reconciliation
  can consume the attempt. An exact completed-result replay is read-only. No
  daemon resume path invokes this retained-result transaction before processing
  a later registration change. The generic lease-projection writer refuses to
  originate a claimed or completed state. **Committed unimplemented
  functionality.** No present transaction performs an initial dispatch that
  consumes a runner-default-directory or workspace receipt. That extension must
  store pin, grant, `InFlight` attempt, and offered lease together under the
  same lock order.

- **Runner loss**: one short transaction locks only the current connection/loss
  head, advances a positive durable loss epoch, and thereby makes every trigger
  reject new offers or claims from that connection. It never holds that global
  row while waiting for a session lock. A restartable propagation cursor pages
  at most 64 affected session identities in order; each session is updated in
  its own transaction by locking `session_scheduler` first, then the loss head,
  placement, current lease, and guarded turn rows. Offered leases with no
  durable claim acquire exact no-execution proof; claimed leases follow effect
  loss law. That same session transaction retires any unacknowledged release the
  lost connection still owed, since no successor inherits authority to complete
  it. A crash resumes at the first uncommitted session, while every
  not-yet-projected placement is already effectively lost through the epoch
  fence.

- **Runner replace, abandon, and release**: an unseen abandonment command owns
  its durable-command claim and terminalizes in one transaction. An unseen
  repository-backed pinned replacement command first claims its immutable
  request and provisioning authorization in a short transaction. That staging
  transaction locks `session_scheduler`, the selected enrollment or pending
  request, its connection and registration, and then the lost placement; it
  accepts an ordinary successor only when it differs from the lost runner, and
  accepts the same runner only for an explicit reenrollment after exact
  registration-triggered loss. It performs no runner I/O under database locks.
  The later terminal transaction follows the runner total order exactly:
  `session_scheduler`, enrollment or pending-request heads, relevant runner
  heads in identity order, registration, placement, grant and lease, then
  guarded semantic-frontier and turn rows. Replacement rechecks and atomically
  activates one pending enrollment, consumes workspace-ready evidence when
  required, installs the placement frontier, and appends the terminal command
  result. A crash before that result leaves the immutable request and
  authorization resumable. Abandon requires an empty active-turn slot and stores
  only terminal placement state. Either transition enqueues a release for the
  retired placement only when two independent conditions both hold. The
  placement must hold a runner-managed workspace — a provisioned repository
  worktree or the runner's own private root — because only those carry the
  workspace-manifest identity the release frame correlates against; a retired
  placement whose writable root is the plain directory its own request named
  enqueues no release at all, since the runner never created that directory and
  must never delete it. And the runner that created that workspace must still be
  reachable on a live connection, because only a reachable runner can produce
  the acknowledgement or cleanup-failure report a release waits on. A retirement
  whose predecessor connection is already durably lost — heartbeat-loss
  replacement onto a different runner or onto a pending enrollment, and every
  abandonment — therefore enqueues nothing either, and its workspace takes the
  recorded-leak response a retired runner's workspace already has. In version
  one the release exchange exists only for the checked same-runner
  re-enrollment, where registration reconciliation retired the placement while
  the connection and enrollment stayed healthy
  ([runner protocol and placement](runner-protocol.md#workspace-provisioning-and-recovery)).
  Why: both frames that can retire a release require the holding runner to
  acknowledge deletion or report cleanup failure, so a release addressed to an
  unreachable identity is a durable record redelivered after every restart with
  no transition able to clear it. Release acknowledgement uses the same
  scheduler-then-placement order and never mutates turn lifecycle. Three
  transitions retire the durable release record and no other does: the release
  acknowledgement itself; durable admission of the runner's
  `workspace_cleanup_failed` operation failure naming that same release, which
  resolves it as refused when the runner cannot complete the deletion; and
  durable loss of the connection that owed it, which resolves it as unowned and
  leaves its workspace under that same recorded-leak response. Until one of the
  three commits, an unacknowledged release is redelivered after restart exactly
  as an unacknowledged result is.

- **Runner operation failure**: durable admission takes `session_scheduler` for
  the correlated session, then the applicable enrollment, connection/loss,
  registration, placement, grant, and lease rows in the runner total order.
  Provisioning and release omit the absent lease row; lease refusal includes it.
  Only after those authority rows are locked does the transaction insert
  `runner_operation_failure` and install the correlated refused/no-execution
  transition; a release refusal also retires that exact release in the same
  transaction. It never performs runner I/O under those locks. A simultaneous
  claim, workspace receipt, release acknowledgement, loss transition, or
  duplicate failure therefore wins the shared authority row and makes the loser
  reread the one committed terminal proof instead of committing both outcomes.

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

Runner-status reconstitution applies the process protocol's closed exclusive
evidence cursor to retained operation failures and to each runner's currently
published, final-acknowledged workspace-leak snapshot. Staged, interrupted, and
superseded leak reports are never readable. Evidence-kind order is failures
before leaks. A null or operation-failure cursor continues failures exclusively
in runner-and-correlation order and, only when that query leaves capacity,
continues from the first leak; a workspace-leak cursor skips failures and
continues leaks exclusively in their complete runner-and-fact-tuple order. The
two queries share one limit of `page_size + 1` checked rows, and the extra row
is used only to produce the continuation cursor; neither query materializes the
retained evidence history.

For each emitted failure, the adapter loads the complete typed target beside the
failure row, decodes the category and every detail bound, requires the target's
runner and full correlation to equal the retained arm, and requires the target
to carry the matching refused/no-execution terminal proof. For each emitted
leak, it loads the published-snapshot header beside the fact row, requires the
header to be the runner's final-acknowledged current snapshot, and validates the
exact runner, closed kind, locator, digest, optional session and placement
revision, and strictly increasing unique fact tuple admitted by runner protocol.
A missing target or snapshot, staged or superseded snapshot membership,
runner/correlation mismatch, success/refusal conflict, invalid failure detail,
impossible category/arm pair, duplicate correlation, malformed leak fact, or
duplicate leak tuple is typed corruption; it is never dropped from the page
count. The checked evidence is projected verbatim by `read_runner_status`, so
restart and continuation reproduce exactly what durable admission acknowledged.
Failure records remain append-only without a retention cap; bounded reads,
rather than lossy retention, keep an arbitrarily long history inspectable.

Persisted data is never normalized into a nearby valid state; malformed durable
rows produce typed corruption errors, authorize no effect, and are not repaired
or dropped on load. An applied metadata receipt load also validates the stored
current-root proof against both the command target and result target. A metadata
list load validates the complete stored content, including attributes that its
public summary deliberately omits, before it constructs the summary. Load paths
do not panic on durable data; checked interrupt application produces the exact
cancellation-requested or reconciliation-required transition, while a projection
that cannot support that transition fails closed as typed corruption. Startup
recovery operates only on successfully reconstituted projections (INV-034). A
stored active delegation-origin lifecycle whose phase is null or outside the
closed phase vocabulary is corruption rather than retryable database failure. A
successful reconstitution does not waive the guarded compare-and-set when a
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
- `Column(field)` — a declared SQL field failed decoding, classified by a static
  field label rather than driver prose;
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
every commit-classifying persistence adapter. This inventory excludes the
phase-insensitive conversation-import repository, whose conservative wire
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

## Delegation storage and locking

**Implemented behavior.** Migration `202608020018_session_delegation.sql`
retains the closed `parent_alone` or `parent_and_descendants` selection on every
stop goal command and interrupt submit-input command. The accepted-input copy
retains the same value for an applied interrupt. Other goal operations and
delivery kinds require a null scope. Command and accepted-input reconstitution
decode the stored selection without substituting a default, so equal replay
returns the recorded result and changed-scope reuse conflicts.

This section is the foundation proposal for migration
`202608020018_session_delegation.sql` and becomes verified only with the full
delegation stack. The migration widens `session.creation_cause` with
`delegated`, adds the spawning request column required only by that cause, and
keeps `ancestry_kind = none` as an independent required fact. The deferred
session-creation-family check admits a delegated session only when one complete
`session_delegation` row names it; user and imported creation families remain
unchanged.

`session_delegation` is append-only and keyed by the globally unique spawning
`tool_request_id`. It correlates that request's parent session and turn, one
unique child session, the closed relationship-policy kind, and the two bound
actions where required. Composite foreign keys prevent cross-session request
use. Admission locks the parent relationship inventory, checks request and child
uniqueness without a fixed active-child-count limit, and inserts the child
session, scheduler/default rows, initial task work, relationship, and spawn
event in one transaction. The same transaction resolves the immutable defaults
row named by the parent turn's frozen defaults version and copies its complete
value into the child's defaults version one; the mutable current-defaults
pointer is not a source for delegated creation.

Initial task work is one delegated-task origin row plus its semantic entry and
first queued turn. The origin references the spawning request and repeats no
independent actor claim; deferred checks resolve that request's checked task,
parent session and turn, child relationship, semantic entry, and turn starting
frontier as one closed shape. It stores the exact requested and frozen model
configuration inherited from the parent turn, including a direct override or a
frozen alias definition and its selected direct model; an equal effective model
does not authorize reconstructing the request as a session default. No
accepted-input row is inserted. The ordinary eligibility pass recognizes that
typed origin directly, activates it without fabricating an accepted input, and
starts model execution from the existing delegated-task semantic entry as the
child's one-member initial frontier. The same typed path activates an idle
recipient's delivery-range wake after its exact terminal predecessor, appending
the contiguous checked message or background-result entries to that predecessor
frontier. Any accepted-input scheduling reread that retains delegation-origin
semantic history supplies each referenced delegated turn's independently stored
defaults version, selected direct model, and active, logical-terminal, or
physical-terminal lifecycle classification. Model-identity, terminal semantic,
and completed-call facts must match that projection; a turn identity alone is
never sufficient reconstitution authority.

`session_delegation_event` is an append-only per-relationship ordinal stream.
Its closed kind/shape checks require every lifecycle disposition to carry one
typed reason and complete provenance columns: the spawning request and either
the exact child turn, the exact parent turn command, or the exact parent goal
command. Parent-turn provenance carries parent session, turn, and durable
command; parent-goal provenance instead carries parent session, positive goal
generation, and durable command, with no turn column populated. The two
parent-command arms are exclusive. Continue-running and already-terminal are
real event kinds, not absence of an evaluation row. An already-terminal event
requires the relationship's unique prior child-result row, records the new
parent command that evaluated the edge, and creates no second child result.
Deferred relationship-state checks reject a terminal or continued outcome
without its event, two terminal child results, ordinal gaps, and an event whose
reason/provenance shape does not match its kind.

`session_delegation_wait` records the exact awaiting tool request, relationship,
parent turn, and foreground/background mode. A foreground row correlates the
turn's `awaiting_child` phase; a background row cannot and instead requires the
exact completed effect-free attempt and normalized registration receipt for its
awaiting request. Equal wait replay independently authenticates that exact
terminal attempt: the foreground arm requires its typed child-wait evidence and
the background arm requires its normalized receipt; both arms also authenticate
the exact update satellite and global outbox header. A definitive process-wait
rejection stores its closed rejection kind and transition evidence when
applicable beside the exact known-failed attempt; exact replay returns that
typed outcome before classifying the request as non-executable.
`session_message` is append-only, uniquely orders messages per relationship, and
requires exact parent/child sender and recipient plus the sending tool request
with its complete session, turn, and request provenance. Equal message replay
authenticates both that provenance and the exact completed external-effect
attempt carrying its normalized receipt, plus the exact update/wake satellites
and their global outbox headers. A concurrent global `message_id` claim loser is
a typed message-identity collision, not an unclassified database failure. A
definitive process-message rejection stores its closed rejection kind,
transition evidence when applicable, and originally minted message identity
beside the exact terminal attempt; exact replay returns that typed outcome
before classifying the request as non-executable. `session_child_result` has at
most one row per spawning request and carries exactly one returned-text, failed,
stopped, or cancelled shape with child turn provenance for returned, failed,
result-unavailable, and child-originated terminal outcomes, or one of the same
exclusive parent-turn-command and parent-goal-command provenance arms for a
policy-driven stop or cancellation. A known provider failure, a pre-send
capability failure, or a known effect-free tool-crash closure publishes the same
typed failed child result in its terminal transaction. An authoritative reread
of an ambiguous pre-send capability failure authenticates that exact delegated
result plus its parent update, wake, and both delegation outbox headers before
reporting the failure committed. Delivery satellites bind messages/results to
their exact semantic entries; no transcript query supplies result content. Every
pending message and background result delivery additionally receives one
positive recipient-wide `delivery_sequence` under the recipient session lock.
That sequence is unique and gap-free per recipient across both kinds;
relationship ordinals remain relationship-local evidence and never order two
different relationships. Foreground results stay ordered by their exact awaiting
request and do not consume an inbox sequence. Their semantic entry repeats that
awaiting request as the ordinary logical tool-result correlation, so the
unchanged proposal-order and single-result checks admit it as the
`await_session` result without admitting a second result for the same request.
Tool-batch outbox decoding and context-compaction evidence count that foreground
correlation as one tool result; a background result has no tool-result
correlation and counts as neither one.

An accepted background wait reserves one future recipient delivery position
until its child result exists. Message and later-wait admission under the same
recipient lock preserve all outstanding reservations; exhausted capacity is a
typed definitive rejection, and a reconstituted executable process request
commits its known-failed attempt end in that same transaction. Thus a child
terminal transaction cannot be rolled back merely because unrelated messages
consumed the result delivery's final position.

`session_delegation_wake_turn_origin` distinguishes an idle-recipient wake from
the delegated child's initial task. It binds the queued turn to one contiguous
recipient delivery range and the terminal predecessor's exact requested and
frozen model configuration. While the turn is queued, later deliveries may only
extend that range; activation freezes it. The requested selection, frozen
selection, and defaults version must equal the exact terminal predecessor's
configuration. The starting frontier extends that predecessor with every typed
message or background-result semantic entry in delivery order, with the final
delivery as the lifecycle origin entry. The schema admits at most one queued
delegation-origin turn per recipient.

Parent-and-descendants termination locks the root and complete reachable session
frontier in ascending session-identity order before the command repository takes
its ordinary root or scheduler locks, before inserting the applied parent
command, and therefore before any outbox allocation. It re-evaluates after a
lock wait, then locks relationship rows in stable spawning-request order before
it writes any disposition. Spawn admission takes the same parent-session lock,
so spawn, message, and cascade transactions do not invert the session/outbox
lock order or omit an edge that committed while the cascade waited. The command
and every evaluated edge commit together; a crash can leave all prior durable
state or the complete typed evaluation, never an unrecorded partial cascade.
Parent-alone takes no descendant authority. Deferred reverse constraints also
reject an applied descendant-scoped root command that lacks its exact cascade
row, so an omitted cascade writer fails closed instead of silently degrading to
parent-alone. Background and bound-keep-running edges still receive a
continue-running event when evaluated. An already-terminal edge receives its
typed already-terminal event and traversal continues through that child's
outgoing relationships, so a terminal intermediate session cannot hide live
descendants.

**SPEC PROPOSAL — cascade terminal authority.** For each newly stopped or
cancelled edge, the cascade transaction appends one immutable logical-terminal
row keyed by spawning request, child session/initial turn, and root command. The
row foreign-keys the exact per-edge parent-termination authority and cannot
commit without the matching parent-provenanced relationship event, unique child
result, update, deliveries, and wake. The same transaction materializes an
immutable retained terminal frontier and a one-to-one monotonic lifecycle flag.
Partial active and queued indexes exclude the flagged row, while queue-order and
start-frontier validation continue to recognize it as the immediate terminal
predecessor of later accepted-input or delegation-wake work. Runtime eligibility
therefore excludes the proof without rewriting the child's retained physical
execution evidence. Provider observation commit rereads the proof under the
session lock and discards a late response instead of persisting it. Transcript
reads join the proof to its exact outcome event and expose the typed logical
terminal state. This proposal is accepted with the implementing stack's merge.

The scheduler sweep treats a deliverable foreground result, an undelivered
background result, and a pending message inbox as durable hints. Every result
and message commit also writes exactly one distinct parent- or recipient-scoped
`delegation_wake` outbox event in the same transaction. A consumer may ignore
the nudge while that session is already active. When a foreground wait is
registered after its result and original wake already committed, the wait
transaction writes a fresh result wake keyed by the awaiting request and ordered
after the wait update. The ordinary nudge remains best effort and the durable
predicate is authoritative after restart. A foreground hit does not try to
activate a new turn: it locks and reconstitutes the exact `awaiting_child` tool
batch, consumes the matching typed result into a `DelegationResult` semantic
entry, and reopens the same turn under a fresh continued turn attempt. The tool
loop then performs its ordinary serialized continuation from that checked
frontier.

## Transactional outbox

Committed client-observable transitions become update events only through the
transactional-outbox family (INV-032 mechanism; observation semantics are
protocol scope). The authoritative typed-record inventory is the implemented
storage below plus the delegation-stack extension identified inline:

- the baseline `outbox_event` header and delegation-owned
  `delegation_outbox_event` header (both carrying allocator-owned
  `event_sequence`, closed `event_kind`, `storage_version`, and `session_id`)
  plus one typed record table per kind — `session_created_outbox_event`,
  `input_accepted_outbox_event`, `session_model_settings_changed_outbox_event`,
  `turn_model_settings_resolved_outbox_event`, `goal_turn_retired_outbox_event`,
  `turn_activated_outbox_event`, `turn_failed_outbox_event`,
  `model_call_transition_outbox_event`, `tool_batch_transition_outbox_event`,
  `tool_approval_decided_outbox_event`, `context_compacted_outbox_event`,
  `turn_completed_outbox_event`, `turn_refused_outbox_event`,
  `turn_cancelled_outbox_event`, `turn_reconciliation_required_outbox_event`,
  `runner_state_transition_outbox_event`, and the delegation header's
  `delegation_update_outbox_event` and `delegation_wake_outbox_event` — with a
  deferred triggers requiring exactly one typed record per header. Both header
  families share the one allocator and delivery prefix, so their committed
  events form one gap-free global sequence. A runner-transition record carries
  the affected runner, the positive placement revision, the sandbox profile, one
  closed transition state, and the relocation facts that state requires, so a
  follower learns of loss, suspicion, recovery, replacement, working-directory
  relocation, and abandonment from the same family. The family is deliberately
  shaped for extension: a later runner fact — another relocation shape, or
  runner metadata and attributes — adds a state and its columns to this one
  record kind rather than a second event kind, so a follower already decoding
  the family needs no new kind to keep hearing runner news. Extension stays
  version-gated rather than silent: an addition every existing decoder can
  ignore leaves the kind-scoped `storage_version` alone, while a new closed
  transition state or a newly required column advances it, and a decoder that
  predates the advance rejects the record as `Unsupported` instead of coercing
  an unknown state onto one it knows. Tool-batch transition records carry the
  producing call and exactly one closed state shape: `proposed` names the
  yielded assistant/tool-use frontier, `results_projected` names the
  all-resolved result frontier, and `recovery_required` names the exact
  ambiguous physical attempt. The header and typed record tables are append-only
  (`reject_immutable_record_change`), and every outbox table rejects `TRUNCATE`.
  A context-compacted record names the authoritative compaction, its completed
  dedicated call, exact positive through position, appended summary, and result
  frontier.

**Session-delegation foundation proposal.** Migration `202608020018` in the full
delegation stack adds one version-one `delegation_outbox_event` header and one
version-one `delegation_update_outbox_event` typed table, keyed by its
`event_sequence` header foreign key and closed `update_kind`. Its common subject
is the exact `spawning_request_id`; the shape-specific columns carry
`child_session_id` and relationship for `child_spawned`, `await_request_id`,
child, and mode for `child_waiting`, child, outcome, reason, and provenance for
`child_lifecycle_disposition`, those fields plus nullable result content for
`child_result`, or message identity, endpoints, ordinal, and content for
`session_message`. A separate version-one `delegation_wake_outbox_event` typed
table carries the internal `delegation_wake` event kind and one closed wake
subject: `result` requires an equal `result_spawning_request_id`; its nullable
`awaiting_tool_request_id` distinguishes the one initial result wake from a
fresh late-foreground-wait wake and, when present, must name that relationship's
exact wait. `message` instead requires a `DelegationMessageId` belonging to that
relationship and no awaiting request. The header's `session_id` is the stream
receiving the update or wake. Per-kind checks require exactly that shape's
columns and reject all others; foreign keys correlate every supplied identity to
the same relationship. Lifecycle-disposition updates admit only
parent-turn-command or parent-goal-command stop/cancel cascade evaluations;
child-origin terminal events are delivered through `child_result` instead.
Dispatch decodes both closed unions and rejects every other storage version. The
delegation-header completeness trigger includes both record kinds; its header
and both typed records are append-only and reject `TRUNCATE` with the rest of
the family.

Every client-observable delegation transition appends its corresponding typed
update record in the transaction that commits the relationship, wait,
disposition, result, or message. Spawn, waiting, other lifecycle, and result
updates go only to the parent stream, and message updates only to the payload
recipient. A stopped or cancelled lifecycle disposition caused by a parent
cascade is emitted on both the parent and child streams. Every result and
message appends exactly one distinct `delegation_wake` record for that same
recipient in the same transaction, even when the recipient is already active and
may ignore the nudge; the internal wake subject does not stand in for the
client-visible result or message update. A guarded transition that changes no
durable state appends no update. State without its promised update, or an update
without its state, is therefore unrepresentable.

- `outbox_sequence_state`, a mutable singleton row (deletion rejected): a
  `BEFORE INSERT` triggers on both headers allocate `last_sequence + 1` by
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
applied defaults replacement that changes model selection or settings appends
`session_model_settings_changed`; every new origin records and appends
`turn_model_settings_resolved` before its correlated `input_accepted`. An
applied `SubmitInput` that creates a turn origin appends `input_accepted`, while
`PendingSteering` appends nothing until terminal reclassification mints its
successor turn and appends that correlated `input_accepted`; an applied
`StartEligibleTurn` appends `turn_activated`. Startup recovery appends
`turn_failed` for a failed lost turn and `turn_reconciliation_required` when
stopped issued work becomes ambiguous; terminal reclassification of pending
steering appends its correlated `input_accepted`. Goal-owned turn creation
appends the same correlated `input_accepted`; dispatch authenticates its exact
`goal_turn` provenance instead of requiring a synthetic `SubmitInput` command. A
stop or supersede that makes a queued goal turn ineligible appends
`goal_turn_retired` in the same transaction; supersede appends retirement before
the replacement `input_accepted`. The typed record names the exact queued,
now-ineligible `goal_turn`, and dispatch rechecks that durable correlation.
Model-call state transitions append `model_call_transition`, tool-round creation
appends `tool_batch_transition { proposed }`, all-resolved result projection
appends `tool_batch_transition { results_projected }`, and an external-effect
ambiguity appends `tool_batch_transition { recovery_required }`. Completion
closure appends `turn_completed`, refusal closure appends `turn_refused`, and
known-failure closure appends `turn_failed`; interrupt-confirmed cancellation
appends `turn_cancelled`, and live stopped ambiguity appends
`turn_reconciliation_required`; completion of a context compaction appends
`context_compacted` in the same transaction as its dedicated call, summary
entry, result frontier, compaction result, and applied command receipt. An
interrupt against a parked ambiguous tool attempt appends the same event kind
with that exact tool-attempt reference. Every durable runner state change
appends one `runner_state_transition` per affected session in the same
transaction that commits it: initial pin, first missed heartbeat, recovery
before durable loss, loss before and after pin, owner replacement,
working-directory relocation, and abandonment. Because a client cannot otherwise
learn that its session lost its runner, this append is not optional bookkeeping:
a loss or replacement committed without its event is a defect the deferred
one-record-per-header trigger is there to catch. A guarded transition that
changes zero rows appends zero events. Why: writing the event in the committing
transaction makes the dual-write failure (state without event, or event without
state) unrepresentable.

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
references. A runner-state-transition record must agree with the retained
immutable placement record at exactly the revision the record itself names, and
with the placement entry when its transition installed one; it is never required
to equal the session's current placement state, and historical runner
transitions remain dispatchable after their placement advances exactly as
historical Prepared and InFlight transition records remain dispatchable after
their call advances. Why: delivery is one ordered singleton cursor, so a
validation that demanded current state would let any later committed transition
— a loss committing behind a queued suspicion — permanently block that event and
every event after it. A context-compacted event must agree with one completed
dedicated call and the authoritative compaction, applied command, exact through
position, summary entry, and result frontier before dispatch. Tool-batch
cancellation and known crash-failure records validate their terminal marker
after the earlier producing model call and ended physical attempts rather than
requiring an otherwise empty call history. Historical Prepared and InFlight
transition records remain dispatchable after their call advances. Exhausted
delivery still validates the allocator singleton and cursor. Daemon task
ownership, polling, fan-out, and client observation semantics are owned by
[process-protocol](process-protocol.md).

**Committed unimplemented functionality — OAuth refresh staging.** No present
migration or repository stores daemon-owned OAuth material. Its implementing
child must supply three durable shapes and no interpretation of them: a
per-generation `refresh_in_progress` marker that exactly one transaction can
win, an atomic replace-and-clear that in one commit rewrites the refresh token,
rewrites or retains the identity token, and clears the marker, and a reread of a
generation that reports whether a replacement committed and whether the marker
is still set. A generation stores the identity token beside its refresh token
and under the same protections, because dispatch requires one on every
invocation while a refresh happens about once per access-token lifetime, so a
generation that held only the refresh token would leave the first preparation
after any restart with no source for it. The replace shape must express both an
exchange that returned a new identity token and one that returned none, without
a second commit for either and without a state in which a new refresh token is
durable beside an identity token from another exchange.

What those facts mean — which outcome each combination selects, which failures
may clear the marker without changing the token, how many attempts a generation
admits, and why an ambiguous exchange is never replayed — is the refresh
protocol, owned by
[the `oauth` delivery](configuration-and-credentials.md#the-oauth-delivery).
This paragraph makes those decisions representable and takes none of them.
Provisioning replaces the quarantined generation with a fresh authorization in
one transaction, and publishes the durable member-availability update the
scheduler consumes in that same transaction, for the reason an accepted clear
does: re-provisioning is the only recovery from an OAuth delivery-origin
quarantine, so a wait held by that quarantine has no other wake, and a crash
between the replacement and any in-memory notification would leave the repaired
profile's turn parked with its session slot held and nothing left to release it.
That transaction also decides account-level independence, and its lock *order*
is what makes that decision total. It reads the pool-policy revisions its
profile is pinned into, forms the complete set of profile rows it will need —
**its own row together with every co-member's** — and acquires that whole set in
one acquisition ordered by profile reference, holding it until after its own
commit. Under those locks it re-reads its memberships; if the set has grown it
releases, repeats with the enlarged set, and proceeds only once the read taken
under the locks agrees with the set it locked.

Two properties of that shape are load-bearing, and neither survives a simpler
one. Including the provisioned row in the *same* ordered acquisition is what
avoids a cycle: taking one's own row first and only then the discovered
co-members lets two concurrent provisionings of co-members A and B each hold
their own row while waiting for the other's, which the database resolves by
aborting one — a deadlock between two provisionings that need not even share an
account. Re-reading the memberships under the locks is what closes the other
direction: a profile currently in no revision would otherwise lock only itself,
while a concurrent interning that first makes it a co-member commits, and the
provisioning then stores a co-member's account identity having never seen it.

The transaction that interns a pool-policy revision acquires the profile row of
every member it is about to freeze in that same global order. Every transaction
therefore takes profile rows in one order, so no cycle exists, and any two that
share a profile serialize on it: an interning that loses sees the stored
identity and refuses to intern, and a provisioning that loses re-reads the new
revision under its locks and fails on the collision. Which memberships are
consulted, and what a collision does, are owned by
[the `oauth` delivery](configuration-and-credentials.md#the-oauth-delivery);
this paragraph supplies only the lock span that makes two concurrent commits
decide it the same way. Each generation stores the exact provisioning tuple it
was minted under — `client_id`, `token_url`, `device_authorization_url`, and
ordered `scopes` — and every refresh and dispatch compares it with the current
registration under the same profile lock, by the canonical components
[configuration and credentials](configuration-and-credentials.md#distinct-members-are-distinct-authorizations)
defines rather than by the configured bytes; a difference quarantines instead of
exchanging, so an edited endpoint cannot receive a token minted for another.
This paragraph constrains the future schema; no present storage surface provides
it.

**Committed unimplemented functionality — credential-pool state.** No present
migration stores a pool-policy revision, pool action, or pre-call exhaustion
failure. The implementing schema must intern each policy as one immutable header
plus ordered membership and action rows. Each membership row stores the expected
adapter and delivery kind beside its profile reference and settings. A deferred
uniqueness constraint covers the complete canonical structural value. The
surrogate identity is reused only after full relational equality succeeds; a
digest is not identity. Cursor rows key that identity and priority, so an
unchanged policy retains its cursor across restart and an edited policy cannot
inherit one. Selection locks the exact cursor row `FOR UPDATE` before reading it
and commits the chosen call and successor cursor together.

Legacy family-to-reference entries are rewritten only by a post-schema backfill
running with the validated profile registry, after the configuration-independent
recovery scan completes and before scheduling is enabled. For each locked entry
it requires the referenced profile registration, copies that registration's
adapter and delivery kind, and never consults the current family mapping or pool
table. It interns the deterministic
`legacy/<session-uuid>/<event-ordinal>/sha256:<model-family-digest>` singleton
policy defined by the configuration contract, using that contract's exact
spelling: one priority-1 member, no headroom reserve, `first_listed`, `fail`,
and `stay` for every trigger. The policy insert and entry rewrite are atomic and
idempotent; a missing registration aborts before any rewrite and blocks
scheduling without blocking recovery of acknowledged work (INV-034). Thus the
migration has an authoritative source for its two profile-owned fields and
canonical values for every policy-owned field.

Every pool-selected model call stores the immutable policy identity beside its
credential reference as an insert-only authorization fact. Observation commit
joins through that call identity to the exact stored policy before applying a
trigger action; a session credential-history update racing with the call cannot
substitute its newer policy. The call's target adapter and the current profile
registration must agree with the membership row's expected adapter and delivery
kind before credential resolution, or preparation fails before send.

Each profile owns one durable action head **per exclusion origin** — policy and
delivery — naming that origin's current exclusion generation. Every transaction
that mints, activates, or clears an exclusion locks the head for its own origin
`FOR UPDATE` before reading it, so concurrent observations in different sessions
cannot mint two active generations for one profile and origin, and a uniqueness
conflict cannot prevent a terminal observation from committing with its required
action record. Origin separates the heads because the clear protocol answers
administrability from it, and a single head would force a policy quarantine and
a delivery quarantine onto one generation with two contradictory answers; the
two are independent states of one profile and neither supersedes the other. Call
preparation locks the head of every member it may select before reading
exclusion state and holds it through the `Prepared` insert, in the modes the
lock protocol above fixes — `FOR SHARE` for a member it only reads, and
`FOR UPDATE` for one whose pending displacement it consumes — so selection and
exclusion mutation are serialized against each other rather than only against
their own kind.

Profile-quarantine, membership-exclusion, and session-displacement rows each
carry a positive generation, active/cleared state, and their exact scope. A
provider-observation-derived row also carries the model-call observation
correlation; deferred constraints require it and that exact terminal observation
to exist together in the same transaction. A delivery-layer quarantine instead
correlates its typed pre-request failure. A session displacement stores its
source turn and cannot apply within that turn. Clearing marks the exact
generation inactive rather than deleting it, which supplies the replay and
`already_cleared` contract. An accepted clear additionally publishes the durable
member-availability update the scheduler consumes, in the same transaction that
marks the generation inactive. Without that atomicity a clear that commits and
then loses its daemon strands the wait it was meant to release: the row is
inactive, no update was ever published, startup reconstitutes the stored wait
without reclassifying it, and a restart alone is not a wake, so a turn whose
only remaining wake was that exclusion holds its session slot indefinitely while
the operator's replay reports `credential_exclusion_cleared`. A clear that
changes no active generation publishes nothing, because nothing became
available.

Each attached correlation stores the reset that observation reported, as a
required-nullable instant, and the generation stores the effective reset derived
from them. That derivation is the accumulation rule the configuration contract
states: the latest reported reset wins, and any correlation reporting none makes
the effective reset null and absorbing, so no later correlation restores a
deadline. Storing the per-correlation resets as well as the derived value is
what lets reconstitution reproduce the rule exactly after a restart instead of
inferring it — without them a repository could clear a member on a stale
deadline or hold it excluded forever, and the two would be indistinguishable
from the stored row.

A chain-exclusion row is scoped to the session, turn, immutable pool policy,
profile, and predecessor model call whose qualifying observation created it. It
carries that exact observation correlation rather than an independently
allocated generation, and two separate states: a *turn-local* fact that this
profile's qualifying failure occurred in this turn, and the *clearable*
active/cleared state an operator command touches.

The turn-local fact is insert-only and never cleared. It is what the execution
contract means when it says nothing readmits a failed member within its turn, so
an operator clear during a parked turn marks the clearable state inactive — with
the availability update every accepted clear publishes, stated once above — and
still leaves the member excluded for the remainder of that turn, so the clear
takes effect from the next turn. It invents no provider evidence either way.
Without the split there would be one row to both retain and clear, and a clear
mid-turn would either readmit the failed profile or make the turn unable to
record why it stayed excluded.

Pre-call exhaustion — the `pre-call fail` and `wait-transition fail (no call)`
endings of
[the credential-availability machine](credential-availability.md#the-credential-availability-machine),
whose durable-records column this page owns — uses one turn-correlated failure
header with cause exactly `credential_pool_exhausted`, the current attempt, and
the immutable policy identity, plus contiguous member rows in policy order
carrying the closed exclusion kind, correlated record generation or predecessor
observation, and optional reset. Deferred constraints require the complete
policy membership, no model call for the attempt, `KnownFailure` attempt end,
`Failed` turn, exact `TurnFailed` marker and terminal frontier, and one typed
preparation-failure outbox row in the same commit; the wait-transition ending
additionally consumes its wait in that same commit and leaves none stored.
Reconstitution rejects a missing, duplicate, reordered, or foreign evidence row
and any correlation with no durable basis at that failure commit. A chain
exclusion's basis is its turn-local fact, which no clear removes, so a member
excluded by a predecessor failure supplies complete evidence even when an
operator cleared its clearable state while the turn was parked. Every other
exclusion kind uses its active-at-commit record, and reconstitution accepts one
later marked inactive by an authorized clear; the immutable generation or
predecessor observation and its active-at-failure fact remain historical
evidence. This paragraph constrains the future schema; no present storage
surface provides it.

**Committed unimplemented functionality — availability-successor storage.** No
present migration, repository operation, or reconstitution path stores an
availability successor or credential-availability wait. Its implementing child
must give a predecessor-linked attempt a closed origin distinct from the
tool-loop continuation origin and atomically persist its predecessor model call,
the qualifying availability cause, and the typed non-acceptance evidence that
authorized substitution. That origin covers only a substitution authorized by a
predecessor call. A wait entered before any call was issued — because a bounded
member was full, or because every member was already excluded — has no such
predecessor, so releasing it needs a second closed origin: a wait-release origin
carrying the exact consumed wait and the call-free attempt that ended
`WithoutStop(YieldedToDurableWait)`. That origin's predecessor evidence is
optional and is present exactly when the released wait was entered after a
qualifying provider failure **this availability chain** had already observed — a
chain that failed and then found no member it could yet substitute to. The scope
is the chain and not the turn: a later tool round opens a fresh chain, and
attaching an earlier round's failure to it would link a new call to a failure it
did not follow. In that case the same origin additionally carries the
predecessor model call, its qualifying availability cause, and the typed
non-acceptance proof, which is what lets the eventual successor record its
authorizing predecessor as the model-call contract requires. Splitting these
across two origins instead would make the continuation chain non-total for the
common post-failure wait, which fails reconstitution closed and loses
acknowledged work at restart. It is distinct from both the tool-loop and
successor origins, so every continuation still names exactly one origin and the
unique continuation chain stays total. The successor's call remains subject to
`model_call_attempt_once`, pins the same target and a different profile, and
cannot exist without that complete predecessor proof. A credential-availability
wait must atomically retain the active turn slot and store a closed
`exhausted`/`contended` discriminator plus the immutable pool-policy identity.
The exhausted form stores every policy member's exclusion evidence and optional
reset plus the optional deadline the machine derives from them. The contended
form stores every durable exclusion in the selection snapshot, the complete
nonempty set of otherwise-admissible bounded members with their exact
invocation-reservation identities, and a deadline over those durable exclusions
derived by that same rule. These shapes store the derived value and its inputs;
the derivation is the machine's alone, so no page but that one states which
exclusion kinds contribute a reset. A reservation identity is the whole of that
evidence: it is allocated once with the reservation row, never reused, and never
versioned, so reconstitution compares identities alone and needs no separate
generation. One shared capacity row per bounded profile serializes reservation
admission across sessions and pools. Preparation locks all candidate capacity
rows in profile-reference byte order, counts their live reservation rows under
those locks, and inserts the selected reservation with the `Prepared` call.
Every `codex_home` invocation inserts a reservation regardless of whether its
profile currently declares a bound; the bound decides only whether preparation
takes that capacity lock and counts, so an unbounded profile records the same
supervision evidence without serializing. A deferred constraint rejects any
commit that *raises* a profile's live reservation count to a value above the
bound the profile's current registration declares; a commit that leaves that
count unchanged or lower is admitted whatever the count is. The constraint is
scoped to admissions rather than to states because the bound is a live profile
property rather than a frozen policy field: it governs the next admission and
never retroactively invalidates a committed reservation, so lowering the bound
below the live count commits, the excess drains as those invocations complete,
and each completion commits while the count is still over the bound. A
state-scoped constraint would instead reject the lowering registration itself,
or reject every draining completion, and leave the system in a state its own
invariant forbids with no legal transition out. Because every invocation is
recorded, a bound newly lowered from unbounded is still enforced against
complete startup fencing evidence rather than against only the invocations that
were bounded when they started. The admission that *creates* a contended wait
holds the capacity row of every bounded member it is about to name `FOR UPDATE`,
from before it counts live reservations until after the wait row is inserted.
Without that span a concurrent invocation can complete between the count and the
insert, publish its wake while no wait yet exists to receive it, and leave the
new wait parked on reservation identities already released — the stale evidence
reconstitution must fail closed on, reached with no crash involved. Invocation
completion releases its reservation and writes the wake signal atomically,
holding that profile's capacity row `FOR UPDATE` across both so a release and
its wake are never visible apart. One release can wake several waits while
admitting one, so each woken transaction reruns admission under the same
capacity locks. The one that acquires the freed reservation releases its wait. A
transaction finding no admissible member instead re-derives its ending from
[the credential-availability machine](credential-availability.md#the-credential-availability-machine)
under those locks and stores whatever that row requires: where a bounded member
still holds the pool it atomically rewrites its own wait's evidence to the live
reservation identities now holding the bound and stays parked; where every
formerly bounded member has become durably excluded, contention is over and the
machine's wait-selection rule decides, so the wait is rewritten to the exhausted
form exactly where an exclusion a wake can clear remains and **no wait is stored
at all otherwise**, which instead terminalizes the turn with the cause that
table gives its row. Deriving it from the surviving exclusions rather than from
the configured value is what stops a `park` pool whose members are all excluded
by this turn's own chain exclusions from being rewritten into a wait no wake
could ever release. Storage never keeps a turn parked under a policy that says
to fail it, and never parks one nothing could wake. Because the rewrite holds
the capacity rows of every bounded member it names, a concurrent completion
cannot release one of them between the read and the commit. A deferred
constraint therefore never has to reject a losing waiter's call, and no stored
wait names a released reservation or misses the only wake that concerned it.
Entering either wait ends the call-free current attempt as
`WithoutStop(YieldedToDurableWait)` in the same transaction. Release atomically
consumes the wait and creates its fresh `Prepared` successor attempt;
`stop_turn` instead atomically consumes it, creates the fresh
immediate-successor attempt, applies the interrupt proof, ends that attempt
`AfterCancellation(Cancelled)`, and terminalizes the turn. Each reservation has
a closed `pending_spawn` state with no process identity and a
`spawned { process_group_identity }` state carrying the child process group's
reuse-safe host identity. Successful spawn replaces `pending_spawn` with
`spawned` immediately, and that attach is guarded on the reservation still being
`pending_spawn` for this exact reservation identity. The invocation path may not
finish while that update's outcome is unknown: a failed or ambiguous commit is
resolved before the caller proceeds. It rereads the reservation authoritatively
— a committed `spawned` carrying this exact process-group identity is adopted,
and a still-`pending_spawn` row is reattached under the same guard. Only when it
can neither commit the attach nor confirm one does it terminate that exact
process group, prove it absent, and close the reservation as lost in one
transaction. Leaving a live child behind a `pending_spawn` row is what the
startup rule below cannot recover from, so this path never exits into that
state. Startup must resolve every prior-process `spawned` reservation before
scheduling — proving that exact group absent, or terminating it and then proving
absence — and only then closes it as lost; failure to establish absence fails
startup. Retaining one for a later death notice is not admitted, because the
terminal observation that would release it died with its daemon. It retains a
`spawned` reservation owned by the live fenced process, whose observation path
this daemon still owns. A prior-process `pending_spawn` reservation is ambiguous
because its child may have started before the identity update, so startup fails
before scheduling rather than releasing it without process-death proof. After
that reconciliation and before scheduling is enabled, startup iterates the
retained `contended` waits themselves rather than the currently bounded
profiles, so a profile whose bound was removed still has an entry to evaluate.
Each wait stores a complete nonempty bounded-member set, so startup evaluates
every member in that set rather than one profile: a member the current
registration leaves unbounded makes the wait eligible outright with no count to
compare against, and a member still bounded makes it eligible when that
profile's surviving live reservation count, taken under its capacity lock, is
below the current bound. Any one such member suffices, since one admissible
member is all preparation needs. A bound raised, lowered, or removed across a
restart is therefore evaluated the same way for every member of every wait,
without waiting for an unrelated release. These are the shapes required by
[turn lifecycle](turn-lifecycle-and-scheduling.md#turns-states-and-the-single-active-slot).
Reconstitution and wake must fail closed on partial, stale, or mismatched
evidence. This paragraph constrains that future schema; no present storage
surface provides it.

## Open edges

- Deferred outbox retention, pruning, and multiple-daemon fan-out are cataloged
  in [open questions](../open-questions.md#protocols-and-persistence).
- Attempt continuation is presently admitted only for the tool-loop
  yield/approval path. The availability-successor producer described above is
  committed but unimplemented; no current producer can construct that second
  predecessor-linked shape.
- Frontier lineage checks admit `none` and checked imported-frontier ancestry;
  native `SingleSource` fork ancestry remains unimplemented.
- The aggregate-map rows for model calls and the tool loop have landed; provider
  evidence, authority transfers, and fatal cancellation intent are not yet in
  the schema.
- Command-handling operator classification covers the tool-loop repository; the
  other command families do not yet implement `ClassifyOperatorFailure`.
- Database-role separation remains a deployment choice; migration invocation
  itself is wired in `apps/signalboxd`.

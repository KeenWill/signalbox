# Identity, commands, and telemetry correlation

The user-vocabulary surface on this page was re-verified through this PR
(`agent/storage-vocabulary-rename`), which renamed the stored actor and issuer
discriminators this page states.

The lifecycle command and creation-receipt statements are re-verified through
this PR (`agent/lifecycle-t5-commands`).

The `SubmitInput` multipart storage-version boundary below is the foundation
proposal from PR `#553` (`agent/blob-storage-foundation`) and becomes verified
with its implementing child stack.

This page describes the implemented identity, durable-command, and
telemetry-correlation behavior of Signalbox, including the imported identity
kinds and command family and the tool-loop identity kinds and decision command,
as verified against the implementing stack through PR #224
(`agent/session-metadata-domain`). The behavior lives in `crates/domain`
(identity newtypes, command payloads, actor attribution, replay equality),
`crates/application` (identity generation, command boundaries),
`crates/persistence` (the user-global command registry and typed record
families), and `apps/signalboxd` (telemetry wiring); those `apps/signalboxd`
code homes were verified through PR #258 (`agent/signalboxd-rename`). Storage
transaction mechanics, locking, and the reconstitution seam are owned by
[persistence-protocol](persistence-protocol.md); per-command product semantics
are owned by [sessions-and-transcript](sessions-and-transcript.md),
[turn-lifecycle-and-scheduling](turn-lifecycle-and-scheduling.md), and
[configuration-and-credentials](configuration-and-credentials.md). The
model-settings command version boundaries were verified through this PR
(`agent/model-settings-persistence`). The tool-attributed metadata command and
reconstitution surface was verified through PR #265 (`agent/tool-batch-tier0`).
The failed tool-attempt telemetry fields were verified through PR #285
(`agent/dev-instance-code-host-credential`). The current command/telemetry
identity-generation, command-family, and ambiguity-ownership inventory was
verified through PR #288 (`agent/audit-fix-docs-coherence`); the
context-compaction command lifecycle was verified through PR #314
(`agent/context-compaction-protocol`). The checked placement-update request
boundary and path-scoped placement command family were verified through PR #400
(`agent/scoped-visibility-wiring`). The runner recovery command families are the
foundation proposal at the bottom of their implementing stack and become
verified only with those child pull requests. The commissioned-session command
construction boundary is verified against this PR
(`agent/commissioned-dispatch-fence`).

## Identity model

The session, command, transcript, model, and tool identities owned by this page
are distinct, opaque, UUID-backed newtypes built by the `define_identity!` macro
in `crates/domain/src/lib.rs`: `DurableCommandId`, `SessionId`,
`AcceptedInputId`, `TurnId`, `TurnAttemptId`, `ModelCallId`,
`ProviderTargetEvidenceId`, `ToolRequestId`, and `ToolAttemptId` there, plus
`ImportedConversationId` and `ImportedTranscriptEntryId`
(`imported_conversation.rs`), `SemanticTranscriptEntryId` and
`ContextFrontierId` (`context_frontier.rs`), `DirectModelSelection` and
`ModelAlias` (`configuration.rs`), and `ProviderModelIdentity`
(`model_call.rs`). Review-run, pass, finding, target, and external-link
identities are owned and inventoried by [review-workflows](review-workflows.md).
Each identity listed here exposes only `from_uuid`, `as_uuid`, and `into_uuid`;
the macro derives value semantics and `Debug` but no storage or serialization
traits, so every storage boundary maps explicitly (INV-001, INV-002). The
derived `Debug` is the one logging-reachable render path (see Encoding).

Identities fall into three supply classes:

- **Caller-supplied idempotency identity** — `DurableCommandId` only. Each
  application request constructor accepts the caller-supplied value, and the
  daemon accepts any non-sentinel RFC 9562 UUID — the nil and max sentinels are
  rejected (see below) — without checking its version bits. Why: idempotency
  correctness comes from the user-global durable claim plus canonical payload
  comparison, never from trusting a caller's clock or version bits (INV-012).
- **Daemon-minted durable-fact identity** — `SessionId`,
  `ImportedConversationId`, `ImportedTranscriptEntryId`, `AcceptedInputId`,
  `TurnId`, `TurnAttemptId`, `SemanticTranscriptEntryId`, `ContextFrontierId`,
  `ModelCallId`, `ToolRequestId`, and `ToolAttemptId` today;
  `ProviderTargetEvidenceId` is assigned here but not yet minted (see Open
  edges). All production generators mint UUIDv7 (`uuid::Uuid::now_v7()`). Why:
  the recorded rationale for UUIDv7 is insertion locality for append-heavy
  Postgres B-tree keys without changing the 128-bit storage shape; no
  index-level artifact measures this.
- **Configuration reference key** — `DirectModelSelection` and `ModelAlias`.
  Callers supply them inside command payloads to name operator-configured model
  selections; they persist in `uuid` columns (`direct_model_selection_id`,
  `model_alias_id`), and alias meaning resolves through a definition lookup at
  domain preparation, so an unknown alias becomes a recorded rejection, not an
  accepted identity.

`ProviderModelIdentity` names the daemon's normalized provider/model value
space. It is persisted (`turn_lifecycle.pinned_provider_model_identity_id`,
`model_call.resolved_provider_model_identity_id`) and supplied as an
operator-configured key from signalboxd's model-configuration file; how
provider-reported data normalizes into it remains open (see Open edges).

UUID contents are never semantic. No code derives acceptance order, queue order,
lifecycle precedence, ancestry, ownership, or authorization from UUID bytes or
embedded timestamps; those facts live in purpose-specific domain values and
records (INV-001, INV-004).

The nil and max UUIDs are rejected as `DurableCommandId` values at two
boundaries: checked command/request construction (`try_new` on
`CreateSessionRequest`, `CreateSessionFromImportedFrontierRequest`,
`ReplaceSessionDefaultsRequest`, `ReplaceSessionMetadataRequest`,
`CommissionDispatchRequest`, and `SubmitInputRequest` and
`UpdateSessionPlacementRequest` in `crates/application`, plus
`DecideToolRequest` in `crates/domain`) and persistence decoding
(`durable_command_id_from_uuid` in `crates/persistence/src/mapping.rs`).
Rejection occurs before a canonical command can reach a transaction and claims
no identifier. Why: sentinel-like values are common accidental defaults and
would otherwise become permanent user-global claims.

## Generation and minting boundary

UUID generation is an application-layer effect. `crates/domain` depends on
`uuid` with `default-features = false` and no generation feature: the domain
crate cannot mint an identity. `crates/application` enables the `v7` feature and
defines one generator trait per orchestration slice, each with a production
UUIDv7 implementation:

| Generator                                            | Mints                                                                                                                        |
| ---------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------- |
| `UuidV7SessionIdGenerator`                           | `SessionId`                                                                                                                  |
| `UuidV7ImportedConversationIdGenerator`              | `ImportedConversationId`, `ImportedTranscriptEntryId`                                                                        |
| `UuidV7CreateSessionFromImportedFrontierIdGenerator` | `SessionId`, `SemanticTranscriptEntryId`, `ContextFrontierId`                                                                |
| `UuidV7SubmitInputIdGenerator`                       | `AcceptedInputId`, `TurnId`, `SemanticTranscriptEntryId`, `ContextFrontierId`, `DurableCommandId`, `TurnAttemptId`           |
| `UuidV7StartEligibleTurnIdGenerator`                 | `SemanticTranscriptEntryId`, `ContextFrontierId`, `TurnAttemptId`                                                            |
| `UuidV7StartupScanIdGenerator`                       | `SemanticTranscriptEntryId`, `ContextFrontierId`, `TurnId` (reclassified successors)                                         |
| `UuidV7ModelCallExecutionIdGenerator`                | `ModelCallId`, `SemanticTranscriptEntryId`, `ContextFrontierId`, `TurnId` (reclassified successors)                          |
| `UuidV7ToolLoopIdGenerator`                          | `ToolRequestId`, `ToolAttemptId`, `ModelCallId`, `SemanticTranscriptEntryId`, `ContextFrontierId`, `TurnAttemptId`, `TurnId` |
| `UuidV7CommissionedDispatchIdGenerator`              | `CommissionedDispatchId`, `DurableCommandId`, `SessionId`                                                                    |

`ProviderTargetEvidenceId` exists as a domain type but has no production minting
seam yet; its generator lands with its owning slice. `WorkspaceId`,
`GitRemoteMintId`, and `GitRemoteWithdrawalId` are in the same position: the
durable schema that stores them exists, nothing writes it yet, and their
generators land with the store and the operator verbs. What each identity scopes
is stated under
[remote destination authority](git-authority-threat-model.md#remote-destination-authority).

Orchestration generates each fresh candidate immediately before the domain
transition that creates the fact. Fixed-cardinality candidates are minted before
the transaction; the submit slice's entry and frontier candidates close an
interrupt directly when it proves pre-send cancellation. When cardinality
becomes authoritative only under the repository lock, orchestration instead
passes an application-owned generator closure into the transaction port. Initial
call preparation draws one steering semantic-entry candidate and one fallback
reclassified-successor candidate per locked pending input; terminal closure and
startup recovery draw one reclassified successor per locked pending input. The
adapter invokes each closure under the lock and immediately supplies the typed
value to the domain transition. Persistence never owns or synthesizes an
identity, and no Postgres column has an identity-generating default (verified
across all migrations).

Imported-frontier session creation draws its fixed session and seed-frontier
candidates before the transaction. It passes the same orchestration slice's
application-owned semantic-entry generator closure into the transaction; after
the adapter checks and resolves the selected imported prefix, it invokes the
closure once per imported entry and immediately supplies each candidate to the
checked seed transition. No pre-transaction inventory read determines that
cardinality.

Why: the domain transition still receives a typed identity while the domain
remains generation-free and deterministic, without pre-lock inventory reads. A
transaction that aborts leaves an unused candidate but no durable fact. Recovery
reconstitutes committed facts under their stored identities; the startup scan's
generator mints identities only for the new facts it records — the `TurnFailed`
semantic entry, the terminal frontier, and a fresh successor `TurnId` per
pending-steering input it reclassifies (INV-007). On equal command replay the
recorded receipt is returned, which may name a different identity than the fresh
candidate generated for that invocation — the candidate is discarded.

## Encoding

Every persisted UUID-backed identity uses native Postgres `uuid` columns.
Identity kind is carried by table, column, and foreign key — never by UUID
contents (INV-002). `crates/persistence/src/mapping.rs` defines named conversion
functions for `DurableCommandId`, `SessionId`, `AcceptedInputId`, and `TurnId`;
the remaining persisted kinds (`ImportedConversationId`,
`ImportedTranscriptEntryId`, `TurnAttemptId`, `ContextFrontierId`,
`SemanticTranscriptEntryId`, `DirectModelSelection`, `ModelAlias`,
`ModelCallId`, `ProviderModelIdentity`, `ToolRequestId`) cross the SQL boundary
through inline `from_uuid`/`into_uuid` calls at typed repository call sites (for
example `crates/persistence/src/conversation_import.rs`, `submit_input.rs`,
`start_eligible_turn.rs`, and `model_execution.rs`). Every crossing is explicit;
none is derive-generated. Version ordinals and queue positions use checked
`numeric(20, 0)` mappings in `mapping.rs` and are not identities.

Telemetry renders identities in two forms. Application sites render the
lowercase hyphenated RFC 9562 form (`session_id = %session.as_uuid()` in
`crates/application/src/scheduler.rs`), with the structured field name
identifying the kind. The signalboxd startup-failure site logs
`session_id = ?error.session` and `turn_id = ?error.turn` — the derived `Debug`
of `Option<SessionId>`/`Option<TurnId>`, which renders `Some(SessionId(..))` or
`None`, not bare canonical UUID text (`apps/signalboxd/src/main.rs`).

The local [process protocol](process-protocol.md) maps identity values at its
wire adapter boundary and admits commands through the same application services;
domain types acquire no serialization trait. Public URL identity forms remain
open.

## Durable command records

All claimed command identifiers live in one user-global, append-only
`durable_command` registry (migration `202607180001` and successors): primary
key `command_id`, a closed `command_kind` discriminator (`create_session`,
`create_session_from_imported_frontier`, `replace_session_defaults`,
`replace_session_metadata`, `submit_input`, `decide_tool_request`,
`override_denied_tool_request`, `review_workflow`, `compact_session`,
`update_session_placement`, `session_lifecycle`, `replace_lost_runner`,
`abandon_lost_runner`, `promote_pending_runner`), a kind-scoped
`storage_version`, `claimed_at` (`transaction_timestamp()`), which is
non-semantic operational metadata, and the authenticated issuer principal —
`issuer_kind` (`core`, `operator`, `module`, `watchdog`) with `issuer_module`
naming the module — stamped by the boundary that admitted the command. The
lifecycle actor classification derives from the principal and the domain actor:
a module principal wins, otherwise the actor classifies. A goal event a command
authored projects the session's actor from that command's envelope issuer, so
daemon core's automatic resume and a module's composed stop never read as the
operator's. No command kind, session, or client has a separate command-ID
namespace.

Each admitted kind has one purpose-specific typed record family
(`create_session_command`, `create_session_from_imported_frontier_command`,
`replace_session_defaults_command`, `replace_session_metadata_command`,
`submit_input_command`, `decide_tool_request_command`,
`override_denied_tool_request_command`, `review_workflow_command`,
`compact_session_command`, `update_session_placement_command`,
`session_lifecycle_command`, `replace_lost_runner_command`,
`abandon_lost_runner_command`, `promote_pending_runner_command`) keyed
one-to-one by `command_id`, storing every caller-supplied semantic field under
`CHECK` constraints and foreign keys. Every family except replacement also
stores its terminal `applied`/`rejected` discriminator and typed result fields
in that row. A compact-session record begins `pending` with its exact dedicated
Prepared call, then changes exactly once to `applied` with its receipt or to
`failed`; its request fields never change. Runner replacement instead has one
immutable request row plus at most one append-only `replace_lost_runner_result`:
the request row satisfies typed-claim completeness while provisioning crosses
the runner boundary, and no success or rejection response exists until the
result row commits. Kind and version agreement between the registry row and its
typed record is enforced by a composite foreign key, and a deferred constraint
trigger (`durable_command_requires_typed_record`, executing function
`require_durable_command_typed_record`) requires exactly one typed record per
claim at every transaction boundary. Why: typed relational records keep each
command's comparison payload and result reviewable and constraint-checked
instead of delegating meaning to a serializer; there is no universal JSONB or
byte-blob payload anywhere.

`replace_lost_runner` is the sole version-one multi-transaction command. Its
first transaction claims the registry identity, stores the complete immutable
request, and stores a single-use provisioning authorization. The handler waits
without holding a database transaction while the pending runner returns or
replays its workspace receipt; the terminal transaction appends exactly one
result and atomically installs the replacement or its typed rejection. Equal
replay during provisioning joins the same durable operation and can neither
start another workspace nor acquire another meaning. Startup resumes an
unterminated request before client admission. `abandon_lost_runner` remains one
ordinary atomic claim-and-terminal-result transaction.

`promote_pending_runner` is the one user command in this set whose payload names
no session. It carries only the command identity and the pending
enrollment-request identity it promotes, and it is a single atomic
claim-and-terminal-result transaction: the deployment-scoped fact it acts on is
that this daemon's active runner is durably gone, so no session placement is a
required argument and none is mutated
([runner protocol and placement](runner-protocol.md#identity-enrollment-and-registration)).

`UpdateSessionPlacement` carries the target session, the exact expected current
placement version, and the complete replacement placement. Its first handling
atomically records either the next immutable event or one of `SessionNotFound`,
`CurrentVersionMismatch`, and `VersionExhausted`; equal replay returns that
recorded result, while a different payload under the same command identity is
conflicting reuse. This is the sole command that advances session path placement
after creation.

For `SubmitInput`, each terminal command result must correlate with exactly its
committed domain effects. Equal replay returns the recorded result only after
the current durable state still proves that correlation; otherwise the adapter
fails closed rather than treating an effectless receipt as truth. The exact
relational representation, deferred triggers, migration evolution, and
lifecycle-transition checks are owned by
[persistence-protocol](persistence-protocol.md#relational-representation).

All registry and typed-record tables are append-only, enforced by
`reject_immutable_record_change` triggers. Why: a claimed identifier's recorded
meaning must never be rewritten, or replay would stop being truthful.

A claimed registry row whose typed record is missing, duplicated, of a
mismatched kind, or undecodable is classified as storage corruption
(`RegistryCorruption` in `crates/persistence/src/command_registry.rs` and
per-kind `*Corruption` types), never as an unseen command. Why: treating an
undecodable claim as unseen would let one identifier acquire a second meaning
(INV-012). Corruption is a distinct error family from infrastructure failure and
from recorded domain rejection.

New `CreateSession` records use storage version 7, new
`CreateSessionFromImportedFrontier` records use version 5, and new
`ReplaceSessionDefaults` records use version 4. Those versions introduce the
commands' model-settings member; earlier supported versions accept only the
provider-default full settings or inherit-all overlay backfilled by the
migration. New `SubmitInput` records use version 3, whose payload authority is
the ordered content-part satellites. The one-time satellite migration rewrites
every version 1 or 2 `SubmitInput` record to version 3 after backfilling its
single text part; the `SubmitInput` decoder accepts only version 3 and has no
rolling compatibility path. Imported-creation version 4 remains
committed-unimplemented compatibility space for that command family's optional
runner-placement payload: no present writer or decoder provides it. All three
defaults-bearing creation and replacement families reconstitute version 1 with
dangerous blanket approval disabled and versions 1 and 2 with no system prompt.
Create-session versions 1 through 3 carry no template provenance; version 4 and
every later supported version require provenance for template mode and require
its absence for explicit mode. Create-session version 5 reserves the optional
session runner placement and remains unsupported until that payload's decoder
lands; version 6 adds path-scoped placement, and version 7 composes model
settings with that implemented shape. Each field is absent before its
introducing version, so an older reader rejects a newer creation record instead
of discarding either decision. `ReplaceSessionMetadata` and `DecideToolRequest`
use version 1. `CreateSession` records applied results (its one preparation
failure is an error, not a recorded rejection); `session_lifecycle` (version 1)
carries `stop{sticky, descendant_scope}`, `supersede{successor}`, `abandon`,
`close_failed{cause}`, `resume`, `adopt{finish_condition}`, and `release` in one
typed record with closed, operation-scoped rejections; every claimed lifecycle
command settles as a `command_settled` receipt (`session_created` is an applied
creation's receipt); `CreateSessionFromImportedFrontier` also records applied
results only, because a missing conversation named by the frontier or a boundary
absent from that conversation is a pre-claim admission error rather than an
authoritative rejection; `ReplaceSessionDefaults`, `ReplaceSessionMetadata`,
`SubmitInput`, and `DecideToolRequest` record both applied results and closed,
typed rejection discriminators. Authoritative rejections claim the identifier
exactly as applied results do.

## Replay and equality

The canonical command payload is the typed domain value constructed at the
boundary — not a serialization. Ordinarily that construction precedes registry
lookup. Template creation is the one narrower caller-intent preflight: the
boundary validates command identity and template name, then looks up the durable
command before consulting the live template catalog or constructing the complete
domain payload. An existing create command compares explicit-versus-template
mode and, for template mode, the caller-supplied name; equality returns the
recorded result without catalog resolution. Only an unseen identity resolves the
startup catalog and constructs the complete defaults-and-provenance payload.
Structural equality (hand-written `PartialEq` on `CreateSession`,
`CreateSessionFromImportedFrontier`, `ReplaceSessionDefaults`, `SubmitInput`,
`ReplaceSessionMetadata`, `DecideToolRequest`, and `UpdateSessionPlacement` in
`crates/domain`) covers every caller-supplied semantic field and excludes
`DurableCommandId`. Why: the identifier is the lookup key that names the
payload, not part of the meaning it names. The optional session runner placement
is such a field in both creation families, so it participates in that equality
in both creation modes — including template-derived creation, whose
daemon-resolved defaults are excluded — and a replay carrying a different
placement, or a placement where the first handling had none, is conflicting
reuse.

Every command repository, including
`crates/persistence/src/context_compaction.rs`, follows one claim protocol, with
registry lookup as the first durable operation, before any current-state
validation (INV-012):

1. Inspect the registry. If the identifier is claimed by the same kind, load and
   reconstruct the recorded typed payload and closed result or lifecycle,
   compare structurally, and roll back: equal replay returns the recorded
   terminal result or exact pending disposition; any difference — including a
   different kind — is conflicting reuse, returned without disturbing the
   recorded meaning.
2. If unclaimed, `INSERT ... ON CONFLICT DO NOTHING` claims the registry row. A
   lost race re-inspects and resolves against the winner's committed record; a
   winner row that cannot then be read is corruption.
3. A single-transaction command's first handling commits the registry row, typed
   payload record, terminal result, and every applied domain effect together.
   Compaction instead commits the claim, pending typed record, and Prepared
   dedicated call together; its later session-locked transaction records the
   terminal call evidence, summary/result and applied receipt, or the failed
   disposition. No applied result is returned before its transaction commits,
   and a failed claim transaction claims no identifier.

Before complete payload construction, template creation may perform only the
caller-intent registry preflight above. After the repository inspects the
registry for a constructed command and before claiming an unseen identifier, a
command may perform a user-specified pre-claim admission read.
`CreateSessionFromImportedFrontier` uses that phase to load the conversation
named by `frontier.conversation()` and resolve the frontier's inclusive
boundary; a missing target returns the corresponding admission error without
claiming the identifier. This is distinct from an authoritative rejection, which
is derived only after claim and stored for replay.

First handling may re-derive the terminal result inside the claim transaction:
`ReplaceSessionDefaults` applies through a compare-and-set `UPDATE` on
`session_current_defaults`, and a CAS lost to a concurrent commit re-prepares
against the winner's committed state and records the re-derived rejection as the
terminal result; a CAS lost without a version change is corruption
(`crates/persistence/src/replace_session_defaults.rs`).

Each single-transaction application service calls its atomic transaction port
exactly once and surfaces infrastructure failure to its caller without retry or
receipt reconstruction (the `CreateSessionTransaction` contract in
`crates/application/src/create_session.rs`, the
`CreateSessionFromImportedFrontierTransaction` contract, and the corresponding
transaction-failure tests in all six services, including
`decide_service_returns_transaction_failure_without_retry`). Because a failed
transaction claims no identifier, retransmitting under the same
`DurableCommandId` is the caller's retry path and replays or claims cleanly.
Compaction's off-transaction provider effect is the deliberate exception: its
runtime retries authorization and terminal persistence after database or
ambiguous-commit outcomes, while the repository rereads and exactly replays an
already-landed transition. This retains one successful summary until its durable
receipt is known instead of issuing another provider call.

Every repository also treats an unreadable claimed payload or result as typed
corruption rather than unclaimed state, including the imported-frontier command.

Reconstructed-then-compare ordering means a storage representation change can
never turn an equal command into conflicting reuse; unknown kinds and storage
versions fail explicitly as corruption. Equal semantic content never merges
distinct commands, and callers needing corrected intent after a recorded
rejection must use a new identifier.

## Actor attribution

`Actor` (`crates/domain/src/actor.rs`) is the closed typed provenance of a
durable command's initiating agency: `User`, `Model { turn: TurnId }`,
`Recovery`, or `Tool { request: ToolRequestId }`. Equality is structural; a
carried identity is a validated reference, not minting authority, and
attribution confers no lifecycle, authorization, or approval authority (INV-001,
INV-020).

`SubmitInput` and `ReplaceSessionMetadata` are the command kinds whose durable
payloads carry an `actor` field. The `SubmitInput` application constructor fixes
`Actor::User`. Metadata replacement has two purpose-specific constructors: the
process-facing form fixes `Actor::User`, while the tool-facing form requires one
exact `ToolRequestId` and fixes `Actor::Tool { request }`. Neither accepts an
arbitrary actor, and model/recovery issuers remain unconstructible.
`SubmitInput` and `ReplaceSessionMetadata` both include actor agency in replay
equality and hashing: replaying a claimed identifier under a different actor is
conflicting reuse (INV-012). Checked metadata reconstitution independently
decodes the stored actor and compares it with the canonical command value. Why:
attribution recorded outside these checks could be laundered by replaying one
claimed identifier under a different claimed agency. For metadata replacement,
the recorded actor is also the applied last-writer provenance.

Storage follows the closed-discriminator convention: `actor_kind`
(`user`/`model`/`recovery`/`tool`) plus `actor_turn_id` and
`actor_tool_request_id` reference columns with a `CHECK`-enforced variant shape
in `submit_input_command` and `replace_session_metadata_command`. Metadata
receipts additionally carry constructor-selected `issuer_kind` (`user`/`tool`)
and `issuer_tool_request_id` columns, sealed separately from the actor
projection. The issuer migration fixes every pre-issuer receipt to the user
agency that its legacy constructor required, rather than trusting the actor
projection being checked. Unknown or malformed stored spellings fail decoding as
corruption. A well-formed `model` or `recovery` actor on a metadata command
fails earlier as unsupported metadata-writer corruption. Metadata loading
constructs the canonical command from the independent issuer proof, then domain
reconstitution compares the separately decoded supported actor against that
command (`CommandActorMismatch`) for both applied and rejected receipts, so a
cross-wired user/tool actor fails closed.

`CreateSession` and `ReplaceSessionDefaults` v1 carry no actor field in payload
or storage, and no recorded-transition family (including startup-scan
terminalizations) has adopted an attribution field. See Open edges. Actor
answers who issued one command; `SessionCreationCause` answers why a session
exists — they are independent facts, and neither substitutes for the other (see
[sessions-and-transcript](sessions-and-transcript.md)).

**Committed unimplemented functionality.** No present surface constructs a
program actor. The closed actor algebra gains a program-issuance arm — a
verified reference to the issuing program run, constructible only by the
[program substrate](program-substrate.md)'s host-side session capability, with
the same validated-reference, no-conferred-authority semantics as every other
arm — and `SubmitInput` gains a program admissibility path fixing that actor, so
a program-driven turn is never recorded as user-issued. This constrains present
change: the actor storage convention (closed `actor_kind` discriminator,
variant-shaped reference columns, replay-equality inclusion) must remain
extensible to that arm, and nothing may assume the `SubmitInput` actor is always
`user`.

## Durable-command telemetry correlation

Operational telemetry is emitted through the `tracing` facade by
`crates/application` and `apps/signalboxd`; `crates/persistence` and
`crates/domain` have no `tracing` dependency and emit none. Subscriber selection
and installation live only in `apps/signalboxd` (see
[runtime-substrate](runtime-substrate.md) for the runtime and the operator
failure taxonomy). Telemetry events correlate durable failures with
daemon-minted aggregate identifiers — `session_id`, turn identities, phase, and
failure-class fields — in the two render forms described under Encoding. The
same events may carry closed classification tokens that name no aggregate: the
tool-loop failed-attempt event adds the dispatched catalog tool name and the
closed tool error kind ([tool-loop](tool-loop.md#serialized-staged-execution)).

No telemetry site emits a caller-supplied `DurableCommandId` in any form: no raw
UUID, prefix, digest, or token appears in any `tracing` call in the codebase.
Typed error `Debug`/`Display` representations may contain a raw command
identifier (for example `DifferentCommandKind` in the persistence repositories)
and are treated as internal values; the telemetry paths log classification
fields, not formatted errors. The keyed correlation-token scheme that would
restore per-command telemetry correlation is a retired unimplemented design;
command-scoped events currently carry no command correlation at all. See Open
edges.

## Open edges

- The `dc1` durable-command telemetry token (HMAC epoch scheme, mounted epoch
  document, fail-closed startup validation, sanitized panic hook) is a retired
  unimplemented design; telemetry currently omits durable-command correlation
  rather than tokenizing it
  ([telemetry correlation](../open-questions.md#telemetry-correlation)).
- `ReplaceSessionDefaults` v1/v2 payloads and storage carry no `actor` field
  despite the accepted adoption path expecting one from the kind's first
  accepted version; the truthful `User` backfill via another kind-scoped storage
  version remains available but unexercised.
- `CreateSession` actor adoption remains an explicit maintainer choice; v1/v2
  leave its attribution implicit.
- No recorded-transition record family has adopted actor attribution;
  startup-scan terminalizations do not yet record a `Recovery` actor.
- Public URL identity encodings remain undecided
  ([identity representation](../open-questions.md#identity-representation));
  local wire forms are owned by [process-protocol](process-protocol.md).
- `ProviderTargetEvidenceId` has an assigned supply class but no production
  minting seam. Tool request and attempt UUIDv7 generators are implemented by
  the application tool-loop service. `ProviderModelIdentity` is persisted and
  configuration-supplied; provider-identity normalization remains open
  ([model fallback and provenance](../open-questions.md#model-fallback-and-provenance)).
- UUIDv7 timestamp disclosure and namespace scope must be reassessed before
  identities are exposed outside the single-user boundary or treated as
  capabilities.
- Which command kinds may admit non-user actors, and under what verification,
  remains with reserved delegation and authorization decisions.

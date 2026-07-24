# Identity, commands, and telemetry correlation

This page describes the implemented identity, durable-command, and
telemetry-correlation behavior of Signalbox, including the imported identity
kinds and command family and the tool-loop identity kinds and decision command.
The behavior lives in `crates/domain` (identity newtypes, command payloads,
actor attribution, replay equality), `crates/application` (identity generation,
command boundaries), `crates/persistence` (the owner-global command registry and
typed record families), and `apps/hubd` (telemetry wiring). Storage transaction
mechanics, locking, and the reconstitution seam are owned by
[persistence-protocol](persistence-protocol.md); per-command product semantics
are owned by [sessions-and-transcript](sessions-and-transcript.md),
[turn-lifecycle-and-scheduling](turn-lifecycle-and-scheduling.md), and
[configuration-and-credentials](configuration-and-credentials.md).

## Identity model

Every semantic identity is a distinct, opaque, UUID-backed newtype built by the
`define_identity!` macro in `crates/domain/src/lib.rs`: `DurableCommandId`,
`SessionId`, `AcceptedInputId`, `TurnId`, `TurnAttemptId`, `ModelCallId`,
`ProviderTargetEvidenceId`, `ToolRequestId`, and `ToolAttemptId` there, plus
`ImportedConversationId` and `ImportedTranscriptEntryId`
(`imported_conversation.rs`), `SemanticTranscriptEntryId` and
`ContextFrontierId` (`context_frontier.rs`), `DirectModelSelection` and
`ModelAlias` (`configuration.rs`), and `ProviderModelIdentity`
(`model_call.rs`). Each exposes only `from_uuid`, `as_uuid`, and `into_uuid`;
the macro derives value semantics and `Debug` but no storage or serialization
traits, so every storage boundary maps explicitly (INV-001, INV-002). The
derived `Debug` is the one logging-reachable render path (see Encoding).

Identities fall into three supply classes:

- **Caller-supplied idempotency identity** — `DurableCommandId` only. Each
  application request constructor accepts the caller-supplied value, and the hub
  accepts any non-sentinel RFC 9562 UUID — the nil and max sentinels are
  rejected (see below) — without checking its version bits. Why: idempotency
  correctness comes from the owner-global durable claim plus canonical payload
  comparison, never from trusting a caller's clock or version bits (INV-012).
- **Hub-minted durable-fact identity** — `SessionId`, `ImportedConversationId`,
  `ImportedTranscriptEntryId`, `AcceptedInputId`, `TurnId`, `TurnAttemptId`,
  `SemanticTranscriptEntryId`, `ContextFrontierId`, `ModelCallId`,
  `ToolRequestId`, and `ToolAttemptId` today; `ProviderTargetEvidenceId` is
  assigned here but not yet minted (see Open edges). All production generators
  mint UUIDv7 (`uuid::Uuid::now_v7()`). Why: the recorded rationale for UUIDv7
  is insertion locality for append-heavy Postgres B-tree keys without changing
  the 128-bit storage shape; no index-level artifact measures this.
- **Configuration reference key** — `DirectModelSelection` and `ModelAlias`.
  Callers supply them inside command payloads to name owner-configured model
  selections; they persist in `uuid` columns (`direct_model_selection_id`,
  `model_alias_id`), and alias meaning resolves through a definition lookup at
  domain preparation, so an unknown alias becomes a recorded rejection, not an
  accepted identity.

`ProviderModelIdentity` names the hub's normalized provider/model value space.
It is persisted (`turn_lifecycle.pinned_provider_model_identity_id`,
`model_call.resolved_provider_model_identity_id`) and supplied as an
owner-configured key from hubd's model-configuration file; how provider-reported
data normalizes into it remains open (see Open edges).

UUID contents are never semantic. No code derives acceptance order, queue order,
lifecycle precedence, ancestry, ownership, or authorization from UUID bytes or
embedded timestamps; those facts live in purpose-specific domain values and
records (INV-001, INV-004).

The nil and max UUIDs are rejected as `DurableCommandId` values at two
boundaries: checked command/request construction (`try_new` on
`CreateSessionRequest`, `CreateSessionFromImportedFrontierRequest`,
`ReplaceSessionDefaultsRequest`, and `SubmitInputRequest` in
`crates/application`, plus `DecideToolRequest` in `crates/domain`) and
persistence decoding (`durable_command_id_from_uuid` in
`crates/persistence/src/mapping.rs`). Rejection occurs before a canonical
command can reach a transaction and claims no identifier. Why: sentinel-like
values are common accidental defaults and would otherwise become permanent
owner-global claims.

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
| `UuidV7SubmitInputIdGenerator`                       | `AcceptedInputId`, `TurnId`, `SemanticTranscriptEntryId`, `ContextFrontierId`                                                |
| `UuidV7StartEligibleTurnIdGenerator`                 | `SemanticTranscriptEntryId`, `ContextFrontierId`, `TurnAttemptId`                                                            |
| `UuidV7StartupScanIdGenerator`                       | `SemanticTranscriptEntryId`, `ContextFrontierId`, `TurnId` (reclassified successors)                                         |
| `UuidV7ModelCallExecutionIdGenerator`                | `ModelCallId`, `SemanticTranscriptEntryId`, `ContextFrontierId`, `TurnId` (reclassified successors)                          |
| `UuidV7ToolLoopIdGenerator`                          | `ToolRequestId`, `ToolAttemptId`, `ModelCallId`, `SemanticTranscriptEntryId`, `ContextFrontierId`, `TurnAttemptId`, `TurnId` |

`ProviderTargetEvidenceId` exists as a domain type but has no production minting
seam yet; its generator lands with its owning slice.

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
identifying the kind. The hubd startup-failure site logs
`session_id = ?error.session` and `turn_id = ?error.turn` — the derived `Debug`
of `Option<SessionId>`/`Option<TurnId>`, which renders `Some(SessionId(..))` or
`None`, not bare canonical UUID text (`apps/hubd/src/main.rs`).

The local [process protocol](process-protocol.md) maps identity values at its
wire adapter boundary and admits commands through the same application services;
domain types acquire no serialization trait. Public URL identity forms remain
open.

## Durable command records

All claimed command identifiers live in one owner-global, append-only
`durable_command` registry (migration `202607180001` and successors): primary
key `command_id`, a closed `command_kind` discriminator (`create_session`,
`create_session_from_imported_frontier`, `replace_session_defaults`,
`submit_input`, `decide_tool_request`), a kind-scoped `storage_version`, and
`claimed_at` (`transaction_timestamp()`), which is non-semantic operational
metadata. No command kind, session, or client has a separate command-ID
namespace.

Each admitted kind has one purpose-specific typed record family
(`create_session_command`, `create_session_from_imported_frontier_command`,
`replace_session_defaults_command`, `submit_input_command`,
`decide_tool_request_command`) keyed one-to-one by `command_id`, storing every
caller-supplied semantic field, the terminal `applied`/`rejected` result
discriminator, and the typed result fields, all under `CHECK` constraints and
foreign keys. Kind and version agreement between the registry row and its typed
record is enforced by a composite foreign key, and a deferred constraint trigger
(`durable_command_requires_typed_record`, executing function
`require_durable_command_typed_record`) requires exactly one typed record per
claim at every transaction boundary. Why: typed relational records keep each
command's comparison payload and result reviewable and constraint-checked
instead of delegating meaning to a serializer; there is no universal JSONB or
byte-blob payload anywhere.

For `SubmitInput`, a second deferred constraint trigger
(`submit_input_command_requires_correlated_effect`, migration `202607180003`,
redefined for occupied-slot pending steering in `202607180005`) enforces effect
correlation at every transaction boundary: an `applied` turn-origin row must
agree field-by-field with exactly one committed `accepted_input` plus
`queued_input_origin` effect, including the frozen model configuration; an
applied `next_safe_point` row instead initially correlates with exactly one
`pending_steering` accepted input naming the expected active turn, with no
`queued_input_origin` effect permitted; a `rejected` row must have no
accepted-input effect; and an `unknown_model_alias` rejection must match real
alias evidence in `session_defaults_version`. The next-safe-point receipt
remains immutable when that accepted input later becomes consumed steering or a
reclassified origin. Equal replay returns its original
`Applied(PendingSteering)` result only after the accepted input's current
lifecycle passes the correlation checks owned by
[persistence-protocol](persistence-protocol.md). Why: replay returns recorded
results as truth, so an applied record without its exact committed effect must
be unable to commit.

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

New `CreateSession`, `CreateSessionFromImportedFrontier`, and
`ReplaceSessionDefaults` records use version 2 for the complete defaults value;
version 1 reconstitutes with dangerous blanket approval disabled. `SubmitInput`
and `DecideToolRequest` use version 1. `CreateSession` records applied results
only (its one preparation failure is an error, not a recorded rejection);
`CreateSessionFromImportedFrontier` also records applied results only, because a
missing conversation named by the frontier or a boundary absent from that
conversation is a pre-claim admission error rather than an authoritative
rejection; `ReplaceSessionDefaults` and `SubmitInput` and `DecideToolRequest`
record both applied results and closed, typed rejection discriminators.
Authoritative rejections claim the identifier exactly as applied results do.

## Replay and equality

The canonical command payload is the typed domain value constructed at the
boundary before registry lookup — not a serialization. Structural equality
(hand-written `PartialEq` on `CreateSession`,
`CreateSessionFromImportedFrontier`, `ReplaceSessionDefaults`, `SubmitInput`,
and `DecideToolRequest` in `crates/domain`) covers every caller-supplied
semantic field and excludes `DurableCommandId`. Why: the identifier is the
lookup key that names the payload, not part of the meaning it names.

Every command repository (`crates/persistence/src/create_session.rs`,
`create_session_from_imported_frontier.rs`, `replace_session_defaults.rs`,
`submit_input.rs`, and the decision path in `tool_loop.rs`) follows one claim
protocol, with registry lookup as the first durable operation, before any
current-state validation (INV-012):

1. Inspect the registry. If the identifier is claimed by the same kind, load and
   reconstruct the recorded typed payload and result through domain-owned
   reconstitution, compare structurally, and roll back: equal replay returns the
   recorded terminal result; any difference — including a different kind — is
   conflicting reuse, returned without disturbing the recorded meaning.
2. If unclaimed, `INSERT ... ON CONFLICT DO NOTHING` claims the registry row. A
   lost race re-inspects and resolves against the winner's committed record; a
   winner row that cannot then be read is corruption.
3. First handling commits the registry row, the typed payload record, the
   terminal result, and every applied domain effect in one transaction. No
   applied result is returned before commit, and a failed transaction claims no
   identifier.

After registry inspection and before claiming an unseen identifier, a command
may perform an owner-specified pre-claim admission read.
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

Each application service calls its atomic transaction port exactly once and
surfaces infrastructure failure to its caller without retry or receipt
reconstruction (the `CreateSessionTransaction` contract in
`crates/application/src/create_session.rs`, the
`CreateSessionFromImportedFrontierTransaction` contract, and the corresponding
transaction-failure tests in all five services, including
`decide_service_returns_transaction_failure_without_retry`). Because a failed
transaction claims no identifier, retransmitting under the same
`DurableCommandId` is the caller's retry path and replays or claims cleanly.
Every repository also treats an unreadable claimed payload or result as typed
corruption rather than unclaimed state, including the imported-frontier command.

Reconstructed-then-compare ordering means a storage representation change can
never turn an equal command into conflicting reuse; unknown kinds and storage
versions fail explicitly as corruption. Equal semantic content never merges
distinct commands, and callers needing corrected intent after a recorded
rejection must use a new identifier.

## Actor attribution

`Actor` (`crates/domain/src/actor.rs`) is the closed typed provenance of a
durable command's initiating agency: `Owner`, `Model { turn: TurnId }`,
`Recovery`, or `Tool { request: ToolRequestId }`. Equality is structural; a
carried identity is a validated reference, not minting authority, and
attribution confers no lifecycle, authorization, or approval authority (INV-001,
INV-020).

`SubmitInput` is the one command kind whose payload carries an `actor` field.
Its constructor fixes `Actor::Owner` — no caller can supply another variant, and
no code path constructs a non-owner command issuer. The actor participates in
replay equality and hashing: replaying a claimed identifier under a different
actor is conflicting reuse (INV-012). Why: attribution recorded outside equality
could be laundered by replaying one claimed identifier under a different claimed
agency. The field is constant `Owner` today; carrying it anyway keeps the
truthful-`Owner`-backfill window open for kinds added before non-owner agencies
exist.

Storage follows the closed-discriminator convention: `actor_kind`
(`owner`/`model`/`recovery`/`tool`) plus `actor_turn_id` and
`actor_tool_request_id` reference columns with a `CHECK`-enforced variant shape
in `submit_input_command`. Unknown or malformed stored spellings fail decoding
as corruption, and domain reconstitution independently compares the stored actor
against the canonical command's actor (`StoredActorMismatch`), so a stored
non-owner actor fails closed.

`CreateSession` and `ReplaceSessionDefaults` v1 carry no actor field in payload
or storage, and no recorded-transition family (including startup-scan
terminalizations) has adopted an attribution field. See Open edges. Actor
answers who issued one command; `SessionCreationCause` answers why a session
exists — they are independent facts, and neither substitutes for the other (see
[sessions-and-transcript](sessions-and-transcript.md)).

## Durable-command telemetry correlation

Operational telemetry is emitted through the `tracing` facade by
`crates/application` and `apps/hubd`; `crates/persistence` and `crates/domain`
have no `tracing` dependency and emit none. Subscriber selection and
installation live only in `apps/hubd` (see
[runtime-substrate](runtime-substrate.md) for the runtime and the operator
failure taxonomy). Telemetry events correlate durable failures with hub-minted
aggregate identifiers — `session_id`, turn identities, phase, and failure-class
fields — in the two render forms described under Encoding.

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
  accepted version; the truthful `Owner` backfill via another kind-scoped
  storage version remains available but unexercised.
- `CreateSession` actor adoption remains an explicit owner choice; v1/v2 leave
  its attribution implicit.
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
  identities are exposed outside the single-owner boundary or treated as
  capabilities.
- Which command kinds may admit non-owner actors, and under what verification,
  remains with reserved delegation and authorization decisions.

# Identity, commands, and telemetry correlation

This subsystem gives each identity-bearing durable fact an opaque identity,
records every caller command once so replay is deterministic, and records who
issued a command without granting that record any authority.

## Overview

Signalbox names each identity-bearing durable fact with a UUID-backed identity
type and each caller command with one durable command identifier. Rows keyed by
a parent identity and an ordinal carry no identity of their own. This page owns
the identity types, the command registry, the actor field that records who
issued a command, and the rule for what operational telemetry may carry.
Transaction mechanics, locking, and reconstitution belong to
[persistence-protocol](persistence-protocol.md); what each command does belongs
to the page for its subsystem.

Identities come from three sources. The caller supplies the `DurableCommandId`
that every application request constructor accepts as its idempotency key. A
caller also passes through a replay identity minted outside the daemon, such as
the webhook delivery key [repo-watch](repo-watch.md) records. The daemon mints
the identity of every other identity-bearing fact it records. Configuration
reference keys are operator-configured references, not identities: a direct
model selection or a model alias arrives inside a command payload and names a
configured selection. `ProviderModelIdentity`, a normalized provider-and-model
value the operator configures, is stored on turn and model-call rows and is
neither minted nor a command key.

The identity types are built by the `define_identity!` macro in `crates/domain`.
That crate depends on `uuid` without a generation feature and cannot mint an
identity, so each orchestration slice in `crates/application` defines its own
generator trait and the daemon runtimes outside those slices mint directly.

The registry is the `durable_command` table. A registry row carries the command
identifier, a closed command kind, a kind-scoped storage version, the claim
time, and the issuer principal the admitting boundary stamped. Each admitted
kind has one typed record family keyed one-to-one by command identifier; the
record holds the caller-supplied fields under check constraints and foreign
keys. The canonical command payload is the typed domain value constructed at the
boundary, not a serialization, and construction ordinarily precedes registry
lookup. Reading the registry yields two error families, storage corruption and
infrastructure failure; `crates/persistence/src/command_registry.rs` defines the
corruption family. A recorded domain rejection is decoded from the typed record
and returned as a replay outcome, not as a registry error.

`Actor` in `crates/domain` records, from a closed set, what issued a command:
the user, daemon core, the model output of one turn, the startup recovery scan,
the execution of one tool request, or one verified program run. Only
submit-input and metadata-replacement commands carry an actor in their durable
payload. Repository watch and commissioned dispatch stamp a module issuer
principal on the registry row. Commissioned dispatch composes its initial input,
the one automated action attributed to the user, under the user actor.
Repository watch emits a held create-session command and does not submit an
initial input. Actor answers who issued one command; a session's creation cause,
owned by [sessions-and-transcript](sessions-and-transcript.md), answers why the
session exists, and neither fact substitutes for the other.

The program host's session capability verifies the retained run and fixes the
submit-input actor; the reference confers no authority. Submit-input writes
version 4 and reads versions 3 and 4, preserving each row's stored actor kind.
The `program` spelling carries only its run reference and is corruption in
version 3. Actor identity participates in replay equality and hashing.
Create-session carries no actor.

## Design decisions

The daemon accepts any non-sentinel RFC 9562 UUID as a `DurableCommandId`
without checking its version bits. Why: idempotency comes from the user-global
claim plus payload comparison, never from a caller's clock or version bits.

Application and daemon-runtime generators mint UUIDv7. Why: insertion locality
on append-heavy Postgres B-tree keys at no change to the 128-bit storage shape;
nothing measures the effect.

When the number of identities a transition needs is known only under the
repository lock, orchestration passes a generator closure into the transaction
port. Why: the domain transition receives a typed identity, the domain stays
generation-free and deterministic, and no inventory read precedes the lock.

Each command's comparison payload and result live in typed relational records,
so they stay reviewable and constraint-checked; there is no universal JSONB or
byte-blob payload.

A same-identifier retry is admitted only after a transaction that left no claim;
a recorded failure replays as failed.

The repository reconstructs the recorded payload before comparing it, so a
change of storage representation can never turn an equal command into
conflicting reuse.

Typed error `Debug` and `Display` output may contain a raw command identifier
and is an internal value; telemetry logs classification fields, never a
formatted error.

## Boundary contracts

Every command handler inspects the registry for the command identifier before it
validates anything against current state. Replaying the same command with the
same payload returns the recorded result once the command has settled; while it
is pending the handler reports busy, except that review orchestration and runner
replacement resume their own pending commands. Replaying it with a different
payload or kind is a conflict and changes nothing. A single-transaction command
commits its registry row, payload record, result, and every effect together, and
a failed transaction leaves no claim behind. A recorded rejection claims the
identifier the same way an applied command does.

Recording who or what caused an action is provenance only. It grants no
lifecycle, authorization, or approval authority. No automated path can attribute
its action to the user, and an action by a model never counts as an action by
the user.

Unsigned 64-bit ordinals are stored as numeric(20,0). What kind of thing an id
names is known from its table and column, never from the UUID's bytes. No code
derives acceptance order, queue order, lifecycle precedence, ancestry,
ownership, or authority from a UUID; listing rows by identifier for display or
paging is not such a derivation.

The identity macro derives value semantics and `Debug` but no storage or
serialization trait, so every storage boundary maps explicitly.

The nil and max UUIDs are rejected as `DurableCommandId` values at checked
request construction and again at persistence decoding. Why: they are common
accidental defaults and would otherwise become permanent user-global claims.

Orchestration generates each fresh identity candidate immediately before the
domain transition that creates the fact. No Postgres column has an
identity-generating default.

Workspace registration, Git remote minting, and withdrawal write their existing
typed registry records with application-supplied UUIDv7 identities. Equal replay
returns the original fact identity, including replay of a withdrawn mint.

Recovery reconstitutes committed facts under their stored identities; the
startup scan mints identities only for the new facts it records.

The recorded receipt returned on equal replay may name a different identity than
the fresh candidate generated for that invocation, and the candidate is
discarded.

All claimed command identifiers live in one user-global registry; no command
kind, session, or client has a separate namespace. The registry and every typed
record table are append-only, enforced by `reject_immutable_record_change`
triggers, except two records that change once under their own guards. A
compaction command record moves once from pending to applied or failed, which
keeps its request fields immutable. A review-orchestration command's pending
intent row is deleted only by the transaction that installs the command's
terminal receipt. A claimed identifier's recorded meaning is therefore never
rewritten.

The boundary that admitted a command stamps the issuer principal on its registry
row: the principal's kind and, for a module, the module name.

A command payload type that implements structural equality by hand covers every
caller-supplied semantic field and excludes the command identifier. Why: the
identifier is the lookup key that names the payload, not part of the meaning it
names.

For submit-input, equal replay returns the recorded result only when current
durable state still proves that result correlates with committed effects;
otherwise the adapter fails closed.

`CancelProgramRun` records its receipt and any applied terminal journal delivery
atomically; equal replay returns the stored receipt before inspecting the run.

`ClearCredentialExclusion` claims its exact target and terminal outcome in one
append-only typed command record; equal replay returns that outcome before
current exclusion state is evaluated.

A claimed registry row whose typed record is missing, duplicated, of a
mismatched kind, or undecodable is storage corruption, never an unseen command.
Why: treating it as unseen would let one identifier acquire a second meaning.

A storage version earlier than a field's introduction cannot carry the semantic
choice that field records: such a record reconstructs with the field's defined
default value, and any other stored value is corruption. An older reader rejects
a newer record instead of discarding a decision it cannot represent.

Imported-creation payloads from storage version 6 and create-session payloads
from storage version 8 retain the optional runner placement; earlier supported
versions reconstitute it absent. Placement participates in replay equality and
hashing, including template-derived creation. Imported-creation version 4 and
create-session version 5 remain unsupported and unwritten.

Each single-transaction application service calls its atomic transaction port
exactly once, returns no applied result before that transaction commits, and
surfaces infrastructure failure to its caller without retry or receipt
reconstruction.

After registry inspection and before it inserts the claim for an unseen
identifier, a handler may read current state and reject on it; such a rejection
is an admission error and claims nothing. The handler inserts the claim together
with its result, applied or rejected.

Equal semantic content never merges distinct commands, and a caller who needs
corrected intent after a recorded rejection uses a new identifier.

No constructor accepts an arbitrary actor; each fixes the one actor its boundary
proves, and model and recovery issuers are unconstructible.

Submit-input and metadata-replacement commands include the actor in replay
equality and hashing, so replaying a claimed identifier under a different actor
is conflicting reuse. Why: otherwise a replay could substitute its agency for
the recorded attribution.

Operational telemetry is emitted through the `tracing` facade by
`crates/application` and `apps/signalboxd`; `crates/persistence` and
`crates/domain` have no `tracing` dependency and emit none.

No telemetry site emits a caller-supplied command identifier in any form: no raw
UUID, prefix, digest, or token. Telemetry never records a blob digest or other
content-derived identifier, reader identity, bytes, extracted text, filename,
declared type, parser message, stderr, path, or credential. The general rule for
what errors and logs may contain is in [process-protocol](process-protocol.md).

`reload_configuration` has a closed registry kind and append-only typed request
and result record families. Its request has no comparison fields; the command
identity is excluded from structural equality. The request record retains the
checked replacement and prior snapshots as canonical JSON and the rule-set
digest; intent and terminal result may commit in separate transactions. A
pre-effect read or validation failure claims the identity with its terminal
result atomically.

`ReplaceLostRunner`, `AbandonLostRunner`, and `PromotePendingRunner` have
immutable typed request and result families in the user-global command registry.
Abandonment, promotion, and an immediately installable non-provisioning
replacement claim and settle in one transaction. A provisioning replacement
claims its complete request and one single-use authorization, waits without a
transaction, and installs its exact retained receipt and terminal result
together. Equal pending replay joins the retained operation; startup resumes
unterminated replacement commands before admitting process clients.

OAuth provisioning, re-provisioning, and deletion have distinct registry kinds
and typed relational request and result families keyed by command identifier.
The deferred typed-record trigger requires each claim's request, both record
families are append-only, and structural replay equality compares the operation
and profile while excluding the command identifier. A no-exchange rejection
claims and commits its terminal receipt in one transaction. Device exchanges
commit the claim before network work and authorization with the terminal
receipt.

## Planned

- A production generator for `ProviderTargetEvidenceId`:
  [design](../design/identity-and-commands.md).

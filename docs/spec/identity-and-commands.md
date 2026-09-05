# Identity, commands, and telemetry correlation

This subsystem gives every durable fact an opaque identity, records every caller
command once so that replay is deterministic, and records who issued a command
without granting that record any authority.

## Map

Signalbox names each durable thing with a UUID-backed identity type and each
caller command with one durable command identifier. This page owns the identity
types, the command registry, the actor field that records a command's initiating
agency, and the rule for what operational telemetry may carry. Transaction
mechanics, locking, and reconstitution belong to
[persistence-protocol](persistence-protocol.md); what each command does belongs
to the page for its subsystem.

Identities come from three sources. The caller supplies exactly one, the
`DurableCommandId` that every application request constructor accepts as its
idempotency key. The daemon mints the identity of every durable fact it records:
sessions, imported conversations and their entries, accepted inputs, turns and
turn attempts, transcript entries and frontiers, model calls, tool requests and
tool attempts. Configuration reference keys are the third source: a direct model
selection or a model alias arrives inside a command payload and names an
operator-configured selection. `ProviderModelIdentity` is a normalized
provider-and-model value the operator configures; it is stored on turn and
model-call rows and is neither minted nor a command key.

The identity types are built by the `define_identity!` macro in `crates/domain`.
Generation is an application-layer effect: `crates/domain` depends on `uuid`
without a generation feature and cannot mint an identity, and
`crates/application` holds one generator per orchestration slice.

The registry is the `durable_command` table. A registry row carries the command
identifier, a closed command kind, a kind-scoped storage version, the claim
time, and the issuer principal the admitting boundary stamped. Each admitted
kind has one typed record family keyed one-to-one by command identifier, which
holds the caller-supplied fields under check constraints and foreign keys. The
canonical command payload is the typed domain value constructed at the boundary,
not a serialization, and construction ordinarily precedes registry lookup.
Reading the registry yields three distinct error families: storage corruption,
infrastructure failure, and recorded domain rejection;
`crates/persistence/src/command_registry.rs` defines the corruption family.

`Actor` in `crates/domain` is the closed provenance of a command's initiating
agency: the user, daemon core, the model output of one turn, the startup
recovery scan, or the execution of one tool request. Only submit-input and
metadata-replacement commands carry an actor in their durable payload. Actor
answers who issued one command; a session's creation cause, owned by
[sessions-and-transcript](sessions-and-transcript.md), answers why the session
exists, and neither fact substitutes for the other.

## Decisions

The daemon accepts any non-sentinel RFC 9562 UUID as a `DurableCommandId`
without checking its version bits. Why: idempotency comes from the user-global
claim plus payload comparison, never from a caller's clock or version bits.

Production generators mint UUIDv7. Why: insertion locality on append-heavy
Postgres B-tree keys at no change to the 128-bit storage shape; nothing measures
the effect.

When the number of identities a transition needs is known only under the
repository lock, orchestration passes a generator closure into the transaction
port, so the domain transition receives a typed identity while the domain stays
generation-free and deterministic and no inventory read precedes the lock.

Each command's comparison payload and result live in typed relational records,
so they stay reviewable and constraint-checked; there is no universal JSONB or
byte-blob payload anywhere.

A caller retries a failed command by retransmitting under the same identifier.
Why: a failed transaction claims nothing, so the retry replays or claims
cleanly.

The repository reconstructs the recorded payload before comparing it, so a
change of storage representation can never turn an equal command into
conflicting reuse.

Typed error `Debug` and `Display` output may contain a raw command identifier
and is an internal value; telemetry logs classification fields, never a
formatted error.

## Contracts

Every command handler first records the command identifier in the registry,
before it validates anything against current state. Replaying the same command
with the same payload returns the recorded result. Replaying it with a different
payload or kind is a conflict and changes nothing. A command's registry row,
payload record, result, and every effect commit in one transaction. If that
transaction fails, the registry holds no row for the identifier. A rejection
records the identifier in the registry exactly as an applied command does.

Recording who or what caused an action is provenance only. It grants no
lifecycle, authorization, or approval authority. No automated path can attribute
its action to the user, and an action by a model never counts as an action by
the user.

Unsigned 64-bit ordinals are stored as numeric(20,0). What kind of thing an id
names is known from its table and column, never from the UUID's bytes. No code
derives order, time, ancestry, ownership, or authority from a UUID.

The identity macro derives value semantics and `Debug` but no storage or
serialization trait, so every storage boundary maps explicitly.

The nil and max UUIDs are rejected as `DurableCommandId` values at checked
request construction and again at persistence decoding. Why: they are common
accidental defaults and would otherwise become permanent user-global claims.

Orchestration generates each fresh identity candidate immediately before the
domain transition that creates the fact. No Postgres column has an
identity-generating default.

Recovery reconstitutes committed facts under their stored identities; the
startup scan mints identities only for the new facts it records.

On equal replay the recorded receipt is returned. It may name a different
identity than the fresh candidate generated for that invocation, and the
candidate is discarded.

All claimed command identifiers live in one user-global registry; no command
kind, session, or client has a separate namespace. The registry and every typed
record table are append-only, enforced by `reject_immutable_record_change`
triggers, so a claimed identifier's recorded meaning is never rewritten.

The issuer principal on a registry row, its kind and for a module the module
name, is stamped by the boundary that admitted the command.

Every command payload type implements structural equality by hand, covering
every caller-supplied semantic field and excluding the command identifier. Why:
the identifier is the lookup key that names the payload, not part of the meaning
it names.

For submit-input, equal replay returns the recorded result only after current
durable state still proves that result correlates with committed effects;
otherwise the adapter fails closed.

A claimed registry row whose typed record is missing, duplicated, of a
mismatched kind, or undecodable is storage corruption, never an unseen command.
Why: treating it as unseen would let one identifier acquire a second meaning.

Each stored field is absent before its introducing storage version, so an older
reader rejects a newer record instead of discarding either decision.

Each single-transaction application service calls its atomic transaction port
exactly once, returns no applied result before that transaction commits, and
surfaces infrastructure failure to its caller without retry or receipt
reconstruction.

After registry inspection and before claiming an unseen identifier, a command
may perform one pre-claim admission read. A failed admission read returns an
admission error and claims nothing; an authoritative rejection is derived only
after the claim and is stored for replay.

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

## Not built

- Registry kinds and typed records for the runner recovery commands (replace,
  abandon, promote): [design](../design/identity-and-commands.md).
- A production generator for `ProviderTargetEvidenceId`:
  [design](../design/identity-and-commands.md).
- Writers and generators for `WorkspaceId`, `GitRemoteMintId`, and
  `GitRemoteWithdrawalId`: [design](../design/identity-and-commands.md).
- Imported-creation storage version 4, the optional runner-placement payload:
  [design](../design/identity-and-commands.md).
- Create-session storage version 5, the optional session runner placement:
  [design](../design/identity-and-commands.md).
- A program arm of `Actor` and a program admissibility path for submit-input:
  [design](../design/identity-and-commands.md).
- An actor field on replace-session-defaults records:
  [design](../design/identity-and-commands.md).
- Actor adoption on create-session records:
  [design](../design/identity-and-commands.md).

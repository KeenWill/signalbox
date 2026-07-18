# ADR-0034: Durable-command storage and equality

- Date: 2026-07-17
  human reviewer assigned
- Supersedes: none
- Superseded by: none
- Depends on: [ADR-0001](0001-domain-terminology-and-identity.md),
  [ADR-0022](0022-persistence-representation.md),
  [ADR-0027](0027-input-delivery-lifecycle.md),
  [ADR-0032](0032-postgres-implementation-dependencies.md), and
  [ADR-0033](0033-identity-generation-supply-and-encoding.md)
- Refines: ADR-0001 and ADR-0027's canonical typed command comparison and
  ADR-0022's owner-global command registry
- Resolves: canonical durable-command payload and result storage, representation
  versioning, and replay equality within the accepted relational baseline
- Decision questions: owner-global registry shape; typed payload and result
  records; structural equality; normalization and version evolution; rejection
  representation; JSON, byte encoding, and hash authority

## Context

ADR-0001 and ADR-0027 already fix the idempotency semantics. Purpose-specific
boundary construction produces a discriminated typed command without consulting
mutable aggregate state. Lookup by owner-global `DurableCommandId` occurs
before current-state validation. The first committed handling stores the
comparison payload and one terminal applied-or-rejected result atomically with
the command's effects. Equal replay returns that result; any different kind,
target, or caller-supplied semantic field is conflicting reuse.

ADR-0022 selects one owner-global `durable_command` registry and normalized
Postgres records, but leaves the canonical versioned payload encoding open. Its
illustrative row uses JSONB only as a placeholder. ADR-0032 deliberately leaves
SQLx JSON support disabled until this decision.

The remaining choice is whether “canonical” means one serialized byte form or
the already-accepted canonical typed domain value. A universal blob is easy to
insert but weakens database constraints, makes representation changes look like
semantic changes, and risks turning serializer behavior or a hash into replay
authority. Purpose-specific records cost more schema, but preserve the
relational and domain boundaries already selected.

## Decision

### One registry, typed subordinate records

Signalbox stores every claimed command ID in one append-only, owner-global
`durable_command` registry. Its primary key is `command_id`; it also records a
closed command-kind discriminator, the applicable storage representation
version, and non-semantic operational metadata such as claim time. No command
kind, session, or client receives a separate command-ID namespace.

Each admitted command kind has one purpose-specific, normalized subordinate
record family keyed one-to-one by `command_id`. That family stores:

- every caller-supplied semantic field in the canonical typed payload except
  the command ID;
- one terminal result discriminator, `applied` or `rejected`;
- every typed result field required to reconstruct the recorded result; and
- a representation version scoped to that command kind when payload and result
  evolution require independent decoding.

Scalar domain values use typed columns with `CHECK` constraints and foreign
keys where the relationship is part of the accepted command. Repeated,
ordered, or set-valued fields use purpose-specific child relations whose
mapping reconstructs the matching domain collection semantics. Server-derived
state that is not part of the caller comparison payload may appear only in the
typed result or the command's atomic domain-effect records.

The registry row, exactly one matching typed subordinate record, the terminal
result, and every applied domain effect commit in the same transaction.
Transactions expose no claimed registry row whose typed payload or result is
missing. The persistence mapping treats a missing, duplicate, mismatched-kind,
or undecodable subordinate record as storage corruption, never as a new
command or an unclaimed identifier.

This decision fixes the representation pattern, not final table or column names
and not speculative tables for commands that are not yet admitted by an
accepted decision.

### Canonical construction and replay equality

“Canonical command payload” means the purpose-specific typed domain value
constructed before lookup, not a canonical serialization.

For a received command, the owning boundary:

1. decodes and normalizes caller fields into the command kind's domain types
   without reading mutable aggregate state;
2. removes `DurableCommandId` from the comparison value;
3. looks up the owner-global registry;
4. if claimed, loads the matching versioned subordinate records and
   reconstructs their typed domain payload and terminal result; and
5. compares the reconstructed payload with the received payload using
   structural domain equality.

The command-kind discriminator, session or other target reference, and every
caller-supplied semantic field participate in equality. Domain distinctions
remain distinctions even when current state could make them operationally
equivalent. For example, `UseSessionDefault` does not equal
`ReplaceWith(current_default)`, and ordered values do not become sets merely
because one current command happens to contain the same members.

Equal replay returns the recorded typed result before any current-state
validation or server-derived recomputation. Unequal replay is conflicting
reuse. A record that cannot be decoded into its exact known domain variant
fails explicitly as storage corruption; it is never compared as raw columns or
silently coerced to a newer meaning.

### Typed terminal results

Applied results record the semantic identities and correlations that the
command created or changed and that callers or later proof validation need.
Authoritative rejections use a closed, command-specific rejection discriminator
and typed fields rather than an error string. Operational diagnostics may be
stored separately, but they do not determine replay equality, result meaning,
or proof authority.

Purpose-specific proof reconstitution remains domain-owned, as decided by
[ADR-0035](0035-domain-owned-persistence-reconstitution.md). These records must
retain the command kind, applied result, target aggregate, and any exact-set
correlation that the accepted proof semantics require, but this ADR does not
expose raw proof constructors to persistence code.

### Representation evolution

Adding a durable command kind requires its own explicit domain payload and
result algebra, normalized record family, fallible mappings, structural
equality tests, and migration. A generic property map or catch-all command row
cannot admit it.

A representation change uses a forward migration or a new kind-scoped storage
version. Every retained version must decode to the same domain value it meant
when committed. Replay equality occurs after decoding, so a storage migration
or equivalent spelling cannot turn an equal command into conflicting reuse.
Unknown versions fail explicitly. Storage versions are not protocol versions.

### No universal serialized authority

The baseline does not store canonical payloads or terminal results as one
universal JSONB value, serialized Rust value, Protobuf message, or opaque byte
string. It does not enable SQLx JSON support for command storage.

A digest may be a non-authoritative lookup or integrity optimization only when
the complete typed record remains available and a hash match is followed by
structural domain comparison. A digest, serializer output, or byte equality is
never command identity, payload equality, terminal-result meaning, or proof
authority.

A future command whose accepted domain payload is itself structured JSON may
justify a purpose-specific JSON field through its owning decision. That would
not convert the owner-global registry into a universal JSON envelope.

## Invariants

- INV-001: the registry records which typed payload and result a command ID
  names; the raw ID, row, or digest alone grants no lifecycle authority.
- INV-002: storage records and version codecs remain outside the domain, while
  comparison is performed over reconstructed domain values.
- INV-005: command receipt records do not become semantic history, operational
  evidence, client presentation, or one universal event representation.
- INV-007: an applied input command's typed receipt and all acceptance effects
  commit before acknowledgement in the same transaction.
- INV-012: one owner-global primary key prevents duplicate claims, and typed
  structural equality distinguishes replay from conflicting reuse.

## Strongest alternative

Store one canonical, versioned JSONB payload and result on every registry row.
That minimizes tables, makes command addition migration-light, and is easy to
inspect. It is rejected because canonical JSON rules would become a second
semantic model beside the domain types, database constraints would not express
the accepted variants and relationships, and serializer or normalization
changes could alter replay behavior. The selected typed records require more
migrations but keep each command's semantics reviewable and constraint-aware.

## Rejected alternatives

- **Serialized in-process domain values.** Rust layout and library changes are
  not a durable storage contract and would cross INV-002's boundary.
- **Canonical Protobuf or another future wire message.** Storage would become
  coupled to a protocol that has not been selected, and wire compatibility is
  not domain equality.
- **Payload hash as the comparison authority.** Collisions, canonicalization,
  and missing typed fields make a hash insufficient; ADR-0001 already rejects
  fingerprints as semantic identity.
- **One command table per kind with no global registry.** Per-table primary keys
  cannot enforce owner-global uniqueness across kinds and sessions.
- **Store applied commands only.** Authoritative rejection claims the ID and
  must replay after state changes; omitting it would let rejected intent gain a
  new meaning.
- **Compare database rows directly.** Column layout and representation versions
  are storage concerns; equality belongs to the reconstructed typed value.

## Consequences

Each new command kind adds explicit schema and mapping work. In return, schema
review exposes its complete comparison payload and result, Postgres can
constrain relationships, and replay semantics survive storage-version changes.
The first persistence slice can implement `CreateSession` without designing
records for every future command, while the one registry reserves its ID
owner-globally.

The persistence adapter needs corruption errors distinct from domain rejection
and infrastructure failure. Operators cannot repair an undecodable claimed
command by treating it as unseen; correction requires repairing or migrating
the durable record.

## Scenario walkthroughs

- **S01:** `CreateSession` and `SubmitInput` each claim the same owner-global
  registry but use their own typed payload/result records. Replaying an equal
  constructed value returns the recorded session or accepted-input result.
  Changing ancestry, defaults, session, delivery variant, or any caller field
  is conflicting reuse even if a serializer could produce similar bytes.
- **S03:** Restart reconstructs a queued turn from its lifecycle records and
  reconstructs the command receipt separately. Replaying the acceptance command
  returns its stored result without deriving eligibility or rereading current
  defaults.
- **S04:** A `ResolveAmbiguity` command stores its exact expected operation set
  in a purpose-specific relation. Replay compares the reconstructed canonical
  set and returns the recorded decision; it does not compare JSON array order
  or current wait membership.

## Open questions

- The domain-owned seam for reconstituting proof-bearing values from correlated
  applied command records is decided by
  [ADR-0035](0035-domain-owned-persistence-reconstitution.md); command replay is
  one consumer of that seam.
- Concrete record families land only with admitted command slices; this record
  does not choose names or fields before their domain payload and result exist.
- Wire command envelopes and compatibility are governed by
  [ADR-0019](0019-process-protocol.md) and
  [ADR-0021](0021-compatibility-and-negotiation.md); concrete schemas arrive
  with their implementation slices.

## Explicit non-decisions

This record adds no tables, migration, dependency, command kind, or public API.
It does not define semantic-entry payloads, physical frontier layout, proof
constructors, transport serialization, authentication, audit retention, or
operator repair tooling. It does not make database records domain types or
require a generic command repository interface.

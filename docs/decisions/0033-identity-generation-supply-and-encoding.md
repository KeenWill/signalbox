# ADR-0033: Identity generation, supply, and encoding

- Date: 2026-07-17 specialist human reviewer assigned
- Supersedes: none
- Superseded by: none
- Depends on: [ADR-0001](0001-domain-terminology-and-identity.md),
  [ADR-0005](0005-model-call-retry-semantics.md),
  [ADR-0022](0022-persistence-representation.md), and
  [ADR-0030](0030-context-frontier-snapshots.md)
- Refines: ADR-0001's open UUID mechanics, ADR-0022's native Postgres UUID
  boundary, and ADR-0030's open snapshot and semantic-entry identity mechanics
- Resolves: generation version, supply authority, minting boundary, and Postgres
  encoding for the UUID-backed identities in the accepted baseline; wire
  representation remains reserved
- Decision questions: caller supply versus hub minting; UUID version by supply
  class; application, domain, and database responsibilities; opacity and
  ordering; relational and diagnostic text encoding; reserved protocol scope

## Context

ADR-0001 gives every accepted semantic identity a distinct private UUID-backed
domain type. ADR-0005 adds provider-target evidence identity, while ADR-0030
adds context-frontier snapshot and semantic-transcript-entry identities.
ADR-0022 selects native Postgres `uuid` columns for the UUID-backed identities
it names, but predates the in-process frontier and semantic-entry identities.
Those records deliberately leave generation version and minting authority open.

The durable-command boundary already constrains one supply choice. A client must
retain the same `DurableCommandId` across retransmission after an
acknowledgement is lost, and command lookup occurs before current-state
validation. In contrast, sessions, accepted inputs, turns, attempts, calls,
evidence, semantic entries, and frontier snapshots become valid only through
hub-owned transitions. Letting callers or database defaults mint those values
would move creation authority away from those transitions.

Persistence now needs one complete rule that preserves the domain/storage
boundary, gives append-heavy UUID indexes reasonable locality, and does not turn
UUID contents into domain ordering or authorization.

## Decision

### Two supply classes

The accepted baseline has two identity supply classes:

| Supply class                         | Identities                                                                                                                                                                                   | Authority                                                                                               |
| ------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------- |
| Caller-supplied idempotency identity | `DurableCommandId`                                                                                                                                                                           | The submitting caller generates and retains the value before first transmission                         |
| Hub-minted durable-fact identity     | `SessionId`, `AcceptedInputId`, `TurnId`, `TurnAttemptId`, `ModelCallId`, `ProviderTargetEvidenceId`, `ToolRequestId`, `ToolAttemptId`, `SemanticTranscriptEntryId`, and `ContextFrontierId` | Hub application orchestration generates the value for the domain transition that first creates the fact |

A caller supplies only `DurableCommandId`. A caller may reference a hub-minted
identity previously returned by the hub, but reference is not minting authority.
Kind, existence, ownership, and aggregate relationship remain independent
validations under ADR-0001.

First-party clients generate `DurableCommandId` values as UUIDv4. The hub
accepts caller-supplied UUIDs without requiring a particular RFC 9562 version:
idempotency correctness comes from owner-global uniqueness plus canonical
payload comparison, not from trusting a caller's clock or version bits. The
boundary rejects the nil and max UUID values as invalid sentinel-like command
identities before canonical command construction; that failure claims no
identifier.

Hub-minted durable-fact identities are UUIDv7. Their time-ordered layout
improves locality for the append-heavy Postgres B-tree indexes selected by
ADR-0022. UUIDv7's embedded timestamp is not a semantic fact. Code must not use
UUID bytes or timestamps to derive acceptance order, queue order, lifecycle
precedence, ancestry, ownership, authorization, or presentation order. Those
facts remain in their purpose-specific domain values and records.

This record does not select assignment rules for model-selection configuration
keys, provider-reported identifiers, runner identities, the opaque
`TranscriptFrontier` boundary, or identities added by future features. The
semantic-history decision that makes transcript frontiers selectable must define
that trusted boundary's production rule. Other owning decisions must place new
identities in a supply class when their creation boundaries are defined.

### Minting boundary

UUID generation is an application-layer effect. Hub application orchestration
generates the candidate immediately for the domain transition and transaction
that create the identified fact. The domain crate remains generation-free and
accepts opaque typed identities through its deliberately named UUID conversion
seams. Persistence maps already-minted domain values; it does not create them.

Postgres columns therefore have no identity-generating defaults. A transaction
that aborts may leave an unused generated UUID, but it leaves no durable fact
and that value is never recycled. Recovery reads committed identities from
storage and never remints them. A retry that creates a genuinely new durable
fact, such as a replacement call, receives a new identity through its creating
transition.

No identity is derived from content, a hash, a parent identity, a database
sequence, or another identity. Equal semantic content may create distinct facts
and therefore distinct identities.

### Postgres and textual encoding

Every UUID-backed identity listed in the two supply classes above, including
`SemanticTranscriptEntryId` and `ContextFrontierId`, uses native Postgres `uuid`
columns for its relational identity and reference fields. Table and column
position, foreign keys, and explicit mapping carry identity kind; matching UUID
bytes across different kinds prove nothing.

Where logs, diagnostics, or operator-facing configuration render one of these
UUIDs as text, they emit the lowercase hyphenated RFC 9562 form. The field name
or structured context identifies its domain kind. Domain identity types gain no
storage, logging, or serialization derives; each boundary maps explicitly
through the UUID conversion seams required by INV-002.

### Reserved wire scope

This record does not choose a protocol field type or public wire encoding.
Reserved protocol ADRs decide whether a wire identity is text, bytes, or a
generated message type; URL placement, compatibility, and decoding errors also
remain there. Any wire mapping must still construct the correct distinct domain
identity and may not infer its kind or authority from UUID contents.

## Invariants

- INV-001: every identity remains a distinct opaque domain type; UUID version
  and timestamp bits confer no kind, relationship, or authority.
- INV-002: generation belongs to application orchestration and encoding to
  boundary code; domain types gain no generator, SQL, or serialization role.
- INV-004: typed creation transitions, never equal content or UUID ordering,
  decide whether work retains or receives an identity.
- INV-007: identities acknowledged for accepted input are committed with the
  accepted facts; recovery reloads rather than regenerates them.
- INV-012: the caller owns command-ID stability across retransmission, while the
  owner-global durable claim and typed payload comparison remain the idempotency
  authority.

## Strongest alternative

Use UUIDv4 for every identity. That gives one simple rule, avoids timestamp
disclosure, and requires no time source. It is rejected for hub-minted values
because those values dominate append-heavy relational keys, where UUIDv7 gives
better insertion locality without changing the 128-bit storage shape. Timestamp
disclosure is acceptable inside the initial single-owner boundary because
identities are not capabilities and no rule depends on their unpredictability.

## Rejected alternatives

- **Require caller UUIDv7.** Client clocks and version bits add no idempotency
  guarantee, while rejecting otherwise unique UUIDs creates needless
  interoperability failures.
- **Hub-mint command IDs through a reservation handshake.** This adds another
  round trip and a partially reserved state merely to let a caller perform a
  retry that caller supply already supports.
- **Generate identities with database defaults.** The domain transition needs
  the typed identity before persistence, and database generation crosses the
  accepted mapping boundary.
- **Content-derived or name-based UUIDs.** Equal content may denote distinct
  accepted inputs, entries, snapshots, and operations; a fingerprint is not
  semantic identity.
- **Sequential integers or another sortable identifier family.** They conflict
  with the accepted UUID backing and add a new representation without a semantic
  benefit.
- **Text Postgres columns.** They are larger, admit formatting drift, and lose
  the database's UUID validation.

## Consequences

Application crates will enable the UUID generation capability they need while
`crates/domain` remains deterministic and generation-free. First-party clients
must persist each command ID until its terminal response is known and use a new
one for corrected intent after a recorded rejection.

Hub-minted UUIDs disclose an approximate creation time to readers of the
identifier. That disclosure must be reconsidered if a future authority model
exposes identities outside the owner's boundary or treats possession as security
authority. Converting an identity into a capability remains forbidden without a
separate decision.

## Scenario walkthroughs

- **S01:** The client creates separate UUIDv4 command IDs for `CreateSession`
  and `SubmitInput`. Hub orchestration creates UUIDv7 session, accepted-input,
  turn, semantic-entry, frontier, and attempt identities as the applicable
  transitions create those facts. Lost-acknowledgement replay uses the original
  command ID and creates none of those facts again.
- **S04:** Recovery reloads the committed turn, attempt, call, evidence, and
  frontier identities. It does not generate an identity for the startup scan. A
  later authorized replacement creates new attempt and call identities without
  using UUID order to establish succession.
- **S12:** A runner result references the hub-minted tool-attempt identity and
  its independent dispatch fence. The UUID timestamp neither replaces nor
  influences the generation comparison.

## Open questions

- Wire field types, public URL forms, and protocol serialization remain open
  within the [ADR-0019](0019-process-protocol.md) and
  [ADR-0021](0021-compatibility-and-negotiation.md) baselines.
- Runner identity supply, configuration-key assignment, and trusted
  `TranscriptFrontier` production remain with their owning feature decisions.
- A multi-owner or externally exposed authority model must reassess information
  disclosure and namespace scope without reinterpreting existing UUIDs.

## Explicit non-decisions

This record adds no dependency or code, selects no wire schema, and defines no
authentication or authorization mechanism. It does not make UUID order a clock,
identity possession a capability, or a database value a domain type. It does not
decide canonical durable-command payload storage, proof rehydration,
semantic-entry payloads, selectable transcript-frontier production, or physical
frontier layout.

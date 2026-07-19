# ADR-0039: Actor attribution for durable commands and recorded transitions

- Date: 2026-07-18
- Supersedes: none
- Superseded by: none
- Depends on: [ADR-0001](0001-domain-terminology-and-identity.md),
  [ADR-0003](0003-session-creation-and-transcript-ancestry.md),
  [ADR-0004](0004-turn-and-attempt-lifecycle.md),
  [ADR-0010](0010-initial-scheduler-mechanics.md),
  [ADR-0022](0022-persistence-representation.md),
  [ADR-0027](0027-input-delivery-lifecycle.md),
  [ADR-0033](0033-identity-generation-supply-and-encoding.md),
  [ADR-0034](0034-durable-command-storage-and-equality.md), and
  [ADR-0035](0035-domain-owned-persistence-reconstitution.md)
- Refines: ADR-0034's canonical comparison-payload composition for command kinds
  whose typed record families are first accepted after this record
- Resolves: which kind of agency initiated a durable command or recorded
  transition, as a first-class typed provenance fact
- Decision questions: closed initial actor set; replay-equality participation;
  relation to session creation cause; storage convention; adoption path for
  pending and already-accepted commands; deferred policy actor

## Context

Every baseline durable command is issued under the single owner's authority, so
no accepted record states which kind of agency initiated it: the answer has been
implicitly "the owner, except where the startup recovery scan acted." That
implicit answer is about to stop being derivable. ADR-0004's scan terminalizes
work no one asked to cancel, tool requests originate from a turn's model output
rather than from the owner, and the owner intends to build AI-actor permission
features whose subjects need provenance the durable records must already carry.
ADR-0034 freezes each command kind's comparison payload at its first accepted
record version, so adding a required attribution field after several agencies
can issue commands would force either an untruthful default or the kind of
semantic reinterpretation ADR-0034 forbids. Today one truthful backfill still
exists — owner authority is the only admissible issuer of every committed
command — which makes seeding attribution now cheap and retrofitting it later
expensive.

Attribution is provenance for a single-owner system. It is explicitly not
authentication, authorization, or identity verification, which remain reserved
(ADR-0015 through ADR-0018), and not multi-user tenancy, which remains a
non-goal.

## Decision

### One closed initial actor algebra

Signalbox records the initiating agency of durable commands and, where adopted,
recorded transitions as one closed typed domain value:

```text
Actor =
    Owner
  | Model {
        turn: TurnId
    }
  | Recovery
  | Tool {
        request: ToolRequestId
    }
```

This is semantic pseudocode, not final Rust, storage, or wire spelling. `Owner`
is the single owner's own authority, however connected. `Model { turn }` is
agency exercised by the model output of one identified turn, such as originating
a tool request. `Recovery` is the startup recovery scan acting under ADR-0004's
accepted authority; it carries no process-incarnation identity because ADR-0010
deliberately records none — the scan's context is the Recovery actor itself, not
a worker or boot identity. `Tool { request }` is agency exercised by the
execution of one identified tool request, such as delivering the result its
acceptance transition applies.

Every variant's referent is already an accepted fact: owner authority, `TurnId`,
and `ToolRequestId` come from ADR-0001, and the scan from ADR-0004 and INV-034.
A policy or scheduler actor is deferred, not included: no accepted decision
defines an autonomous policy agency that initiates anything, ADR-0010's
scheduler only executes transitions the accepted lifecycle already authorizes,
and ADR-0003's precedent excludes variants with no accepted referent. Equality
is structural over the variant and its carried identity. A carried identity is a
reference, not minting authority (ADR-0033), and confers no lifecycle authority
(INV-001).

In the baseline, the command boundary constructs only `Owner`: no accepted
decision lets a model, tool, or the scan issue a durable command, and `Recovery`
performs recorded transitions rather than commands. A decision that admits a
non-owner actor for a command kind must say so explicitly; the closed algebra
does not itself grant any agency the right to issue anything.

### Attribution is payload and participates in replay equality

The canonical typed payload of every durable command kind whose typed record
family is first accepted after this record contains one required `actor` field.
It is a caller-supplied semantic fact fixed at purpose-specific construction
before owner-global lookup, and it participates in ADR-0034's structural replay
equality: replaying a claimed command identifier with a different actor is
conflicting reuse, exactly as for any other payload field.

The alternative — recording attribution beside the payload as registry metadata
outside comparison — is rejected. ADR-0034 already routes non-semantic
operational metadata such as claim time outside the comparison and every
caller-supplied semantic fact inside it, and attribution is semantic: it is
precisely the fact later permission decisions will consume. Left outside
equality, one claimed identifier could be replayed under a different claimed
agency, and the stored attribution would be an unverifiable annotation rather
than part of what the identifier names.

### Actor and creation cause answer different questions

ADR-0003's `SessionCreationCause` answers why a session exists and lives in the
session's immutable creation provenance. `Actor` answers who or what issued one
command. `OwnerInitiated` is not replaced, renamed, or made redundant: a
`CreateSession` carrying `actor: Owner` and cause `OwnerInitiated` states one
fact about the command and one durable fact about the created session, and a
future delegated cause need not match the actor of the command that carried it.
Whether future cause variants reference actors is deferred to the decisions that
add those causes.

### Recorded transitions

A recorded lifecycle transition whose initiating agency is not already
established by an actor-bearing command may record the initiating `Actor` in its
typed record. The startup scan's terminalizations are attributed to `Recovery`;
a tool request's creation is attributable to `Model { turn }`; a tool result's
acceptance is attributable to `Tool { request }`. This record fixes the algebra
and its meaning. Which transition record families adopt an attribution field,
and when, lands with their owning slices, and attribution never changes a
transition's authority, guards, or ordering.

### Storage convention and adoption path

Actor is stored within each command's typed record family under ADR-0022 and
ADR-0034 conventions: a closed discriminator plus reference columns for the
variant identities, never a free-form string or generic principal table. Exact
table and column shapes are deferred to implementation slices, following
ADR-0035 practice.

Adoption follows the record-family boundary:

- A command kind whose typed record family is not yet accepted when this record
  is accepted carries `actor` from its first accepted version. At drafting time,
  this includes `SubmitInput`, which has no committed handling, and
  `ReplaceSessionDefaults`, whose merged four-field domain payload (decision
  log, 2026-07-18) precedes its still-in-flight record family; extending that
  payload with `actor` is an ordinary decision-log slice because no committed
  durable record's replay equality changes.
- `CreateSession`, whose typed record family and structural equality are already
  accepted, is amended only if the owner explicitly chooses. That choice would
  land as a dated amendment to ADR-0003's `CreateSession` payload plus an
  ADR-0034 kind-scoped storage version whose decoding of retained records
  assigns `Owner` — truthful and meaning-preserving, because owner authority was
  the only admissible issuer of every retained record. This record does not
  silently amend ADR-0003; declining leaves `CreateSession` attribution implicit
  and both states coherent.

## Invariants

- INV-001: an actor's carried identity is a validated reference to an existing
  fact of the right kind; it mints nothing and grants no lifecycle authority.
- INV-012: `actor` is part of the canonical comparison payload for the command
  kinds that carry it; equal replay requires an equal actor, and a different
  actor under a claimed identifier is conflicting reuse.
- INV-020: model agency stays a distinct typed actor; a `Model`-attributed fact
  never masquerades as `Owner`, and attribution is never approval.

## Strongest alternative

Introduce attribution only when the first non-owner agency can actually issue a
command, avoiding a field that is constant `Owner` today. It is rejected because
ADR-0034 fixes each command family's comparison payload at first acceptance:
retrofitting a required field after several agencies exist would need a backfill
rule chosen when `Owner` is no longer the only truthful answer, turning a cheap
closed field into a semantic migration. Seeding the algebra now costs one field
per new command family while the truthful backfill for already-accepted records
still exists.

## Rejected alternatives

- **Attribution outside the comparison payload.** Rejected above: replay could
  launder agency, and the recorded attribution would be unverifiable metadata
  rather than part of what the identifier names.
- **A free-form actor string or extensible principal table.** An unstructured
  value is not a substitute for a typed variant carrying an exact durable
  identity, matching ADR-0003's rule for creation causes.
- **A `Worker` or `BootId` variant.** ADR-0010 deliberately records no
  process-incarnation identity; the scan is the `Recovery` actor's context, not
  a per-process principal.
- **A `Policy` variant now.** No accepted decision defines an autonomous policy
  agency; an uninhabitable placeholder would hide an undecided semantic case.
- **Overloading `SessionCreationCause` as command attribution.** Cause is
  immutable session-owned provenance answering a different question; conflating
  them would repeat the `parent_session` mistake ADR-0003 rejected.
- **Inferring the actor at read time from correlated records.** Attribution is
  fixed at construction; deriving it later would make provenance depend on query
  logic and change as correlated records accrue.

## Consequences

Every future command record family carries one attribution discriminator and,
for `Model` and `Tool`, a reference column, and boundary construction must fix
the actor before owner-global lookup. In a single-owner system that field is
constant `Owner` for every baseline command — an accepted cost paid to keep the
truthful-backfill window open. In return, audit questions become typed domain
facts: which turn's model requested this tool operation, and whether recovery or
the owner terminalized a turn, are answered by records rather than
reconstruction. Later AI-actor permission decisions gain their subject
vocabulary without a schema retrofit, though they still owe their own
authorization, verification, and admissibility rules.

## Scenario walkthroughs

- **S01:** The owner's `SubmitInput` carries `actor: Owner` in its canonical
  payload from the command kind's first accepted version. Equal replay returns
  the recorded result; the same identifier with any other actor is conflicting
  reuse.
- **S04:** The startup scan ends a lost attempt; the terminalization record,
  where its family adopts attribution, states `Recovery` rather than implying
  owner action, and no process or boot identity appears.
- **S10:** A turn's model output creates a tool request attributable to
  `Model { turn }`; the runner's result is applied through a transition
  attributable to `Tool { request }`. Neither attribution alters approval,
  fencing, or outcome authority.

## Open questions

- Which command kinds admit non-owner actors, and under what verification,
  remains with the owning decisions: delegation (reserved ADR-0002), tool policy
  (reserved ADR-0011 through ADR-0014), and identity and authorization (reserved
  ADR-0015 through ADR-0018). The open questions on owner client authentication
  and LLM-judge influence in [open-questions.md](../open-questions.md) stay
  open; this record supplies their provenance vocabulary only.
- Whether future creation-cause variants reference actors is deferred to the
  decisions that add those causes.
- A `Policy` actor, per-transition adoption schedules, exact DDL, and wire
  encoding (reserved to [ADR-0019](0019-process-protocol.md) and
  [ADR-0021](0021-compatibility-and-negotiation.md)) remain open.

## Explicit non-decisions

This record adds no Rust type, table, migration, command kind, or dependency. It
defines no authentication, authorization, permission model, identity
verification, credential, multi-user or tenancy concept, per-actor quota, or
resource limit; those remain reserved scope (ADR-0015 through ADR-0018 and
resource governance). It does not let any non-owner agency issue a durable
command, does not amend `CreateSession` or ADR-0003, does not make an actor
value a capability or proof, and does not change the authority, guards, or
ordering of any accepted transition.

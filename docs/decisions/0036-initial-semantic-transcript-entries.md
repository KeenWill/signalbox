# ADR-0036: Initial semantic transcript entries

- Date: 2026-07-17
- Supersedes: none
- Superseded by: none
- Depends on: the accepted foundation set
  ([ADR-0001](0001-domain-terminology-and-identity.md),
  [ADR-0003](0003-session-creation-and-transcript-ancestry.md),
  [ADR-0004](0004-turn-and-attempt-lifecycle.md),
  [ADR-0005](0005-model-call-retry-semantics.md), and
  [ADR-0027](0027-input-delivery-lifecycle.md)) and the accepted persistence and
  frontier refinements ([ADR-0022](0022-persistence-representation.md) and
  [ADR-0030](0030-context-frontier-snapshots.md))
- Refines: ADR-0027's origin-input and explicit-failure semantic-history
  requirements and ADR-0030's semantic-entry boundary
- Refined by: [ADR-0037](0037-baseline-user-content.md) for the immutable
  content value referenced by `OriginAcceptedInput` and
  [ADR-0042](0042-assistant-content-and-completion.md) for assistant text,
  assistant tool-use references, and completed-turn entries
- Resolves: semantic-entry payload and commit granularity for
  accepted-input-origin eligibility and eligible failure; later semantic
  variants remain open
- Decision questions: first semantic-entry payload variants; accepted-input
  content correlation; entry identity and source; eligibility and failure commit
  boundaries; deferred variants and rendering

## Context

ADR-0027 requires an accepted-input-origin turn's starting frontier to include
its origin input and requires later successor context to retain an explicit
failure marker for a failed predecessor. ADR-0030 gives every semantic
transcript entry a distinct identity, requires source-qualified ordered frontier
membership, and makes eligibility atomically commit the new origin entry and
snapshot. Neither record defines the entry payload that represents those
accepted facts.

That omission blocks the first S01/S03 vertical slice. Creating a generic
transcript message would conflate semantic history with presentation. Copying
user content into an unrelated entry payload could diverge from the immutable
accepted input. Defining assistant, tool, approval, steering, mismatch,
cancellation, and reconciliation payloads now would select commit and rendering
semantics for slices whose authoritative aggregates do not exist.

The first slice needs only enough semantic history to activate
accepted-input-origin work and to represent the eligible-failure alternative
honestly for a later successor.

## Decision

### Initial closed payload set

The initial semantic transcript entry is one immutable identified domain fact:

```text
SemanticTranscriptEntry = {
    identity: SemanticTranscriptEntryId,
    source_session: SessionId,
    payload: InitialSemanticTranscriptEntryPayload
}

InitialSemanticTranscriptEntryPayload =
    OriginAcceptedInput {
        accepted_input: AcceptedInputId
    }
  | TurnFailed {
        turn: TurnId
    }
```

This is semantic pseudocode, not a Rust, storage, presentation, or wire API. The
two variants are the complete constructible set for the first slice; there is no
generic text, role, metadata map, serialized event, or “other” case.

`OriginAcceptedInput` projects the exact immutable semantic content already
owned by the referenced accepted input. It does not copy another independently
authoritative content value. Construction validates that the input belongs to
`source_session`, is the typed origin of exactly one accepted-input turn, and is
the origin whose eligibility transition is committing the entry. One accepted
input can establish at most one such entry.

`TurnFailed` is the explicit semantic marker that the referenced turn reached
`TurnDisposition::Failed`. Construction validates that the turn belongs to
`source_session` and that the same transaction terminalizes that exact turn as
failed. It carries no invented user-facing explanation and does not copy
operational evidence; provider, preparation, or other purpose-specific evidence
remains in its authoritative records. One turn can establish at most one failed
marker.

The entry identity is not the accepted-input or turn identity. Equal
accepted-input content in two inputs produces distinct origin entries, and equal
failed dispositions on two turns produce distinct markers. Identity equality and
source-qualified frontier equality remain those of ADR-0030; rendering or text
equality establishes neither.

### Commit boundaries and order

An accepted origin input becomes semantic transcript history when its turn
becomes eligible, not when the hub accepts or queues it. Before eligibility, the
accepted-input record, disposition, queue facts, and frozen configuration are
durable, but no `OriginAcceptedInput` semantic entry exists for that turn.

The successful activation transaction atomically:

1. validates durable eligibility, order, slot ownership, and the exact starting
   lineage;
2. creates the one origin semantic entry;
3. creates the new session-owned immutable context snapshot whose complete
   ordered membership is the resolved ancestry or terminal-predecessor prefix
   followed by that origin entry;
4. binds the snapshot in `AcceptedInputTurnStart`; and
5. activates the turn with its initial `Prepared` attempt.

If static pre-activation validation instead selects ADR-0027's eligible failure,
the same transaction performs steps one through four, terminalizes the turn as
failed without an attempt, and appends its `TurnFailed` entry after the origin
entry in semantic history. The failed marker is not part of the turn's starting
snapshot; it is the next semantic fact and must be included, in order, by every
later successor frontier through that terminal turn.

If an already-active turn later reaches `Failed`, its terminalization
transaction appends the same `TurnFailed` variant after every earlier committed
semantic entry. This record defines that marker's meaning and atomicity but does
not authorize a provider, tool, or recovery transition that is not otherwise
implemented.

A crash before either transaction commits exposes none of its new semantic
entries, snapshot, start binding, attempt, or terminal state. A restart after
commit reconstructs all of them. A retry after an uncertain commit reads the
authoritative result and cannot append a second origin or failure entry;
purpose-specific uniqueness and the guarded lifecycle update enforce that
boundary.

For ancestry-derived sessions, inherited entries retain their source sessions
and identities. The new origin entry is sourced by the consuming session and
follows the inherited prefix. Copying physical rows does not remint the
inherited entries or make them new-session content.

### Separation from rendering and operations

Semantic transcript entries are context facts, not accepted-input records,
operational audit rows, provider request messages, streaming chunks, or client
presentation messages. The entry-to-provider-prompt and entry-to-client
rendering projections may format or group facts differently, but they cannot
change entry identity, source, order, or payload.

This record intentionally defines no assistant-content, completed-turn, refusal,
cancellation, reconciliation, duplicate-risk, mismatch-invalidation, steering,
tool-request, tool-result, approval, or delegation payload. Several of those
facts are already required to appear in future semantic history by accepted
ADRs, but their exact entry boundaries and correlations must be decided with the
aggregate slice that can commit them. They are not represented temporarily as
text or as `TurnFailed`.

## Invariants

- INV-001: semantic-entry, accepted-input, turn, session, and frontier
  identities remain distinct even when an entry payload references another
  identity.
- INV-005: the initial entries are semantic-history facts; accepted-input
  storage, operational failure evidence, transient streaming, and presentation
  remain separate representations.
- INV-007: accepted input remains durable before acknowledgement, but semantic
  projection waits for eligibility and never substitutes for the accepted-input
  record.
- INV-009: origin entry, starting snapshot, start binding, and activation or
  eligible failure commit under the same eligibility and slot boundary.
- INV-015: the new snapshot resolves to the exact inherited or predecessor
  prefix followed by the origin entry; a later successor through failed work
  additionally retains the failed marker.
- INV-030: inherited entry identities and source sessions survive a fork while
  the new origin is sourced by the consuming session.

## Strongest alternative

Define one general semantic message with role, text, optional related
identities, and metadata, then use it for user input, assistant output, tool
results, and outcome markers. It would make the first persistence schema and
renderer superficially simple.

It is rejected because optional fields and free-form roles would make
accepted-input correlation, failure-marker meaning, and future assistant/tool
authority conventions rather than closed domain rules. It would also invite
presentation formatting and provider wire payloads to become the canonical
semantic model, violating INV-005.

## Rejected alternatives

- **Create the origin entry at input acceptance.** Queue acceptance has not
  fixed lineage or the snapshot that consumes the entry; ADR-0030 makes those
  facts one eligibility-time boundary.
- **Use `AcceptedInputId` as the semantic-entry identity.** Accepted delivery
  intent and semantic transcript history have different creation and recovery
  rules, and ADR-0001 keeps them distinct.
- **Copy user content into a second authoritative payload.** The accepted input
  already owns immutable content; two copies could disagree and would need an
  unnecessary precedence rule.
- **Combine the origin and eligible failure into one entry.** They are ordered
  independent semantic facts. A later frontier must be able to retain the input
  while identifying failure explicitly.
- **Use one generic turn-outcome string.** That would erase the accepted
  distinction among failure, refusal, cancellation, and proof-bearing
  reconciliation before their payload boundaries are decided.
- **Define every future semantic variant now.** The missing aggregates cannot
  yet prove the correct source, commit boundary, or outcome authority for those
  entries.
- **Canonicalize equal payloads to one entry identity.** Equal content is not
  shared semantic occurrence or construction authority under ADR-0030.

## Consequences

The first semantic-history mapping needs closed representations for two typed
payload variants and purpose-specific uniqueness constraints. Queued input
remains visible from its accepted-input record before it becomes transcript
history; clients must not infer semantic commitment from presentation.

S01 activation and both S03 eligibility outcomes can commit a complete origin
prefix without waiting for assistant or tool design. Later provider and tool
slices remain blocked on their own semantic-entry variants instead of inheriting
an accidental generic-message contract.

## Scenario walkthroughs

- **S01:** The accepted input is durable while queued. Immediate eligibility
  commits one origin entry sourced by the new session, a snapshot containing
  exactly that entry for an ancestry-free session, the start binding, and the
  initial prepared attempt.
- **S03:** Restart finds no origin entry for a still-queued turn. Once durable
  order makes it eligible, one guarded transaction commits the origin entry and
  activation facts; if static validation selects eligible failure, it also
  terminalizes without an attempt and appends one failed marker.
- **S09:** A later queued turn creates no semantic entry at acceptance. At
  eligibility its snapshot retains the exact terminal-predecessor prefix before
  appending its own origin entry.
- **S17:** The fork's first snapshot preserves every selected inherited
  source-qualified entry, then appends one origin entry whose source is the fork
  session. Physical copying cannot change those identities.

## Open questions

- Assistant content and completion, refusal, cancellation, reconciliation,
  mismatch, accepted-risk, steering, tool, approval, and delegation entry
  variants and their exact commit granularity remain open.
- The concrete accepted-input content algebra is defined by
  [ADR-0037](0037-baseline-user-content.md); this record continues to reference
  that immutable value rather than copying it.
- Provider-prompt rendering, client transcript rendering and grouping,
  selectable `TranscriptFrontier` boundaries, and any public or wire
  representation remain open.
- Semantic compaction, selective omission, merging, and non-input origins remain
  future foundation decisions under ADR-0030.

## Explicit non-decisions

This record does not select a Rust enum spelling, storage table or column,
serialization format, provider role, prompt template, presentation message,
token-budget policy, identity generation, protocol field, or client API. It does
not enable safe-point steering consumption, provider calls, tools, approvals,
delegation, regeneration, compaction, or merge, and it does not weaken any
semantic-entry requirements already accepted for those future slices.

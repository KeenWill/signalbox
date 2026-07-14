# ADR-0001: Core domain terminology and identity boundaries

- Status: Proposed
- Date: 2026-07-12
- Owners: Repository owner
- Reviewers: Domain, provider, and tool-safety reviewers unassigned
- Supersedes: none
- Superseded by: none
- Acceptance dependency: must be accepted atomically with ADR-0003, ADR-0004, ADR-0005, and ADR-0027 in the current foundation set
- Decision-ledger questions: owner-global durable-command identity scope; final names and boundaries for session, accepted input, turn, turn attempt, model call, tool request, and tool attempt

## Context

Signalbox coordinates durable user intent with physical provider and tool effects. Recovery, steering, regeneration, approval, and audit become untestable if a conversation, a user submission, logical work, an orchestration tenure, and an external call share an identity merely because a simple success path displays them together.

The repository already accepts most of these conceptual distinctions, but several names and the boundary around accepted user input remain provisional. The first domain state machines need stable semantic identities before any storage or wire representation is designed.

## Decision

Signalbox uses the domain terms **session**, **accepted input**, **turn**, **turn attempt**, **model call**, **tool request**, and **tool attempt**. Each has a distinct durable semantic identity type. None may be substituted for another at a domain boundary, even if an initial workflow happens to create them one-to-one. A separate **durable command identity** is an owner-global idempotency identity, not a semantic work identity.

The identity boundary is semantic rather than representational:

| Concept | Identity denotes | Identity does not denote |
| --- | --- | --- |
| Durable command | One owner-global, durably handled command submission and its terminal applied-or-rejected result across all command kinds, sessions, and clients within the hub's owner authority | An accepted input, turn, effect, transport failure, or reusable per-session/per-command token |
| Session | One durable, independently browsable conversation | A connection, context window, turn, or login session |
| Accepted input | One user submission durably accepted with its requested delivery treatment | A transcript entry, turn, provider request, or transport command retry |
| Turn | One logical request for Signalbox to produce a conversational outcome under one frozen effective configuration | A message, every orchestration process, or one immutable model context |
| Turn attempt | One exclusive physical orchestration tenure that advances one turn until it ends or yields to a durable wait | A startup recovery scan, runner, model call, tool attempt, or logical retry |
| Model call | One durable hub authorization to attempt a physical interaction with a model provider, whether it reaches the provider or terminates before send | A turn, a provider SDK's hidden retry group, or committed assistant content |
| Tool request | One logical request for a normalized tool operation whose policy and outcome are tracked | Model-generated syntax, approval presentation, or executor dispatch |
| Tool attempt | One physical effort to execute one tool request at one placement | The logical tool request or a scheduler delivery retry |

An accepted input can originate a turn or steer an existing turn. A turn has exactly one durable typed origin and can consume zero or more additional accepted inputs as steering. The first implementable origin variant is accepted input. Manual regeneration and future scheduled or delegated work require explicit typed variants together with the lifecycle and context rules that make those variants implementable; they are not catch-all values in the first turn state machine.

A model call belongs to exactly one turn attempt and therefore one turn. A turn attempt belongs to exactly one turn. A tool request belongs to exactly one turn and may survive across that turn's durable waits and replacement attempts. Each tool attempt belongs to exactly one tool request and exactly one issuing turn attempt; one logical request may own more than one policy-authorized attempt across tenures. Detailed tool states and retry eligibility remain outside this ADR, but these ownership cardinalities do not.

`DurableCommandId` has one namespace across all command kinds, sessions, and clients for the single owner. Its comparison payload is a typed discriminated command variant containing every caller-supplied field except the identifier itself; the variant discriminator and session or other owner reference are part of structural equality. The first transaction that commits handling of an unseen identifier atomically stores that payload and one terminal typed command result: either the command was applied, or authoritative domain validation rejected it. The latter creates no semantic work identity, but it still claims the command identifier. Replaying the same payload returns that recorded result even after domain state changes; correcting or refreshing rejected intent requires a new command identifier. Reusing one identifier for another kind, session, or payload is conflicting reuse, even if those commands would otherwise be independently valid.

A malformed transport request, a request for which owner authority cannot be established, or an infrastructure/transaction failure before that atomic record commits does not claim the identifier. These exclusions define the domain boundary without selecting an authentication mechanism or transport error model. The rule keeps command references in cancellation, ambiguity, and authority-transfer history unambiguous and prevents rejected commands from acquiring different meanings later. It does not choose a wire representation or require one storage table.

Identifiers are opaque domain values. Whether their stored encodings are UUIDs, integers, or another format is not part of the decision.

## Terminology

The following typed pseudocode is conceptual and is not a final Rust, storage, or wire API:

```text
opaque DurableCommandId
opaque SessionId
opaque AcceptedInputId
opaque TurnId
opaque TurnAttemptId
opaque ModelCallId
opaque ToolRequestId
opaque ToolAttemptId

InitialTurnOrigin =
    AcceptedInput(AcceptedInputId)

AcceptedInputDisposition =
    OriginOf(TurnId)
  | PendingSteering {
        binding: SteeringBinding
    }
  | ConsumedAsSteering {
        call: ModelCallId
    }
  | ReclassifiedAsTurnOrigin {
        turn: TurnId,
        reason: NoSafePointBeforeTerminal
    }

SteeringBinding = {
    source_turn: TurnId
}
```

These are the complete baseline disposition categories defined with ADR-0027. A pending input stores its source turn exactly once in `SteeringBinding`; it derives immutable source configuration from that canonical turn rather than copying a second value that could disagree. A consumed input references the immutable model call from which the owning turn and exact frontier are derived. Reclassification is distinct from an input originally accepted as turn-origin work so recovery and presentation can explain that the target turn terminated without consuming the steering. Future origin or disposition variants, including a typed regeneration relation, require explicit domain additions; the pseudocode does not use a catch-all variant that would hide an unknown semantic case or an origin whose context rules are still unresolved.

**Accepted input** describes durable user-originated semantic content and its delivery request. It is distinct from a transport command, which may be delivered more than once, and from a transcript entry, which is a semantic projection created when the input is used.

**Message** remains a presentation or semantic-history term, not the identity of accepted input. Narrative documents should qualify it as user message, assistant message, protocol message, or transcript entry where ambiguity matters.

## Invariants

- INV-001 is changed to include durable-command and accepted-input identifiers explicitly and to name every identity in this ADR.
- INV-002, INV-004, and INV-005 are preserved.
- A new constraint is added to INV-004: every turn has one typed durable origin, while accepted steering remains separately identified.
- Boundary mappings must validate identifier kind, namespace, and ownership relationship. Matching underlying bytes do not make two domain identities interchangeable; command lookup is owner-global over a discriminated typed payload.
- No execution digest, request hash, or “fingerprint” defines semantic identity. Such values may support deduplication or comparison only after the relevant domain identities and transitions are known.

## Strongest alternative

Use a smaller model with a session, transcript messages, and a generic execution record; infer logical identity from content hashes and parent references. This would reduce the number of early domain types and may be adequate for an append-only chat client.

Signalbox rejects that alternative because safe-point input can share a turn without sharing a provider call, recovery can replace physical work without replacing user intent, and an external call can be ambiguous without making its logical request ambiguous. Those cases require identities that cannot be reconstructed reliably from equal content.

## Rejected alternatives

- **Use “thread” instead of “session.”** It collides with execution-thread terminology and gives less emphasis to independently browsable durable continuity.
- **Use “operation” or “work item” instead of “turn.”** These are too broad to communicate conversational ordering. The precise boundary is defined here and in ADR-0004 rather than delegated to the name.
- **Use “run” for a turn attempt.** It collides with runner and does not identify the logical owner.
- **Call every provider or tool effect an attempt without a logical counterpart.** That loses the approval, retry, and outcome boundary.
- **Make accepted input identical to a turn.** Safe-point steering disproves the one-to-one relationship.
- **Make a transcript message the canonical input identity.** Transcript projections and accepted delivery intent have different creation and recovery rules.

## Consequences

The future domain will contain more explicit types and mappings than a message-centric chat model. Commands, records, and protocol messages will need conversions rather than sharing identifier fields indiscriminately.

In return, stale results, retries, steering, and regeneration can be tested against named ownership relationships. A client can present a simple transcript without forcing the domain to use that projection as its state machine.

## Scenario walkthroughs

- **S01:** The session and first accepted input have different identities. The input later originates a turn without becoming the session or the transcript row.
- **S02:** One turn and turn attempt can own several model calls. Final assistant content is correlated with those calls but is not a call identity.
- **S04:** Startup recovery ends a lost attempt without creating another. Where operation-specific policy permits, later resolving evidence may create a continuation attempt for other unfinished work, or an exact-set owner decision may authorize a duplicate-risk retry; both retain the original turn and accepted-input identities. ADR-0005 separately forbids known-provider-failure retry in version one.
- **S08:** A safe-point accepted input is used as steering for the existing turn and does not create a turn merely because it is durable.
- **S10:** A tool request retains its identity through approval and owns a separately identified physical attempt.
- **S12:** A late result is rejected using tool-attempt ownership and fencing without guessing from its arguments.

## Extension implications

Typed turn origins leave room for regeneration, scheduled work, and delegated work without changing the identity of accepted input. Adding one requires its acceptance, ordering, and starting-context rules to be decided at the same time. Additional physical call kinds can follow the logical/physical split. A future merge model would need its own identity and provenance rather than overloading session or turn identifiers.

Names used in public protocols may still be versioned before such a protocol is accepted, but mappings must preserve these semantic distinctions.

## Open questions

- Global versus owner-scoped uniqueness for non-command identities, cross-owner physical encoding, and concrete identifier formats remain part of persistence and protocol design. `DurableCommandId` is explicitly owner-global within one established owner authority; this open question does not reopen that namespace.
- Tool-request and tool-attempt state machines remain assigned to later tool ADRs.
- The exact transcript-entry model and assistant-content commit granularity remain open.
- Whether non-text user commands share an accepted-input envelope is a later command-model question; they must not erase the distinction decided here.

## Explicit non-decisions

This ADR does not choose a database schema, event model, protocol field, Rust newtype spelling, crate layout, idempotency-key format, tool-risk taxonomy, provider SDK, or client presentation. It does not define delegation results, delegation cancellation, archive behavior, or transcript merging.

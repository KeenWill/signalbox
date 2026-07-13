# ADR-0001: Core domain terminology and identity boundaries

- Status: Proposed
- Date: 2026-07-12
- Owners: Repository owner
- Reviewers: Domain reviewer pending; provider and tool-safety reviewers unassigned
- Supersedes: none
- Superseded by: none
- Decision-ledger questions: final names and boundaries for session, accepted input, turn, turn attempt, model call, tool request, and tool attempt

## Context

Signalbox coordinates durable user intent with physical provider and tool effects. Recovery, steering, regeneration, approval, and audit become untestable if a conversation, a user submission, logical work, an orchestration tenure, and an external call share an identity merely because a simple success path displays them together.

The repository already accepts most of these conceptual distinctions, but several names and the boundary around accepted user input remain provisional. The first domain state machines need stable semantic identities before any storage or wire representation is designed.

## Decision

Signalbox uses the domain terms **session**, **accepted input**, **turn**, **turn attempt**, **model call**, **tool request**, and **tool attempt**. Each has a distinct durable identity type. None may be substituted for another at a domain boundary, even if an initial workflow happens to create them one-to-one.

The identity boundary is semantic rather than representational:

| Concept | Identity denotes | Identity does not denote |
| --- | --- | --- |
| Session | One durable, independently browsable conversation | A connection, context window, turn, or login session |
| Accepted input | One user submission durably accepted with its requested delivery treatment | A transcript entry, turn, provider request, or transport command retry |
| Turn | One logical request for Signalbox to produce a conversational outcome under one frozen effective configuration | A message, every orchestration process, or one immutable model context |
| Turn attempt | One exclusive physical orchestration tenure that advances one turn until it ends, yields to a durable wait, or is replaced | A runner, model call, tool attempt, or logical retry |
| Model call | One durable hub authorization to attempt a physical interaction with a model provider, whether it reaches the provider or terminates before send | A turn, a provider SDK's hidden retry group, or committed assistant content |
| Tool request | One logical request for a normalized tool operation whose policy and outcome are tracked | Model-generated syntax, approval presentation, or executor dispatch |
| Tool attempt | One physical effort to execute one tool request at one placement | The logical tool request or a scheduler delivery retry |

An accepted input can originate a turn or steer an existing turn. A turn has exactly one durable typed origin and can consume zero or more additional accepted inputs as steering. The first implementable origin variant is accepted input. Manual regeneration and future scheduled or delegated work require explicit typed variants together with the lifecycle and context rules that make those variants implementable; they are not catch-all values in the first turn state machine.

A model call belongs to exactly one turn attempt and therefore one turn. A turn attempt belongs to exactly one turn. Tool-request and tool-attempt ownership must likewise be explicit, but their detailed lifecycle is outside this ADR.

Identifiers are opaque domain values. Whether their stored encodings are UUIDs, integers, or another format is not part of the decision.

## Terminology

The following typed pseudocode is conceptual and is not a final Rust, storage, or wire API:

```text
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
        turn: TurnId,
        fallback_configuration: EffectiveConfiguration
    }
  | ConsumedAsSteering {
        turn: TurnId,
        call: ModelCallId,
        frontier: ContextFrontier
    }
  | ReclassifiedAsTurnOrigin {
        turn: TurnId,
        reason: NoSafePointBeforeTerminal
    }
```

These are the complete baseline disposition categories defined with ADR-0027. Reclassification is distinct from an input originally accepted as turn-origin work so recovery and presentation can explain that the target turn terminated without consuming the steering. Future origin or disposition variants, including a typed regeneration relation, require explicit domain additions; the pseudocode does not use a catch-all variant that would hide an unknown semantic case or an origin whose context rules are still unresolved.

**Accepted input** describes durable user-originated semantic content and its delivery request. It is distinct from a transport command, which may be delivered more than once, and from a transcript entry, which is a semantic projection created when the input is used.

**Message** remains a presentation or semantic-history term, not the identity of accepted input. Narrative documents should qualify it as user message, assistant message, protocol message, or transcript entry where ambiguity matters.

## Invariants

- INV-001 is changed to include accepted-input identifiers explicitly and to name every identity in this ADR.
- INV-002, INV-004, and INV-005 are preserved.
- A new constraint is added to INV-004: every turn has one typed durable origin, while accepted steering remains separately identified.
- Boundary mappings must validate both identifier kind and ownership relationship. Matching underlying bytes do not make two domain identities interchangeable.
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
- **S04:** Recovery may create a replacement turn attempt and model call while retaining the original turn and accepted-input identities.
- **S08:** A safe-point accepted input is used as steering for the existing turn and does not create a turn merely because it is durable.
- **S10:** A tool request retains its identity through approval and owns a separately identified physical attempt.
- **S12:** A late result is rejected using tool-attempt ownership and fencing without guessing from its arguments.

## Extension implications

Typed turn origins leave room for regeneration, scheduled work, and delegated work without changing the identity of accepted input. Adding one requires its acceptance, ordering, and starting-context rules to be decided at the same time. Additional physical call kinds can follow the logical/physical split. A future merge model would need its own identity and provenance rather than overloading session or turn identifiers.

Names used in public protocols may still be versioned before such a protocol is accepted, but mappings must preserve these semantic distinctions.

## Open questions

- Global versus owner-scoped identifier uniqueness and concrete encoding remain part of persistence and protocol design.
- Tool-request and tool-attempt state machines remain assigned to later tool ADRs.
- The exact transcript-entry model and assistant-content commit granularity remain open.
- Whether non-text user commands share an accepted-input envelope is a later command-model question; they must not erase the distinction decided here.

## Explicit non-decisions

This ADR does not choose a database schema, event model, protocol field, Rust newtype spelling, crate layout, idempotency-key format, tool-risk taxonomy, provider SDK, or client presentation. It does not define delegation results, delegation cancellation, archive behavior, or transcript merging.

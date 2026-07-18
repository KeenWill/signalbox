# ADR-0038: Long-lived session aggregate boundary

- Date: 2026-07-18
- Supersedes: none
- Superseded by: none
- Depends on: [ADR-0001](0001-domain-terminology-and-identity.md), [ADR-0003](0003-session-creation-and-transcript-ancestry.md), [ADR-0022](0022-persistence-representation.md), [ADR-0027](0027-input-delivery-lifecycle.md), and [ADR-0035](0035-domain-owned-persistence-reconstitution.md)
- Refines: ADR-0003's session/defaults separation, ADR-0022's session current-defaults pointer, and ADR-0035's purpose-specific complete reconstitution boundary
- Resolves: the minimal long-lived domain `Session` aggregate boundary and load-by-`SessionId` semantics
- Decision questions: long-lived session contents; distinction from creation candidates and command receipts; excluded session-associated facts; complete checked reconstitution; current-defaults load consistency

## Context

The accepted records define a session as one durable, independently browsable conversation and already settle three durable session-level facts. A session has a distinct identity; its creation cause and transcript ancestry form one immutable provenance value; and its model-selection defaults are immutable versions selected by one mutable current-version pointer. The first `CreateSession` implementation consequently has an `InitialSession` candidate, a typed applied result naming the created `SessionId`, normalized creation/defaults records, and complete command-receipt reconstitution.

Those creation values do not yet define the long-lived domain aggregate consumers should load after creation. Reusing `InitialSession` would imply that defaults remain at version one. Treating the command receipt as the session would make idempotency history part of conversational state. Loading only a `SessionId` would leave each consumer to combine provenance and current defaults independently. At the other extreme, embedding transcript entries, turns, calls, queues, and scheduler state would turn an ordinary session-level read into an unbounded reconstruction of facts whose accepted lifecycles and consistency boundaries already differ.

The missing boundary must let application orchestration consume one complete current session value without weakening the creation, persistence, or reconstitution decisions and without deciding input submission, protocols, archive behavior, or defaults-replacement mechanics.

## Decision

### Minimal long-lived aggregate

The minimal long-lived domain `Session` is one complete value containing exactly:

```text
Session = {
    identity: SessionId,
    creation_provenance: SessionCreationProvenance,
    current_configuration_defaults: VersionedSessionConfigurationDefaults
}
```

This is semantic pseudocode, not a final Rust, storage, repository, or wire API.

The identity denotes the durable conversation defined by ADR-0001. Creation provenance is the complete immutable cause-and-ancestry pair defined by ADR-0003. Current configuration defaults are the complete immutable versioned value selected as current for that session, not merely a version number or unversioned model selection.

The aggregate is a current domain snapshot. Its provenance cannot change. Its carried defaults value does not mutate in place; a later accepted defaults-replacement transition may install a new immutable version and move the durable current pointer under ADR-0027, after which a new load yields a new `Session` snapshot. This ADR neither adds that command nor decides its application API.

There is no optional, partially loaded, or “unknown” field in the baseline `Session`. Adding another session-level fact requires the decision that defines that fact and demonstrates that it belongs in this aggregate rather than in a purpose-specific projection.

### Creation values and receipts remain distinct

`InitialSession`, `CreateSessionAppliedResult`, `ReconstitutedSessionCreation`, and `Session` serve different purposes:

- `InitialSession` is the sealed pre-commit creation candidate. It always carries defaults version one and does not claim that a transaction committed.
- `CreateSessionAppliedResult` is the terminal typed command result used for durable replay. It names the created `SessionId`; it is not conversational state and does not expand when the session later changes.
- `ReconstitutedSessionCreation` validates the complete typed command payload, its creation-time effects, and recorded result. It reconstructs the creation receipt and initial defaults correlation, not the session's current defaults.
- `Session` is the long-lived current aggregate loaded by semantic identity. It contains no durable-command identity or result.

A successful first application or equal replay of `CreateSession` returns the recorded typed receipt unchanged and may be followed by `load_session` using that receipt's `SessionId`. An implementation may additionally return an already validated `Session` alongside the receipt when the same transaction has established the equivalent complete current projection, but it cannot replace, reshape, or recompute the replay result. The receipt remains the replay authority and `Session` remains a separate domain value; one is never substituted for the other.

### Session-associated facts outside the aggregate

The baseline `Session` does not embed collections or optional summaries of:

- accepted inputs or semantic transcript entries;
- turns, turn attempts, model calls, tool requests, or tool attempts;
- queue-order facts, active-slot state, waits, or derived scheduler eligibility;
- durable-command payloads, results, or command history;
- provider, recovery, mismatch, approval, or audit evidence; or
- client presentation, rendered transcript, transient streaming, connection, archive, or retention state.

Those facts remain independently stored domain facts, lifecycle aggregates, purpose-specific complete projections, or presentation projections and correlate through their typed identities, including `SessionId` where applicable. Their exclusion does not make them less authoritative or less session-owned. It prevents one small session-level load from becoming a universal document and preserves ADR-0035's rule that each use case loads every fact required for its own authority.

A transition that requires current defaults plus turn, input, queue, or scheduler facts must use a purpose-specific complete projection and transaction guard for that transition. Possessing `Session` alone never proves queue eligibility, active-slot ownership, command application, transcript-frontier validity, or authority to perform an effect.

### Complete checked reconstitution

The domain owns a purpose-specific session reconstitution input containing checked domain values for:

- the requested and stored `SessionId`;
- the complete stored `SessionCreationProvenance`;
- the owning session and version of the current-defaults pointer;
- the owning session and version of the selected defaults record; and
- the complete `SessionConfigurationDefaults` value stored at that version.

Reconstitution returns `Session` only when those facts form one complete value. The requested identity, stored session identity, pointer owner, defaults owner, pointer version, and defaults-record version must agree exactly. The provenance and defaults values must be constructible accepted variants. A mismatch, absent required fact, unknown variant, invalid ordinal, or other malformed durable shape fails closed as corruption or incompatible representation under ADR-0035. It never becomes `None`, an initial/default value, or a partially populated session.

The domain seam accepts no SQL row, nullable storage discriminator, SQLx type, repository error, or command receipt. It performs no I/O, command replay, identity minting, repair, or lifecycle effect.

### Load by session identity

`load_session(SessionId)` looks up the session by semantic session identity, independently of the command that created it. It reads:

1. the session's immutable identity and creation provenance;
2. that session's one current-defaults pointer; and
3. the complete defaults-version record named by that pointer.

The pointer is authoritative. A load must not infer current defaults from the creation command, version one, the greatest stored version, a caller-supplied version, or a process cache.

The provenance, pointer, and selected defaults record are observed through one database-consistent read. If a future defaults update races the load, the load may return the complete session immediately before or immediately after that pointer change; it cannot combine the pointer from one state with the defaults record from another. This is snapshot consistency for one load, not a promise that the returned value remains current after the load completes. A later write that depends on current defaults still uses ADR-0027's transaction-time compare-and-set guard.

The load returns absence only when no session row for the requested `SessionId` exists in the read snapshot. Once a session row exists, a missing pointer, missing selected version, ownership mismatch, or invalid value is a fail-closed integrity error. Database constraints provide defense in depth but do not replace domain reconstitution.

Application orchestration depends on this domain boundary rather than on persistence records. Exact repository trait spelling, async/runtime types, and whether a use case loads a session separately or receives an equivalent validated projection from its committing port remain implementation choices.

## Invariants

- **INV-002:** `Session` and its reconstitution input contain only domain values. Storage records, SQLx values, protocol messages, framework types, and repository failures remain outside.
- **INV-003:** the aggregate carries the complete immutable `SessionCreationProvenance`; it does not merge cause and ancestry or treat current defaults as a third provenance fact.
- **INV-005:** semantic history, operational coordination, audit evidence, transient streams, command receipts, and presentation remain distinct from the session aggregate.
- **INV-008:** a loaded session carries the exact complete defaults version selected by its current pointer. It never derives current meaning from version one, the maximum stored version, or a cached unversioned value.
- **INV-012:** the `CreateSession` result remains a typed command receipt separate from the long-lived aggregate. Loading by `SessionId` does not replay, claim, or validate a durable command.

## Strongest alternative

Make `Session` the root object containing every transcript entry, accepted input, turn, attempt, call, queue fact, command receipt, and scheduler status. This gives callers one object graph and can make in-memory traversal convenient.

It is rejected because the graph is unbounded, mixes immutable history with several independent lifecycle and transaction boundaries, and requires unrelated facts for ordinary session-level work. An optional or lazy-loaded version would be worse: absence could mean either “not loaded” or “no domain fact,” contradicting complete projection semantics. Purpose-specific projections preserve aggregate validation where a use case genuinely spans those facts without making every `Session` load universal.

## Rejected alternatives

- **Rename `InitialSession` to `Session` and keep version one forever.** This confuses a pre-commit candidate with current durable state and makes later accepted defaults versions invisible.
- **Use the reconstituted `CreateSession` receipt as the loaded session.** Command equality and replay require the original payload and result; conversational reads require current defaults. Their change rates and authority differ.
- **Represent a session as `SessionId` alone.** Identity-only handles are useful references, but they do not establish the complete current session-level state application orchestration needs.
- **Load provenance and defaults as unrelated repository calls.** Consumers could combine values from different snapshots or omit a required correlation; the domain would have no complete fail-closed boundary.
- **Select `MAX(version)` as current.** Append-only existence does not mean installation. The explicit pointer is the accepted authority and remains compatible with future staged or retained versions.
- **Put command history or derived scheduler summaries on `Session`.** Command receipts and scheduler eligibility have separate sources of truth and cannot become authoritative by being copied into a convenience object.

## Consequences

The domain gains one central, deliberately small `Session` type plus a separate complete reconstitution seam. Persistence gains a load-by-identity query that joins the current pointer to its exact version and distinguishes true absence from corruption. Application orchestration can use one domain value without importing SQL records or reconstructing current defaults itself.

Callers that need transcript, turn, scheduling, or audit state perform purpose-specific loads. That is additional interface work, but it keeps optional-loading ambiguity and unbounded history out of the core aggregate. A previously loaded `Session` is a snapshot rather than a live cache; transaction-time decisions still revalidate mutable pointers.

The creation receipt loader remains useful and unchanged in meaning even after defaults advance: it validates what `CreateSession` originally established. The session loader answers a different question and therefore must not be implemented by returning the receipt's embedded `InitialSession`.

## Scenario walkthroughs

- **S01:** Applying or equally replaying the owner-initiated, no-ancestry `CreateSession` returns its typed receipt naming the new `SessionId`. Loading that identity reconstructs a `Session` with the same immutable provenance and complete current defaults version one. A later accepted defaults update would make a subsequent load return the new complete version without rewriting provenance or the original command receipt. The later `SubmitInput` slice can consume the current version while retaining ADR-0027's transaction-time compare-and-set rule.

## Extension implications

Future accepted creation-cause or ancestry variants remain part of `SessionCreationProvenance` and therefore the same aggregate field; this ADR does not admit one. Future defaults categories extend the complete typed defaults algebra under ADR-0027 rather than becoming independent optional session fields.

Archive, retention, collaboration, ownership, delegation, related-session navigation, and client summary state require their own decisions. A future decision may add a session-level lifecycle value to the aggregate, or keep one in a separate projection, only after defining its authority and consistency needs. Existing `SessionId`, immutable provenance, and exact current-defaults meaning must survive that extension.

## Open questions

- Exact Rust module names, repository trait spelling, and whether the creation transaction returns an equivalent validated `Session` in addition to the receipt are implementation choices.
- Defaults replacement is already semantically admitted by ADR-0027, but its command payload, application port, persistence transaction, and API remain a later slice.
- Archive and retention behavior remain reserved for ADR-0028 and ADR-0029; this record does not introduce an archive field or status.

## Explicit non-decisions

This ADR adds no `SubmitInput`, protocol, authentication, client, fork, defaults-replacement, archive, retention, transcript-loading, scheduling, or command-history behavior. It selects no SQL statement, transaction isolation level, repository framework, cache, serialization, wire encoding, or presentation shape. It does not make `Session` authority for a transition whose purpose-specific complete projection requires additional facts.

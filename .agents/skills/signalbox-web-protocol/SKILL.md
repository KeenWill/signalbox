---
name: signalbox-web-protocol
description: Preserve Signalbox authority, synchronization, command identity, typed boundaries, and fail-closed semantics when adding browser contracts and client projections.
---

# Signalbox web protocol

Use this skill for browser endpoints and transports, DTOs, decoders,
synchronization reducers, commands, read models, stream handling, retries, and
recovery.

This bootstrap does not decide open browser transport, client language, wire,
or cross-component questions. Each implementing stack records foundation-weight
choices in its owning living specification and ordinary choices in its
pull-request description before this guidance applies.

## Separate representations

Keep these types distinct:

- domain and application values;
- persistence records and read projections;
- process-protocol messages;
- browser HTTP DTOs;
- client synchronization state; and
- React view models.

Do not export storage rows or process-wire frames merely because they already
serialize. The implementing stack's owning specification decides browser DTO
ownership, client language, contract generation or checking, and runtime
validation.

## Authority

- Durable Signalbox state is authoritative.
- Historical windows and current/live projections are read models over that
  authority.
- Provider text deltas and transient overlays remain absent until an owning
  implemented contract defines them.
- When that contract exists, its replacement rules govern transient client
  overlays.
- Unknown variants and contradictory correlations fail closed.
- Generic UI fallback may preserve evidence for a known valid generic record; it
  must not reinterpret an unknown protocol variant as a familiar one.

## Session synchronization

Separate the historical plane from the live plane.

- Historical reads expose stable logical addresses, bounded windows, and typed
  detail.
- Live subscribe/follow begins with a coherent current projection and durable
  cursor, then sends ordered durable updates above it. It relays ephemeral
  drafts only when an owning implemented contract defines their identity,
  sequencing, replacement, backpressure, and redaction.
- A full follower queue follows its owning implemented contract. The current
  `follow_session` contract stops incremental delivery, reports
  `resync_required`, and resumes from a fresh snapshot; it never drops durable
  events or substitutes backpressure.
- Lag produces resynchronization, not partial best-effort continuation.
- Client presentation choices never become server `full`, `condensed`, or
  `results` modes.

## Mutations

Preserve the implemented contract's idempotency mechanism and typed ambiguity
handling.

- For a mutation whose owning contract carries a durable command identity,
  generate or retain that identity before network I/O and retry the exact
  identity and semantic payload after ambiguity.
- Retry a contractually identity-free mutation only as its owning contract
  prescribes; never invent a command identity for it.
- Do not convert HTTP status alone into durable command truth.
- Do not optimistically insert authoritative transcript entries. Local pending
  commands remain distinct until acknowledged and observed durably.
- Validate the identity, correlation, and successor-version fields that the
  owning contract provides before adopting a receipt.

## HTTP boundary

- Follow only the boundary and security semantics in the active implementing
  stack's owning specification. This bootstrap does not choose transport,
  origin policy, authentication, authorization, TLS or proxy placement,
  mutation encoding, browser validation, streaming, or blob delivery.
- Preserve the explicit item and byte bounds, cancellation, command identity,
  and authority rules that the implemented contract defines.

## Review

Block changes that invent server facts, weaken command replay, conflate durable
and ephemeral state, expose secrets or private storage detail, silently
truncate, or require unbounded client materialization. Exact new semantics
belong in the implementing stack's owning living specification.

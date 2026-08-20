---
name: signalbox-web-protocol
description: Preserve Signalbox authority, synchronization, command identity, typed boundaries, and fail-closed semantics when adding browser HTTP contracts and client projections.
---

# Signalbox web protocol

Use this skill for HTTP endpoints, DTOs, decoders, synchronization reducers,
commands, read models, stream handling, retries, and recovery.

## Separate representations

Keep these types distinct:

- domain and application values;
- persistence records and read projections;
- process-protocol messages;
- browser HTTP DTOs;
- client synchronization state; and
- React view models.

Do not export storage rows or process-wire frames merely because they already
serialize. Rust owns the web DTO and generates or mechanically checks the
TypeScript contract and runtime decoder.

## Authority

- Durable Signalbox state is authoritative.
- Historical windows and current/live projections are read models over that
  authority.
- Provider text deltas are ephemeral presentation only.
- A fresh authoritative live snapshot replaces transient client overlays.
- Unknown variants and contradictory correlations fail closed.
- Generic UI fallback may preserve evidence for a known valid generic record; it
  must not reinterpret an unknown protocol variant as a familiar one.

## Session synchronization

Separate the historical plane from the live plane.

- Historical reads expose stable logical addresses, bounded windows, and typed
  detail.
- Live subscribe/follow begins with a coherent current projection and durable
  cursor, then sends ordered durable updates above it plus ephemeral drafts.
- Lag produces resynchronization, not partial best-effort continuation.
- Client presentation choices never become server `full`, `condensed`, or
  `results` modes.

## Mutations

Preserve Signalbox command identity and typed ambiguity handling.

- Generate or retain a durable command identity before network I/O.
- An ambiguous mutation retries the exact identity and semantic payload.
- Do not convert HTTP status alone into durable command truth.
- Do not optimistically insert authoritative transcript entries. Local pending
  commands remain distinct until acknowledged and observed durably.
- Validate echoed identity, correlation, and successor versions before adopting
  a receipt.

## HTTP boundary

- Same-origin assets and API; no permissive CORS.
- No Signalbox authentication in the first trusted-network deployment shape.
- Deployment proxies and TLS are external and unnamed by the protocol.
- Mutations use JSON and validate browser origin/authority where supplied.
- Responses and streams have explicit item/byte bounds and cancellation.
- Use normal HTTP range semantics for immutable blob bytes rather than wrapping
  large binary ranges in JSON.

## Review

Block changes that invent server facts, weaken command replay, conflate durable
and ephemeral state, expose secrets or private storage detail, silently truncate,
or require unbounded client materialization. Exact new semantics belong in the
implementing stack's owning living specification.

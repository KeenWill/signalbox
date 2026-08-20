---
name: signalbox-web-performance
description: Design and verify bounded Signalbox web behavior for huge transcripts, tables, artifacts, streaming updates, memory, network windows, and virtualized rendering.
---

# Signalbox web performance

Use this skill when work can scale with session history, fleet size, event rate,
artifact size, table rows, provider deltas, search results, or diagnostic
history.

## Governing law

No UI operation may require materializing an unbounded collection in browser
memory. Browser work should follow the currently inspected or hot region rather
than lifetime corpus size.

## Before implementation

State the relevant dimensions and bounds:

- server rows and projected bytes;
- response/window item and byte limits;
- decoded and retained client records;
- mounted DOM rows and overscan;
- queued stream messages and batched display updates;
- syntax-highlighting or structured-decoding work;
- DevTools/action-history retention; and
- blob bytes fetched, decoded, or derived.

A virtual DOM list alone does not satisfy the law.

## Data loading

- Use stable keyset or logical timeline addresses, not offset pagination for
  mutable/unbounded data.
- Let the server provide size facts; let the client choose an explicit loading
  policy.
- Greedily load bounded histories under a generous configured budget.
- Cancel or stop eager loading and continue incrementally when conditions or
  policy change.
- Do not expose presentation modes as server query semantics.
- Prefetch near the visible region when evidence supports it; do not prefetch
  the lifetime corpus.

## Streaming

- Apply every durable event in order and deduplicate only by its authority.
- When an implemented owning contract exposes ephemeral provider display
  fragments, follow its batching, cadence, backpressure, and replacement policy;
  this skill selects none of those semantics.
- Never debounce or drop durable events.
- Bound follower queues, pending decode work, Redux history, and debug traces.
- Follow the owning contract's transient-state policy during resynchronization;
  do not invent provider-token replay.

## Large renderers

- Virtualize transcript, table, source-line, and large structured views.
- Bound syntax parsing and highlighting to visible or requested regions.
- Follow the active owning contract for media rendering. Do not introduce byte
  delivery, previews, thumbnails, original-download behavior, or media decoding
  before an implementing stack decides and implements them.

## Proof

Add deterministic scale cases and assert the bound directly. Useful evidence
includes mounted-row counts, loaded page counts, retained records, heap trends,
request sizes, response latency, update throughput, long-task counts, and
interaction latency.

Test at least one size well beyond ordinary use so an accidental linear path is
visible. Do not claim scalability from a fixture too small to distinguish the
bounded design from full materialization.

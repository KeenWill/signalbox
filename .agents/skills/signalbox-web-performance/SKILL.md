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

- Follow the owning implemented contract's addressing model. Use stable keyset
  or logical timeline addresses when that contract exposes them; do not impose
  a new pagination surface from this skill.
- Use server-provided size facts only when the owning implemented contract
  exposes them. Always measure and bound the records and bytes the client
  actually receives.
- Greedily load bounded histories under a generous configured budget.
- Cancel or stop eager loading and continue incrementally when conditions or
  policy change.
- Follow the [protocol skill](../signalbox-web-protocol/SKILL.md) and the owning
  implemented browser contract for whether presentation modes are server query
  semantics; do not choose that boundary here.
- Prefetch near the visible region when evidence supports it; do not prefetch
  the lifetime corpus.

## Streaming

The [protocol skill](../signalbox-web-protocol/SKILL.md) owns durable ordering,
deduplication authority, transient replacement, backpressure, and
resynchronization semantics.

- Bound follower queues, pending decode work, Redux history, and debug traces.
- Prove the queue bound under the protocol-defined overflow behavior.

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

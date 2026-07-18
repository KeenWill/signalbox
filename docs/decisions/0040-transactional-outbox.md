# ADR-0040: Transactional outbox for client-visible update events

- Date: 2026-07-18
- Owners: Repository owner
- Reviewers: pull-request review; no specialist human reviewer assigned
- Supersedes: none
- Superseded by: none
- Depends on: [ADR-0010](0010-initial-scheduler-mechanics.md), [ADR-0019](0019-process-protocol.md), [ADR-0021](0021-compatibility-and-negotiation.md), [ADR-0022](0022-persistence-representation.md), and [ADR-0034](0034-durable-command-storage-and-equality.md)
- Refines: ADR-0019's durable-transition event and cursor semantics with their committing-side mechanism, and ADR-0022's justified event/log use
- Resolves: how committed durable state changes become ADR-0019 durable-transition events, and the cursor-derivation fragment of the streaming question ADR-0019 leaves open; resumable-history retention and draft checkpointing remain open
- Decision questions: atomic event append with state commit; commit-ordered sequence as the subscription cursor; delivery semantics and idempotent consumption; drain and wake mechanics; event payload discipline; what stays deferred

## Context

ADR-0019 fixes what subscribers observe: durable-transition events that each advance an opaque cursor, arrive in strictly increasing cursor order with no gaps, and resume from a stated durable point, with a typed `SnapshotRequired` refusal instead of a silent skip (INV-032). It deliberately leaves open how the hub derives those events and cursors from committed state. No live-update implementation exists yet, which is exactly the moment to fix the mechanism: the first slice that publishes an update from application code after its transaction commits establishes an ad-hoc dual-write path, and every later slice copies it.

The failure mode is the classic dual write. A process that commits state and then publishes the event as a separate step can crash, disconnect, or race between the two, leaving committed state whose event never existed or delivering events for transactions that rolled back. ADR-0019's no-gap contract makes the first failure a protocol violation, not a nuisance: a client must never learn that durable state advanced past its cursor without a deliverable event for the advance.

The surrounding decisions constrain the shape. ADR-0022 makes guarded current-state rows the statement of record, rejects event sourcing, and justifies append-only tables where the fact itself is immutable once committed. ADR-0010 fixes the single-hub deployment contract and the wake-up pattern — in-process nudge as a hint, periodic sweep as the correctness backstop, `LISTEN`/`NOTIFY` documented as the multi-process extension only. ADR-0034 fixes the versioned purpose-specific record discipline for durable typed payloads.

## Decision

Signalbox adopts a **transactional outbox** as the sole mechanism by which committed durable state changes become client-visible update events. The outbox is an append-only table family of event rows written inside the same transactions that commit the state they describe, drained by an in-process publisher that feeds ADR-0019's subscriptions. No other path from a commit to a client-visible update event may exist, now or in later slices.

### One transaction, state plus events

Every transaction that commits a client-observable durable transition — command-handling effects, a lifecycle transition, a transcript append, a defaults replacement — appends the corresponding outbox event row(s) in that same transaction. Facts ADR-0019 requires to be observed together are carried in one event row. A transaction with no client-visible effect appends nothing. Which transitions are client-visible is fixed by the protocol projections as their slices define them; what this record fixes is that whichever events exist are appended here and nowhere else.

Two consequences follow directly. A guarded transition that changes zero rows appends zero events: a raced, stale, or replayed write that ADR-0022's predicates reject produces no client-visible transition, so subscribers can never observe an update for an effect that did not happen. And command responses are untouched: the recorded applied-or-rejected result still travels on ADR-0019's unary command path, while the outbox carries the transition events that result committed — the same fact reaching the submitting client as a response and every subscriber as an event, each on its own surface.

Publishing is never a second application-code step after commit. There is no "commit, then notify subscribers" call anywhere in the hub: application code's only obligation is the in-transaction append, and everything after commit belongs to the publisher below. The transaction itself performs no external effect — no delivery, no network send, no connection write; it appends rows and nothing else.

### The outbox sequence is the subscription cursor

Each outbox event carries a monotonic, commit-ordered sequence. This sequence is the durable fact behind ADR-0019's cursor — the "updates after N" resumption point INV-032 requires. One per-hub global sequence is sufficient: under ADR-0010's single-hub contract one process commits every durable transition, so a single commit-ordered event stream exists without cross-process coordination. The per-session guarantee is explicit: a session's events form a strictly increasing subsequence of the global sequence, consistent with the commit order of that session's transitions, which satisfies ADR-0019's requirement that cursors for one session be totally ordered. The protocol continues to promise nothing about comparing cursors across sessions even though the underlying sequence is global; the wire cursor remains an opaque protocol value derived from, not equal to, the stored sequence.

Sequence allocation must make delivered prefixes stable. A naive shared counter read inside each transaction does not by itself guarantee commit order: a transaction holding a lower number can commit after a higher number is already visible, and a publisher that delivered past it would expose a gap that later fills in — exactly what ADR-0019 forbids. The normative requirement is that a delivered cursor is never later discovered to have skipped a committed event; the publisher may hand an event onward only once no in-flight transaction can still commit a lower sequence. The technique that establishes this (an in-flight allocation horizon, a serialized allocation step, or an equivalent) is implementation scope for the outbox slice, tested against this requirement.

### At-least-once delivery, idempotent consumption

Delivery from the outbox to subscribers is at-least-once. A crash or disconnect between handing an event onward and recording that it was handed redelivers the event; nothing deduplicates on the way out. Consumers are therefore idempotent keyed by sequence: a subscriber that has applied events through cursor `c` discards any redelivered event at or below `c`. ADR-0019 already gives clients exactly this shape — stored cursor, resume strictly after it — so at-least-once costs clients nothing new.

The outbox is a change feed, not a source of truth. The guarded state rows remain the durable statement of record (ADR-0022); nothing reconstructs current state by replaying outbox events, and this record does not reintroduce event sourcing. Snapshots remain the authoritative recovery path: when a subscriber's cursor predates what the hub can serve, the answer is `SnapshotRequired` and a fresh authoritative snapshot, never a best-effort replay.

### Transient streams stay out

Transient stream content — provider token deltas, drafts, progress — is explicitly outside the outbox. It flows on ADR-0019's transient-event path, never advances the cursor, and is never durable (INV-005; the [architecture](../architecture.md#sources-of-truth) keeps streaming drafts in the live hub process unless selectively checkpointed). A reconnecting client missing draft content that streamed while it was away remains correct behavior, not loss. Whether any draft content is ever checkpointed stays with ADR-0022's open streaming-checkpoint question; if a checkpoint policy arrives, it does not enter events into this feed by that fact alone.

### Drain and wake

An in-process publisher task drains the outbox and feeds the subscription layer. It is woken by the pattern ADR-0010 establishes, applied to a second consumer: after a transaction that appended outbox rows commits, the committing code nudges the publisher in-process — a hint, never authority — and a periodic sweep drains whatever a lost nudge missed. Publisher memory (positions, buffers, timers) may be discarded at any moment; the rows are the queue, and a restarted publisher resumes from durable delivery state. `LISTEN`/`NOTIFY` remains the documented multi-process extension exactly as ADR-0010 records it, not a baseline requirement.

### Event payloads are versioned storage records

Outbox rows carry purpose-specific storage records in the ADR-0022/ADR-0034 discipline: a closed event-kind discriminator, a storage representation version, the session and other typed references the event concerns, and the typed payload fields the corresponding protocol event states. They are protocol-facing projections of the committed transition — not domain types, not serialized in-process values, and not the persistence schema re-served (INV-002, INV-005). The boundary maps an outbox record outward to a wire event deliberately, under the negotiated version and capability set; cross-version payload compatibility and the rule that a new cursor-advancing event kind is a version increment remain ADR-0021's, referenced rather than restated.

### Deferred to later slices and decisions

Exact DDL, table and column names, the delivered-marking scheme, and the prefix-stability technique are implementation slices under ADR-0022's migration discipline. Retention and pruning of delivered rows — and therefore how far back a subscription can resume before `SnapshotRequired` — is a separate policy decision alongside ADR-0019's open retention question. Multi-process fan-out arrives only with the revisit ADR-0010 already requires for any second committing process. Cross-version payload negotiation details are ADR-0021's.

## Invariants

- INV-032: this record supplies the committing-side mechanism behind the catalog row — the gap-free, resumable durable-update feed that snapshots plus subscriptions promise. The row remains the statement of record.
- INV-005: transient streaming stays a distinct non-durable representation; outbox event records are a projection representation distinct from semantic content, audit evidence, and presentation.
- INV-002: outbox records, SQL, and mappings live in the persistence boundary; no domain transition consumes or produces them directly.
- INV-007: unchanged and relied upon — the event append rides the same transaction whose commit precedes acknowledgement, so an acknowledged effect always has its event.
- INV-010: nothing durable references the publisher's process state; queued events survive restart as rows.

## Strongest alternative

**Change data capture via Postgres logical replication** — derive the event stream from the WAL, so no application code can ever forget the append. It is rejected because it buys that guarantee at operational cost the single-hub baseline cannot justify (replication slots, WAL retention coupling, decoding management), because it emits physical row changes that would still need re-projection into protocol-facing typed events — recreating this record's payload discipline with the storage schema as its input, which ADR-0022 explicitly keeps out of wire contracts — and because slot-based delivery is a second coordination system where ADR-0010 deliberately has one. The outbox keeps event meaning a reviewed, typed decision in the same transaction that knows it.

## Rejected alternatives

- **Publish-after-commit from application code.** The dual write this record exists to exclude: a crash between commit and publish silently loses the update, violating ADR-0019's no-gap contract with no record that anything is missing.
- **Full event sourcing.** Already rejected by ADR-0022; the state rows stay authoritative, and this feed is derivative. Making the outbox the source of truth would re-import every projection-code enforcement problem that rejection names.
- **Synchronous in-transaction notify as the only mechanism.** Coupling delivery to the commit path makes client availability a commit concern and inherits the `NOTIFY` limits ADR-0010 documents (connected-listener-only delivery, global queue serialization, payload caps); with no durable rows there is nothing to resume from.
- **Deriving events at read time from the state tables, with no outbox.** Current-state rows overwrite their discriminators, so a transition's occurrence and order are not generally recoverable, and a cross-table commit-ordered fact would have to be added anyway — that fact is the outbox.

## Consequences

Every future state-changing slice carries a standing obligation: its transaction appends the outbox rows for whatever client-visible events its transition produces, reviewed with the slice like any other invariant. The subscription implementation consumes this sequence rather than inventing a cursor. Each state-changing transaction pays a small fixed cost — one or a few appended rows — and the outbox table grows until the deferred pruning policy lands, which that policy must address rather than this record pre-deciding it. In exchange, no committed transition can ever be client-invisible by accident, and reconnection semantics rest on a durable fact instead of process memory.

## Scenario walkthroughs

- **S02:** Activation, attempt creation, and call creation each append their event rows in their committing transactions; each commit nudges the publisher, which delivers durable-transition events in sequence order. Provider deltas stream as transient events and never touch the outbox; the committed assistant content arrives as a durable transition whose row was appended by the commit itself.
- **S12:** A stale or duplicated runner result arrives at [ADR-0009](0009-dispatch-fencing.md)'s compare-and-set and changes zero rows; zero events are appended, and subscribers see exactly one durable-transition event for the one application that committed — the observable half of ADR-0019's S12 walkthrough now has its mechanism.
- **S24:** A client reconnects and subscribes from stored cursor `c'`. The hub serves the outbox suffix after `c'` in order, or refuses with `SnapshotRequired` when `c'` predates retained history; the client re-snapshots and resubscribes. Redelivered events at or below the client's cursor are discarded by sequence.
- **S03:** Restart discards the publisher's memory. The events appended by pre-restart commits are still rows; the post-startup sweep finds undelivered ones and delivery resumes, duplicating at most what at-least-once permits. No committed transition lacks its event, because no code path could commit one without the append.

## Open questions

- Exact schema, delivered-marking, prefix-stability technique, and publisher structure — the outbox implementation slice.
- Retention and pruning of delivered events, jointly fixing the resumable-history window ADR-0019 leaves open.
- Draft checkpointing remains open under ADR-0022's streaming-checkpoint question, outside this feed.
- Cross-process fan-out and `LISTEN`/`NOTIFY` adoption timing remain with ADR-0010's multi-process revisit.

## Explicit non-decisions

This record adds no code, DDL, crate, or dependency and defines no wire schema; the record shapes above are semantic, not final storage or API names. It does not decide subscription backpressure, batching, or resource limits (open under ADR-0019), the wire encoding of the cursor (an opaque protocol value there), or which concrete event kinds exist — each arrives with its slice under ADR-0021's rules. It does not decide the provider-facing cancellation-intent delivery mechanics whose storage ADR-0022 leaves open; the "outbox" named there is that separate outbound-effect concern, and whether it reuses this pattern is that question's to answer.

# Usage evidence

This contract is verified against PR #1138 (`agent/web-usage-http`).

## Canonical evidence

Usage reads project terminal physical model calls without materializing the
transcript. Ordinary and approval-judge calls belong to a turn;
context-compaction calls belong directly to a session and therefore have no turn
identity. Each projected row retains the resolved provider/model target,
non-secret credential-profile reference, evidence provenance, input-token
semantics, independently optional token axes, and projection time. The
projection is append-only.

Canonical credential references remain exact and do not gain a terminalization
bound from this read projection. The exposed profile label is nonempty and at
most 256 UTF-8 bytes. References of at most 250 bytes use an `exact:` label,
while longer references receive a stable projection-owned `mapped:` identity.
The exact-to-bounded mapping is unique, so distinct oversized references cannot
collide, and the discriminators keep mapped identities disjoint from literal
names. A bounded digest lookup serializes each mapping bucket, while exact
comparison resolves digest collisions without indexing or bounding the canonical
reference. The exact reference is retained once in that mapping and is not
copied into each projected call. Reads expose only the bounded, collision-free
profile label, while server-side configured-cost derivation reconstructs the
exact reference from that label and mapping. Each physical token axis is either absent or an
exact integer in the `u64` domain. Aggregate token sums use `u128`, so every sum
admitted by the bounded source-call ceiling remains exact.

## Compatibility grouping

Aggregate reads group only calls that agree on call kind, resolved target,
credential profile, provenance, input-token semantics, and the typed presence
state of every token axis. Absence is never replaced by zero. This separation is
the compatibility boundary for later rate-based cost derivation. The aggregate
cache-normalization flag says only whether cache-inclusive input can be
normalized without underflow. It does not claim that a configured rate's decimal
arithmetic is representable or equivalent to checked per-call costing; rate
consumers must establish those properties independently.

An aggregate consumes at most 10,000 newest matching calls and returns at most
256 compatibility groups. `truncated` is true when either source calls or groups
exceed those hard safety ceilings. Why: bounding before grouping prevents an
unscoped lifetime query from imposing work proportional to retained history.

## Selection and time

Both read forms accept exact optional session, turn, target, provenance, and
call kind filters. A turn filter excludes session-level context-compaction
calls. Time ranges are half-open: the lower boundary is inclusive and the upper
boundary is exclusive. Missing boundaries mean unbounded in that direction.
Empty and reversed ranges are rejected when constructed, and accepted timestamps
are limited to the shared PostgreSQL/`time` representable range through
`9999-12-31T23:59:59.999999Z`.

## Detail pagination

Detail reads return at most 100 calls in newest-first order by
`(recorded_at, model_call_id)`. `recorded_at` is the terminal statement time,
not the enclosing transaction's start time. A continuation cursor is an
exclusive boundary at that same pair. The cursor provides deterministic keyset
traversal of rows already visible ahead of it, not a cross-page snapshot.
Oldest-first traversal is not exposed because transaction start timestamps can
become visible behind an already emitted oldest-first cursor when concurrent
transactions commit.

Indexes led by session, turn, target, provenance, call kind, combined
session/call-kind selection, combined session/provenance selection, combined
session/target selection, combined turn/call-kind selection, combined
target/provenance selection, combined target/call-kind selection, and combined
provenance/call-kind selection support selective chronological pages.

## Browser/API presentation and configured cost

`GET /api/usage/summary` exposes at most 256 compatibility groups and
`GET /api/usage/calls` exposes at most 100 newest-first calls. Both accept the
selection and time filters above, including all three closed call-kind spellings:
`model_call`, `approval_judge`, and `context_compaction`. Malformed bounds, closed
values, UUIDs, or partial cursors are application errors.

Dollar cost is not stored in the projection. Signalboxd reconstructs the exact
non-secret credential reference from the bounded profile label, then derives cost
at read time from configuration-owned target rates. Independently reported axes
remain priceable when cache normalization is incomplete; only a contradictory
cache-inclusive breakdown is invalid. Each derived amount carries its rate
version and `real` or `metered_equivalent` label. Unavailable cost carries one
closed reason: no token evidence, unknown input semantics, incomplete cache axes,
invalid cache breakdown, or unavailable configuration.

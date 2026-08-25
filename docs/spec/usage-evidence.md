# Usage evidence

This contract is verified against PR #1137 (`agent/web-usage-cost`).

## Canonical evidence

Usage reads project terminal physical model calls without materializing the
transcript. Ordinary and approval-judge calls belong to a turn;
context-compaction calls belong directly to a session and therefore have no turn
identity. Each projected row retains the resolved provider/model target, bounded
non-secret credential-profile label, evidence provenance, input-token semantics,
independently optional token axes, and projection time. The projection is
append-only.

Canonical credential references remain exact and do not gain a terminalization
bound from this read projection. The exposed profile label is nonempty and at
most 256 UTF-8 bytes. References of at most 250 bytes use an `exact:` label,
while longer references receive a stable projection-owned `mapped:` identity.
The exact-to-bounded mapping is unique, so distinct oversized references cannot
collide, and the discriminators keep mapped identities disjoint from literal
names. A bounded digest lookup serializes each mapping bucket, while exact
comparison resolves digest collisions without indexing or bounding the canonical
reference. The exact reference is retained once in that mapping and is not
copied into each projected call; reads and aggregates use only its bounded,
collision-free profile label. Each physical token axis is either absent or an
exact integer in the `u64` domain. Aggregate token sums use `u128`, so every sum
admitted by the bounded source-call ceiling remains exact. A projected row's
call kind must correlate with the call's immutable global identity record: an
insertion guard rejects any row — including one from maintenance SQL — whose
kind contradicts that identity, because the append-only projection would
otherwise misclassify the physical call permanently.

## Compatibility grouping

Aggregate reads group only calls that agree on call kind, resolved target,
credential profile, provenance, input-token semantics, and the typed presence
state of every token axis. Absence is never replaced by zero. This separation is
the compatibility boundary for later rate-based cost derivation. Each group
carries a named two-state cache-normalization axis that says only whether
cache-inclusive input can be normalized without underflow. It does not claim
that a configured rate's decimal arithmetic is representable or equivalent to
checked per-call costing; rate consumers must establish those properties
independently.

An aggregate consumes at most 10,000 newest matching calls and returns at most
256 compatibility groups. The report carries a named two-state completeness axis
that records truncation when either source calls or groups exceed those hard
safety ceilings. Why: bounding before grouping prevents an unscoped lifetime
query from imposing work proportional to retained history.

The result shapes hold their bounds by construction rather than by adapter
discipline: the credential-profile label is a checked bounded discriminated
type, a group's optional sums must agree with its key's declared presence
coverage, a report cannot carry more than the group ceiling, and a detail page
cannot exceed its requested limit. A reader result that would violate any of
these is unconstructable, and the PostgreSQL adapter fails closed on a
projection row that would require one.

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
not the enclosing transaction's start time, so ties arise between calls
projected by one statement, such as one backfill statement, and the call UUID
breaks them. A continuation cursor is an exclusive boundary at that same pair.
The cursor provides deterministic keyset traversal of rows already visible ahead
of it, not a cross-page snapshot. Oldest-first traversal is not exposed because
a statement timestamp is assigned before its transaction commits, so a
late-committing concurrent transaction can make a row with an earlier statement
time visible behind an already emitted oldest-first cursor.

Every allowed exact-selection conjunction has an index whose leading columns are
exactly the selected dimensions followed by the chronological page order: each
single dimension, every combination of session, target, provenance, and call
kind, and every combination of turn with target, provenance, and call kind. A
turn belongs to exactly one session, so when a selection supplies both, only the
turn predicate reaches the projection: the session filter is decided by one
bounded probe of the unique turn-ownership record, a matched pair reads exactly
the turn scope through a turn-led index, and a mismatched pair is proven empty
without scanning either dimension's history. Each read statement is assembled
with only the selected dimensions' predicates, so every selection shape carries
its own prepared statement and even a cached generic plan sees exactly the
conjunction its ordered index serves. Why: pairwise prefixes are not enough —
each pair can be common while a deeper intersection is rare or empty, which
would force a large range to be scanned and filtered before the bounded detail
or aggregate limit applies — and an optional-predicate statement shared across
shapes would let a generic plan discard those ordered paths entirely.

## Open edges

Committed unimplemented functionality: configured currency rates and browser/API
presentation will consume these compatibility groups in later slices. No current
surface provides either capability; this contract supplies only exact bounded
evidence and the compatibility boundary those later surfaces must preserve.

# Usage evidence

This contract is verified against PR #1137 (`agent/web-usage-cost`).

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
while longer references use a stable `digest-md5:` label. These discriminators
make literal names and digest labels disjoint. Exact canonical references remain
a private grouping dimension, so the bounded label cannot combine otherwise
distinct evidence. Each physical token axis is either absent or an exact integer
in the `u64` domain. Aggregate token sums use `u128`, so every sum admitted by
the bounded source-call ceiling remains exact.

## Compatibility grouping

Aggregate reads group only calls that agree on call kind, resolved target,
credential profile, provenance, input-token semantics, and the typed presence
state of every token axis. Absence is never replaced by zero. This separation is
the compatibility boundary for later rate-based cost derivation.

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
`(recorded_at, model_call_id)`. A continuation cursor is an exclusive boundary
at that same pair. The cursor provides deterministic keyset traversal of rows
already visible ahead of it, not a cross-page snapshot. Oldest-first traversal
is not exposed because transaction start timestamps can become visible behind an
already emitted oldest-first cursor when concurrent transactions commit.

Indexes led by session, turn, target, provenance, call kind, combined
session/call-kind selection, and combined provenance/call-kind selection support
selective chronological pages.

## Open edges

Configured currency rates and browser/API presentation are owned by later
slices; this contract supplies exact bounded evidence and compatibility groups.

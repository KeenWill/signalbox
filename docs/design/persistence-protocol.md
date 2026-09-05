# Persistence protocol design

This design is not built; it extends
[persistence-protocol](../spec/persistence-protocol.md).

## Goal

Complete the durable storage that built subsystems already reserve space for:
runner replacement, abandonment, and operation-failure evidence; retirement of
an unacknowledged workspace release; runner placement in imported-create command
records; the instruction admitted set; credential-pool state and availability
waits; a producer for the session-state-changed event; and daemon-owned OAuth
material.

## Design

Runner replacement and abandonment each end in one terminal orchestration
transaction that holds authority outside the placement aggregate, moves the
placement, and appends one runner-state-transition event per affected session in
the same transaction. Replacement of a pinned placement also appends the
placement transcript entry: one positive placement revision with a foreign key
to the same session's placement record at that revision, which reconstitution
resolves, rejecting a missing, cross-session, non-successor, or duplicated
reference. Replacing a `RunnerLostBeforePin` placement updates only the exact
selector and returns to `Unpinned`, appending no such entry. The record kind
already carries the replacement and abandonment states; the transactions that
produce them do not exist.

A daemon transaction retires a workspace release the lost runner never
acknowledged, so a lost runner leaves no release outstanding.

Runner operation-failure evidence is stored in the transaction that resolves the
correlated operation as refused, and the daemon acknowledges the failure to the
runner only after that commit. The record is append-only and keyed by the
refused operation's correlation identity, so success and refusal are exclusive
after the operation head retires. The record keeps the bounded code, message,
and exact payload of the admitted detail, so runner status inspection reproduces
the failure. Equal retransmission rereads the equal record; unequal reuse is a
correlation error.

Imported-create command records at storage version 4 carry the complete
placement request, and replay compares it with the created session's
revision-one placement.

The instruction admitted set is one durable table with one repository operation
that writes it, beside the immutable append-only admission record
[workspace-instructions](../spec/workspace-instructions.md) commits, which holds
each admission's prior set hash, bundle, rendered evidence, exact rendered
wrapper bytes, and request identity independently of that mutable head. Its row
locks join `crates/persistence/src/lock_inventory.rs`, and a transaction takes
the admitted-set head after the session's scheduler row and before the
current-defaults pointer row and any credential-pool row, FOR SHARE to snapshot
or replace and FOR UPDATE to admit; admission takes the current-defaults pointer
FOR SHARE after that head and holds it through commit, so admission and defaults
replacement serialize.

Credential-pool state, capacity reservations, and availability waits are durable
rows with locks recorded in the same inventory. A transaction takes the
action-head row of every member of the policy it may select after the session's
scheduler row and the admitted-set head, in profile-reference byte order, FOR
SHARE for a member it only reads and FOR UPDATE for a member whose exclusion
state it writes. It takes a capacity or cursor row FOR UPDATE only after every
action head, and multiple capacity rows in profile-reference byte order before
any cursor row. An admission that will insert a contended wait takes the
capacity row of every bounded member the wait will name before it counts
reservations and holds those locks through commit. A reservation release and the
wake it grants commit in one transaction that holds that profile's capacity row.
A capacity reservation records its invocation's process-group identity at spawn,
and startup releases the reservation only after proving that group absent or
terminating it. A pool-selected call pins an interned immutable pool-policy
identity, so a fresh availability chain resolves the policy the call was
authorized under rather than the current document. A chain-exclusion row holds a
separately clearable state beside its insert-only turn-local fact. Exhaustion
evidence carries contiguous per-member rows in policy order beside its failure
header, each row carrying its closed exclusion kind and optional reset. The
machine they serve is owned by
[credential-availability](../spec/credential-availability.md), and its design
fixes their transitions.

A session-state-changed event is appended, through the outbox append, in the
transaction that commits a nonterminal session state change; the transition to
terminal has its own event. Its typed record and decoder exist.

OAuth material storage supplies three shapes: a per-generation
refresh-in-progress marker that exactly one transaction can win; an atomic
replace-and-clear that installs the new material and clears the marker in one
commit; and a reread that reports whether a replacement committed and whether
the marker is set. The replace shape expresses an exchange that returns a new
identity token and one that returns none, without a second commit and without
mixing tokens from different exchanges. The replace-and-clear commit publishes
the durable member-availability update that wakes a parked deadline-free
exhausted wait, as every accepted exclusion clear does in its own transaction,
and a clear that removes no exclusion publishes nothing. Provisioning locks its
own profile row and every co-member profile row in one reference-ordered
acquisition, rereads membership under those locks and repeats when the set has
grown, and interning a pool-policy revision locks every member's profile row in
the same order. Delivery of OAuth material to a model call is owned by
[configuration-and-credentials](../spec/configuration-and-credentials.md).

## Compatibility constraints

The placement snapshot writer keeps refusing loss, replacement, and abandonment;
the new transactions gain that authority elsewhere.

No writer produces imported-create storage version 4, and the version gate keeps
rejecting it until a record at that version carries placement.

The session-state-changed decoder stays, and nothing appends the kind until its
producer lands.

Failure detail is never acknowledged before it is stored, because a restart
would forget evidence operators must inspect; the operation transition is never
delayed until after acknowledgement, because the runner would keep resending a
failure the daemon had already acted on.

Every new table follows the spec page: kind-scoped storage versions on
durable-command and outbox records, append-only facts under triggers, events
appended in the committing transaction, and the row locks the inventory names
issued from that file.

## Acceptance criteria

Replacement and abandonment each apply the placement move and one
runner-state-transition event per affected session in one terminal transaction,
and the placement snapshot writer is unchanged.

After a runner is lost, no workspace release that runner held stays
unacknowledged; a daemon transaction has retired it. Releases held by reachable
runners stay pending.

Every acknowledged operation failure is readable after restart, and an equal
retransmission returns the recorded receipt.

An imported-create record at storage version 4 stores placement, and an older
reader rejects it as unsupported.

The admitted set has a table, a repository operation, and inventory-recorded
locks.

Pool state, reservations, and waits are durable and reconstitute after restart.

Every committed nonterminal session state change appears as a
session-state-changed event in the outbox.

Exactly one refresh transaction wins per generation, replace-and-clear is one
commit, and a reread distinguishes a committed replacement from none.

A replace-and-clear commit publishes a durable member-availability update, and a
deadline-free exhausted wait parked on that member wakes.

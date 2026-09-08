# Persistence protocol design

This design is not built; it extends
[persistence-protocol](../spec/persistence-protocol.md).

## Goal

Complete the durable storage that built subsystems already reserve space for:
general runner operation-failure evidence; retirement of an unacknowledged
workspace release; runner placement in imported-create command records; the
instruction admitted set; credential-pool state and availability waits; and
daemon-owned OAuth material.

## Design

A daemon transaction retires a workspace release the lost runner never
acknowledged, so a lost runner leaves no release outstanding.

For operations other than replacement provisioning, runner operation-failure
evidence is stored in the transaction that resolves the correlated operation as
refused, and the daemon acknowledges the failure to the runner only after that
commit. The record is append-only and keyed by the refused operation's
correlation identity, so success and refusal are exclusive after the operation
head retires. The record keeps the bounded code, message, and exact payload of
the admitted detail, so runner status inspection reproduces the failure. Equal
retransmission rereads the equal record; unequal reuse is a correlation error.

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

The replace-and-clear commit publishes the durable member-availability update
that wakes a parked deadline-free exhausted wait, as every accepted exclusion
clear does in its own transaction, and a clear that removes no exclusion
publishes nothing. Delivery of OAuth material to a model call is owned by
[configuration-and-credentials](../spec/configuration-and-credentials.md).

## Compatibility constraints

No writer produces imported-create storage version 4, and the version gate keeps
rejecting it until a record at that version carries placement.

Failure detail is never acknowledged before it is stored, because a restart
would forget evidence operators must inspect; the operation transition is never
delayed until after acknowledgement, because the runner would keep resending a
failure the daemon had already acted on.

Every new table follows the spec page: kind-scoped storage versions on
durable-command and outbox records, append-only facts under triggers, events
appended in the committing transaction, and the row locks the inventory names
issued from that file.

## Acceptance criteria

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

A replace-and-clear commit publishes a durable member-availability update, and a
deadline-free exhausted wait parked on that member wakes.

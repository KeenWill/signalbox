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

## Shape

Runner replacement and abandonment are each one orchestration transaction that
holds authority outside the placement aggregate, moves the placement, and
appends one runner-state-transition event per affected session in the same
transaction. The record kind already carries the replacement and abandonment
states; the transactions that produce them do not exist.

A daemon transaction retires a workspace release the runner never acknowledged,
so a lost runner leaves no release outstanding.

Runner operation-failure evidence is stored in the transaction that resolves the
correlated operation as refused, and the daemon acknowledges the failure to the
runner only after that commit. Equal retransmission rereads the equal record;
unequal reuse is a correlation error.

Imported-create command records at storage version 4 carry runner placement.

The instruction admitted set is one durable table with one repository operation
that writes it. Its row locks join `crates/persistence/src/lock_inventory.rs`
and take their place in the fixed lock order the spec page states.

Credential-pool state, capacity reservations, and availability waits are durable
rows with locks recorded in the same inventory. The machine they serve is owned
by [credential-availability](../spec/credential-availability.md), and its design
fixes their transitions; this document fixes only that they are stored and
locked under the persistence rules.

A session-state-changed event is appended, through the outbox append, in the
transaction that commits the session state change. Its typed record and decoder
exist.

OAuth material storage supplies three shapes: a per-generation
refresh-in-progress marker that exactly one transaction can win; an atomic
replace-and-clear that installs the new material and clears the marker in one
commit; and a reread that reports whether a replacement committed and whether
the marker is set. The replace shape expresses an exchange that returns a new
identity token and one that returns none, without a second commit and without
mixing tokens from different exchanges. Delivery of OAuth material to a model
call is owned by
[configuration-and-credentials](../spec/configuration-and-credentials.md).

## Constraints on present code

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

Every new table follows the spec page: typed records with kind-scoped storage
versions, append-only facts under triggers, events appended in the committing
transaction, and every explicit row lock recorded in the inventory.

## Acceptance

Replacement and abandonment each commit in one transaction with one
runner-state-transition event per affected session, and the placement snapshot
writer is unchanged.

After a runner is lost, no workspace release stays unacknowledged; a daemon
transaction has retired it.

Every acknowledged operation failure is readable after restart, and an equal
retransmission returns the recorded receipt.

An imported-create record at storage version 4 stores placement, and an older
reader rejects it as unsupported.

The admitted set has a table, a repository operation, and inventory-recorded
locks.

Pool state, reservations, and waits are durable and reconstitute after restart.

Every committed session state change appears as a session-state-changed event in
the outbox.

Exactly one refresh transaction wins per generation, replace-and-clear is one
commit, and a reread distinguishes a committed replacement from none.

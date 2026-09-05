# Turn lifecycle and scheduling design

This design is not built; it extends
[turn-lifecycle-and-scheduling](../spec/turn-lifecycle-and-scheduling.md).

## Goal

Four capabilities land here. A turn parks durably while no credential in its
pool is available and resumes when one is. A session whose runner is lost is
recovered on a replacement runner or abandoned, and a restart reconciles
retained runner work before the generic scan can end it. Activation freezes the
session's instruction eligibility for the turn.

## Shape

The pool-availability wait is a distinct active phase with a closed cause,
exhausted or contended. It retains the session slot and binds the frozen
pool-policy snapshot. Which ending a selection attempt reaches, and therefore
whether a wait is stored and in which form, is owned by
[credential-availability](../spec/credential-availability.md); this subsystem
owns the turn phase with its attempt disposition and the wake conditions, and
states no rule about which ending is reached. Entering either wait form
atomically ends the call-free current attempt with a yielded-to-durable-wait
disposition and stores the wait, leaving no live attempt. Startup reconstitutes
the stored wait only from its complete evidence and does not reclassify it. The
scheduler makes the wait eligible on a reached deadline, an exact reservation
release, or a durable member-availability update. Release atomically consumes
the wait, creates a fresh prepared successor attempt, and returns the same turn
to running, resuming the availability chain the wait was part of rather than
starting a new one.

Runner-loss recovery has two user commands, replace and abandon, whose request
shapes and placement transitions are owned by
[runner-protocol](../spec/runner-protocol.md). This subsystem owns their effect
on the turn. Replacement is never refused because a model call is in flight; it
is staged behind that call. The command claims its identity and provisioning
authorization immediately, and the terminal transaction that installs the
successor placement and extends the next context frontier commits only after any
authorized in-flight daemon-local call for the session reaches its observation
boundary, so the call's entries append before the placement boundary. A call
that ends known-failed, refused, cancelled, or ambiguous reaches an observation
boundary too, so staging never waits indefinitely. Abandonment requires no
active turn; with a turn active it records that the turn needs existing control,
and the user empties the slot through the stop, approval, or reconciliation flow
first. A queued turn remains queued and cannot activate while its placement is
lost. Both commands are administrative recovery: they neither widen the
interrupt delivery nor create a standalone cancellation path, and no case turns
ambiguous effect evidence into known failure.

Recovery-only startup binds the runner socket in recovery-only mode after
migrations, reconciles retained runner inventory, evidence, and nonterminal
replacement commands, completes the generic startup scan, binds the process
socket, and only then enables ordinary runner enrollment and scheduling. The
generic scan skips runner-owned attempts until that phase has resolved them,
then classifies only the remaining daemon-owned tenure. With no retained runner
work the phase completes immediately.

The instruction-eligibility freeze extends the activation transaction. Under the
same scheduler lock, activation copies the session's ordered eligibility entries
and the admitted-set head, locking the admitted-set head at the position and in
the mode the persistence lock protocol fixes, and inserts the turn-start
instruction manifest in that transaction. The replacement command takes the
scheduler lock and then the admitted-set lock, so a replacement either precedes
activation and enters the snapshot or follows it and affects only a later turn.
The manifest and eligibility shapes are owned by
[workspace-instructions](../spec/workspace-instructions.md).

## Constraints on present code

No present active phase, storage discriminator, startup-scan branch, scheduler
path, or process state supplies the pool-availability wait. The active-phase
vocabulary and its storage discriminators admit a new phase without
reinterpreting an existing one.

The recovery-only ordering exists so that generic recovery cannot terminalize
authority that retained runner evidence resolves. The present order, generic
scan before runner-socket bind, stays compatible with inserting a runner
reconciliation phase before the scan.

No present surface performs retained runner reconnect or replacement recovery,
and no runner execution surface depends on the projected loss state. The present
loss projection, which marks the placement lost and moves an active turn at a
runner boundary to the runner-recovery wait, remains the only producer of that
state.

The activation transaction remains the one atomic boundary for everything a turn
start owns. Later eligibility work changes only the copied values and rendered
rows, never that boundary.

## Acceptance

A turn whose model call finds no available credential parks in the new phase
with its attempt ended by a yield, survives restart unchanged, becomes eligible
on exactly the three wake conditions, and resumes with a fresh prepared attempt
on the same availability chain.

A replacement command issued while a call is in flight is accepted, and its
placement boundary commits after that call's observation boundary; a commit in
any other order is rejected by the prefix-preserving frontier triggers.

An abandonment command against a session with an active turn is rejected with
the existing-control result and creates no cancellation.

A queued turn whose placement is lost is not activated by any pass or sweep.

A restart with retained runner work resolves every runner-owned attempt before
the generic scan runs, and the generic scan ends no attempt a runner still owns.

Every started or terminal turn owns exactly one turn-start instruction manifest,
written in its activation transaction and never after it.

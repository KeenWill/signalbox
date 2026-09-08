# Turn lifecycle and scheduling design

This design is not built; it extends
[turn-lifecycle-and-scheduling](../spec/turn-lifecycle-and-scheduling.md).

## Goal

This design adds four capabilities. A turn parks durably while no credential in
its pool is available and resumes when one is. An active turn whose runner is
lost admits staged replacement, and a restart reconciles retained runner work
before the generic scan can end it. Activation freezes the session's instruction
eligibility for the turn.

## Design

The pool-availability wait is a distinct active phase with a closed cause,
exhausted or contended. It retains the session slot and binds the frozen
pool-policy snapshot. Which ending a selection attempt reaches, and therefore
whether a wait is stored and in which form, is owned by
[credential-availability](../spec/credential-availability.md); this subsystem
owns the turn phase with its attempt disposition and the wake conditions, and
states no rule about which ending is reached. Entering either wait form
atomically ends the call-free current attempt with a yielded-to-durable-wait
disposition and stores the wait, leaving no live attempt. Startup reconstitutes
a stored wait only from its complete evidence and does not reclassify it; it
re-evaluates every retained contended wait against the current registrations,
and a restart alone wakes nothing. A contended wait becomes eligible on a
reservation release by one of the bounded members it names, on its deadline, on
that startup re-evaluation, on a durable member-availability update, or on an
operator clearing a credential exclusion. An exhausted wait becomes eligible on
its deadline, when it has one, on a durable member-availability update, or on an
operator clearing a credential exclusion. A wake re-runs admission from current
state, and the turn resumes only when that admission selects a member. A woken
contended wait whose admission selects no member and still finds an admissible
member at its bound stays parked, rewritten from current state with the
surviving bounded members and their reservation identities, the remaining
exclusions, and the derived deadline. One that finds no admissible bounded
member left takes the exhaustion outcome instead. A woken exhausted wait whose
admission selects exhausted-wait again stays parked and rewrites its evidence
and derived deadline in place; one whose cleared exclusion leaves the newly
admissible member at its bound is rewritten as a contended wait, whose bounded
members with their reservations, remaining exclusions, and derived deadline are
recomputed from current state. Release atomically consumes the wait, creates a
fresh prepared successor attempt, and returns the same turn to running, resuming
the availability chain the wait was part of rather than starting a new one. A
stop-turn request against a parked wait consumes the wait, creates a fresh
immediate-successor attempt carrying the applied-interrupt proof, ends that
attempt cancelled, appends the cancellation entry after the wait's latest
frontier, and terminalizes the turn cancelled.

Active-turn replacement extends the user command whose request shape and
placement transitions are owned by
[runner-protocol](../spec/runner-protocol.md). This subsystem owns its effect on
the turn; [runner design](runner-protocol.md) owns the installation transaction.
Replacement is never refused because a model call is in flight; it stays staged
and resumable while daemon-locus requests execute on the current placement.
[Tool loop](tool-loop.md) owns lost-placement resolution. After every request
resolves, replacement takes over in the
[continuation transaction](../spec/tool-loop.md), after all tool results are
appended and before the next call is prepared against the changed placement. The
one exception is an offered runner attempt, either pure or idempotent or backed
by durable no-execution proof, that must be retried on the successor: after
every preceding request resolves, the tool-loop design's distinct
pre-continuation takeover transaction installs the successor and consumes the
staged replacement while that request remains recovery-pending. That transaction
fences successor execution but defers the relocation entry until after all batch
results, as the runner design requires. It projects no result and prepares no
call; result projection and continuation remain deferred until the retry and
later requests resolve and the whole batch is complete. When an interrupt or
crash-loss reconciliation terminalizes the batch, that path completes or retires
the staged replacement before terminalizing the turn. If terminalization wins
before retry dispatch, its transaction resolves the retained attempt and request
with the terminal-turn outcome, projects `ToolClosed`, and suppresses the retry.
If dispatch wins, terminalization waits for the retry attempt's completion or
crash classification and closes the retained dependency before ending the turn.
The command claims its identity immediately and provisioning authorization only
when needed, and its terminal transaction commits only after any authorized
in-flight daemon-local call for the session reaches its observation boundary. A
pinned loss installs the successor placement in that transaction; the runner
design orders its relocation entry and frontier after the call's entries and
batch results. A pre-pin replacement returns the placement to unpinned at the
successor revision and appends no boundary. The terminal transaction also moves
the turn out of the runner-recovery wait when it is still parked there: to
running with a fresh attempt when the loss interrupted no tool attempt, and
otherwise to the phase the retained tool attempt justifies. A staged call that
completed, refused, failed, cancelled, or ended ambiguous leaves the turn in the
state that outcome produced. A call that ends known-failed, refused, cancelled,
or ambiguous reaches an observation boundary too, so staging never waits
indefinitely. A queued turn remains queued and cannot activate while its
placement is lost. Staged replacement is administrative recovery: it neither
widens the interrupt delivery nor creates a standalone cancellation path, and no
case turns ambiguous effect evidence into known failure.

Recovery-only startup binds the runner socket in recovery-only mode after
migrations, reconciles retained runner inventory, evidence, and nonterminal
replacement commands, completes the generic startup scan, binds the process
socket, and only then enables ordinary runner enrollment and scheduling. The
generic scan skips runner-owned attempts until that phase has resolved them,
then classifies only the remaining daemon-owned tenure. With no retained runner
work the phase completes immediately. Recovery-only admission precedes the blob
namespace checks that [blob-storage](../spec/blob-storage.md) runs after the
generic scan, and no recovery frame touches blob state.
[Configuration and credentials](../spec/configuration-and-credentials.md)
commits retained OAuth-marker resolution, scratch-home scavenging, prior-process
capacity-reservation recovery, and the legacy family-to-policy backfill. Those
gates sit after the generic scan and before the daemon binds the process socket
or enables ordinary enrollment and scheduling, so a credential failure cannot
block recovery of acknowledged work and no admitted work runs against stale
credential state.

The instruction-eligibility freeze extends the activation transaction. Under the
same scheduler lock, activation copies the session's ordered eligibility entries
and the admitted-set head, locking the admitted-set head at the position and in
the mode the persistence lock protocol fixes, and inserts the turn-start
instruction manifest in that transaction. The replacement command takes the
scheduler lock and then the admitted-set lock, so a replacement either precedes
activation and enters the snapshot or follows it and affects only a later turn.
The manifest and eligibility shapes are owned by
[workspace-instructions](../spec/workspace-instructions.md).

## Compatibility constraints

No present active phase, storage discriminator, startup-scan branch, scheduler
path, or process state supplies the pool-availability wait. The active-phase
vocabulary and its storage discriminators admit a new phase without
reinterpreting an existing one.

The recovery-only ordering exists so that generic recovery cannot terminalize
authority that retained runner evidence resolves. The present order, generic
scan before runner-socket bind, stays compatible with inserting a runner
reconciliation phase before the scan.

No present surface performs retained runner reconnect, and no runner execution
surface depends on the projected loss state. The present loss projection, which
marks the placement lost and moves an active turn at a runner boundary to the
runner-recovery wait, remains the only producer of that state.

Only the path that prepares the turn's initial model call inside the activation
transaction records the manifest there. The ordinary path records it after
activation, and a turn that stops being active first has none. The freeze moves
every manifest into the activation transaction.

## Acceptance criteria

A turn whose model call finds no available credential parks in the new phase
only when the credential machine selects a wait; an exhaustion that selects no
wait, such as one under the `fail` policy, terminalizes the turn through the
failure rows [credential-availability](../spec/credential-availability.md) owns
and never enters the phase. A parked turn has its attempt ended by a yield,
survives restart unchanged, becomes eligible on exactly the wake conditions of
its wait form, and resumes with a fresh prepared attempt on the same
availability chain only when the woken admission selects a member. When one
release wakes several contended waits, the turn whose admission selects the
freed member resumes; each loser that still finds an admissible member at its
bound stays contended and rewrites its complete wait snapshot from current
state. A loser that finds no admissible bounded member left is exhausted rather
than contended, so its wake re-runs the exhaustion decision and a `fail` pool
terminalizes it through the same failure rows. A stop-turn request against a
parked turn terminalizes it cancelled through a fresh cancelled successor
attempt and leaves no wait stored.

A pinned-loss replacement command issued while a call is in flight is accepted,
and its placement boundary commits after that call's observation boundary. A
commit that places the replacement boundary before the call's observation
boundary is rejected.

A queued turn whose placement is lost is not activated by any pass or sweep.

A restart with retained runner work resolves every runner-owned attempt before
the generic scan runs, and the generic scan ends no attempt a runner still owns.

Every activated turn owns exactly one turn-start instruction manifest, written
in its activation transaction rather than by a post-activation scan.

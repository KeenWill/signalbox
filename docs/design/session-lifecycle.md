# Session lifecycle design

This design is not built; it extends
[session lifecycle](../spec/session-lifecycle.md) with the committed lifecycle
behavior that no present surface provides.

## Goal

Every owned session reaches a terminal outcome or a human, and no module writes
lifecycle machinery of its own beyond the retained repo-watch start lease, the
core-integrated convergence sweep's cool-off, and the repo-watch
dispatch-attempt budget whose exhaustion parks the owned sessions the obligation
wraps as the module. A failure parks the session instead of ending it or
stalling silently, a redispatch closes the session it replaces, modules receive
deadline expiries as events, every dispatched session records what it was
handed, and a program run acts under its own actor. An active stall parks the
session, a repo-watch achievement is verified against its gate, a sticky stop
holds off redispatch, and a closure removes the worktree and container the
session held.

## Design

A structural failure, an unknown failure, or an exhausted retry budget on a live
owned session moves the session to parked with the matching park cause
(structural failure, unknown failure, or retry budget exhausted) and, on the
structural and retry-budget parks, the standing failure cause attached; it never
terminalizes the session directly. A retryable failure moves the session through
recovering or blocked while budgeted retries run, and the retryable outcome is
recorded only when the session closes with the retryable cause standing. A
structural failure is never resumed automatically: the session parks with the
structural cause, and the expected closure is a fresh session that supersedes
it. When [goal mode](../spec/goal-mode.md) reaches either its attempt budget or
its lifetime attempt ceiling, the goal's session moves from blocked to parked
with cause retry budget exhausted, where the owner sees it; exhaustion is never
a silent stop. The domain already defines the park causes, the rule that a
park's cause must admit its standing failure cause, and the closures that carry
the standing cause forward. The missing part is the driver: the turn liveness
pass, the goal disposition pass, and model-call failure classification call the
park path with the classified cause inside the transaction that records the
failure.

When a module's redispatch owns the retry of parked work, the redispatch issues
supersede against the parked predecessor, naming the successor, in the same
transaction that creates the successor, so the successor never duplicates a
pursuit the predecessor still owns. The unattended
[repo-watch](../spec/repo-watch.md) escalation is the present case: its fresh
dispatch fails the predecessor's turn instead of superseding the session. The
superseded outcome and the supersede command exist; no dispatch path issues it.

Every deadline expiry is a published transition with the expired deadline named
in its cause. Admission expiry reaches the terminal event the satellite's
trigger appends; the state-change event for a waiting or active-stall expiry is
to be built, because the deadline pass parks the row and appends no event. The
daemon reads the active-stall bound from configuration, and the pass parks an
active or recovering session whose stall exceeds it, with the cause selected
from the state: active-stall deadline expired from active, recovering deadline
expired from recovering. Modules and the program substrate subscribe to those
events and run no timer over a session of their own, the repo-watch start lease
and the core-integrated convergence sweep's cool-off excepted. A module that
needs a deadline core does not arm asks for a new deadline kind in core.

The lifecycle actor vocabulary gains a run-scoped program-run actor for commands
issued by a registered program's run, as
[program substrate](../spec/program-substrate.md) and
[identity and commands](../spec/identity-and-commands.md) commit it. It is a
reference to the program run, not a module name. Storage admits the new
discriminator with a run reference beside it, and classification treats a
program-run principal as it treats a module principal: it wins over the domain
actor.

Every dispatched session records the size of its initial payload at creation, as
the token count estimated for the target model and the byte count, on the
session's lifecycle satellite row. The creation command carries the payload, as
the commissioning path already supplies it, so a dispatched session never
precedes its recorded payload and no dispatch path skips the measurement. An
interactive session has no payload at creation; it records the same two
measurements when its first input is accepted, because that input is what the
session was handed.

A repo-watch session's finish check re-tests the external gate on the exact head
the achievement names, and only a pass settles the achievement as verified. A
redispatch reads the sticky flag on a stopped predecessor and dispatches nothing
for that source until the source is updated. A closure whose outcome releases
resources removes the session's worktree and container.

## Compatibility constraints

No automatic failure-handling path terminalizes an owned session on a structural
failure, an unknown failure, or an exhausted retry budget; a path that today
ends the run as a blocked goal leaves the goal blocked and the session
non-terminal, which is the state the parking driver will read.

No dispatch path that replaces parked work terminalizes the predecessor as
failed; the predecessor stays in a state supersede can close.

No module adds a timer that compares a session-scoped timestamp to the clock to
decide a lifecycle transition.

Readers of the lifecycle actor column treat the present four actors as
extensible and decode an unknown discriminator as typed corruption, never as one
of the four.

No new dispatch path splits the payload from the creation command.

## Acceptance criteria

A structural failure, an unknown failure, or an exhausted retry budget on a live
owned session leaves the session parked with the matching cause, and with the
standing failure on the structural and retry-budget parks; the session is
terminal only after a closure command.

The goal disposition pass at either limit leaves the goal's session parked with
cause retry budget exhausted, and the operator queue lists it.

A redispatch of parked repo-watch work leaves the predecessor terminal as
superseded, naming the successor, in the commit that creates the successor.

No module owns a session timer beyond the repo-watch start lease and the
core-integrated convergence sweep's cool-off; every other module transition that
follows a deadline follows a published expiry event.

A lifecycle command issued by a program run records the program-run actor with
its run reference, and replaying the command classifies it identically.

Every dispatched session's lifecycle row carries both payload measurements from
creation, and an interactive session's row carries them from its first accepted
input.

An active or recovering session whose stall exceeds the configured bound is
parked by the deadline pass, and a waiting or active-stall expiry appends a
state-change event naming the deadline.

A repo-watch declaration settles verified only after the external gate passes on
the exact head.

After a sticky stop, new activity on the same dispatch source dispatches nothing
until the source is updated.

A closure that releases resources leaves no worktree or container for the
session.

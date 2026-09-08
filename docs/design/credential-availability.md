# Credential availability design

This design extends
[credential availability](../spec/credential-availability.md) with its typed
terminal-client projection.

## Goal

Project credential waits as active turns and preserve terminal failure evidence
when a wait release ends a call-free successor.

## Design

Wait-transition failure after a predecessor call needs a wire shape correlating
that call with a terminal attempt that owns no call. The provider cause remains
the predecessor's; the terminal evidence never reports pool exhaustion.

Wire: a parked turn projects an active transcript turn state that retains the
turn and its slot, never a terminal one, and no rejection detail.
Wait-transition fail (no call) projects a turn state naming pool exhaustion, the
live event `turn_failed`, and a typed `turn_credential_pool_exhausted` live
event. The ending owns no call, so its evidence uses the frozen pool-policy
revision. The read's rejection detail for a revision it cannot resolve names the
session, turn and policy. Which member served an ordinary selection, and whether
a completed successor chain is shown to a client, stay undecided in
[open questions](../open-questions.md).

## Compatibility constraints

A wait projection never terminalizes the retained turn or reports rejection
detail. A terminal release after a predecessor call retains that provider cause
and correlates it with the fresh terminal attempt.

## Acceptance criteria

- A parked turn projects its typed wait cause and retains the active turn.
- A terminal release projects its call-free terminal attempt and the predecessor
  call when one exists; no predecessor uses the pre-call exhaustion producer.

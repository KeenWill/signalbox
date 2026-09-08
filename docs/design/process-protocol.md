# Process protocol design

This design is not built; it extends
[process-protocol.md](../spec/process-protocol.md) with the wire surfaces the
owner has committed and the daemon and terminal client do not implement.

## Goal

Future implementation of these surfaces under protocol version 1 must pair each
daemon handler with its terminal-client consumer in the same change: terminal
failure after credential wait release.

## Design

The after-call wait-transition failure projects the predecessor call identity
alongside its call-free terminal attempt. The endings these shapes project
belong to [credential-availability.md](../spec/credential-availability.md).

## Compatibility constraints

A retry-bound failure with an admissible fallback keeps the generic failed
projection.

A headroom-only exclusion reports
`headroom_reserve { observed_headroom_percent, reserve_percent }` without a
generation, last.

A credential-wide transient exclusion reports
`transient_exclusion { observation_model_call_id }` after `chain_exclusion` and
before `headroom_reserve`.

Chain exclusions remain insert-only and turn-local and are neither listed nor
clearable.

`runner_state_transition` is an admitted version-1 event variant that the daemon
projects when the outbox carries a runner state transition. Every other request
and message above is outside the closed inventories in `crates/process-protocol`
until the daemon and client implement its surface together.

No response code is reserved for an authorization failure, because client
identity, authentication, authorization, and revocation are undecided.

## Acceptance criteria

Every request and message above decodes under version 1 with unknown fields
rejected, and each surface ships with its daemon handler and the terminal-client
consumer it lacks; a follower-visible addition also ships the native client's
decoder and projection updates.

A terminal wait release correlates its call-free attempt with the predecessor
call whose provider failure remains authoritative.

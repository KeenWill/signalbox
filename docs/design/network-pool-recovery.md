# Network pool recovery design

This unbuilt design extends
[credential availability](../spec/credential-availability.md).

## Goal

Preserve a park pool's turn when transport loss exhausts provider-internal
retries and resume it when an existing recovery observer establishes
availability.

## Design

Only a member whose adapter has an automatic recovery observer may be excluded
from the turn after transport loss exhausts provider-internal retries. The typed
transport cause distinguishes this case from server and other provider-internal
failures, which retain terminal failure at the retry limit. When every member is
excluded and at least one exclusion is recoverable this way, the turn retains
its input, frontier, target, and policy in a durable credential wait with cause
`network_unavailable`. Today this transition is limited to Codex members; direct
HTTP and Claude CLI members retain terminal failure at the retry limit.

The existing Codex capacity re-observation loop probes network-unavailable
members as well as quota-limited members. A successful current observation or a
successful changed-profile reload releases the corresponding network exclusion
and wakes the existing wait. Failed observations preserve the wait. No new timer
or scheduler is introduced.

A wake does not reset `numeric_bounds.max_same_credential_attempts_per_turn`.
A parked turn resumes only while the woken member has attempts remaining. If
every member has reached that ceiling, the turn terminalizes instead of parking.

Release is durable and idempotent and retains predecessor failures. Domain,
stored, process-protocol, and browser wait causes carry the typed reason across
their existing boundaries.

## Compatibility constraints

Non-park pools retain terminal failure at the retry limit. Terminal turns are
never revived. Authentication recovery and other failure categories retain their
existing behavior.

## Acceptance criteria

PostgreSQL tests verify all-member exhaustion, typed parking, automatic
re-observation, changed-profile reload, competing wakeups, repeated outages, and
non-park exhaustion. Client tests verify the typed network-unavailable reason.

# Network pool recovery design

This unbuilt design extends
[credential availability](../spec/credential-availability.md).

## Goal

Preserve a park pool's turn through exhausted provider-internal retries and
resume it when existing recovery observations establish availability.

## Design

Each member that exhausts provider-internal retries is excluded from the turn.
When every member is excluded, the turn retains its input, frontier, target, and
policy in a durable credential wait with cause `network_unavailable`.

The existing Codex capacity re-observation loop probes network-unavailable
members as well as quota-limited members. A successful current observation or a
successful changed-profile reload releases the corresponding network exclusion
and wakes the existing wait. Failed observations preserve the wait. No new timer
or scheduler is introduced.

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

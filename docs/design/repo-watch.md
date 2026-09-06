# Repository watch design

None of this is built; it extends [repository watch](../spec/repo-watch.md).
Landed material is removed as each capability lands, and the document is deleted
when no planned capability remains.

## Goal

Two capabilities are committed. Dispatched sessions record repository watch as
their cause and actor, with durable provenance that resolves the dispatch they
came from. The poll cache survives a daemon restart, so the first complete poll
after a restart sends conditional requests instead of one complete unconditional
fetch.

## Design

Provenance: session creation and input submission accept a repository-watch
creation cause and actor identity. A durable provenance record is linked to
`RepoWatchDispatchId` and names the dispatch, session, context, and input
identities the dispatch transaction records.

Poll-cache persistence: each bounded canonical resource or page key is stored
with its HTTP validator and a typed, minimal accepted snapshot sufficient to
reconstruct that resource's normalized contribution and the identities nested
fetches need. The store is transport state beside the cursor, not part of the
event or rule surface.

## Compatibility constraints

The dispatch transaction records dispatch, session, context, and input
identities; provenance references those identities and never recreates or
reinterprets them. A composed repository-watch worker cannot dispatch until this
attribution is available.

The persisted cache never holds raw provider JSON, credential values, or
reactions from actors outside the configured signal-reviewer set. Rules and
durable events cannot inspect it. The present cursor persists no validator and
no accepted transport snapshot. Polling begins only when a repository-watch
worker is composed.

## Acceptance criteria

A dispatched session's creation cause and actor identify repository watch, and
its provenance record resolves the same dispatch, session, context, and input
identities the dispatch transaction recorded, with no second copy of any of
them.

After a daemon restart, the first complete poll sends a conditional request for
every resource whose validator was persisted, and no persisted snapshot contains
raw provider JSON, a credential value, or a reaction from an actor outside the
signal-reviewer set.

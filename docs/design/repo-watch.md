# Repository watch design

None of this is built; it extends [repository watch](../spec/repo-watch.md).
Landed material is removed as each capability lands, and the document is deleted
when no planned capability remains.

## Goal

Four capabilities are committed. Dispatched sessions record repository watch as
their cause and actor, with durable provenance that resolves the dispatch they
came from. The poll cache survives a daemon restart with an unchanged reviewer
set, so the first complete poll sends conditional requests instead of one
complete unconditional fetch. The module runtime composes and reloads the
webhook listener and repository polling tasks.

## Design

On reload, the repository-watch runtime starts a polling task for each added
repository, stops each removed repository's task, and replaces a repository's
task when its poll interval, polling credential file, or
`repository_watch.signal_reviewers` changes. A signal-reviewer change stops and
joins the old pollers, then invalidates persisted poll validators and accepted
snapshots before starting replacement pollers.

Adding `[repository_watch.webhook]` on reload composes the listener; removing it
stops the listener.

Webhook listener: the module runtime composes the listener; a reload that keeps
the bind address swaps the running listener's path and hook map atomically. Only
an address change binds a replacement before retiring the running listener; a
bind failure keeps the running listener and fails the reload. Deliveries in
flight are retried.

Provenance: session creation accepts a repository-watch creation cause and
module actor identity. A durable provenance record is linked to
`RepoWatchDispatchId` and names the dispatch and session identities recorded
with the held `create_session` command.

Poll-cache persistence: each bounded canonical resource or page key is stored
with its HTTP validator and a typed, minimal accepted snapshot sufficient to
reconstruct that resource's normalized contribution and the identities nested
fetches need. The store is transport state beside the cursor, not part of the
event or rule surface. The cache persists the reviewer set used to build its
snapshots. Before composing startup pollers, the runtime compares that set with
configured `repository_watch.signal_reviewers` and invalidates both validators
and accepted snapshots when they differ.

## Compatibility constraints

The persisted cache never holds raw provider JSON, credential values, or
reactions from actors outside the configured signal-reviewer set. Rules and
durable events cannot inspect it. The present cursor persists no validator and
no accepted transport snapshot. Polling begins only when a repository-watch
worker is composed.

## Acceptance criteria

A dispatched session's creation cause and actor identify repository watch, and
its provenance record resolves the same dispatch and session identities as the
held `create_session` command, with no second copy of either identity.

After a daemon restart with an unchanged reviewer set, the first complete poll
sends a conditional request for every resource whose validator was persisted,
and no persisted snapshot contains raw provider JSON, a credential value, or a
reaction from an actor outside the signal-reviewer set. A reviewer-set change
while stopped invalidates validators and snapshots before the first poll, which
fetches those resources unconditionally under the new set.

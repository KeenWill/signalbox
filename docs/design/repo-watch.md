# Repository watch design

None of this is built; it extends [repository watch](../spec/repo-watch.md).
Landed material is removed as each capability lands, and the document is deleted
when no planned capability remains.

## Goal

Dispatched sessions record repository watch as their cause and actor, with
durable provenance that resolves the dispatch they came from.

## Design

Provenance: session creation accepts a repository-watch creation cause and
module actor identity. A durable provenance record is linked to
`RepoWatchDispatchId` and names the dispatch and session identities recorded
with the held `create_session` command.

## Acceptance criteria

A dispatched session's creation cause and actor identify repository watch, and
its provenance record resolves the same dispatch and session identities as the
held `create_session` command, with no second copy of either identity.

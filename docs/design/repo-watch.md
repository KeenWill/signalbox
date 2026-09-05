# Repository watch design

None of this is built; it extends [repository watch](../spec/repo-watch.md).
Landed material is removed as each capability lands, and the document is deleted
when no planned capability remains.

## Goal

Three capabilities are committed. Dispatched sessions record repository watch as
their cause and actor, with durable provenance that resolves the dispatch they
came from. The structured-rule dispatch surface converges onto the program
substrate, each rule becoming a subscription whose action is a built-in dispatch
program. The poll cache survives a daemon restart, so the first complete poll
after a restart sends conditional requests instead of one complete unconditional
fetch.

## Design

Provenance: session creation and input submission accept a repository-watch
creation cause and actor identity. A durable provenance record is linked to
`RepoWatchDispatchId` and names the dispatch, session, context, and input
identities the dispatch transaction already records.

Substrate cutover: each configured rule is replaced by a subscription over the
repository-watch event vocabulary whose action is a built-in dispatch program.
The cutover commits at one event frontier in one transaction, after every
old-rule event through that frontier has a terminal evaluation, and transfers
each occupied singleton batch and cooldown boundary to substrate-owned state, so
a boundary event is neither omitted nor dispatched twice. Subscription identity,
delivery, continuation cursor inheritance, and cancellation follow
[program substrate](../spec/program-substrate.md). A rule may run shadowed
beside its subscription while the replacement is validated.

Poll-cache persistence: each bounded canonical resource or page key is stored
with its HTTP validator and a typed, minimal accepted snapshot sufficient to
reconstruct that resource's normalized contribution and the identities nested
fetches need. The store is transport state beside the cursor, not part of the
event or rule surface.

## Compatibility constraints

The dispatch transaction keeps recording dispatch, session, context, and input
identities; provenance must reference those identities, never recreate or
reinterpret them. Until provenance exists, dispatch uses the user-initiated
creation and input interfaces, and a reader of session ancestry must not take
that attribution as user action.

Shadowing is validation only and never owns delivery; a shadowed subscription
must not become a second producer of dispatches, singleton batches, or audit
rows. The rule surface grows only in ways a subscription can express, so the
cutover need not reproduce a rule feature the substrate lacks.

The persisted cache never holds raw provider JSON, credential values, or
reactions from actors outside the configured signal-reviewer set. Rules and
durable events cannot inspect it. The present cursor persists no validator and
no accepted transport snapshot; the warm-restart schedule keeps the full
re-fetch on the configured cadence until this store exists.

## Acceptance criteria

A dispatched session's creation cause and actor identify repository watch, and
its provenance record resolves the same dispatch, session, context, and input
identities the dispatch transaction recorded, with no second copy of any of
them.

Every configured rule has a subscription with a built-in dispatch program, and
the change recreated no session, released no occupied singleton, and altered no
append-only audit identity. A shadowed subscription dispatched nothing while it
was shadowed.

After a daemon restart, the first complete poll sends a conditional request for
every resource whose validator was persisted, and no persisted snapshot contains
raw provider JSON, a credential value, or a reaction from an actor outside the
signal-reviewer set.

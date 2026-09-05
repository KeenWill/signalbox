# Persistence protocol

The persistence protocol is how the daemon keeps its state in Postgres: the
schema and its migrations, durable command storage, row locking, reconstitution
of stored rows into domain values, and the transactional outbox.

## Overview

`crates/persistence` holds the Postgres representation. It uses SQLx on Tokio:
the Postgres driver, one `PgPool`, and an embedded migrator. The production
connection parser forces certificate and hostname verification regardless of a
weaker `sslmode` in the database URL; only the local test path disables TLS.
Domain types live in `crates/domain` and know nothing of the database; each
persistence module decodes its own rows and hands checked input to the domain
for validation. What the rows mean is owned elsewhere: session semantics by
[sessions-and-transcript](sessions-and-transcript.md), turn and attempt
lifecycle by [turn-lifecycle-and-scheduling](turn-lifecycle-and-scheduling.md),
identity kinds and the command claim protocol by
[identity-and-commands](identity-and-commands.md), runtime wiring by
[runtime-substrate](runtime-substrate.md), the blob catalog by
[blob-storage](blob-storage.md), repository-watch storage by
[repo-watch](repo-watch.md), and the credential-availability machine by
[credential-availability](credential-availability.md). This page states how
those facts are stored, locked, published, and read back.

The schema is defined by forward-only versioned SQL files in
`crates/persistence/migrations/`, embedded as the static `MIGRATOR` and applied
by one `migrate` call. Fifteen files form the baseline: one schema split by
domain, applied only as a whole in filename order, with
`HUB_FENCE_MIGRATION_VERSION` naming the last of them. Every later file is a
forward migration on top of that baseline. SQLx records each applied file and
its checksum in `_sqlx_migrations`, and `.gitattributes` pins migration files to
LF so an embedded file's checksum matches the recorded one on every checkout.

The schema is normalized and purpose-specific: mutable current-state rows
guarded by constraints and triggers, and append-only facts that triggers protect
from update and delete, and in the guarded table families from truncate as well.
The core file holds three singletons: the hub-fence generation that fences the
connection pool, the outbox sequence allocator, and the outbox delivery cursor.

One append-only, user-global `durable_command` registry claims every command
identifier across all kinds and sessions, and each command kind has one typed
record table keyed by that identifier. The claim protocol, replay equality, and
conflicting reuse are owned by
[identity-and-commands](identity-and-commands.md); this page owns their storage.

`crates/persistence/src/lock_inventory.rs` records the row locks it names and
documents the lock order stated under Contracts. Row locks issued inline
elsewhere in Rust are stated at the statement that takes them, and locks taken
inside triggers are stated in the migrations that define them.

A program capability answer is appended to the program journal inside the
transaction that commits its consequence. The append locks that run's journal
sequence FOR UPDATE, inserts the frame, and advances the sequence, so the
consequence and the answer commit together.

Reconstitution turns rows back into domain values and returns one complete value
or a typed corruption error. Failures that reach an operator are classified in
`crates/application` as infrastructure, fail-closed corruption, identity
collision, or a caller bug, and an infrastructure failure carries a flag saying
whether it happened at the commit boundary. The automatic-reconciliation ledger
records the recovery of turns whose outcome is unknown; two singleton cursors
page its discovery and supersession passes.

Delegation storage holds child sessions, their relationships to a parent, waits,
messages, and results, with a delivery sequence per recipient. Its semantics are
owned by [sessions-and-transcript](sessions-and-transcript.md).

The transactional outbox is the only path from a committed transition to a
client-visible event. Two header tables, one for core kinds and one for
delegation kinds, feed one delivery sequence, and each event kind has a typed
record table. `OutboxDispatcher` is the single consumer; it hands one event at a
time to a synchronous consumer.

## Design decisions

One driver, pool, and migration stack, and no ORM: the module boundary, not the
driver, enforces the split between stored records and domain values, and one
stack keeps the dependency surface small.

Columns for unsigned 64-bit domain ordinals avoid `bigint` because it is signed
and silently narrows valid ordinals above `i64::MAX`; `numeric(20, 0)` keeps the
full range and its ordering, and a bounded ordinal uses `bigint`, `integer`, or
`smallint`.

Migrations are checksummed forward-only files so every schema change is a
reviewed, immutable artifact and no deployed database's history is silently
edited.

When a migration recorded by a rehearsal installation but not yet on `main`
fails form validation, the file is corrected before it reaches `main` and the
rehearsal ledger row is corrected as a documented step at its next deployment.
Why: freezing a file that fails validation would freeze a defect into every
future installation.

A collapse to a regenerated baseline is proved schema-equivalent first and cuts
the deployed database over by replacing the `_sqlx_migrations` rows in one
transaction; it changes bookkeeping, never schema or data. A collapse keeps a
dump of the prior ledger, so a cutover whose verification fails is undone by
restoring the previous `_sqlx_migrations` rows and redeploying the previous
binary.

Every planned schema improvement lands as an ordinary forward migration in its
own campaign, never inside a collapse.

While there is exactly one deployment and no release, the collapse repeats
whenever forward migrations accumulate on the baseline.

Dogfood session and conversation data survive every collapse. Repository-watch
data is disposable, which matters only for a repository-watch campaign that
drops those tables by forward migration; a collapse authorizes no drop.

Every function reachable from a table constraint or index expression pins its
search path, because `pg_restore` replays a backup under an empty search path
and evaluates check constraints while copying data, so an unpinned body fails
only during restore. Restorability is part of the schema's contract, because a
backup that cannot restore fails silently until recovery.

A stack holding a reserved prefix block does not renumber its migrations after a
base merges while the reserved prefix still exceeds the highest prefix on
`main`. A migration merged into a stack branch is immutable to the branches
stacked on it, and only the pull request that adds a migration may still edit
that file.

Serialization of concurrent migration runs is SQLx behavior, relied on and not
demonstrated in this repository.

There is no general-purpose event store: the guarded row is the durable
statement of record, and current state is not rebuilt by replaying events.
Session plans and commissioned goals are the two exceptions, each with a
session-local append-only event sequence folded into current state. Why:
database-level invariants stay declarative over current-state rows, while plan
and goal history is retained product evidence rather than an implementation log.

Immutable fact tables reject update and delete through triggers rather than by
convention, and the guarded table families reject truncate as well, because
restart trusts durable rows as evidence.

The session system prompt joins its selection key through a generated SHA-256
digest, because megabyte text cannot be a btree key, and an absent prompt is
stored as the empty digest rather than null so the foreign key still checks.

Durable command and outbox records carry a kind-scoped storage version, and a
change an older reader could misread advances it, so an older reader rejects the
record instead of projecting it wrongly. Typed event families such as goal,
plan, and delegation events carry a closed kind discriminator and no version.

Some rules are enforced twice, as typed domain transitions and as database
constraints, because a row set that passes SQL checks can still fail domain
correlation.

No template catalog or mutable template object exists in Postgres; a template
reaches the schema only as provenance recorded on the rows that used it.

Frontier lineage is either absent or checked imported-frontier ancestry; native
fork ancestry is not admitted.

Each command kind stores its caller-supplied fields in one typed,
check-constrained record family, a parent row and its typed satellites, ordered
content parts and set or map satellites such as metadata tags and attributes,
rather than a serialized payload column, because a universal serializer would
become a second semantic authority.

Duplicate concurrent submission of a command is a database conflict on the
registry, not an application race, and the loser rereads the winner.

Compaction and review orchestration are the two built commands whose claim and
settlement are separate transactions. Compaction commits its registry row,
pending typed command, and prepared call before provider work, and a later
session-locked transaction settles it exactly once. Preparation locks the
session's scheduler row and then the current-defaults pointer FOR UPDATE through
commit, and returns a defaults-changed rejection when the request's epoch is no
longer current. Review orchestration commits its registry row and immutable
intent before returning its guard, and the guard's later transaction replaces
that intent with the receipt.

The row locks the lock inventory names are issued from that one reviewed file,
so their order is auditable instead of scattered through query strings.

Creating a session from an imported frontier takes no explicit row lock, because
the selected imported aggregate is immutable and append-only.

Approval-judge preparation takes the scheduler row before its insert, whose
trigger locks the request row and then the active turn-lifecycle row.
Approval-judge completion takes the session row FOR NO KEY UPDATE before the
scheduler row so a goal-closing transition and the completion recheck exclude
each other. Every goal transition takes that same session lock before it reads
the goal's event stream and before any scheduler lock.

Input submitted to a delegated child locks both endpoint session rows FOR NO KEY
UPDATE in identity order before the child's scheduler row, because processing
the input can terminalize the delegated turn.

Runner-loss propagation may be interrupted at any session: a crash resumes at
the first uncommitted session, and every placement not yet projected is already
lost through the epoch fence.

A recovery that spent its attempt budget is parked only while its turn still
holds the matching recovery wait; otherwise it is left for supersession, because
a park raises an operator alert that cannot be retracted.

Authority comes only from complete validated projections, because the dangerous
corruption cases are rows that look valid alone while their cross-record
correlations do not hold.

A pending steering row is an accepted delivery obligation, so every recovery
branch accounts for it rather than blocking startup or stranding it.

Turn activation and recovery surface a commit-boundary failure as infrastructure
with the ambiguity flag set, because they mint fresh identities instead of
claiming a command identifier, and replay cannot resolve them.

Operator classification is adopted per repository: the command repositories that
implement the classifier map their failures into the shared taxonomy, and the
rest distinguish corruption from infrastructure in their error types alone.

A stopped delegated child keeps its physical execution evidence; eligibility
excludes it through its logical terminal proof, and a late provider response is
discarded rather than stored.

A guarded transition that changes no durable state appends no event, so where
the transition has a producer, state without its event, or an event without its
state, is unrepresentable.

A claimed session-lifecycle command appends its command-settled receipt in the
transaction that records its applied or rejected result. The command-settled
record authenticates its header without the session column, because that column
is null for a receipt with no session and a null key member would disable the
check.

A later runner fact adds a state and its columns to the one
runner-state-transition record kind rather than a second event kind, so an
existing follower needs no new kind.

The sequence allocator is a locked row held until commit, not a sequence object,
so committed sequences are contiguous and commit-ordered and a delivered prefix
never skips a lower in-flight sequence.

A goal-owned turn appends the same correlated input-accepted event as a
submitted input, and dispatch authenticates its goal-turn provenance instead of
requiring a synthetic submit command.

Dispatch validates a runner transition against the placement revision it names
because delivery is one ordered cursor, and a check demanding current state
would let a later transition block that event and every event after it.

Database-role separation is a deployment choice; migration invocation is wired
in `apps/signalboxd`, not in the crate.

## Boundary contracts

No database transaction stays open during I/O with a provider, a credential
source, the blob store, or a runner. The daemon reads what it needs, commits,
does the I/O, then opens a new transaction to record the result.

Any writer of turn-lifecycle rows first takes the session's scheduler row with
FOR UPDATE. No production path takes FOR UPDATE on the session row itself,
because that deadlocks against foreign-key checks. Within a session the order is
the session row, then the session-lifecycle satellite row, then the scheduler
row. Any path that records a delegated child's result locks the parent and child
session rows in ascending session id order before it locks the child's scheduler
row. A transaction that takes more than one runner lock takes them in one fixed
order: scheduler, enrollment or request heads, connection and loss heads,
registration, placement, grant, lease, failure evidence; within one class it
takes those rows in ascending order of the locked row's identity.

Unsigned 64-bit ordinals are stored as numeric(20,0). What kind of thing an id
names is known from its table and column, never from the UUID's bytes. No code
derives acceptance order, queue order, lifecycle precedence, ancestry,
ownership, or authority from a UUID; listing rows by identifier for display or
paging is not such a derivation.

A new migration's version prefix is greater than every prefix on `main` and on
the stack's own ancestor branches, and a sibling stack's reserved prefix block
stays valid while it still exceeds `main`. Once a migration is recorded in any
deployed database, or once its pull request merges to main, it is immutable: fix
it with a new forward migration, never by editing, replacing, or renumbering the
file. The one exception is a full collapse to a regenerated baseline, allowed
only while there is exactly one deployment and no release.

A durable row that does not decode produces a typed corruption error. The row is
never normalized into a nearby valid value, never repaired or dropped on load,
and authorizes nothing. Reconstitution returns a complete session or an error,
never a default or a partial session.

A committed, client-visible transition becomes an event only through the outbox
append on the same connection, inside the same transaction. No separate step
publishes after the commit. Delivery is ordered and at-least-once, and consumers
deduplicate by cursor. A runner transition event is validated against the
placement revision it names, never against the session's current placement.

Domain types carry no SQLx or serialization traits. Each adapter module decodes
its own rows through explicit fallible functions and assembles a checked input;
the domain validates that input and returns one canonical value or a typed
failure.

A durable-command load returns nothing only for a command identifier it has
never seen; a claimed row that cannot be reconstructed is corruption, never an
unclaimed identifier. Load paths never panic on durable data. Startup recovery
operates only on projections that reconstituted successfully. A guarded write
that matches zero rows is benign staleness to reload and rederive, unless the
transaction's own premises made a match mandatory, in which case it is
corruption.

A load that must be complete proves it: the scheduling load counts queued input
origins against lifecycle rows and fails on mismatch rather than trusting
whichever rows a filter returned.

A commit failure is ambiguous only for SQLSTATE 08007 or 40003, or for a
non-database error while awaiting the commit response; every other rejection is
definite. When the ambiguity flag is set a caller that holds a command
identifier rereads durable state instead of assuming either outcome. A caller
that minted a fresh identity surfaces the ambiguity instead.

At startup the daemon takes the singleton guard and applies the migrations
through the hub-fence migration on its bootstrap connection, so a fresh database
holds the fence singleton. It then fences the prior pool generation, applies the
remaining migrations through the fenced pool, runs the startup scan, and starts
the runtime. Fencing locks the singleton, waits on the prior generation's
exclusive advisory lock until its pooled sessions end, and advances the row. It
acquires the matching session-level lock before commit and holds it through
construction of the new pool.

The bottom pull request of a stack that adds migrations declares a reserved
prefix block in its description, and sibling stacks pick disjoint blocks.

A regenerated baseline omits `_sqlx_migrations`, which the migrator creates
itself. It carries the seed rows a schema-only dump discards: the
outbox-sequence, outbox-delivery, and hub-fence singletons and both
automatic-reconciliation cursors. It carries no schema qualifications, and
fresh-apply equivalence is proved on the default schema and again with the
role's `search_path` selecting a nondefault schema. The old files are deleted in
the same commit that adds it, because a tree carrying both chains would collide
on every create.

Decode paths for stored row shapes, such as storage-version thresholds and
legacy readers, stay until an authorized migration rewrites every row they
decode. A collapse changes bookkeeping, not rows, and retires none of them.

Closed variant sets are text discriminators under check constraints, with
payload columns present exactly when the discriminator requires them. A fact
spanning several tables uses deferrable, initially deferred foreign keys and
constraint triggers, so its rows may be inserted in any order the triggers
permit while every commit boundary sees the complete shape. A multipart receipt
inserts its satellites first, because a satellite insert is rejected once the
parent row has sealed the receipt.

An applied `SubmitInput` receipt references its accepted-input or interruption
effect, and the schema rejects a receipt with no matching effect.

An applied `OverrideDeniedToolRequest` receipt retains its user-override effect
row; triggers and a one-shot consumption column admit at most one consuming
approval.

Accepted user content is stored only in the ordered part satellites of the
command and of the accepted input; neither parent row has a content column.

Once a session has a recorded metadata write, its metadata root is never
deleted, so root absence is the initial snapshot only before that first write,
and the root's mutable fields cannot move to another session.

A null delegate-denial reason means one thing everywhere: the rationale
sanitized to nothing.

A configuration root written since the resolved-settings record exists carries a
correlated resolved-settings row; transcript reconstruction treats its absence
as corruption, never as a legacy null.

A frontier is stored as a header with its immutable total member count and an
optional same-session prefix frontier, plus a delta holding only the members
beyond that prefix.

Pending steering is durable current state: an accepted-input row with the
pending-steering disposition records a next-safe-point delivery and names the
active turn it expects. Lifecycle transitions preserve that command's immutable
receipt, so equal replay after any transition returns the original result.

The placement snapshot writer refuses loss, replacement, and abandonment,
because those transitions require authority outside the placement aggregate.
Each placement records the connection-loss epoch observed when it selected its
enrollment, derived under scheduler, enrollment, and connection authority; a
caller cannot supply it. Placement and enrollment insertion, loss propagation,
and loss completion take one transaction-scoped runner-identity advisory lock,
placement taking it between the scheduler and enrollment locks.

Runner loss advances a durable loss epoch in one short transaction that locks
only the current connection-loss head, never holding that global row while
waiting for a session lock. Propagation then pages, under repeatable read,
through the current placements whose baselines precede the authenticated loss,
in session-identity order after a durable cursor. Each page is a fixed size the
code pins.

The triggers that check a runner-recovery wait against its loss evidence lock
the session's scheduler row first, so a concurrent advance cannot validate the
wait against stale evidence. Restart reconstitutes that wait from correlated
placement, attempt, and lease facts rather than from the stored discriminator.
Stopping the wait retires retryable authority before releasing the active slot,
and the claimed-retry writer rechecks under the same scheduler lock that the
source attempt is still in flight.

The first model-call insertion of a turn takes the transaction-scoped
model-activity advisory lock keyed by session; inactivity parking takes the
pull-request target lock and then that same model-activity lock. A
pursuit-starting goal on a pull-request target takes that target lock before it
checks for a competing live session and claims the command.

Every automatic-reconciliation claim, application, and failure-record
transaction installs a local `lock_timeout` before it reads or writes anything,
floored under the caller's configured deadline.

A delegated terminal observation locks both endpoint session rows FOR NO KEY
UPDATE, then both endpoint scheduler rows, in ascending session-identity order,
and only then the delegation row. A delegated await locks the issuing session,
then its scheduler, then the relationship. A peer message locks both endpoint
session rows FOR NO KEY UPDATE in ascending session-identity order, then both
scheduler rows, and only then the relationship row. A descendant-scoped stop or
interrupt locks the complete reachable session frontier in ascending
session-identity order before the ordinary root or scheduler locks and before it
allocates any outbox event, then the relationships in spawning-request order.

A durable user-command claim precedes the runner lock subsequence.

Updating a session's placement locks the current-placement head FOR UPDATE
before checking the expected version, and holds it through the successor event's
insertion and the head's advance.

Replacing session defaults locks the current-defaults pointer row FOR UPDATE
before loading, and the compare-and-set update on that locked row is the
applying check. `SubmitInput` takes that same pointer FOR UPDATE after the
session's scheduler row and holds it through origin insertion, so a replacement
cannot commit between the frozen epoch it reads and the origin it writes. The
pointer has no guard trigger, so beyond its range check and deferred foreign key
this is its only discipline.

Replacing session metadata locks the target session row FOR NO KEY UPDATE so
concurrent writers serialize, samples the Postgres statement time once, and
writes that exact value to both the current root and the applied receipt.

A session-plan append locks the session row FOR NO KEY UPDATE before allocating
the ordinal, holds it through the insert, and then locks the authorizing tool
attempt FOR SHARE.

Graceful shutdown closes the pool and waits for every outstanding checkout
before closing the guard session; a shutdown that is omitted or cancelled
retains the guard session until process exit.

Each discovery and supersession lap fixes its highest eligible identity before
paging and wraps at that bound, so a row that becomes eligible behind the cursor
is reached next lap. No reconciliation path acquires either cursor row while
holding a recovery-row lock. Discovery locks each turn it enrols at the strength
the accepting interrupt's terminalization takes, so the two contend instead of
committing a live recovery beside a terminalized turn. The reconciled recovery
row is the typed authority the deferred final-state assertion admits when no
applied interrupt exists; it asserts nothing about the physical outcome. A lost
daemon leaves an attempting recovery durable, and a later claim pass classifies
it as an infrastructure failure after its deadline before retrying.

Delegated admission checks request and child uniqueness without a fixed
active-child limit, and commits the child session, its scheduler and defaults
rows, its first task work, the relationship, and the spawn event in one
transaction. The first task work is one delegated-task origin row, its semantic
entry, and its first queued turn; no accepted-input row is inserted. That origin
stores the exact requested and frozen model configuration inherited from the
parent turn, and an equal effective model never authorizes reconstructing it as
a session default.

A scheduling reread that retains delegation-origin history supplies each
referenced delegated turn's defaults version, selected direct model, and
lifecycle classification; a turn identity alone is never sufficient authority.

Continue-running and already-terminal are real delegation event kinds, not the
absence of an evaluation row. Equal wait and message replay authenticate the
exact terminal attempt, update satellite, and outbox header rather than trusting
the stored row alone. Delivery satellites bind messages and results to their
exact semantic entries; no transcript query supplies result content.

Every pending message and background result receives one positive recipient-wide
delivery sequence, unique and gap-free per recipient across both kinds, under
the recipient session lock. A foreground result stays ordered by its awaiting
request and consumes no sequence. An accepted background wait reserves one
future position until its child result exists, and later message and wait
admissions preserve every outstanding reservation. A foreground result counts as
one tool result for outbox decoding and compaction evidence; a background result
counts as neither.

Parent-and-descendants termination commits the command and every evaluated edge
together, so a crash leaves either all prior state or the complete evaluation.
An already-terminal edge receives its typed event and traversal continues
through that child's relationships, so a terminal intermediate session cannot
hide live descendants.

Every result and message commit appends exactly one distinct recipient-scoped
delegation wake in the same transaction, even when the recipient is already
active. A foreground wait registered after its child result committed appends
another result wake keyed by the awaiting request in the wait transaction. The
wake is best effort after restart and never stands in for the client-visible
update.

Both outbox header tables share one allocator and one delivery prefix, so their
committed events form one gap-free global sequence. Extension is version-gated:
an addition every existing decoder can ignore leaves the kind's storage version
alone, while a new closed state or required column advances it, and a decoder
that predates the advance rejects the record as unsupported.

Dispatch locks the delivery singleton FOR UPDATE, reads exactly the next
sequence and its typed record, and advances the singleton only when the
synchronous consumer accepts, in the same transaction. A transaction that
appends an event never advances the delivery cursor and one that advances the
cursor never appends; the schema rejects both orders. An absent header for a
sequence the allocator has already allocated fails the dispatch instead of
reporting an idle queue, and a delivery cursor or any committed header beyond
the allocator fails it too. A consumer retry or exit before the commit request
leaves the prefix unchanged for redelivery, and a lost commit response is
resolved by the next locked cursor read.

Dispatch validates each record against durable state: an activation against the
turn's attempt, a call transition against monotonic call state, and a terminal
record against its turn and frontier. A terminal record naming a transcript
entry must name that frontier's last member, and a reconciliation-required
record must name the turn's own ambiguous call or tool attempt. A
context-compaction event whose compaction, producing call, summary, and result
frontier do not correlate fails the dispatch. A session-terminal record fails
the dispatch unless the session's lifecycle row matches its outcome, cause, end
timestamp, stop stickiness, superseder, and actor. A tool-batch transition fails
the dispatch unless its round, frontier, and attempt correlate: a proposed
frontier is the round boundary, a projected frontier contains every result of
the round, and a recovery attempt belongs to a request the named call produced.
An input-accepted record fails the dispatch unless its accepted input, queued
origin, and lifecycle row correlate and an applied submit command or a goal-turn
row authored it. A session-created record fails the dispatch unless its creation
cause, dispatch reference, spawning request, and initial ownership match the
session and its first ownership journal entry. A settings event fails the
dispatch unless a session change matches its applied replacement command and
both defaults epochs, or a turn resolution matches its accepted origin and its
frozen selection, defaults, overlay, and adjustments.

An applied submit-input that creates a turn origin appends an input-accepted
event; pending steering appends nothing until terminal reclassification mints
its successor turn and appends the correlated event. Every turn origin produced
from an accepted input appends its resolved-settings event before its
input-accepted event; a delegated-task origin appends neither. Turn activation
appends the turn's activation event in the activating transaction. Binding an
already-accepted turn to a goal generation appends nothing. Every durable goal
event appends a goal-changed event in the transaction that stores it. Every turn
that reaches a terminal state appends its typed turn-terminal event in the
terminalizing transaction. A stop or supersede that makes a queued goal turn
ineligible appends a retired turn-terminal event in the same transaction, and
supersede appends that retirement before the replacement's input-accepted event.
Adopting or releasing a session appends an ownership-changed event in the
transaction that journals it. A claimed tool-decision command appends its
injection-settled receipt in the decision transaction, not delivered when the
request was already resolved. Completing a compaction appends the
context-compacted event atomically with the completed call, summary, result
frontier, and applied command.

The transaction that terminalizes a session's last live turn settles the
session's pending closure at commit through a deferred constraint trigger, which
appends the session-terminal event, so the causal turn's own event precedes it.

Lifecycle-disposition updates admit only cascade evaluations caused by a parent
turn command or parent goal command; a child-origin terminal event is delivered
as a child result instead. Spawn, waiting, lifecycle, and result updates go only
to the parent stream and message updates only to the recipient, while a
cascade-caused disposition is emitted on both.

Every durable runner state change appends one runner-state-transition event per
affected session in the transaction that commits it.

Every pool-selected model call stores, beside its credential reference, an
insert-only snapshot of the pool it was authorized under: the pool name, its
ordered members, and its trigger actions. The observation commit joins through
the call to that exact snapshot before applying a trigger action, so a racing
credential-history update cannot substitute a newer policy. A chain-exclusion
row carries the correlation of its qualifying observation rather than its own
generation, as an insert-only turn-local fact. Exhaustion evidence is one
turn-correlated failure header naming the pool and its cause. An availability
successor is a predecessor-linked attempt with its own closed origin, distinct
from the tool-loop continuation origin, and stores its predecessor call,
qualifying cause, and non-acceptance evidence atomically. What these rows mean
is owned by [credential-availability](credential-availability.md).

## Planned

- Runner replacement and abandonment transactions:
  [persistence-protocol design](../design/persistence-protocol.md).
- Retiring an unacknowledged workspace release:
  [persistence-protocol design](../design/persistence-protocol.md).
- Runner operation-failure evidence stored before acknowledgement:
  [persistence-protocol design](../design/persistence-protocol.md).
- Runner placement in imported-create command records, for which storage version
  4 is reserved:
  [persistence-protocol design](../design/persistence-protocol.md).
- Instruction admitted-set storage and its locks:
  [persistence-protocol design](../design/persistence-protocol.md).
- Credential-pool state, capacity reservations, and availability-wait storage:
  [persistence-protocol design](../design/persistence-protocol.md).
- A producer for the session-state-changed outbox event:
  [persistence-protocol design](../design/persistence-protocol.md).
- Daemon-owned OAuth material storage:
  [persistence-protocol design](../design/persistence-protocol.md).

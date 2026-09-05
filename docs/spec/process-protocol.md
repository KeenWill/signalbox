# Process protocol

The process protocol is the wire boundary between a local client process and
`signalboxd`.

## Overview

`crates/process-protocol` defines protocol version 1 and a closed set of
request, message, event, and field shapes; that set is the whole wire surface
the daemon implements. The daemon side of the boundary is
`apps/signalboxd/src/process_runtime/`; the client side is the `signalbox`
terminal binary in `apps/client` and the native macOS client's
`SignalboxProcessClient` in `clients/native`, which opens the same socket.

The transport is one Unix domain stream socket. The daemon reads the socket path
from `SIGNALBOX_SOCKET_PATH`, and the terminal client reads the same variable
unless given an explicit path. Each frame is one UTF-8 JSON object followed by a
newline, at most 8 MiB including the newline; an empty line is a malformed
frame. Every frame carries the version as a JSON integer, a request identity,
and exactly one request or message object tagged by its `type` string; a field
the variant does not admit is rejected. Signalbox's own durable identifiers are
canonical UUID strings, while configured rule identities, provider review node
identities, and provider-owned external objects keep their own bounded-string
grammars. Indices, counts, and event cursors are canonical decimal strings that
preserve the full unsigned 64-bit range, while a listing request resumes from
the identity of the last row it returned. A request identity is a nonzero
decimal string; zero is reserved for an error the daemon cannot correlate to a
request. Every correlated server frame repeats the initiating request's identity
unchanged, and the client rejects one that does not.

A client sends requests and receives receipts, reads, or errors. A
durable-command mutation carries a client-supplied command identity under the
claim protocol in [identity-and-commands.md](identity-and-commands.md). A
delegation mutation admits no command identity and carries only its invoking
session, turn, and tool-request correlation. Submitted input is an ordered array
of parts, each a text part or an attachment reference carrying a blob digest and
its metadata. The request states how the input is delivered: started when the
session is idle, steered into the active turn, or queued behind it, with idle
start as the default. A session placement is pathless or names a directory path.
A model selection names a configured selection directly or by alias. Session
metadata is one object replaced whole; its title, tags, and attribute keys are
nonempty and contain no U+0000.

A conversation import whose source fits one frame is one single-shot request; a
larger source, and every blob upload, moves bytes in chunks through a begin,
append, commit sequence. Either path holds the one process-wide bulk-ingest
permit. A chunked operation has a five-minute inactivity deadline that resets
after each accepted append and a non-resetting twenty-four-hour deadline from
when its begin takes the permit; the first bounds a stalled client and the
second bounds one making indefinite minimal progress. A single-shot import has
the twenty-four-hour deadline from when it takes the permit and no inactivity
deadline. Blob storage is owned by [blob-storage.md](blob-storage.md) and the
import pipeline by [conversation-import.md](conversation-import.md).

A client observes a session by following it. A follow connection receives a
transcript snapshot and then the session's durable events above the snapshot's
cursor. The snapshot projects the session's turns with their states, the
terminal model calls with their usage, and the transcript entries in frontier
order; an imported entry identifies its source conversation and carries a closed
attestation of whether the source recorded the speaker. Model-call usage rows
follow their owning turn's acceptance position and then model-call identity, and
the client rejects any other order. A turn's projected state follows
[turn-lifecycle-and-scheduling.md](turn-lifecycle-and-scheduling.md); a failure
that credential selection produces is owned by
[credential-availability.md](credential-availability.md). The one ephemeral
message is the provider text delta, admitted only on a follow connection and
correlated to its session, turn, model call, and part. Delegation between
sessions has its own requests and events on this surface; the relationship and
cascade rules are owned by
[sessions-and-transcript.md](sessions-and-transcript.md).

Durable events reach followers through the dispatcher. It reads the
transactional outbox owned by [persistence-protocol.md](persistence-protocol.md)
one sequence at a time through the `process_protocol` consumer cursor and offers
each session event that has a wire projection to two process-local fan-outs: one
durable-only and one composite that also admits deltas; a sessionless receipt or
an event kind with no projection advances that cursor and reaches no follower. A
database-scoped advisory guard and a generation fence in
`crates/persistence/src/hub_fence.rs` enforce one active daemon process per
database, and therefore one dispatcher and its fan-outs. The guard is taken on a
dedicated connection before migrations run and held until shutdown.

## Design decisions

Version 1 is edited in place until the first durable deployment, a client that
cannot be rebuilt at will, such as an owner-operated remote daemon or installed
app; after that, every incompatible change allocates a permanent new version
number.

Every frame carries the version so captured traffic and errors are
self-describing without connection-global negotiation state. Selecting a session
never requires a feature-specific version gate, because every durable
representation is expressible at version 1.

The socket's immediate parent directory must be owner-private even when the
socket node itself is mode 0600, because not every Unix enforces socket-node
permissions. Every ancestor above that parent must be owned by root or the
effective user. A group- or other-writable ancestor is admitted only when it is
sticky and its child is owned by the effective user. A daemon holds an exclusive
lock beside the socket for its lifetime and pins the bound inode, and it unlinks
a socket path only after revalidating that pin, so a restart never removes a
live successor's socket. Socket filesystem access is the deployment boundary;
the daemon adds no application-level file-owner proof. The absence of
authentication is provisional: remote access needs an authenticated identity and
revocation design that does not exist, recorded in
[open-questions.md](../open-questions.md).

A denial on the wire requires a reason although the domain command admits its
absence, so every client-issued denial is explainable.

A transitional pending imported title survives every title filter, so the read
fails closed rather than silently omitting an unresolved row.

Conversation import carries no durable command identity because exact
format-and-source replay already resolves through the import digest.

No standalone cancellation command for an active turn exists; `stop_turn` is the
interrupt delivery on the wire.

The template listing exposes no template prompt text, model selection, approval
posture, or digest. The alias read exposes no provider credential,
provider-native model identifier, or mutable configuration operation. The
imported-entry projection carries no tool fields, results, thinking, media,
source-event payload, absence detail, or raw record, so the immutable aggregate
stays the authority. No delegation event embeds or links the child transcript.
Storage-version columns are not exposed as wire-version fields.

Pool construction requires a non-cloneable capability that borrows the live
fence session, so a copied generation value cannot construct work after the
guard is released. Importing or upgrading a database created before the
generation fence is unsupported.

## Boundary contracts

Errors, logs, and diagnostic evidence contain classes, counts, and canonical
identifiers. They never contain source bytes, host or credential paths, raw or
unsanitized provider payloads, SQL, or user content other than a bounded,
credential-redacted provider error body; a tool failure may name a bounded
workspace-relative path. Retained source content, such as an imported transcript
entry, is not diagnostic evidence. The guarantee covers protocol output and
diagnostic evidence; a local log recording a rejected workspace configuration
names the configured root.

The transport is local-machine and single-user; the protocol has no
authentication, no authorization exchange, and no remote transport.

Domain values, PostgreSQL records, and wire messages are distinct
representations. The daemon constructs domain and application values from a
request before any mutation and rejects an incompatible shape without
normalizing it. A wire snapshot is a presentation projection, not a domain
session, a storage record, or a provider prompt; a stored variant the projection
does not recognize fails closed.

Version admission is the one centralized wire gate: an unknown version produces
`unsupported_version` and the server closes the connection. The server may close
a connection after any error, and a client never reinterprets an unknown message
as a known one. An oversized outbound frame terminates only its connection;
every other encoding failure is fatal runtime evidence.

A connection processes one request at a time, and a follow request consumes its
connection until it closes. Inbound admission is bounded globally by an
active-connection ceiling, a bounded pre-admission read-ahead, and an aggregate
buffered-frame budget that reserves one slot for an active import. One
process-wide bulk-ingest permit admits at most one chunked or single-shot
conversation import or blob upload at a time. The daemon admits one review
mutation at a time and releases the inbound frame slot after acquiring that
permit and before application handling.

Every accepted non-review mutation, import transport request, or blob transport
request produces exactly one receipt message or an error, except an import or
blob request that crosses its non-resetting deadline and is closed without one.
A mutation whose commit outcome is unknown returns `commit_ambiguous`; an
infrastructure failure known to precede the commit returns `unavailable`.

The client, never the server, supplies a durable-command mutation's command
identity, so an equal retransmission reaches the replay boundary in
[identity-and-commands.md](identity-and-commands.md). An ambiguous submission is
retried with the same command identity, session, content, expected version, and
treatment; changing any of them is conflicting reuse. Durable command equality
is computed over the validated semantic request, never the frame: a review
command's equality key is its operation kind plus the SHA-256 of its validated
request, recorded beside its client-supplied identity, and tag order and
attribute order do not affect a metadata command's equality.

A replay answers from the durable record before anything current is consulted: a
replayed commission resolves before the live template catalog, a replayed
metadata receipt is the snapshot its original handling installed even after a
later command replaced the metadata, and a replayed delegation request returns
its stored receipt before current attempt state and creates no further effect.

A pre-command refusal claims no command identity and has no replay projection,
so a corrected request reuses the same identity; such refusals include a turn
not awaiting reconciliation, a tool request not in the named session, an unknown
imported conversation or position, and a defaults replacement rejected before
its repository command. The `reconcile_turn` precondition is skipped for a
command identity that already names durable intent and for an absent session,
because the durable boundary owns both answers. `commission_target_busy` is a
transient rejection naming the authoritative session, so the caller retries the
same command identity and payload.

The daemon applies the delivery the client selected and never guesses an
interrupt, steering, or queued treatment. Steer and queue name the exact active
turn the client observed; an idle slot or a changed turn is a typed rejection,
never a retarget. The acceptance receipt carries correlation and turn identity
without the parts; the durable event, the snapshot's queued turn, and the
transcript user entry carry the same parts array in order, with attachment
metadata and no blob bytes.

An applied interrupt binds its immediate-successor origin in the same
transaction. A stop can neither decide nor bypass a tool-approval wait; the
caller denies through `decide_tool_request` first, then stops.
`descendant_scope` is required on `stop_goal` and `stop_turn` and is durable
command intent, so reusing an identity with another scope is conflicting reuse.

The session named by `decide_tool_request` is a routing precondition and not
part of the canonical decision payload; the session named by
`override_denied_tool_request` is part of its canonical payload because the
recorded override is a session-scoped standing fact.

A one-segment root path is legal only under the `root_global_read` placement,
which records the explicit intent that the session gains global conversation
read. The client accepts a placement receipt only when the session and placement
echo its request and the version is exactly one greater than the version it
expected.

A commission binds every supplied template name by copy, so a name is never
mutable authority; its receipt names only the created session and its dispatch
record. The alias catalog read reports current deployment configuration, not
durable session state; an existing session keeps its frozen selection when the
catalog changes.

A metadata request that violates shape, uniqueness, or byte bounds is a
malformed frame, refused before application construction; a request that exceeds
a configured tag, attribute, required-tag, or page-size limit is
`invalid_request`, as is the fail-closed mapping case.

Every database-backed read is served from the authoritative tables, with no
materialized view, cache, or analytical artifact, and takes no row or table lock
an application mutation waits on. A single-statement read, such as session
defaults or a blob catalog entry, runs directly on a pooled connection, and a
read answered from deployment configuration opens no transaction. A read whose
answer must be coherent across statements is one repeatable-read, read-only
transaction. The transcript snapshot answers from one such snapshot and observes
the outbox cursor, the session's semantic frontier, and every turn in acceptance
order together. The conversation list answers from one and orders by
conversation identity value, a native session before an imported conversation of
equal value. The review-orchestration projection answers from one, while the
review-findings listing takes its run and its findings from separate
transactions. Every reported duration is clamped nonnegative and sampled against
the database transaction timestamp, not a client clock.

Every read that holds a pooled connection across more than one statement takes
one snapshot-reader admission; the single-statement defaults read takes none.
Every request states its admission class before dispatch, so no read verb
reaches the pool by omission. The reader budget leaves at least two pool
connections outside snapshot work.

The imported seed frontier is selected only when no persisted turn-start lineage
exists; a queued but unstarted first native turn does not hide it.

The transcript snapshot and the operator-status read stream their rows through
server-side cursors into a secure unnamed temporary file, commit the
transaction, and only then stream the completed file; the imported-conversation
and goal reads load the complete aggregate first, then spool it the same way. A
projection or spool failure before transmission returns `unavailable` and
exposes no partial snapshot. Every bounded sequence read is a start message, its
items, and an end message carrying the count. A session metadata page orders its
summaries by strictly increasing session identity, continuing after the
requested cursor. The terminal client spools each complete snapshot or page into
an owner-private anonymous temporary file. A client treats a snapshot or page as
authoritative only after the end message arrives and its counts, indices,
fragment sequence, session, and cursor validate. Snapshot deduplication uses the
complete semantic identity of source session and entry; a second occurrence of
that key fails the snapshot.

The imported-conversation read is `imported_conversation_start` naming the
inspected conversation, one `imported_conversation_entry` per normalized entry,
then `imported_conversation_end` repeating that name with the entry count. Each
entry carries its one-based imported position, imported entry identity,
source-speaker attestation, and content kind. An entry with exact attested text
also carries a bounded preview of that text and its truncation marker; every
other entry carries a null preview.

Tool entry arguments and content are JSON strings, never nested untyped JSON
values, and a client never infers the semantic arm by reparsing either string.
The projection resolves the domain's reference-only tool entries before crossing
the wire, so a client never needs private storage access. A physically ambiguous
tool attempt never becomes an execution result; it projects as `tool_closed`,
carrying the tool request identity and the closure content and omitting the
attempt identity. `operator_action_required` is false while automatic recovery
is scheduled or attempting and true only after the recovery budget in
[turn-lifecycle-and-scheduling.md](turn-lifecycle-and-scheduling.md) is
exhausted.

Each delegation request carries the invoking session, turn, and tool request
identity, which must reconstitute one matching logical request before any
mutation; reconstitution is not execution authority, and the daemon must also
prove an authorized executable attempt that is not awaiting approval, denied,
closed, or ended. A foreground await subscribes before registering its durable
wait and queries durable delivery before blocking, so a completion cannot be
lost; a disconnect or daemon shutdown abandons only the socket wait, never the
durable child wait. An `already_terminal` disposition requires the
relationship's pre-existing immutable child result and never creates or replaces
it. `delivery_sequence` is allocated under the recipient session lock and is
unique and gap-free per recipient across messages and background deliveries. The
internal `delegation_wake` outbox event is a scheduler signal, not a
session-follow update; a client observes the durable result instead. A stopped
or cancelled `child_lifecycle_disposition` caused by a parent cascade is emitted
on both the parent and child streams; every other typed update has one recipient
stream.

An import begin declares the format and the exact total byte count, and commit
requires the assembled count to equal the declared count before conversion.
`begin_blob_upload` live-verifies a recorded replica in the routed store before
it returns `blob_upload_already_present`. A blob chunk read names an exact range
of at most 4 MiB; an overflowing or out-of-range request is rejected rather than
truncated at the end of the blob. When no replica succeeds, `unavailable` takes
precedence over `blob_corrupt`, which takes precedence over `blob_missing`. The
terminal client answers `publication_ambiguous` and `commit_ambiguous` by
beginning the same digest, length, and bytes again rather than retrying commit
alone.

`DATABASE_URL` must name a direct or session-affine PostgreSQL endpoint;
transaction- and statement-pooled proxy modes are unsupported because the
singleton guard and generation fence use locks owned by one server session. A
successor daemon can acquire the singleton guard immediately but cannot pass the
exclusive prior-generation fence until every old pooled session is gone.

The durable-only and composite fan-outs each retain 64 update events; wire
followers use the composite one. Durable cursor advancement never waits for a
connected follower. A follower that overruns its fan-out receives
`resync_required` and reconnects for a fresh authoritative snapshot and its
durable cursor. Delivery order and deduplication by cursor are the outbox
contract in [persistence-protocol.md](persistence-protocol.md).

For a follow, the server subscribes to the fan-out before reading the snapshot,
sends the snapshot first, then sends the events above its cursor in order; every
client-visible transition committed before the snapshot is represented in it
even when it adds no semantic transcript entry. The server discards the deltas
queued in the fixed prefix recorded when the snapshot completed, so a terminal
reply is never followed by stale fragments.

A delta is a process-local presentation event with no outbox cursor: it is never
appended to the outbox, never advances the follow cursor, never enters the
transcript, and is never replayed from storage. The HTTP adapter in
[runtime-substrate.md](runtime-substrate.md) applies credential redaction before
text leaves the runtime; the bridge and daemon copy that text unchanged and add
no redaction of their own.

A `goal_turn_retired` event clears only the exact queued turn it names, and a
superseding transaction publishes it before the replacement `input_accepted`.

A client disconnect never cancels model or tool work. A side reread does not
advance the follow connection's observed cursor; only events consumed from the
subscribed connection do.

The terminal client reads submitted conversation input from standard input,
never from process arguments. When no command identity is given, it generates a
fresh one and prints it to standard error before any socket I/O; every
client-generated or server-discovered recovery value is printed before the
commit it belongs to can become ambiguous, each recovery set is printed all or
none, and the client never substitutes a new command identity for an ambiguous
attempt. Its ambiguity diagnostic never echoes standard-input content and never
synthesizes a shell command. It renders every C0 control code point, DEL, and
every C1 code point in process-derived text as a visible escape, preserving a
line feed only in flowing text and escaping it in a single-line field such as a
provider delta or a metadata title or tag. A single explicit raw-output option
is the only opt-in to unescaped text. A recorded review finding carries an
opaque caller-supplied file-path key.

## Planned

- Credential-exclusion administration, a listing read and a clear mutation over
  active exclusions: [design](../design/process-protocol.md).
- Configuration reload request: [design](../design/process-protocol.md).
- Program-run cancellation request and receipt:
  [design](../design/process-protocol.md).
- Runner creation, status, and recovery requests, and the status read's failure
  evidence: [design](../design/process-protocol.md).
- `spawn_session` creation of a delegated child:
  [design](../design/process-protocol.md).
- Cascade metadata on stop receipts: [design](../design/process-protocol.md).
- Typed projection of credential-pool exhaustion and of the
  credential-availability wait: [design](../design/process-protocol.md).

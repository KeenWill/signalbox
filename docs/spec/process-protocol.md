# Process protocol

This page specifies Signalbox process protocol version one and the terminal
client that consumes it, verified against the implementing stack through PR #177
(`agent/terminal-client`). It is the normative boundary between a local client
process and `signalbox-hubd`; domain values, PostgreSQL records, and wire
messages remain distinct representations.

Invariant law lives in [docs/invariants.md](../invariants.md), cited here by
tag. Durable update storage and the delivered-through cursor are owned by
[persistence-protocol](persistence-protocol.md).

## Transport and trust boundary

Version one uses one Unix domain stream socket. The hub requires its path in
`SIGNALBOX_SOCKET_PATH`; the terminal client uses its `--socket <path>` override
when present and otherwise requires that environment value. `signalbox-hubd`
binds the socket with owner-only `0600` permissions. The configured path must be
absolute and must end in an explicit filename component; a trailing separator,
`/.`, or `/..` is rejected rather than normalized. The hub canonicalizes its
existing parent once and uses that resolved parent for the socket lifetime; the
parent must be a directory owned by the hub's effective user with traditional
permission mode exactly `0700`. This owner-private immediate parent is required
even when the socket node itself has mode `0600`; version one does not rely on
every supported Unix implementation enforcing socket-node permissions. Every
resolved ancestor up to the filesystem root must also resist same-machine
replacement: a group- or other-writable ancestor is accepted only when it has
the sticky bit and the next path component toward the socket is owned by the
hub's effective user. Every ancestor must itself be owned by either root or the
hub's effective user, so an unprivileged different owner cannot make a currently
protected directory writable after validation. An untrusted owner, a non-sticky
writable ancestor, or a sticky writable ancestor containing a component owned by
another user fails startup.

Before inspecting the final path, the hub opens or creates the adjacent
`<socket-path>.lock` as a no-follow regular file owned by the effective user
with exact `0600` permissions, takes its nonblocking exclusive advisory file
lock, and holds that lock through final socket cleanup. Failure to open, verify,
or lock the sidecar fails without touching the socket path. The sidecar remains
after shutdown so a later hub can lock the same inode. While holding that
lifetime path lock, the hub also reclaims a retained socket left at the reserved
`<socket-path>.identity` name by an abrupt prior exit only when the public and
reserved names still identify the same owned socket. An orphaned or differently
paired entry at the reserved name fails startup without modification. It then
handles the final path as follows:

1. an absent entry is available;
2. an entry that is not a socket fails startup without modification;
3. a socket that accepts a connection is live and fails startup; and
4. a socket owned by the effective user is first retained by a hard link at the
   reserved identity name so its device and inode cannot be recycled, and a
   connection failure with `ConnectionRefused` proves it stale only if a second
   `lstat` still observes that retained identity. The hub removes only that
   revalidated entry and then binds; every other ownership, connection, or
   metadata result fails startup without modification.

The path lock makes the final revalidation and removal indivisible with respect
to another conforming hub. The bind itself must still create a new socket and
never replace another entry. The hub binds a new unlistening Unix stream socket
inside the verified owner-private parent, captures its socket type,
effective-user ownership, device, and inode with `lstat`, and retains that inode
with a hard link at the reserved identity name. Without changing the
process-wide creation mask, it sets exact owner-only `0600` permissions through
the retained name, then verifies that both names still identify that socket with
the required mode and that the descriptor's local address is the resolved path
before calling `listen`; no connection can be queued before that sequence
completes. The identity link remains for the listener lifetime so the device and
inode cannot be recycled. Any address, identity, ownership, or permission
mismatch fails startup and removes no raced entry. Graceful shutdown keeps the
listener and identity link live while a final `lstat` proves the public path
still names this hub's socket and removes that path, then releases the identity
link and path lock.

The transport is local-machine and single-user only. Version one's lack of
protocol authentication is provisional; it has no authorization exchange or
remote transport. Socket filesystem access is the deployment boundary; it is not
represented as application-level owner proof.

The hub owns at most 128 accepted connection tasks. At that limit it leaves new
connections in the bounded listener backlog until an active task exits, then
resumes accepting. The limit counts long-lived follow connections and ordinary
request connections alike. At most eight connection tasks may accumulate an
inbound frame simultaneously. An idle connection holds no frame slot: each
connection may buffer at most 8 KiB while waiting for its first byte, then
reserves a slot before extending that buffered prefix into a frame. This bounds
pre-admission read-ahead across 128 tasks at 1 MiB and aggregate admitted raw
frame accumulation at 64 MiB. After decoding, the task consumes the frame into
one owned request rather than cloning its payload. Submitted text moves into
application admission: rejection drops it before awaiting response output, and
acceptance reuses the decoded allocation. A peer that stops reading responses
therefore cannot retain out-of-policy input content after its rejection is
known.

Why: the first client needs a small local process boundary, while remote access
would require an authenticated identity and revocation design that does not yet
exist.

Authenticated transports and remote clients remain an
[open upgrade path](../open-questions.md#protocols-and-persistence).

## Framing and compatibility

Each frame is exactly one UTF-8 JSON object followed by `\n`. A frame may be at
most 8 MiB including the newline; an oversized or invalid UTF-8 line is
rejected. Empty lines are malformed frames. Connections process one request at a
time. A `follow_session` request consumes the connection until it closes; no
later request is read from that connection.

Every client and server frame has these required top-level members:

- `version`: JSON integer `1`;
- `request_id`: the canonical decimal string of an unsigned 64-bit integer; a
  client request, success response, or correlated error requires a nonzero value
  copied unchanged through the exchange;
- `request` on a client frame or `message` on a server frame: one closed tagged
  object described below.

Unknown top-level members, unknown tagged variants, missing required members,
and members with the wrong JSON type fail explicitly (INV-033). A frame may
contain at most 127 simultaneously open JSON objects and arrays; deeper input is
a `malformed_frame`. Within that bound, repeating a decoded member name in any
JSON object is a `malformed_frame`, including when two different JSON string
spellings decode to the same name. An unsupported `version` produces a
version-one `unsupported_version` error naming the supported version, then the
server closes the connection. A server error uses `request_id = "0"` only when
the incoming frame prevents recovery of a valid nonzero identity; zero is never
a valid client identity or success-response identity. Leading zeroes, a plus
sign, whitespace, and any spelling other than the shortest ASCII decimal form
are invalid.

The server may close a connection after any error. Clients never reinterpret an
unknown message as a known one.

Why: a required version on every independent line makes captured traffic and
errors self-describing without connection-global negotiation state.

## Client requests

Request objects carry a required string `type` and reject fields not admitted by
that variant.

| Type              | Additional required members                                                                                                        | Meaning                                                                                                                                                            |
| ----------------- | ---------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `create_session`  | `command_id` (canonical UUID string), `initial_model_selection` (selection object)                                                 | Create an owner-initiated session with no ancestry and establish defaults version one.                                                                             |
| `list_sessions`   | none                                                                                                                               | Read all current sessions as summaries, ordered by session identity.                                                                                               |
| `submit_input`    | `command_id` and `session_id` (canonical UUID strings), `content` (string), `expected_defaults_version` (canonical decimal string) | Submit exact owner text as `StartWhenNoActiveTurn`, using the caller-observed defaults version and no per-input model override.                                    |
| `read_transcript` | `session_id` (canonical UUID string)                                                                                               | Read one authoritative durable transcript snapshot and its observation cursor.                                                                                     |
| `follow_session`  | `session_id` (canonical UUID string)                                                                                               | Receive an initial authoritative snapshot, then this process incarnation's ordered durable update events committed after the snapshot cursor for the same session. |

A selection object is exactly one of:

- `{"kind":"direct","selection_id":"<canonical UUID>"}`;
- `{"kind":"alias","alias_id":"<canonical UUID>"}`.

Canonical UUID strings are lowercase hyphenated values. Nil and all-ones command
identities fail request validation before application construction. The server
does not generate mutation command identities on a client's behalf. Equal
command retransmission therefore reaches the existing durable replay boundary; a
new request identity does not change command meaning (INV-012). The expected
defaults version is part of the canonical submit payload. A caller retries an
ambiguous submission with the same command identity, session, content, expected
version, and treatment; changing any of them is a conflicting reuse, not
recovery.

`submit_input` deliberately exposes only the daily sequential-conversation
treatment in version one. If a turn is already active, the normal typed
application result is returned as a rejection; the protocol does not guess an
interrupt, steering, or after-current treatment.

Submitted `content` is limited to 1 MiB of UTF-8. The hub applies that boundary
before application construction or mutation and returns `invalid_request` when
it is exceeded. This leaves enough space for worst-case JSON escaping when the
same accepted content is projected in a queued turn or durable update event. The
exact capacity choice is recorded in the
[input-bound decision](../decisions.md#2026-07-23--bound-process-protocol-input-at-1-mib).

## Server messages

Message objects carry a required string `type` and reject fields not admitted by
that variant. Every accepted `create_session` or `submit_input` request produces
exactly one of:

- `session_created` with `session_id`;
- `input_submitted` with `session_id`, `accepted_input_id`,
  `acceptance_position`, and `turn_id`;
- `error` with a stable `code` and a non-sensitive `message`.

In the server shapes below, notation such as `queued` or
`terminal { disposition }` means a closed JSON object with `"type":"queued"` or
`"type":"terminal"` plus exactly the named members.

A session summary contains `session_id`, `defaults_version`, and
`model_selection`. A successful `list_sessions` response is `sessions_start`,
one `session_summary` per result in session-identity order, then
`sessions_end { session_count }`. The summaries are read in one read-only
repeatable-read transaction and spooled from one decoded row at a time before
client output. A slow client therefore retains temporary disk rather than the
complete session catalog in request heap or an open database transaction. The
sequence becomes authoritative only after the end message and count validate.
This avoids an aggregate frame-size limit. Identifiers are canonical UUID
strings. Request identities, ordinal versions, indices, counts, and outbox
cursors are canonical decimal strings, preserving their full unsigned 64-bit
range without JSON-number precision loss.

An application rejection is an `error` with `code = "rejected"` and a required
`detail` object whose variants are closed. For the version-one treatment, its
exact variants are `session_not_found { session_id }`,
`active_turn_present { session_id, active_turn_id }`,
`defaults_version_mismatch { session_id, expected, current }`,
`unknown_model_alias { session_id, alias_id }`, and
`acceptance_position_exhausted { session_id, last }`. Other error codes have no
`detail`. An equal replay returns the same success or rejection projection as
the first handling.

The error-code set in version one is:

| Code                  | Meaning                                                            |
| --------------------- | ------------------------------------------------------------------ |
| `malformed_frame`     | JSON, UTF-8, framing, field, or size validation failed.            |
| `unsupported_version` | The frame version is not one.                                      |
| `invalid_request`     | A boundary value cannot construct the requested application input. |
| `not_found`           | The selected session does not exist.                               |
| `conflicting_reuse`   | A durable command identity already names different intent.         |
| `rejected`            | The canonical command was durably rejected by current typed state. |
| `resync_required`     | A follower fell behind the bounded process-local event fan-out.    |
| `unavailable`         | Infrastructure failed; no requested mutation may have committed.   |
| `commit_ambiguous`    | Infrastructure obscured whether the requested mutation committed.  |
| `internal`            | Fail-closed corruption or a hub defect stopped the request.        |

For `create_session` and `submit_input`, a lost commit response maps to
`commit_ambiguous`; the client retries the exact command identity and payload to
discover the recorded outcome. A definitely pre-commit infrastructure failure
maps to `unavailable`.

Errors contain no database URL, socket path, credential path or value, SQL,
caller content, or provider payload.

An oversized outbound frame terminates only its connection. Other encoding
failures remain fatal evidence that the runtime cannot satisfy the closed wire
contract.

## Transcript snapshots

A transcript snapshot is read in one PostgreSQL repeatable-read, read-only
transaction. The transaction observes all of:

- the global last committed outbox sequence, returned as `cursor`; and
- the selected session's latest authoritative semantic frontier, selected from
  the tip of persisted turn-start predecessor lineage rather than acceptance
  order; and
- every turn in acceptance order with its authoritative lifecycle state.

One logical snapshot is a bounded message sequence sharing the request identity:

1. `transcript_snapshot_start { session_id, cursor }`;
2. one `transcript_turn` per turn, with canonical decimal `acceptance_position`;
3. the entry messages below in frontier-member order; and
4. `transcript_snapshot_end { session_id, cursor, turn_count, entry_count }`.

The hub builds that complete sequence in a secure unnamed temporary file before
writing its first snapshot frame to the connection. Persistence validates the
execution lineage in PostgreSQL and yields one turn or frontier member at a time
from the same read-only repeatable-read transaction; hubd encodes each item
directly to the spool, commits the transaction after the final item, rewinds,
and streams the completed file. A slow client therefore holds neither a
PostgreSQL snapshot nor transcript-sized heap state. Per request, heap retention
is bounded by one decoded row, one protocol frame, and fixed I/O buffers;
temporary disk usage follows the complete encoded transcript size. Projection or
spool failure before transmission returns `unavailable` and exposes no partial
snapshot sequence. Once transmission starts, peer-write failure closes only that
connection, while an unexpected read failure from the completed spool is fatal
runtime evidence because a valid snapshot has already begun. A follow request
closes the spool immediately after transmitting the snapshot, before waiting for
live events.

Session-list, transcript-read, and follow-snapshot construction share bounded
admission that reserves application-pool capacity for non-snapshot work. The
exact reservation is owned by the
[snapshot-resource decision](../decisions.md#2026-07-23--bound-process-snapshot-construction-resources).

Each `transcript_turn` has `turn_id` and one closed `state` object:

- `queued { accepted_input_id, content }`;
- `active_running { current_attempt_id, current_model_call }`, where
  `current_model_call` is null before preparation or `{ model_call_id, state }`
  with state exactly `prepared`, `in_flight`, or `cancellation_requested`;
- `active_awaiting_model_call_recovery { ended_attempt_id, recovery_model_call_id }`;
- `failed { terminal_frontier_id, terminal_attempt_id, terminal_model_call }`,
  where `terminal_attempt_id` is null only for an evidence-free recovery
  failure, and `terminal_model_call` is null when that failure or physical
  attempt owns no call; otherwise it is `{ model_call_id, disposition }` with
  disposition exactly `known_failed` or `cancelled`. A nonnull
  `terminal_model_call` requires a nonnull `terminal_attempt_id`;
- `completed { terminal_frontier_id, terminal_attempt_id, terminal_model_call_id }`;
- `refused { terminal_frontier_id, terminal_attempt_id, terminal_model_call_id }`;
- `cancelled { terminal_frontier_id, terminal_attempt_id, terminal_model_call_id }`,
  where `terminal_model_call_id` is null when cancellation closed the turn
  before a call was prepared; or
- `reconciliation_required { terminal_frontier_id, terminal_attempt_id, terminal_model_call_id }`.

Each non-text frontier member is one `transcript_entry` with `entry_index`,
`source_session_id`, `entry_id`, and one closed `entry` object:
`turn_completed { turn_id }`, `turn_failed { turn_id }`, or
`turn_cancelled { turn_id }`. A text member begins with
`transcript_text_entry { entry_index, source_session_id, entry_id, entry }`. Its
`entry` is either `user { accepted_input_id, turn_id }` or
`assistant { turn_id, model_call_id }`. It is followed by one or more
`transcript_content` messages carrying the same `entry_index`, a zero-based
`fragment_index`, `final_fragment`, and `content_fragment`. Fragment indices
start at zero and are contiguous: each fragment index is exactly its predecessor
plus one. Exactly the last fragment carries `final_fragment = true`; every
earlier fragment carries `false`. The content is split only at UTF-8 scalar
boundaries into fragments of at most 1 MiB of UTF-8; even empty content has one
final empty fragment. The 1 MiB content bound leaves room below the 8 MiB frame
limit even when every byte requires worst-case JSON escaping.

`entry_index` is zero-based and contiguous in frontier-member order; the first
entry is zero and each later entry is exactly its predecessor plus one.

A snapshot is authoritative only after the matching end message arrives and its
counts, indices, fragment sequence, session, and cursor validate. A connection
failure or error before then discards the partial snapshot. This bounded
multi-frame representation can carry every valid durable transcript rather than
making aggregate transcript size a frame-size precondition. A session with no
semantic frontier has no entry messages.

The wire snapshot is a presentation projection, not a domain `Session`, a
storage record, or a provider prompt. Unknown stored variants fail closed until
a protocol version maps them.

## Durable update dispatch

`DATABASE_URL` must name a direct or otherwise session-affine PostgreSQL
endpoint. Transaction- and statement-pooled proxy modes are unsupported because
the guard and generation fences below use locks owned by one PostgreSQL server
session.

Before migration or recovery, `signalbox-hubd` acquires
`pg_try_advisory_lock(1396856881, 1213547057)` on one dedicated database
connection and retains that connection—and therefore the session-level
lock—until shutdown. Failure to acquire the fixed database-scoped guard fails
startup. The two integer keys are the ASCII namespaces `SBX1` and `HUB1`.

The singleton `hub_fence_state` stores a positive generation. Every application
pool connection acquires and retains a shared session advisory lock keyed by the
ASCII namespace `SBF1` (`1396852273`) and this hub's generation, then requires
the durable singleton still to equal that generation before the connection
becomes usable. A mismatch rejects the connection. A successor holding the
singleton guard takes and retains the exclusive prior-generation fence, then
transactionally advances the row before constructing its fenced pool. That
exclusive request waits for all prior pooled sessions and prevents the old
process from opening another usable connection: an older generation that tries
again after a failed intermediate successor can acquire only its old shared
lock, then fails the current-generation check. Pool construction requires a
non-cloneable capability borrowing the still-live fence session; the copyable
generation value is observational and cannot construct work after guard release.
The first migration creates and initializes the row for a database that cannot
have a prior fenced hub; later startups fence before running any newer
migration. This fence migration belongs to Signalbox's initial deployment: the
owner confirms that no deployed database or hub predates it, so there is no
legacy unfenced writer to drain during the first installation. Importing or
upgrading a pre-fence database is unsupported. Exhaustion or corruption fails
startup rather than wrapping.

Together these guards enforce one active hub process—and therefore one
dispatcher and one process-local fan-out—for a database, while preventing a
successor's migration or recovery from overlapping an old hub's authoritative
work. Guard-session monitoring and fatal-loss behavior are owned by
[Hub runtime: startup order and shutdown](turn-lifecycle-and-scheduling.md#hub-runtime-startup-order-and-shutdown).
For each attempt, the dispatcher:

1. starts a PostgreSQL transaction and locks the singleton
   `outbox_delivery_state`;
2. loads exactly `delivered_through + 1` and its one typed record;
3. maps the storage record to a distinct process-update value and offers it to
   the in-process fan-out;
4. only after that offer is accepted, advances `delivered_through` to the same
   sequence and commits.

An idle dispatcher polls again after 50 ms. It never skips a sequence and never
dispatches two events concurrently. Delivery failure, task cancellation, or a
crash before the cursor commit leaves the prefix unchanged, so the same event is
offered again after recovery. A crash after the offer but before commit may
therefore duplicate that cursor; delivery is at least once and globally ordered
(INV-032). Consumers deduplicate by cursor.

The process-local fan-out retains 64 update events. Having no connected
followers does not block durable cursor advancement: reconnecting clients use a
fresh authoritative snapshot. A follower that overruns the bounded fan-out
receives `resync_required` and reconnects for another snapshot.

Each `session_event` message carries `cursor`, `session_id`, and exactly one of
these closed `event` objects:

| Event                          | Additional members                                                                         |
| ------------------------------ | ------------------------------------------------------------------------------------------ |
| `session_created`              | none                                                                                       |
| `input_accepted`               | `accepted_input_id`, `turn_id`, `acceptance_position`, and `content`                       |
| `turn_activated`               | `turn_id` and `current_attempt_id`                                                         |
| `model_call_transition`        | `turn_id`, `model_call_id`, and `state`                                                    |
| `turn_completed`               | `turn_id`, `model_call_id`, `completion_entry_id`, and `terminal_frontier_id`              |
| `turn_failed`                  | `turn_id`, `failure_entry_id`, and `terminal_frontier_id`                                  |
| `turn_refused`                 | `turn_id`, `model_call_id`, and `terminal_frontier_id`                                     |
| `turn_cancelled`               | `turn_id`, `cancellation_entry_id`, and `terminal_frontier_id`                             |
| `turn_reconciliation_required` | `turn_id`, exactly one of `model_call_id` or `tool_attempt_id`, and `terminal_frontier_id` |

The model-call `state` object is exactly `prepared`, `in_flight`,
`cancellation_requested`, or `terminal { disposition }`; terminal disposition is
one of `completed`, `known_failed`, `refused`, `cancelled`, or `ambiguous`.
Storage-version columns are not exposed as wire-version fields.

## Follow synchronization

For `follow_session`, the server subscribes to process-local fan-out before
reading the repeatable-read transcript snapshot. It sends that snapshot first,
then discards subscribed events at or below its cursor and sends matching
session events above it in cursor order.

This ordering closes the snapshot/subscription race: every listed client-visible
transition committed before the snapshot is represented by its durable queued
content, turn state, and current model-call projection even when it adds no
semantic transcript entry, while a transition committed after the snapshot has a
greater cursor and was observed by the preexisting subscription. A refused turn
is therefore terminal in the initial snapshot and cannot leave `send` waiting
for an event at or below the snapshot cursor. Previously seen transient display
state may always be replaced by the new snapshot (INV-032).

Version one forwards durable transition events only. Provider token deltas
remain transient inside the model-runtime boundary and are not added to the
outbox. The terminal `send` command follows the submitted turn, accepts terminal
state from the initial snapshot or waits for its durable terminal event, rereads
the authoritative transcript, and prints the committed assistant text. A client
that observes `active_awaiting_model_call_recovery` in the initial snapshot
exits with a typed nonzero recovery-required diagnostic: version one has no
writer that can complete that wait. When the selected turn instead emits a live
terminal `ambiguous` model-call transition, `send` rereads the authoritative
snapshot and produces that same diagnostic if the turn has entered the recovery
wait. A client disconnect never cancels model work. After each terminal turn
event, `follow` uses a separate connection to read and validate a fresh
authoritative transcript before it resumes printing later followed events. That
side reread does not advance the follow connection's observed cursor: only
events consumed from the subscribed connection do so, and every buffered event
remains eligible for ordered presentation. Although the reread may have a cursor
later than the triggering event, it makes presentation eligible only the
previously undisplayed semantic material attributable to that exact terminal
event: assistant text from its named turn and model call plus the exact
completion marker for `turn_completed`, the exact failure marker for
`turn_failed`, the exact cancellation marker for `turn_cancelled`, and no
semantic material for `turn_refused` or `turn_reconciliation_required`, whose
terminalization creates no content entry. It does not present material
introduced by any later cursor. Such material remains ordered behind its
buffered followed event, or behind a new authoritative snapshot after
`resync_required`. Final durable content is deduplicated by source-qualified
semantic-entry identity while transition-only events remain visible instead of
being suppressed by a newer side snapshot.

## Terminal client

The `signalbox` binary is the daily version-one surface. It accepts a global
`--socket <path>` override or reads `SIGNALBOX_SOCKET_PATH`, and provides:

- `create (--model <selection-uuid> | --alias <alias-uuid>) [--command-id <uuid>]`;
- `list`;
- `send <session-uuid> [--command-id <uuid> --defaults-version <decimal>]`;
- `transcript <session-uuid>`;
- `follow <session-uuid>`.

`send` reads the exact input text from standard input through EOF and never
accepts conversation content in process arguments. Empty or oversized input
fails before socket I/O.

When `--command-id` is absent, the client generates a fresh UUIDv7 identity and
prints it to standard error before any socket I/O. `send` first reads the
session summary and uses its defaults version, then prints that expected version
to standard error before sending the mutation. Thus every client-generated or
server-discovered recovery value is visible before its commit can become
ambiguous. Exact replay also requires the original selection or session argument
and, for `send`, the exact standard-input content; the client does not echo that
potentially sensitive input or synthesize a shell command. Its ambiguity
diagnostic directs the user to retry the original command with those arguments
and input plus any printed recovery values. For recovery, the user supplies the
printed command identity; `send` then also requires the exact
`--defaults-version`, and the two flags are rejected unless supplied together.
The client never silently substitutes a new command identity for an ambiguous
attempt. It uses a fresh nonzero request identity per connection, renders only
known version-one messages, and exits nonzero on protocol or application errors
other than the follow-specific `resync_required` control case, which reconnects
for a fresh snapshot.

The client validates each complete snapshot and its terminal counts into an
owner-private anonymous temporary-file spool before replay or presentation. Turn
and source-qualified entry identity indexes are disk-backed too, so the wire's
intentionally unbounded aggregate snapshot size does not become unbounded client
memory. Before adopting an initial or resynchronized snapshot cursor, `follow`
presents its acceptance-ordered turn projections, including queued owner
content, active attempt and current-call state, recovery waits, and terminal
state. A transition committed at or below that cursor therefore remains visible
even when it has not added a semantic transcript entry.

The unbounded aggregate session-summary sequence is bounded the same way. `list`
validates ordering and the terminal count while spooling summary frames to an
anonymous temporary file, then presents them only after the complete sequence
validates. `send` validates the whole sequence with constant memory and retains
only the selected session's defaults version.

After completion, `send` rereads and prints only authoritative committed
assistant text produced for its exact turn. A failed or refused turn produces a
typed diagnostic and a nonzero exit without reply text; cancelled and
reconciliation-required turns do the same with their distinct typed diagnostics.
`follow` prints the initial transcript and subsequent typed durable updates
until interrupted. By default every process-derived text field written to a
terminal preserves line feed but renders every other C0 code point, DEL, and C1
code points as visible `\u{...}` escapes, preventing ESC/OSC execution.
`--raw-output` is the explicit opt-in that writes those fields unchanged; the
same safe-rendering choice covers assistant text, typed diagnostics, and durable
updates. Each complete raw text value is flushed before the client awaits
another frame, without adding a delimiter.

The existing `signalbox-debug` binary is unchanged and remains a development
harness, not a protocol client.

## Open edges

Deferred transport, compatibility, update-stream, retention, and operation
questions are cataloged under
[Protocols and persistence](../open-questions.md#protocols-and-persistence);
later client-form choices are cataloged under
[Client scope](../open-questions.md#client-scope).

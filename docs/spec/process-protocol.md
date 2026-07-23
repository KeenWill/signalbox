# Process protocol

This page specifies Signalbox process protocol version one and the terminal
client that consumes it. It is the normative boundary between a local client
process and `signalbox-hubd`; domain values, PostgreSQL records, and wire
messages remain distinct representations.

Invariant law lives in [docs/invariants.md](../invariants.md), cited here by
tag. Durable update storage and the delivered-through cursor are owned by
[persistence-protocol](persistence-protocol.md).

## Transport and trust boundary

Version one uses one Unix domain stream socket at the path supplied in
`SIGNALBOX_SOCKET_PATH`. The hub and terminal client both require that
deployment value. `signalbox-hubd` binds the socket with owner-only `0600`
permissions, refuses to replace an existing filesystem entry at the configured
path, and removes the socket entry after graceful listener shutdown.

The transport is local-machine and single-user only. Version one's lack of
protocol authentication is provisional; it has no authorization exchange or
remote transport. Socket filesystem access is the deployment boundary; it is not
represented as application-level owner proof.

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
- `request_id`: a JSON integer in the unsigned 64-bit range, copied unchanged
  into every response produced for that request;
- `request` on a client frame or `message` on a server frame: one closed tagged
  object described below.

Unknown top-level members, unknown tagged variants, missing required members,
and members with the wrong JSON type fail explicitly (INV-033). An unsupported
`version` produces a version-one `unsupported_version` error naming the
supported version, then the server closes the connection. A malformed frame
whose request identity cannot be recovered produces an error with
`request_id = 0`, which is reserved for that purpose. Each request identity must
otherwise be nonzero.

The server may close a connection after any error. Clients never reinterpret an
unknown message as a known one.

Why: a required version on every independent line makes captured traffic and
errors self-describing without connection-global negotiation state.

## Client requests

Request objects carry a required string `type` and reject fields not admitted by
that variant.

| Type              | Additional required members                                                                    | Meaning                                                                                                                                                            |
| ----------------- | ---------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `create_session`  | `command_id` (canonical UUID string), `initial_model_selection` (selection object)             | Create an owner-initiated session with no ancestry and establish defaults version one.                                                                             |
| `list_sessions`   | none                                                                                           | Read all current sessions as summaries, ordered by session identity.                                                                                               |
| `submit_input`    | `command_id` (canonical UUID string), `session_id` (canonical UUID string), `content` (string) | Submit exact owner text as `StartWhenNoActiveTurn`, using the session's current defaults version and no per-input model override.                                  |
| `read_transcript` | `session_id` (canonical UUID string)                                                           | Read one authoritative durable transcript snapshot and its observation cursor.                                                                                     |
| `follow_session`  | `session_id` (canonical UUID string)                                                           | Receive an initial authoritative snapshot, then this process incarnation's ordered durable update events committed after the snapshot cursor for the same session. |

A selection object is exactly one of:

- `{"kind":"direct","selection_id":"<canonical UUID>"}`;
- `{"kind":"alias","alias_id":"<canonical UUID>"}`.

Canonical UUID strings are lowercase hyphenated values. Nil and all-ones command
identities fail request validation before application construction. The server
does not generate mutation command identities on a client's behalf. Equal
command retransmission therefore reaches the existing durable replay boundary; a
new request identity does not change command meaning (INV-012).

`submit_input` deliberately exposes only the daily sequential-conversation
treatment in version one. If a turn is already active, the normal typed
application result is returned as a rejection; the protocol does not guess an
interrupt, steering, or after-current treatment.

## Server messages

Message objects carry a required string `type` and reject fields not admitted by
that variant. Every accepted non-follow request produces exactly one of:

- `session_created` with `session_id`;
- `sessions` with a `sessions` array of session summaries;
- `input_submitted` with `session_id`, `accepted_input_id`, and either the
  created `turn_id` or the typed non-turn disposition;
- `transcript_snapshot` with `session_id`, `cursor`, and ordered `entries`;
- `error` with a stable `code` and a non-sensitive `message`.

A session summary contains `session_id`, `defaults_version`, and
`model_selection`. Identifiers are canonical UUID strings. Ordinal versions and
outbox cursors are decimal strings, preserving their full unsigned 64-bit range
without JSON-number precision loss.

The error-code set in version one is:

| Code                  | Meaning                                                                  |
| --------------------- | ------------------------------------------------------------------------ |
| `malformed_frame`     | JSON, UTF-8, framing, field, or size validation failed.                  |
| `unsupported_version` | The frame version is not one.                                            |
| `invalid_request`     | A boundary value cannot construct the requested application input.       |
| `not_found`           | The selected session does not exist.                                     |
| `conflicting_reuse`   | A durable command identity already names different intent.               |
| `rejected`            | The canonical command was durably rejected by current typed state.       |
| `resync_required`     | A follow connection fell behind the bounded process-local event fan-out. |
| `unavailable`         | Infrastructure prevented completion; commit ambiguity is not hidden.     |
| `internal`            | Fail-closed corruption or a hub defect stopped the request.              |

Errors contain no database URL, socket path, credential path or value, SQL,
caller content, or provider payload.

## Transcript snapshots

A transcript snapshot is read in one PostgreSQL repeatable-read, read-only
transaction. The transaction observes both:

- the global last committed outbox sequence, returned as `cursor`; and
- the selected session's latest authoritative semantic frontier.

The snapshot emits frontier entries in member order. The version-one entry
objects are `user`, `assistant`, `turn_completed`, and `turn_failed`. They carry
their semantic-entry identity and exact typed subject; user and assistant
variants additionally carry exact committed text. Assistant entries also name
their producing call and owning turn. A session with no semantic frontier has an
empty transcript.

The wire snapshot is a presentation projection, not a domain `Session`, a
storage record, or a provider prompt. Unknown stored variants fail closed until
a protocol version maps them.

## Durable update dispatch

`signalbox-hubd` runs exactly one outbox dispatcher. For each attempt it:

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

The process-local fan-out retains 1,024 update events. Having no connected
followers does not block durable cursor advancement: reconnecting clients use a
fresh authoritative snapshot. A follower that overruns the bounded fan-out
receives `resync_required` and reconnects for another snapshot.

The version-one update variants are the already implemented outbox family:
`session_created`, `model_call_transition`, `turn_completed`, `turn_failed`, and
`turn_refused`. Each `session_event` message carries `cursor`, `session_id`, and
the variant's typed identities and state. Storage-version columns are not
exposed as wire-version fields.

## Follow synchronization

For `follow_session`, the server subscribes to process-local fan-out before
reading the repeatable-read transcript snapshot. It sends that snapshot first,
then discards subscribed events at or below its cursor and sends matching
session events above it in cursor order.

This ordering closes the snapshot/subscription race: a transition committed
before the snapshot is represented by durable state; a transition committed
after the snapshot has a greater cursor and was observed by the preexisting
subscription. Previously seen transient display state may always be replaced by
the new snapshot (INV-032).

Version one forwards durable transition events only. Provider token deltas
remain transient inside the model-runtime boundary and are not added to the
outbox. The terminal `send` command follows the submitted turn, waits for its
durable terminal event, rereads the authoritative transcript, and prints the
committed assistant text. A client disconnect never cancels model work.

## Terminal client

The `signalbox` binary is the daily version-one surface. It accepts a global
`--socket <path>` override or reads `SIGNALBOX_SOCKET_PATH`, and provides:

- `create --model <selection-uuid>` or `create --alias <alias-uuid>`;
- `list`;
- `send <session-uuid> <text>`;
- `transcript <session-uuid>`;
- `follow <session-uuid>`.

The client generates a fresh UUIDv7 durable command identity for `create` and
`send`, uses a fresh nonzero request identity per connection, renders only known
version-one messages, and exits nonzero on protocol or application errors.
`send` prints only authoritative committed assistant text for its exact turn.
`follow` prints the initial transcript and subsequent typed durable updates
until interrupted.

The existing `signalbox-debug` binary is unchanged and remains a development
harness, not a protocol client.

## Open edges

- Authenticated transports, remote clients, revocation, and client authority
  remain open.
- Browser transport and any compatibility window beyond exact version one remain
  open.
- Update-event retention, pruning, and multi-process fan-out remain open in
  [persistence-protocol](persistence-protocol.md).
- Transient provider-delta relay, draft identity and delta sequencing remain
  open; version one exposes durable progress and final content only.
- Protocol operations for defaults replacement, delivery treatments other than
  `StartWhenNoActiveTurn`, cancellation, approval, and tools await their owning
  product slices.

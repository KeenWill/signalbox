# Process protocol

This page is the normative boundary between a local client process and
`signalboxd`; domain values, PostgreSQL records, and wire messages remain
distinct representations.

Multipart content arrays are proposed and not implemented.

Signalbox admits one process-protocol version, integer `1`. Its closed
vocabulary contains every request, response, event, and required field
implemented in this tree. The version field is required on every frame and
exact-version admission is fail closed, so a later version can be introduced
without rebuilding the mechanism.

**Freeze condition.** In-place protocol editing ends at the first durable
deployment: the first client that cannot be rebuilt at will. In practice, that
means a macOS app installed on a device or a daemon reached remotely rather than
from this tree. At that point version `1` becomes permanent, subsequent
incompatible vocabulary changes allocate permanent new numbers, and
compatibility policy must be decided explicitly. Until that condition occurs,
protocol changes modify version `1` in place.

Invariants are defined in [docs/invariants.md](../invariants.md), cited here by
tag. Durable update storage and the delivered-through cursor are owned by
[persistence-protocol](persistence-protocol.md).

## Transport and trust boundary

The process protocol uses one Unix domain stream socket. The daemon requires its
path in `SIGNALBOX_SOCKET_PATH`; the terminal client uses its `--socket <path>`
override when present and otherwise requires that environment value.
`signalboxd` binds the socket with owner-only `0600` permissions. The configured
path must be absolute and must end in an explicit filename component; a trailing
separator, `/.`, or `/..` is rejected rather than normalized. The daemon
canonicalizes its existing parent once and uses that resolved parent for the
socket lifetime; the parent must be a directory owned by the daemon's effective
user with traditional permission mode exactly `0700`. This owner-private
immediate parent is required even when the socket node itself has mode `0600`;
the trust boundary does not rely on every supported Unix implementation
enforcing socket-node permissions. Every resolved ancestor up to the filesystem
root must also resist same-machine replacement: a group- or other-writable
ancestor is accepted only when it has the sticky bit and the next path component
toward the socket is owned by the daemon's effective user. Every ancestor must
itself be owned by either root or the daemon's effective user, so an
unprivileged different owner cannot make a currently protected directory
writable after validation. An untrusted owner, a non-sticky writable ancestor,
or a sticky writable ancestor containing a component owned by another user fails
startup.

Before inspecting the final path, the daemon opens or creates the adjacent
`<socket-path>.lock` as a no-follow regular file owned by the effective user
with exact `0600` permissions, takes its nonblocking exclusive advisory file
lock, and holds that lock through final socket cleanup. Failure to open, verify,
or lock the sidecar fails without touching the socket path. The sidecar remains
after shutdown so a later daemon can lock the same inode. While holding that
lifetime path lock, the daemon also reclaims a retained socket left at the
reserved `<socket-path>.identity` name by an abrupt prior exit when the public
and reserved names still identify the same owned socket, or when the public
socket entry is absent and the reserved entry is an owned socket. It revalidates
and removes the retained entry before binding; a differently paired entry at the
reserved name fails startup without modification. It then handles the final path
as follows:

1. an absent entry is available;
2. an entry that is not a socket fails startup without modification;
3. a socket that accepts a connection is live and fails startup; and
4. a socket owned by the effective user is first retained by a hard link at the
   reserved identity name so its device and inode cannot be recycled, and a
   connection failure with `ConnectionRefused` proves it stale only if a second
   `lstat` still observes that retained identity. The daemon removes only that
   revalidated entry and then binds; every other ownership, connection, or
   metadata result fails startup without modification.

The path lock makes the final revalidation and removal indivisible with respect
to another conforming daemon. The bind itself must still create a new socket and
never replace another entry. The daemon binds a new unlistening Unix stream
socket inside the verified owner-private parent, captures its socket type,
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
still names this daemon's socket, then releases the identity link before
removing the public path and releasing the listener and path lock.

The transport is local-machine and single-user only. The process protocol's lack
of authentication is provisional; the protocol has no authorization exchange or
remote transport. Socket filesystem access is the deployment boundary; it is not
represented as application-level file-owner proof.

The daemon owns at most 128 accepted connection tasks. At that limit it leaves
new connections in the bounded listener backlog until an active task exits, then
resumes accepting. The limit counts long-lived follow connections and ordinary
request connections alike. At most eight connection tasks may accumulate an
inbound frame simultaneously. An idle connection holds no frame slot: each
connection may buffer at most 8 KiB while waiting for its first byte, then
reserves a slot before extending that buffered prefix into a frame. This bounds
pre-admission read-ahead across 128 tasks at 1 MiB and aggregate admitted raw
frame accumulation at 64 MiB. After decoding, the task consumes the frame into
one owned request rather than cloning its payload. Submitted text moves into
application admission: rejection drops it before awaiting response output, and
acceptance reuses the decoded allocation. Conversation-import source bytes and
blob chunks likewise move directly into one bulk-ingest admission path. At most
one in-progress or single-shot conversation import or blob upload holds the
process-wide bulk-ingest permit. A connection that already owns the permit for a
chunked operation rejects any cross-kind begin or single-shot import with
`bulk_ingest_already_in_progress` before entering the waiter queue; the
connection's sequential handler therefore never waits on its own permit. One of
the eight inbound-frame slots is reserved for the connection that owns the
active chunked operation; other connections share the remaining seven. An
admitted begin that must wait for the permit first enters a shared seven-waiter
bound, then releases its small decoded frame slot before waiting. Further begins
retain a general frame slot until a waiter place opens. A source-bearing
single-shot import retains its frame slot while waiting. The reservation
preserves frame progress for the active append or commit without allowing queued
sources to escape the aggregate raw-frame bound. Once admitted, each append
moves its decoded chunk from the inbound frame into the disk-backed blob spool
or per-connection import assembly and releases the frame slot. The configured
total bounds limit the active assembly, so retained bulk-ingest assembly storage
is at most the larger of `max_blob_bytes` and
`conversation_import.max_source_bytes`, plus one bounded inbound chunk. During
filesystem publication, its store-local temporary file may coexist with the
completed staging spool, so the maximum transient blob disk footprint is twice
`max_blob_bytes` plus one bounded inbound chunk; each file is independently
limited to `max_blob_bytes`. Import commit runs the whole-source conversion on
the blocking pool so synchronous conversion does not occupy an asynchronous
runtime worker. Commit, abort, terminal size or conversion rejection, or
disconnect drops the assembly and releases the permit before response output. An
`already_in_progress` refusal is nonterminal and leaves the existing assembly
available for append, commit, or explicit abort. A peer that stops reading a
terminal response therefore cannot retain rejected input or completed import
content.

An admitted chunked conversation import or blob upload has a five-minute
monotonic inactivity deadline while awaiting its next append, commit, or abort
frame. The deadline starts after the begin receipt and resets after each
successfully accepted append; time spent executing an accepted lifecycle request
is not idle time. Expiry cancels pending receipt output, unlinks the partial
assembly, releases the bulk-ingest permit, and closes the connection without
accepting another request. A retry therefore starts on a fresh connection and
uses the operation's ordinary idempotency contract.

The same operation also has a 24-hour monotonic whole-session deadline beginning
when its begin acquires the bulk-ingest permit. Appends never reset it, and time
spent executing lifecycle requests counts toward it. Expiry cancels active work,
unlinks the partial assembly, releases the permit, and closes the connection
with the same retry consequence. The inactivity deadline therefore bounds a
stalled client while the whole-session deadline bounds one making indefinite
minimal progress.

A single-shot conversation import has the same non-resetting 24-hour monotonic
operation deadline beginning when it acquires the bulk-ingest permit. The
deadline spans conversion, raw-blob publication and verification, catalog
registration, and aggregate persistence; it is never restarted by progress in
one of those phases. Expiry cancels active work, releases the permit, and closes
the connection with the same retry consequence. A single-shot request has no
inactivity deadline because its complete body has already been received.

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

- `version`: the JSON integer `1`;
- `request_id`: the canonical decimal string of an unsigned 64-bit integer; a
  client request, success response, or correlated error requires a nonzero value
  copied unchanged through the exchange; and
- `request` on a client frame or `message` on a server frame: one closed tagged
  object described below.

Unknown top-level members, unknown tagged variants, missing required members,
and members with the wrong JSON type fail explicitly (INV-033). A frame may
contain at most 127 simultaneously open JSON objects and arrays; deeper input is
a `malformed_frame`. Within that bound, repeating a decoded member name in any
JSON object is a `malformed_frame`, including when two different JSON string
spellings decode to the same name.

Version admission is the one centralized wire gate. Integer `1` is admitted;
zero, every other integer, a non-integer, or an integer outside the unsigned
64-bit range is refused. An unknown integer produces `unsupported_version`, the
error frame uses version `1` as the pre-admission fallback, and the server
closes the connection. Every admitted response echoes version `1`. A
response-version mismatch fails locally in a client. The field and check remain
present so a permanent post-freeze version can be introduced without rebuilding
the framing mechanism.

A server error uses `request_id = "0"` only when the incoming frame prevents
recovery of a valid nonzero identity; zero is never a valid client identity or
success-response identity. Leading zeroes, a plus sign, whitespace, and any
spelling other than the shortest ASCII decimal form are invalid.

The server may close a connection after any error. Clients never reinterpret an
unknown message as a known one.

Why: a required version on every independent line makes captured traffic and
errors self-describing without connection-global negotiation state.

## Client requests

Request objects carry a required string `type` and reject fields not admitted by
that variant.

| Type                                    | Additional required members                                                                                                                                                                                                                                                                                                                                     | Meaning                                                                                                                                                                                                                                                                                                                                                                         |
| --------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `create_session`                        | `command_id` (canonical UUID string), `initial_model_selection` (selection object), `model_settings` (settings overlay), `system_prompt` (string or null), optional `placement` (session-placement object; omission means pathless), `runner_placement` (proposed; runner-placement object or null)                                                             | Create a user-initiated session with no ancestry and establish defaults version one.                                                                                                                                                                                                                                                                                            |
| `create_session_from_template`          | `command_id` (canonical UUID string), `template_name` (template-name string), optional `placement` (session-placement object; omission means pathless), `runner_placement` (proposed; runner-placement object or null)                                                                                                                                          | Resolve one daemon-held template and copy its complete bundle into a user-initiated session's defaults version one.                                                                                                                                                                                                                                                             |
| `commission_session`                    | `command_id` (canonical UUID string), `template_name` (template-name string), `fence` (closed authority-fence object: `target` of `pull_request` with `repository`, positive `pull_request`, `head_sha`, `head_repository`, `head_branch`, and `base_branch`, or `target` of `branch` with `repository` and `branch`), `statement` (string), `content` (string) | Atomically create a template session under a recorded immutable authority fence, attach the statement as its commissioned goal, and submit the content as its first input through the start-when-idle path.                                                                                                                                                                     |
| `list_sessions`                         | none                                                                                                                                                                                                                                                                                                                                                            | Read all current sessions as basic summaries, ordered by session identity.                                                                                                                                                                                                                                                                                                      |
| `read_operator_status`                  | none                                                                                                                                                                                                                                                                                                                                                            | Read one coherent repository-watch operator-status snapshot.                                                                                                                                                                                                                                                                                                                    |
| `update_session_placement`              | `command_id` and `session_id` (canonical UUID strings), `expected_placement_version` (positive canonical decimal string), `replacement` (session-placement object)                                                                                                                                                                                              | Append one immutable placement event conditional on the exact current placement version.                                                                                                                                                                                                                                                                                        |
| `list_templates`                        | none                                                                                                                                                                                                                                                                                                                                                            | Read every available template's name and version in name order.                                                                                                                                                                                                                                                                                                                 |
| `attach_goal`                           | `command_id` and `session_id` (canonical UUID strings), `statement` (string)                                                                                                                                                                                                                                                                                    | Attach the first immutable commissioned statement and begin pursuing it.                                                                                                                                                                                                                                                                                                        |
| `read_goal`                             | `session_id` (canonical UUID string)                                                                                                                                                                                                                                                                                                                            | Read the current goal projection and complete ordered lineage.                                                                                                                                                                                                                                                                                                                  |
| `resume_goal`                           | `command_id` and `session_id` (canonical UUID strings), `guidance` (string or null)                                                                                                                                                                                                                                                                             | Resume the blocked current generation, optionally using exact guidance as its next turn input.                                                                                                                                                                                                                                                                                  |
| `stop_goal`                             | `command_id` and `session_id` (canonical UUID strings), `descendant_scope` (`parent_alone` or `parent_and_descendants`)                                                                                                                                                                                                                                         | Terminalize the pursuing or blocked current generation by explicit user action, with an explicit delegated-child scope.                                                                                                                                                                                                                                                         |
| `supersede_goal`                        | `command_id` and `session_id` (canonical UUID strings), `statement` (string)                                                                                                                                                                                                                                                                                    | Atomically supersede the current generation and begin pursuing a new immutable statement in the same lineage.                                                                                                                                                                                                                                                                   |
| `stop_session`                          | `command_id` and `session_id` (canonical UUID strings), `sticky` (boolean), `descendant_scope` (`parent_alone` or `parent_and_descendants`)                                                                                                                                                                                                                     | Close the session `stopped{sticky}` from any non-terminal state; a live turn settles through the interrupt machinery first.                                                                                                                                                                                                                                                     |
| `supersede_session`                     | `command_id`, `session_id`, and `successor_session_id` (canonical UUID strings)                                                                                                                                                                                                                                                                                 | Close the session `superseded{by}` in favour of its successor.                                                                                                                                                                                                                                                                                                                  |
| `abandon_session`                       | `command_id` and `session_id` (canonical UUID strings)                                                                                                                                                                                                                                                                                                          | Write off a parked session as `abandoned`.                                                                                                                                                                                                                                                                                                                                      |
| `close_session_failed`                  | `command_id` and `session_id` (canonical UUID strings), `cause` (closed failure-cause string, or null for the park's standing cause)                                                                                                                                                                                                                            | Close a parked session as failed with its standing cause.                                                                                                                                                                                                                                                                                                                       |
| `resume_session`                        | `command_id` and `session_id` (canonical UUID strings)                                                                                                                                                                                                                                                                                                          | Return a parked session whose goal is not blocked to the state its suspended turn maps to.                                                                                                                                                                                                                                                                                      |
| `adopt_session`                         | `command_id` and `session_id` (canonical UUID strings), `finish_condition` (condition or null: `external_gate`, or `declared` with statement text)                                                                                                                                                                                                              | Take the liveness obligation, declaring the finish condition when the session carries none.                                                                                                                                                                                                                                                                                     |
| `release_session`                       | `command_id` and `session_id` (canonical UUID strings)                                                                                                                                                                                                                                                                                                          | Drop the liveness obligation.                                                                                                                                                                                                                                                                                                                                                   |
| `release_start`                         | `command_id` and `session_id` (canonical UUID strings)                                                                                                                                                                                                                                                                                                          | Open a held start gate so queued work becomes eligible to run.                                                                                                                                                                                                                                                                                                                  |
| `submit_input`                          | `command_id` and `session_id` (canonical UUID strings), `content` (ordered content-part array), `expected_defaults_version` (canonical decimal string, or null only for steering), `model_settings` (settings overlay), and optional `delivery`                                                                                                                 | Submit exact user content with the selected closed delivery intent; omitting `delivery` retains `StartWhenNoActiveTurn`.                                                                                                                                                                                                                                                        |
| `read_transcript`                       | `session_id` (canonical UUID string)                                                                                                                                                                                                                                                                                                                            | Read one authoritative durable transcript snapshot and its observation cursor.                                                                                                                                                                                                                                                                                                  |
| `follow_session`                        | `session_id` (canonical UUID string)                                                                                                                                                                                                                                                                                                                            | Receive an initial authoritative snapshot, then this process incarnation's ordered durable update events committed after the snapshot cursor for the same session; also receive ephemeral provider-text deltas.                                                                                                                                                                 |
| `spawn_session`                         | `session_id`, `turn_id`, and `tool_request_id` (canonical UUID strings), `task` (string), and `relationship` (closed object)                                                                                                                                                                                                                                    | Execute one exact already-issued spawn request.                                                                                                                                                                                                                                                                                                                                 |
| `await_session`                         | `session_id`, `turn_id`, `tool_request_id`, and `child_session_id` (canonical UUID strings), and `mode` (`foreground` or `background`)                                                                                                                                                                                                                          | Register delivery for one related child.                                                                                                                                                                                                                                                                                                                                        |
| `send_session_message`                  | `session_id`, `turn_id`, `tool_request_id`, and `peer_session_id` (canonical UUID strings), and `content` (string)                                                                                                                                                                                                                                              | Send one bounded message across the exact relationship.                                                                                                                                                                                                                                                                                                                         |
| `list_session_metadata`                 | `required_tags` (string array), `title_contains` (string or null), `include_archived` (boolean), `page_size` (canonical decimal string), `after_session_id` (canonical UUID string or null)                                                                                                                                                                     | Read one filtered metadata-summary page in session-identity order.                                                                                                                                                                                                                                                                                                              |
| `read_session_metadata`                 | `session_id` (canonical UUID string)                                                                                                                                                                                                                                                                                                                            | Read one complete current metadata snapshot.                                                                                                                                                                                                                                                                                                                                    |
| `replace_session_metadata`              | `command_id` and `session_id` (canonical UUID strings), `metadata` (the complete metadata object below)                                                                                                                                                                                                                                                         | Durably replace one complete metadata snapshot as the user actor.                                                                                                                                                                                                                                                                                                               |
| `import_conversation`                   | `format` (`claude_code_session_jsonl_v2` or `codex_rollout_jsonl_v1`), `source` (canonical padded base64 string)                                                                                                                                                                                                                                                | Convert and idempotently resolve or insert one complete external conversation snapshot.                                                                                                                                                                                                                                                                                         |
| `begin_conversation_import`             | `format` (`claude_code_session_jsonl_v2` or `codex_rollout_jsonl_v1`), `declared_size_bytes` (canonical decimal string)                                                                                                                                                                                                                                         | Begin one per-connection chunked import after admitting its declared total source size.                                                                                                                                                                                                                                                                                         |
| `append_conversation_import`            | `chunk` (nonempty canonical padded base64 string carrying at most 4 MiB decoded bytes)                                                                                                                                                                                                                                                                          | Append exact source bytes to the import in progress on this connection.                                                                                                                                                                                                                                                                                                         |
| `commit_conversation_import`            | none                                                                                                                                                                                                                                                                                                                                                            | Verify the assembled size, then convert and idempotently resolve or insert the complete source.                                                                                                                                                                                                                                                                                 |
| `abort_conversation_import`             | none                                                                                                                                                                                                                                                                                                                                                            | Discard the import in progress on this connection.                                                                                                                                                                                                                                                                                                                              |
| `begin_blob_upload`                     | `expected_digest` (canonical blob-digest string), `expected_length_bytes` (positive canonical decimal string)                                                                                                                                                                                                                                                   | Begin one per-connection user-attachment upload or report that the routed store already holds verified bytes.                                                                                                                                                                                                                                                                   |
| `append_blob_upload`                    | `chunk` (nonempty canonical padded base64 string carrying at most 4 MiB decoded bytes)                                                                                                                                                                                                                                                                          | Append exact bytes to the blob upload in progress on this connection.                                                                                                                                                                                                                                                                                                           |
| `commit_blob_upload`                    | none                                                                                                                                                                                                                                                                                                                                                            | Verify, publish, and catalog the assembled blob upload.                                                                                                                                                                                                                                                                                                                         |
| `abort_blob_upload`                     | none                                                                                                                                                                                                                                                                                                                                                            | Discard the blob upload in progress on this connection.                                                                                                                                                                                                                                                                                                                         |
| `read_blob_metadata`                    | `digest` (canonical blob-digest string)                                                                                                                                                                                                                                                                                                                         | Read bounded catalog metadata for one blob.                                                                                                                                                                                                                                                                                                                                     |
| `read_blob_chunk`                       | `digest` (canonical blob-digest string), `offset_bytes` and `length_bytes` (canonical decimal strings)                                                                                                                                                                                                                                                          | Read one exact bounded byte range through the recorded replica catalog.                                                                                                                                                                                                                                                                                                         |
| `create_session_from_imported_frontier` | `command_id` and `imported_conversation_id` (canonical UUID strings), `through_position` (positive canonical decimal string), `relationship` (`resume` or `fork`), `initial_model_selection` (selection object), `model_settings` (settings overlay), `runner_placement` (proposed; placement object or null)                                                   | Create an independent live session seeded through the selected inclusive imported position.                                                                                                                                                                                                                                                                                     |
| `replace_session_defaults`              | `command_id` and `session_id` (canonical UUID strings), `expected_defaults_version` (canonical decimal string), `model_selection` (selection object), `model_settings` (settings overlay), `dangerous_tool_auto_approval` (boolean), `system_prompt` (string or null)                                                                                           | Install one complete immutable defaults epoch as the user actor, conditional on the exact current epoch.                                                                                                                                                                                                                                                                        |
| `reconcile_turn`                        | `command_id`, `session_id`, and `expected_active_turn_id` (canonical UUID strings), `content` (ordered content-part array), `expected_defaults_version` (canonical decimal string), `model_settings` (settings overlay)                                                                                                                                         | Supply the user reconciliation decision for the named turn parked on an ambiguous model call, accepting `content` as its immediate successor origin.                                                                                                                                                                                                                            |
| `stop_turn`                             | `command_id`, `session_id`, and `expected_active_turn_id` (canonical UUID strings), `content` (ordered content-part array), `expected_defaults_version` (canonical decimal string), `model_settings` (settings overlay), `descendant_scope` (`parent_alone` or `parent_and_descendants`)                                                                        | Apply the accepted interrupt treatment to the named active turn, accepting `content` as its immediate-successor origin and explicitly selecting delegated-child scope.                                                                                                                                                                                                          |
| `decide_tool_request`                   | `command_id`, `session_id`, and `tool_request_id` (canonical UUID strings), `decision` (a decision object below)                                                                                                                                                                                                                                                | Supply the user decision for one pending tool request through the canonical decision command.                                                                                                                                                                                                                                                                                   |
| `override_denied_tool_request`          | `command_id`, `session_id`, and `tool_request_id` (canonical UUID strings)                                                                                                                                                                                                                                                                                      | Record one one-shot user override of the named delegate-denied tool request through the canonical override command.                                                                                                                                                                                                                                                             |
| `read_session_defaults`                 | `session_id` (canonical UUID string), `defaults_version` (canonical decimal string or null)                                                                                                                                                                                                                                                                     | Read one complete immutable defaults epoch: the current one for null, otherwise exactly the named one.                                                                                                                                                                                                                                                                          |
| `list_conversations`                    | `title_contains` (string or null), `origin` (`native`, `imported`, or `all`), `include_archived` (boolean), `page_size` (canonical decimal string), `after` (cursor object or null)                                                                                                                                                                             | Read one filtered unified conversation-summary page across native sessions and imported conversations in unified keyset order.                                                                                                                                                                                                                                                  |
| `read_imported_conversation`            | `imported_conversation_id` (canonical UUID string)                                                                                                                                                                                                                                                                                                              | Read one immutable imported conversation's complete entry inventory, including the positions `create_session_from_imported_frontier` consumes.                                                                                                                                                                                                                                  |
| `list_model_aliases`                    | none                                                                                                                                                                                                                                                                                                                                                            | Read the deployment's complete configured alias-to-direct-selection catalog.                                                                                                                                                                                                                                                                                                    |
| `list_model_capabilities`               | none                                                                                                                                                                                                                                                                                                                                                            | Read the deployment's complete configured per-direct-selection settings-capability catalog.                                                                                                                                                                                                                                                                                     |
| `compact_session`                       | `command_id` and `session_id` (canonical UUID strings), `through_position` (positive canonical decimal string or null)                                                                                                                                                                                                                                          | Append a dedicated-call summary through the exact requested safe position, or through the latest safe boundary for null, without deleting or rewriting transcript history.                                                                                                                                                                                                      |
| `cancel_program_run`                    | `command_id` and `run_id` (canonical UUID strings)                                                                                                                                                                                                                                                                                                              | Terminally cancel the exact named program run without overwriting an existing terminal outcome.                                                                                                                                                                                                                                                                                 |
| `read_runner_status` (proposed)         | `page_size` (canonical decimal string) and `after` (runner-evidence cursor object or null)                                                                                                                                                                                                                                                                      | Read the active and optional pending runner registrations, connection/loss state, advertised availability, retained operation failures, and startup workspace-leak reports, with one bounded evidence page.                                                                                                                                                                     |
| `replace_lost_runner` (proposed)        | `command_id` and `session_id` (canonical UUID strings), `expected_placement_revision` (positive canonical decimal string), and `replacement` (target object)                                                                                                                                                                                                    | Replace the exact current lost placement with a different live runner, atomically activate one pending replacement enrollment, or — for a registration-triggered loss, where it is the only version-one recovery — re-enroll the same runner against its current connection; pinned loss provisions a new workspace boundary, while pre-pin loss returns to unpinned selection. |
| `abandon_lost_runner` (proposed)        | `command_id` and `session_id` (canonical UUID strings), `expected_placement_revision` (positive canonical decimal string)                                                                                                                                                                                                                                       | Terminalize the exact lost placement only after the existing turn-control algebra has left no active turn; queued work remains and later sees only daemon-executable tools.                                                                                                                                                                                                     |
| `promote_pending_runner` (proposed)     | `command_id` and `pending_request_id` (canonical UUID strings)                                                                                                                                                                                                                                                                                                  | Activate the one provisioning-only pending enrollment on the deployment-scoped fact that this daemon's active runner is durably gone; it names and mutates no session placement.                                                                                                                                                                                                |

`create_session` and `create_session_from_template` accept an optional
`lifecycle` object: `start_gate` (`open`, the default, or `held`), `ownership`
(`unmonitored`, the default, or `owned`), and `finish_condition`
(`external_gate`, or `declared` with statement text). A held gate is durable on
the lifecycle satellite: the session stays `created`.

| Type                                 | Additional required members                                                                                                | Meaning                                                                  |
| ------------------------------------ | -------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------ |
| `create_review_target`               | `command_id`, `target_id`, `provider`, `repository`, `subject`, `head_revision`, `base_revision`, `stack_parent_target_id` | Register one immutable external target snapshot.                         |
| `start_review_run`                   | `command_id`, `target_id`, `run_id`, `pass_id`, `workflow`, `session_id`, `accepted_input_id`                              | Admit one run and its sole session-backed pass.                          |
| `activate_review_pass`               | `command_id`, `run_id`, `pass_id`, `turn_id`                                                                               | Bind the queued run and pass to their canonical active turn.             |
| `complete_review_pass`               | `command_id`, `run_id`, `pass_id`, `turn_id`, `output_frontier_id`, `outcome`                                              | Conclude a pass carrying no other typed result.                          |
| `record_review_findings`             | `command_id`, `run_id`, `pass_id`, `turn_id`, `output_frontier_id`, `findings`                                             | Atomically succeed a read-only pass with its complete finding inventory. |
| `record_review_finding_event`        | `command_id`, `run_id`, `pass_id`, `turn_id`, `output_frontier_id`, `finding_id`, `event_ordinal`, `event`                 | Atomically conclude a result-bearing pass and append its finding event.  |
| `reserve_review_external_link`       | `command_id`, `external_link_id`, `finding_id`, `provider`, `object_kind`                                                  | Reserve one provider object identity before an external write.           |
| `attach_review_external_link`        | `command_id`, `external_link_id`, `run_id`, `pass_id`, `turn_id`, `output_frontier_id`, `external_object`, `event_ordinal` | Atomically bind an external object and its exact publication result.     |
| `start_review_orchestration`         | `command_id`, `attempt_id`, `target_id`, `concern_set_version`, four stage template names, `concerns`                      | Freeze one client-driven concern fan-out attempt.                        |
| `record_review_import_outcome`       | `command_id`, `attempt_id`, `pass_id`, `external_link_id`, `context_digest`, `outcome`                                     | Seal imported-context evidence or its terminal incomplete outcome.       |
| `record_review_concern_outcome`      | `command_id`, `attempt_id`, `concern`, `pass_id`, `outcome`                                                                | Seal one frozen concern member.                                          |
| `record_review_judgment_plan`        | `command_id`, `attempt_id`, `analysis_pass_id`, `members`                                                                  | Seal the complete judgment plan over a complete fan-out.                 |
| `record_review_judgment_effect`      | `command_id`, `attempt_id`, `finding_id`, `event_pass_id`, `outcome`                                                       | Seal one planned finding-event application.                              |
| `record_review_repair_outcomes`      | `command_id`, `attempt_id`, `outcomes`                                                                                     | Seal the exact repair-member inventory.                                  |
| `record_review_publication_outcomes` | `command_id`, `attempt_id`, `outcomes`                                                                                     | Seal the exact publication-member inventory.                             |
| `read_review_orchestration`          | `attempt_id`                                                                                                               | Read one complete orchestration attempt.                                 |
| `read_review_target`                 | `target_id`                                                                                                                | Read one immutable target snapshot.                                      |
| `read_review_run`                    | `run_id`                                                                                                                   | Read one run and its optional admitted pass.                             |
| `read_review_finding`                | `finding_id`                                                                                                               | Read one complete finding aggregate.                                     |
| `list_review_findings`               | `run_id`                                                                                                                   | List one run's findings in finding-identity order.                       |

`submit_input` has one optional closed `delivery` object. Its exact variants are
`start_when_idle {}`, `steer { expected_active_turn_id }`, and
`queue { expected_active_turn_id }`. Absence means `start_when_idle`; an
explicit `start_when_idle` is equivalent. `start_when_idle` and `queue` require
a non-null `expected_defaults_version`. Configuration-free `steer` instead
requires that member to be present as JSON null, so it cannot carry an
independent defaults choice or settings override; its `model_settings` overlay
must inherit every member. Both active-work variants name the exact active turn
the client observed; an idle slot or a changed turn is a typed rejection rather
than a retarget. Unknown delivery tags or members, explicit JSON null in place
of a delivery object, and every other correlation of delivery with the nullable
defaults member are malformed.

The `content` member on `submit_input`, `reconcile_turn`, and `stop_turn` is a
nonempty array of at most 256 closed part objects. A text part is exactly
`{ "type": "text", "text": T }`. An attachment part is exactly
`{ "type": "attachment", "digest": D, "kind": K, "media_type": M, "display_filename": F }`,
where `D` is a canonical blob digest, `K` is `image`, `document`, or `file`, and
`F` is a string or JSON null. Adjacent text parts are malformed. The aggregate
text bytes and attachment member bounds are owned by
[blob storage](blob-storage.md); the wire applies them before application
construction. A one-part text array is the sole spelling of text-only content.

The `content` member on transcript `queued` states and `input_accepted` session
events is that same closed ordered parts array. Together with
`transcript_user_entry`, snapshots, follow updates, and reconnect recovery
therefore preserve part order and attachment metadata while never carrying blob
bytes.

### Credential-exclusion administration

**Committed unimplemented functionality.** No present process request, server
message, application command, or repository operation supplies this
administrative surface; the implemented request inventory above remains closed
and rejects it. That surface must be an authorized
`list_credential_exclusions { page_size, after }` read and one
`clear_credential_exclusion` mutation carrying a user-global `command_id` and
one closed `target` object:

- `profile_quarantine { profile, record_generation }` names one profile-wide
  quarantine the clear mutation admits, for a profile of any delivery, `oauth`
  included: one a pool trigger such as `on_rate_limited` or `on_overloaded`
  minted, and one a failed `codex_home` identity walk minted, whose store is
  repaired outside the daemon. It does not name an OAuth-refresh quarantine,
  which that mutation rejects and the listing therefore omits. The union carries
  exactly what can be cleared, so a record the mutation accepts always has a
  request and response representation and a record it rejects has none;
- `membership_exclusion { pool_policy_id, profile, record_generation }` names
  one `avoid_new_sessions` exclusion; and
- `session_displacement { session_id, pool_policy_id, profile, record_generation }`
  names one `switch_next_turn` displacement; and
- `chain_exclusion { session_id, turn_id, pool_policy_id, profile, predecessor_model_call_id }`
  names the exact qualifying predecessor observation that excluded one member
  from an availability-successor chain.

The read lists every active exclusion the caller is authorized to administer
except those the clear mutation rejects, as its exact closed target object, so
the generation or predecessor correlation required by that mutation is
observable even while another pool member remains usable. The filter is the
mutation's own, stated once below and turning on the exclusion's **origin**: it
omits exactly the delivery-origin quarantines that cannot be cleared and lists
every other record, including every throttling exclusion attached to an `oauth`
profile. The read and the mutation must not disagree about what is clearable.
`page_size` is a canonical decimal string from 1 through 100. `after` is either
null or one complete target object and is an exclusive keyset cursor. Results
sort first by the closed target tags in the order above, then by each tagged
target field's owned canonical order — UTF-8 bytes for configured names, UUID
bytes for durable identities, and unsigned numeric order for generations.
`credential_exclusion_page { exclusions, next_after }` returns no more than the
requested count and uses null `next_after` only at the end. Clearing or creating
an exclusion between page requests may change a traversal, so an operator
needing one fresh inventory restarts from null. The read exposes only non-secret
references and correlations.

Both operations are authorized exactly as every other request on this transport
is: reaching the owner-private socket is the authority. The process protocol has
no authentication or authorization exchange, and socket filesystem access is the
deployment boundary, so a connected client may enumerate and clear credential
exclusions. These operations must not be treated as gated by an authority no
contract defines or as safer than the socket. No response code is reserved for a
future authorization failure: client identity, authentication, authorization,
and revocation are undecided
([open questions](../open-questions.md#identity-credentials-and-resource-governance)),
and preallocating one error's semantics would constrain that decision. An
authorizing principal, when introduced, carries its own denial response and its
own existence-hiding rule.

Every identity is a nonempty bounded configuration or durable identity already
owned by the credential contract. `pool_policy_id` is the canonical lowercase,
hyphenated UUID string of the daemon-minted immutable `PoolPolicyId`;
persistence stores that UUID as the policy header's surrogate identity, and
every request, receipt, snapshot, and event uses the same spelling. Each
`record_generation` is a positive canonical decimal string. The mutation
atomically marks only that exact active generation or predecessor correlation
cleared. For a fresh command, the existence of a newer active generation *at the
target's own exact scope* returns `stale_generation` before the named older
generation is considered for `already_cleared`; an operator can therefore never
mistake clearing historical evidence for clearing the current exclusion at that
scope. The comparison is confined to that scope — the profile **and the
exclusion's origin** for `profile_quarantine`, since a policy quarantine and a
delivery quarantine on one profile are independent states and neither makes the
other stale; the pool policy and profile for `membership_exclusion`, and the
session, pool policy, and profile for `session_displacement` — because a newer
generation elsewhere describes a different exclusion the operator did not name.
Without that confinement a still-active target the listing API returned could
not be cleared until every unrelated newer exclusion was cleared first, and a
profile taking continuous triggers in another pool could block the requested
repair indefinitely. A chain target whose named predecessor correlates with no
retained record for that profile and scope, or whose retained record does not
exactly match the named correlation, is `unknown_credential_exclusion` — as is
any other target with no such record. An exact retained record that an earlier
command already marked inactive is not unknown: it follows the `already_cleared`
path below, so the idempotent repair returns one answer rather than depending on
which command ran first. Equal `command_id` replay still returns its stored
receipt before this current-state precedence is evaluated. A quarantine of
**delivery origin** is a broken credential rather than a throttled one, and what
clears it is whatever can re-establish the credential — which differs by which
delivery produced it, so the two are stated separately rather than rejected
together. A rejected daemon-owned OAuth refresh rejects this mutation, because
only re-provisioning can clear it and this daemon has a provisioning command
that does exactly that. A `codex_home` identity walk that failed instead
**accepts** it, because no daemon `codex_home` provisioning transaction exists:
the store is external, an operator repairs it outside the daemon, and rejecting
the clear would leave that quarantine with no transition out at all — the
profile is no longer selected, so no preparation reruns the walk that would
clear it. Accepting the clear is safe because the walk runs at every preparation
and re-quarantines immediately if the home is still broken; the operator's clear
asserts only that the walk should run again. The rejection therefore turns on
the quarantine's origin and never on the profile's delivery. Rejecting by
delivery would conflate the two and leave a rate-limited `oauth` member with no
clearing path at all where its adapter offers no zero-cost probe, which is the
one case the operator command exists for.

Success returns `credential_exclusion_cleared { target, outcome }`, where
`outcome` is `cleared` for the winning transition or `already_cleared` when a
fresh command names that same inactive generation or predecessor correlation.
The inactive record is retained so the latter result is durable. Equal
`command_id` replay returns its original logical receipt before inspecting
current state; structurally different reuse is the ordinary durable-command
conflict. These rules give an indefinite `park` wait a concrete writer without
making a model call or inventing adapter liveness evidence.

The session-placement object is exactly `pathless {}`, `scoped { path }`, or
`root_global_read { path, intent: "acknowledged" }`. A path is one through 64
nonempty dot-separated ASCII label segments; each segment is at most 64 bytes
and admits only letters, digits, hyphen, and underscore, so the complete maximum
is 4,159 bytes including separators. `scoped` requires at least two segments. A
one-segment root path is legal only in the `root_global_read` variant: its
explicit intent records that the session gains global conversation read.
Creation defaults an omitted object to `pathless`; updates always carry the
complete replacement object. Requests, update receipts, and `session_summary`
admit that same complete structural range, preserving exact durable-command
replay and previously stored placement rows.

The review-workflow requests have these shapes. Every `*_id` is a canonical UUID
string, ordinal and count values are canonical decimal strings, and every
nullable member is required with either its value or JSON `null`.

| Type                                 | Additional required members                                                                                                | Meaning                                                                  |
| ------------------------------------ | -------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------ |
| `create_review_target`               | `command_id`, `target_id`, `provider`, `repository`, `subject`, `head_revision`, `base_revision`, `stack_parent_target_id` | Register one immutable external target snapshot.                         |
| `start_review_run`                   | `command_id`, `target_id`, `run_id`, `pass_id`, `workflow`, `session_id`, `accepted_input_id`                              | Admit one run and its sole session-backed pass.                          |
| `activate_review_pass`               | `command_id`, `run_id`, `pass_id`, `turn_id`                                                                               | Bind the queued run and pass to their canonical active turn.             |
| `complete_review_pass`               | `command_id`, `run_id`, `pass_id`, `turn_id`, `output_frontier_id`, `outcome`                                              | Conclude a pass carrying no other typed result.                          |
| `record_review_findings`             | `command_id`, `run_id`, `pass_id`, `turn_id`, `output_frontier_id`, `findings`                                             | Atomically succeed a read-only pass with its complete finding inventory. |
| `record_review_finding_event`        | `command_id`, `run_id`, `pass_id`, `turn_id`, `output_frontier_id`, `finding_id`, `event_ordinal`, `event`                 | Atomically conclude a result-bearing pass and append its finding event.  |
| `reserve_review_external_link`       | `command_id`, `external_link_id`, `finding_id`, `provider`, `object_kind`                                                  | Reserve one provider object identity before an external write.           |
| `attach_review_external_link`        | `command_id`, `external_link_id`, `run_id`, `pass_id`, `turn_id`, `output_frontier_id`, `external_object`, `event_ordinal` | Atomically bind an external object and its exact publication result.     |
| `start_review_orchestration`         | `command_id`, `attempt_id`, `target_id`, `concern_set_version`, four stage template names, `concerns`                      | Freeze one client-driven concern fan-out attempt.                        |
| `record_review_import_outcome`       | `command_id`, `attempt_id`, `pass_id`, `external_link_id`, `context_digest`, `outcome`                                     | Seal imported-context evidence or its terminal incomplete outcome.       |
| `record_review_concern_outcome`      | `command_id`, `attempt_id`, `concern`, `pass_id`, `outcome`                                                                | Seal one frozen concern member.                                          |
| `record_review_judgment_plan`        | `command_id`, `attempt_id`, `analysis_pass_id`, `members`                                                                  | Seal the complete judgment plan over a complete fan-out.                 |
| `record_review_judgment_effect`      | `command_id`, `attempt_id`, `finding_id`, `event_pass_id`, `outcome`                                                       | Seal one planned finding-event application.                              |
| `record_review_repair_outcomes`      | `command_id`, `attempt_id`, `outcomes`                                                                                     | Seal the exact repair-member inventory.                                  |
| `record_review_publication_outcomes` | `command_id`, `attempt_id`, `outcomes`                                                                                     | Seal the exact publication-member inventory.                             |
| `read_review_orchestration`          | `attempt_id`                                                                                                               | Read one complete orchestration attempt.                                 |
| `read_review_target`                 | `target_id`                                                                                                                | Read one immutable target snapshot.                                      |
| `read_review_run`                    | `run_id`                                                                                                                   | Read one run and its optional admitted pass.                             |
| `read_review_finding`                | `finding_id`                                                                                                               | Read one complete finding aggregate.                                     |
| `list_review_findings`               | `run_id`                                                                                                                   | List one run's findings in finding-identity order.                       |

`complete_review_pass.outcome` is exactly `succeeded`, `failed`, `blocked`, or
`cancelled`. Success requires non-null `turn_id` and `output_frontier_id`.
Failure and blockage require a non-null turn and a null frontier. Cancellation
always requires a null frontier and admits either a non-null admitted turn or
null when cancellation preceded activation. Every other correlation is
malformed. Its receipt carries the exact matching terminal pass state; queued or
running is malformed in `review_pass_completed`.

A finding `event` is exactly one of `accepted {}`, `rejected { reason }`,
`duplicate { canonical_finding_id }`, `superseded { successor_finding_id }`,
`stale {}`, `fixed {}`, or `blocked_with_reason { reason, external_link_id }`.
The blocked link member is required and nullable. `output_frontier_id` is itself
required and nullable: `blocked_with_reason` requires JSON null because its pass
is blocked, while every other finding event requires the successful output
frontier. Reasons are nonempty exact text, reject U+0000, and carry at most
65,536 UTF-8 bytes. Duplicate and superseded references cannot name the finding
receiving the event. `posted` is absent from this request: publication is the
reservation-then-attachment operation, whose successful attachment appends the
posted event.

An orchestration start carries one through 32 ordered `{ key, template_name }`
concern objects. Keys and template names are nonempty, each inventory is unique
by key and by name, keys carry at most 1,024 UTF-8 bytes without U+0000, and
template names use the existing 128-byte lowercase ASCII template-name grammar.
The daemon resolves and copy-binds every supplied name; the durable projection
reports canonical lowercase 64-hex template digests rather than treating names
as mutable authority. `concern_set_version` is a checked review key. Context and
template digests are exactly 64 lowercase hexadecimal characters; uppercase,
shortened, or prefixed spellings are malformed.

Import and concern outcomes use the closed `succeeded`, `failed`, `blocked`, and
`cancelled` vocabulary. Successful import requires non-null pass and context
digest; its external link may be null. Failed or blocked import requires a
non-null pass and null link and context. Cancelled import admits a null pass but
also requires null link and context. A concern outcome requires a non-null pass
unless cancelled. Judgment dispositions are exactly `accepted {}`,
`rejected { reason }`, `duplicate { canonical_finding_id }`,
`superseded { successor_finding_id }`, or `stale {}`. A judgment plan carries at
most 1,024 members and cannot repeat a finding identity. Judgment-effect outcome
is `applied`, `failed`, `blocked`, or `cancelled`; exactly `applied` requires a
non-null `event_pass_id`. Repair outcome is `fixed`, `failed`, `blocked`, or
`cancelled`; exactly `fixed` requires a non-null event pass. Publication outcome
is `published`, `failed`, `blocked`, or `cancelled`; exactly `published`
requires a non-null external link. Repair and publication arrays each carry at
most 1,024 members and cannot repeat a finding identity. Unknown outcome tokens,
tagged members, array members, and any contradictory nullable evidence are
malformed before application construction.

The orchestration read snapshot is
`{ attempt_id, target_id, state, concern_set_version, stage_template_digests, concerns, counts }`.
State is exactly `awaiting_import`, `import_incomplete`, `awaiting_concerns`,
`fanout_incomplete`, `awaiting_judgment`, `awaiting_judgment_effects`,
`judgment_incomplete`, `awaiting_repair`, `repair_incomplete`,
`awaiting_publication`, `publication_incomplete`, or `complete`. Each frozen
concern carries `{ key, template_digest, status, pass_id }`; status is
`pending`, `succeeded`, `failed`, `blocked`, `cancelled`, or `superseded`.
Pending requires a null pass; success, failure, blockage, and supersession
require a non-null pass; cancellation admits either. The required counts are
`finding_count`, `judgment_member_count`, `judgment_effect_applied_count`,
`repair_fixed_count`, and `publication_published_count`. Each is at most 1,024;
judgment members cannot exceed findings, and applied, fixed, or published counts
cannot exceed judgment members. The snapshot repeats the same nonempty unique
one-through-32 concern inventory and checked concern-set key. Before import
every concern is pending. Awaiting concerns requires a pending member; fan-out
incomplete requires a complete inventory with at least one non-success; every
state from awaiting judgment onward requires every concern to have succeeded.
Judgment-effect states require strictly fewer applied effects than plan members,
while repair and later states require equality. Counts belonging to a later
barrier must remain zero until that barrier can exist. Any state, status, or
count combination violating those relations is an impossible server projection
and is malformed. The whole projection is reconstructed inside one read-only
repeatable-read transaction, so every reported fact comes from a single database
snapshot and no two of them can disagree. That read is pure and acquires no lock
any writer waits on: submitting input, starting or advancing a turn, recording
an approval, and every review mutation proceed unimpeded while a snapshot is
under construction.

<a id="snapshot-reader-capacity"></a>

Snapshot construction consumes one unit from the shared pool-capacity budget.
That budget admits at most eight concurrent snapshot readers and is also bounded
to leave at least two configured connections outside snapshot work; a pool with
fewer than three configured connections cannot start the process listener.
Daemon shutdown cancels the capacity wait and releases its reservation.

Target subjects, workflows, pass and finding state, finding content, events, and
external-link vocabularies are the distinct wire representations of the
[review-workflow domain](review-workflows.md). The daemon constructs domain and
application values before mutation and rejects an incompatible target shape,
workflow/pass pair, session/input/turn binding, terminal frontier, finding
inventory, cross-run reference, orchestration barrier, event, or attachment
without normalizing it.

A selection object is exactly one of:

- `{"kind":"direct","selection_id":"<canonical UUID>"}`;
- `{"kind":"alias","alias_id":"<canonical UUID>"}`.

The proposed runner-placement object has exactly:

- `selector`, either `{"type":"runner","runner_id":"<canonical UUID>"}` or
  `{"type":"capability_class","name":"<checked name>"}`;
- `working_directory`, a bounded string or JSON null for runner default;
- `credential_profile`, a checked name or JSON null;
- `workspace`, either `{"type":"none"}` or
  `{"type":"repository_worktree","repository":"<checked key>"}`;
- `sandbox_profile`, exactly `workspace-restricted` or `ambient`; and
- `tool_permission_overrides`, an object from at most 64 exact checked tool
  names to `auto` or `confirm`.

The object is complete immutable placement intent. Every member is required and
carries either its value or JSON null, and every member is an independent axis
under [session composition](runner-protocol.md): no member is inferred from
another and none is filled in by the daemon. A null `credential_profile` is the
explicit choice of no credential rather than a request that the daemon select
one; a `{"type":"none"}` workspace is admissible with either sandbox profile and
with any credential choice, and paired with a present `working_directory` it
selects a plain-directory workspace. The identical object is carried by all
three creation requests, including `create_session_from_template`, so a template
choice and a placement choice compose and neither discards the other. Unknown or
duplicate override names, an override for a tool outside the compiled runner
registry, unadvertised availability, malformed working directory, or
unconfigured repository fails before command identity is claimed. The terminal
defaults omitted sandbox input to `workspace-restricted`, but the wire never
omits it. `ambient` therefore always reflects an explicit command-line choice. A
null `runner_placement` creates a daemon-only session.

The proposed runner replacement target is exactly one of:

- `{"type":"runner","runner_id":"<canonical UUID>"}` for a different current
  live runner;
- `{"type":"pending_enrollment","request_id":"<canonical UUID>"}` for the one
  provisioning-only checked request visible in runner status; or
- `{"type":"same_runner_reenrollment","runner_id":"<canonical UUID>"}` for the
  lost runner itself.

The second arm promotes that exact pending candidate only inside the successful
replacement command. The third arm is admitted only when the named runner is the
lost runner, is currently connected under its existing enrollment, and the loss
was triggered by its own re-registration; every other use of it records
`replacement_same_runner`, which therefore remains the rejection for naming the
lost runner after any other loss source. No arm creates automatic placement
authority.

A decision object is exactly one of:

- `{"type":"approve"}`;
- `{"type":"deny","reason":"<string>"}`.

The wire surface requires a denial reason. The daemon validates it against the
domain denial-reason contract — nonempty, at most 1,024 UTF-8 bytes, no
surrounding POSIX whitespace, no control scalars — and returns
`invalid_request`, claiming no command identity, when the reason cannot
construct it. The domain command itself admits an absent reason; that narrower
wire posture keeps every client-recorded denial explainable to the model.

Canonical UUID strings are lowercase hyphenated values. Nil and all-ones command
identities fail request validation before application construction. The server
does not generate mutation command identities on a client's behalf. Equal
command retransmission therefore reaches the durable replay boundary; a new
request identity does not change command meaning (INV-012). The expected
defaults version is part of the canonical submit payload. A caller retries an
ambiguous submission with the same command identity, session, content, expected
version, and treatment; changing any of them is a conflicting reuse, not
recovery. The expected defaults version is likewise part of a replacement's
canonical payload; exact recovery preserves it together with the complete model
selection and dangerous-tool auto-approval posture.

Review mutations, including every orchestration start and stage seal, use the
same user-global command namespace. The terminal prints a generated identity
before request I/O; an exact retry supplies `--command-id` and the same scalar
and file content. Equality is the closed operation kind plus SHA-256 of the
validated semantic request object; frame version and request identity are
excluded. Before hashing, the daemon canonicalizes a complete
`record_review_findings` request into finding-identity order, so array order
does not distinguish the same semantic inventory. Before an orchestration
effect, the adapter commits an immutable typed intent binding command identity,
semantic digest, attempt, and closed operation kind. The effect and an
append-only marker naming its exact command commit atomically. A concern marker
also binds the immutable claim sequence it created; later replacement of a
failed claim cannot redirect replay to the successor. They are followed, while
the daemon still holds exclusive review-mutation admission, by an append-only
recovery result containing the operation-derived stage and progress. The typed
user-global receipt then atomically replaces the intent. The intent, recovery,
and receipt constraints keep operation kind, stage, and constituent progress
coherent. If the process stops after the effect but before recovery, the intent
preserves exact-retry identity and the effect marker proves that this command
caused the operation-specific durable effect; the retry must also authenticate
that equal effect independently of the aggregate current stage, reconstruct
progress for the original operation stage without later facts, and finish the
recovery and receipt. A fresh command at the later stage remains rejected. If
receipt commit is lost, an equal retry materializes it from the recovery result
and returns that original operation-stage response rather than later aggregate
state. A recorded receipt is inspected before mutable aggregate-state
preconditions. A semantically equal start similarly preserves
`review_orchestration_started`; a different frozen attempt fails closed. Fresh
run admission creates its run and sole pass in one transaction; recovery also
recognizes and completes a matching committed run-only intermediate. Reusing a
command identity for a different digest, operation kind, aggregate payload,
complete inventory, event, or attachment fails closed. This representation is
the durable review-command contract.

The daemon admits one review mutation at a time and retains that admission
through claim inspection, aggregate effect recovery, and receipt recording. A
decoded review mutation retains its inbound-frame budget slot while it waits for
that admission, so queued maximum-size requests remain inside the same 64 MiB
aggregate frame budget; the frame slot is released after the review permit is
acquired and before application handling. Review reads remain concurrent. This
bound composes with the snapshot-reader reservation so an open claim cannot form
a circular pool wait with its nested aggregate transaction; this section owns
the review-command admission capacity.

Before application construction, `replace_session_defaults` validates the
requested direct selection or alias against the process's immutable model
catalog. An unknown catalog identity is `invalid_request` and claims no command
identity. This check is read-only: the protocol does not register models or
change the catalog.

The `system_prompt` member is required on `create_session` and
`replace_session_defaults`: JSON null states explicitly that the complete
defaults carry no prompt, and a string carries the exact prompt. An absent
member is a `malformed_frame`. A present prompt is nonempty exact Unicode text
that rejects U+0000. The daemon applies
`numeric_bounds.max_system_prompt_utf8_bytes` before domain construction;
`"none"` removes that policy while the frame-size guard remains structural.

A metadata object has exactly `title` (string or null), `tags` (string array),
`attributes` (an object whose values are strings), and `archived` (boolean).
Present titles, tags, and attribute keys are nonempty; every metadata string
rejects U+0000. Attribute values may be empty. Duplicate tags produce
`malformed_frame`. Repeating a decoded attribute member name also produces
`malformed_frame` under the frame-wide duplicate-object-member rule above. Tag
order and attribute member order do not affect durable command equality. Wire
validation enforces the structural capacity contract: at most 262,144 total
UTF-8 bytes across the object and at most 1,024 UTF-8 bytes in each tag or
attribute key. The daemon separately applies the deployment-owned tag and
attribute count policies before domain construction; either may be `"none"`.

`list_session_metadata` admits the configured metadata page-size range.
`required_tags` is an exact AND-filter, a present `title_contains` is nonempty
and applies an exact case-sensitive substring filter, `include_archived = false`
selects the default all-non-archived view, and `after_session_id` is an
exclusive keyset cursor. An empty tag array, null title query, false archive
switch, page size 50, and null cursor form the ordinary default request; the
wire carries every field explicitly. The daemon applies the configured
required-tag count policy. Tags are nonempty, reject U+0000, and carry at most
1,024 UTF-8 bytes each; a title query rejects U+0000; and all required tags plus
the title query carry at most 262,144 UTF-8 bytes. Every metadata-object and
metadata-filter string, shape, cardinality, and byte rule in these two
paragraphs is client-frame field or size validation. A violation returns
`malformed_frame` before application construction. `invalid_request` is reserved
for the fail-closed case where an admitted wire value cannot construct the
corresponding application input; no currently valid metadata frame is intended
to reach that mapping error.

`list_conversations` is the unified read surface over both conversation record
classes and mirrors the metadata list's pagination discipline exactly: one
configured page per request, with no silent truncation. It is a plain keyset
read over the authoritative session, current-defaults, metadata, and
imported-conversation tables in one repeatable-read, read-only transaction — no
materialized view, cache, or analytical artifact stands between the caller and
committed state, so every listed row is transactionally fresh. The unified order
is by conversation identity UUID value, with a native session ordered before an
imported conversation carrying a theoretical equal identity value. A cursor
object has exactly `origin` (`native_session` or `imported_conversation`) and
`conversation_id` (canonical UUID string); `after` is the exclusive keyset
cursor at that total position, so no row can be skipped at a page boundary. A
present `title_contains` is nonempty, rejects U+0000, carries at most 262,144
UTF-8 bytes, and applies the same exact case-sensitive substring filter to a
present native metadata title or imported display title; an absent title matches
no title query, and a transitional pending imported title survives every title
filter so the read fails closed on it
([conversation-import](conversation-import.md)) rather than silently omitting an
unresolved row. `origin` selects native rows, imported rows, or both;
`include_archived = false` selects the default view excluding archived native
sessions, and imported conversations carry no archive state, so the switch never
affects them. Every bound in this paragraph is client-frame field or size
validation returning `malformed_frame` before application construction, exactly
as for the metadata list.

The daemon maps the client-selected delivery object without reinterpretation:
`start_when_idle` to `StartWhenNoActiveTurn`, `steer` to `NextSafePoint`, and
`queue` to `AfterCurrentTurn`. Omitting `delivery` remains the sequential
default. The protocol therefore never guesses an interrupt, steering, or queued
treatment; the client must select one explicitly.

`reconcile_turn` is the one request that names a treatment explicitly. The
daemon reads whether the named turn is the session's active turn parked in the
`awaiting_model_call_recovery` phase and refuses anything else with `rejected`
and a `turn_not_awaiting_reconciliation` detail, before any durable command is
recorded. That precondition is skipped in exactly the two cases the durable
boundary owns the answer to: a command identity that already names durable
intent replays its recorded result unconditionally (INV-012), because the first
handling already released the wait it would now be refused for; and an absent
session is left to the transaction's recorded `session_not_found`. Every other
request reaches the authoritative transaction, which applies the accepted
`Interrupt` delivery in
[turn-lifecycle-and-scheduling](turn-lifecycle-and-scheduling.md#occupied-slot-input-handling)
and revalidates the expected active turn under the scheduler lock. A caller that
loses a race there receives `active_turn_mismatch` when another turn took the
slot, or `no_active_turn` when the winning decision left the slot empty. The
verb therefore supplies the interrupt authority a reconciliation-required
terminal already requires and never becomes a standalone active-turn stop.

`create_session_from_imported_frontier` addresses the sealed domain frontier by
`imported_conversation_id` plus its positive inclusive `through_position`. For
an unclaimed command, the daemon loads the immutable imported aggregate and
resolves that position to the canonical entry identity and frontier before
invoking the application service. For a claimed command, it first compares the
recorded canonical frontier, relationship, and defaults and returns equal replay
or conflicting reuse without resolving the wire address again. An absent
conversation or position returns `not_found` without claiming the command.
Resume and fork are explicit and have the semantics owned by
[sessions-and-transcript](sessions-and-transcript.md#create-from-an-imported-frontier).

`stop_turn` is the explicit stop verb, and it is the accepted `Interrupt`
delivery in
[turn-lifecycle-and-scheduling](turn-lifecycle-and-scheduling.md#occupied-slot-input-handling)
on the wire — no standalone active-turn cancellation command exists (INV-029).
The request names the exact turn the caller observed active and carries the
successor content the interrupt algebra requires: an applied interrupt is the
only cancellation authority, and it binds an immediate-successor origin in the
same transaction. Terminalization flows through the lifecycle — a running turn
with no prepared call, or a prepared call, cancels directly, while an issued
call first enters its durable `cancellation_requested` state and the turn
terminalizes when physical cancellation resolves. The authoritative transaction
validates the expected active turn under the session lock and records every
refusal as a typed rejection: `no_active_turn` when no turn holds the slot,
`active_turn_mismatch` for a stale expected turn, `interrupt_already_applied`
when a distinct earlier stop already carries the proof, and
`interrupt_unavailable_while_awaiting_approval` when the active turn is parked
on a tool-approval wait, which a stop can neither decide nor bypass — the caller
denies the pending request through `decide_tool_request` first, then stops
([tool-loop](tool-loop.md#approval-policy-and-decision-sources) owns the
deny-first caller protocol).

`decide_tool_request` carries the canonical user decision command for one
pending tool request. A claimed command identity reaches the durable replay
boundary unconditionally (INV-012). Otherwise the daemon reads which session
owns the named request and refuses a mismatch with `rejected` and a
`tool_request_not_in_session` detail before any durable command is recorded; the
named session is a routing precondition and is not part of the canonical command
payload. An absent request is left to the transaction's recorded
`tool_request_not_found`. Every other outcome is the recorded result of the
canonical command: an applied decision returns the `tool_request_decided`
receipt, an already-resolved request records `tool_request_already_resolved`,
and a decision naming a later request while an earlier one is undecided records
`tool_request_not_earliest_undecided` naming the exact request owed a decision
first.

`override_denied_tool_request` carries the canonical user override command for
one delegate-denied tool request; its behavior is owned by
[tool-loop](tool-loop.md#approval-policy-and-decision-sources). A claimed
command identity reaches the durable replay boundary unconditionally (INV-012).
Unlike `decide_tool_request`, the named session is part of the canonical payload
— the recorded override is a session-scoped standing fact consumed by a later
proposal — so an other-session request is the transaction's recorded
`tool_request_not_in_session` rejection, not a pre-command refusal. Every
outcome is the recorded result of the canonical command: an applied override
returns the `tool_denial_overridden` receipt; the recorded rejections are
`tool_request_not_found`, `tool_request_not_in_session`,
`tool_request_not_delegate_denied`, `tool_request_not_terminally_denied`, and
`tool_denial_already_overridden`.

Every implemented request in the tables above belongs to the single admitted
vocabulary. The closed-enum decoder rejects any unknown request, response,
event, or nested tagged member rather than interpreting it as an older shape.
Because every durable representation implemented in this tree is expressible at
version `1`, selecting a session never requires a feature-specific version gate.
The proposed runner requests are outside the implemented vocabulary. The
runner-bearing projections named above are implemented at version `1`.

Submitted content carries at most 1 MiB of aggregate text UTF-8. The daemon
applies that boundary before application construction or mutation and returns
`invalid_request` when it is exceeded. This leaves enough space for worst-case
JSON escaping when the same accepted content is projected in a queued turn or
durable update event. This section owns the exact text capacity.

An import source is an exact byte sequence encoded with RFC 4648
standard-alphabet padded base64. A noncanonical spelling is a malformed frame.
The server validates canonical padding and trailing bits in the same decode that
constructs source bytes under the inbound-frame permit; validation does not
construct a second full-size canonical encoding. `MAX_FRAME_BYTES` is 8 MiB
including the newline for every request. A single-shot `import_conversation`
carries the complete source when its exact encoded frame fits. Otherwise the
terminal uses one connection for `begin_conversation_import`, one or more
`append_conversation_import` requests, and `commit_conversation_import`. Each
append carries at most 4 MiB decoded bytes, leaving base64 and maximum-envelope
headroom inside the frame bound.

Begin declares the format and exact total byte count. The daemon admits at most
one in-progress import per connection and rejects the declaration before
assembly when it exceeds `conversation_import.max_source_bytes`. Append accepts
only a nonempty chunk and acknowledges the resulting assembled byte count.
Commit rechecks the configured bound and requires the actual assembled count to
equal the declared count before passing the complete bytes to the converter.
Abort or disconnect discards partial per-connection state. The source path
remains client-local and never appears in a request.

Blob digest strings are exactly `sha256:` followed by 64 lowercase hexadecimal
characters. Blob upload admits at most one in-progress upload per connection and
competes for the process-wide bulk-ingest permit described above. Begin
validates the declared length from 1 through `blob_storage.max_blob_bytes` and
live-verifies a recorded replica in the routed store before it can return
`blob_upload_already_present`; a missing or corrupt replica proceeds through
staged upload repair. Otherwise begin returns `blob_upload_begun` and creates
disk-backed staging. Append accepts only a nonempty chunk of at most 4 MiB
decoded bytes and acknowledges the cumulative count. Commit rejects an empty
staging file and requires its count and streaming digest to match both
declarations, then publishes and catalogues the blob. Abort, disconnect, or a
terminal upload refusal discards staging.

`read_blob_chunk` admits lengths from 1 through 4 MiB. Checked
`offset_bytes + length_bytes` must not overflow or cross the catalogued byte
length. The response is exact rather than truncating at end-of-blob. Upload and
read state, length, digest, and range failures use the exhaustive content-silent
`invalid_request` details below; an absent digest is `not_found`, storage
availability is `unavailable`, an all-missing recorded replica set is
`blob_missing`, a definitively corrupt set with no unavailable candidate is
`blob_corrupt`, an S3 publication whose acceptance remains unknowable after its
single reconciliation pass is `publication_ambiguous`, and an ambiguous catalog
commit is `commit_ambiguous`. Either ambiguity terminally discards the
connection-local staging state and releases the bulk-ingest permit without
claiming success. The terminal client's high-level upload handles
`publication_ambiguous` and `commit_ambiguous` by beginning the same digest,
length, and bytes again rather than retrying commit alone; live verification of
the deterministic routed key then returns already-present or repairs it before
registration. When no replica succeeds, `unavailable` takes precedence over
`blob_corrupt`, which takes precedence over `blob_missing`, because an
unavailable candidate prevents a definitive integrity conclusion.

### Configuration reload

**Committed unimplemented functionality.** The implemented request inventory
above remains closed and rejects it. The reload surface must be one authorized
`reload_configuration` request carrying no members and no `command_id`: the swap
changes process memory alone, so a repeat re-reads and re-validates. Success
returns `configuration_reloaded { reloaded_sections }`, an array of the closed
values `model_catalog`, `session_templates`, and `repository_watch`. Failure
returns `configuration_reload_failed { phase, reason }`, sanitized exactly as
startup logs are, and leaves the running configuration unchanged.
[Configuration and credentials](configuration-and-credentials.md#configuration-reload)
owns which sections are reloadable and the validate-then-swap rule.

## Server messages

Message objects carry a required string `type` and reject fields not admitted by
that variant. Every accepted non-review mutation, conversation-import transport
request, or blob-upload transport request — `create_session`,
`create_session_from_template`, `commission_session`,
`create_session_from_imported_frontier`, `stop_session`, `supersede_session`,
`abandon_session`, `close_session_failed`, `resume_session`, `adopt_session`,
`release_session`, `release_start`, `submit_input`, `reconcile_turn`,
`stop_turn`, `decide_tool_request`, `replace_session_metadata`,
`replace_session_defaults`, `compact_session`, `cancel_program_run`,
`update_session_placement`, `import_conversation`, `begin_conversation_import`,
`append_conversation_import`, `commit_conversation_import`,
`abort_conversation_import`, `begin_blob_upload`, `append_blob_upload`,
`commit_blob_upload`, `abort_blob_upload`, `spawn_session`, `await_session`,
`send_session_message`, `replace_lost_runner`, `abandon_lost_runner`, or
`promote_pending_runner` — produces exactly one of:

- `session_created` with `session_id` and the complete installed
  `model_settings` snapshot;
- `session_lifecycle_command_applied` with `session_id` and `effect`
  (`start_released`, `closed`, `closure_pending { live_turn_id }`, `resumed`,
  `ownership_changed`); a recorded rejection is `rejected` with
  `session_lifecycle_command_rejected { session_id, reason }`;
- `session_commissioned` with the created `session_id` and the `dispatch_id` of
  the append-only recorded fence; an equal replay of the same command identity
  re-emits the committed receipt, and the same identity naming a different
  template, fence, statement, or initial content is a conflicting reuse. Replay
  is resolved from the durable record before the live template catalog is
  consulted, so a committed commission stays discoverable through the exact
  retry even after configuration removed or renamed its template;
- `session_placement_updated` with `session_id`, the positive successor
  `placement_version`, and the complete recorded `placement` object; the client
  accepts it only when the session and placement echo its request and the
  version is exactly one greater than its stated expected version;
- `input_submitted` with `session_id`, `accepted_input_id`,
  `acceptance_position`, `turn_id`, and the complete frozen `model_settings`
  snapshot; a queued submit names the ordinary origin turn held behind its
  expected active turn, and a `stop_turn` acceptance names the accepted
  immediate successor and additionally carries its exact `descendant_scope` and
  canonical decimal `disposition_count`;
- `steering_submitted` with `session_id`, `accepted_input_id`,
  `acceptance_position`, and `source_turn_id`; this is the normal receipt for
  accepted pending steering, which creates no turn;
- `tool_request_decided` with `tool_request_id` and the exact recorded
  `decision` object; the receipt mirrors the recorded applied result and
  intentionally echoes no session, because the session is not part of the
  canonical decision payload;
- `tool_denial_overridden` with the overridden `tool_request_id`; the receipt
  mirrors the recorded applied override result;
- `session_metadata_replaced` with `session_id`, the complete `metadata`
  snapshot installed by that recorded handling, and its non-null `last_writer`;
- `session_defaults_replaced` with `session_id`, the newly installed
  `defaults_version`, complete `model_selection` and `model_settings`, and
  `dangerous_tool_auto_approval`, and the installed `system_prompt` (string or
  null);
- `session_compacted` with `session_id`, `context_compaction_id`, dedicated
  `model_call_id`, exact positive `through_position`, appended
  `summary_entry_id`, and complete `result_frontier_id`; an equal replay returns
  these original values before resolving configuration needed only for a fresh
  call;
- `program_run_cancellation_receipt` with the request's `command_id` and
  `run_id`, plus exactly one closed `outcome` object:
  `{ "kind": "applied", "terminal_state": "cancelled", "result": null }`;
  `{ "kind": "not_found" }`; or
  `{ "kind": "already_terminal", "terminal_state": <standing terminal state>, "result": <standing terminal result> }`;
  under the command-identity claim protocol in
  [identity and commands](identity-and-commands.md), an identical
  `cancel_program_run` request bearing the same `command_id` is a replay and
  returns the originally stored receipt even if the run's standing state later
  changes; reuse of that `command_id` with a different `run_id` or other payload
  is conflicting reuse and is rejected as such;
- `conversation_import_inserted` with `imported_conversation_id`;
- `conversation_import_already_imported` with `imported_conversation_id`;
- `conversation_import_begun` with the admitted `declared_size_bytes`;
- `conversation_import_appended` with the exact `assembled_size_bytes`;
- `conversation_import_aborted` with no additional member;
- `blob_upload_begun` with the `expected_digest` and admitted
  `expected_length_bytes`;
- `blob_upload_already_present` with `digest` and `byte_length`;
- `blob_upload_appended` with the exact `assembled_length_bytes`;
- `blob_upload_committed` with the verified `digest` and `byte_length`;
- `blob_upload_aborted` with no additional member;
- `session_spawned` with `tool_request_id`, `child_session_id`, and the exact
  `relationship`;
- `session_await_registered` with `tool_request_id`, `child_session_id`, and
  `mode`;
- `child_result` with `await_request_id`, `spawning_request_id`,
  `child_session_id`, `outcome`, nullable `content`, closed `reason`, and exact
  `provenance`;
- `session_message_sent` with `tool_request_id`, `message_id`, `direction`, and
  `ordinal` plus the positive recipient-wide `delivery_sequence`;
- `runner_replaced` with `session_id`, `prior_runner_id`, `new_runner_id`,
  successor `placement_revision`, and `sandbox_profile`;
- `runner_abandoned` with `session_id` and `placement_revision`;
- `runner_promoted` with `pending_request_id` and the promoted `runner_id`,
  `enrollment_id`, and `registration_revision`; it names no session, because
  promotion changes no session placement; or
- `error` with a stable `code` and a non-sensitive `message`.

`read_blob_metadata` returns
`blob_metadata { digest, byte_length, replica_count }` and `read_blob_chunk`
returns `blob_chunk { digest, offset_bytes, bytes }`. Lengths, offsets, and the
replica count are canonical decimal strings; `bytes` is canonical padded base64
for exactly the admitted range. A client validates every echo and count before
presenting or writing bytes.

**Implemented behavior.** An accepted goal mutation returns
`goal_transition_applied { session_id, event_ordinal, generation }`. A
`stop_goal` form additionally carries its exact `descendant_scope` and canonical
decimal `disposition_count`; equal replay returns that same complete receipt. A
successful `read_goal` returns `goal_history_start` with the current generation
and immutable statement; `goal_history_state` with the current state; one or
more contiguous `goal_history_item` messages with event ordinal, generation,
event, and provenance; then `goal_history_end { event_count }`. Each of the two
projection frames carries at most one bounded goal text, so even maximally
JSON-escaped text remains below the frame cap. Because every attached lineage
has its commissioning event, `event_count` is the exact number of preceding
items. Before presenting any line, the client validates the complete sequence
and count, replays every event through the admitted lifecycle transitions, and
checks that the replay derives the declared current projection. Goal text uses
the ordinary bounded text grammar; the closed lifecycle, event, reason, and
provenance correlations are owned by [goal mode](goal-mode.md). The daemon
completes an effective-user-private temporary-file spool, then releases both the
decoded goal aggregate and snapshot-reader permit before writing the first
history frame to the connection.

Review mutations return exactly one stable acknowledgement:

- `review_target_created { target_id }`;
- `review_run_started { run_id, pass_id }`;
- `review_pass_activated { run_id, pass_id }`;
- `review_pass_completed { run_id, pass_id, state }`;
- `review_findings_recorded { run_id, pass_id, finding_count }`;
- `review_finding_event_recorded { finding_id, status }`;
- `review_external_link_reserved { external_link_id }`;
- `review_external_link_attached { external_link_id, external_object }`;
- `review_orchestration_started { attempt_id }`; or
- `review_orchestration_advanced { attempt_id, state }`.

The complete-pass receipt accepts only the terminal pass states `succeeded`,
`failed`, `blocked`, and `cancelled`. Generic `succeeded` completion refuses a
read-only-review pass; `record_review_findings` is its only success admission,
including for an empty inventory. Finding-event status is the exact state
derived from the submitted event. Every orchestration acknowledgement must name
the same attempt and a state compatible with the submitted facts and earlier
attempt members. A successful concern may close `fanout_incomplete` when an
earlier concern did not succeed, but an incomplete concern submission cannot
reach judgment. An incomplete judgment effect is `judgment_incomplete`, any
blocked repair is `repair_incomplete`, and publication is `complete` exactly
when every submitted member is published. The terminal refuses any contradictory
acknowledgement.

Single-aggregate reads return `review_target { target }`,
`review_run { run, pass }`, `review_finding { finding }`, or
`review_orchestration { snapshot }`; an absent identity returns `not_found`.
Target snapshots carry the immutable subject and revisions. Run snapshots carry
frozen workflow, policy values, lifecycle, and optional pass identity; the
nullable `pass` carries its exact session/input/origin-turn, lifecycle, optional
turn, and optional successful frontier. Finding snapshots carry immutable
content, derived status, and event count. The orchestration snapshot carries the
complete frozen template and concern inventory plus its current barrier state
and progress counts as specified above.

A successful `list_review_findings` response is
`review_findings_start { run_id }`, zero or more
`review_finding_item { finding }` messages in strictly increasing
finding-identity order, then `review_findings_end { finding_count }`. The client
validates the selected run, ordering, and terminal count before presenting the
list.

A successful `list_templates` response is `templates_start`, zero or more
`template_summary { name, version }` messages in strictly increasing UTF-8 name
order, then `templates_end { template_count }`. `version` and `template_count`
are canonical decimal strings. The sequence is one immutable in-memory catalog
snapshot and becomes authoritative only after ordering and count validation; the
terminal client buffers no prompt or bundle content because none crosses the
protocol.

The proposed `read_runner_status` response is one bounded page:
`runner_status_start`; zero or more `runner_status` messages, of which version
one emits at most one and only a null-cursor request emits registration status;
zero or one `pending_runner_status` message under that same condition; zero
through the requested page size of `runner_operation_failure` and
`runner_workspace_leak` messages in the total order below; and
`runner_status_end { runner_count, failure_count, leak_count, next_after }`.
`page_size` is one through 100 and `after` is an exclusive keyset cursor. The
ordinary first request carries page size 100 and null `after`; a nonnull
`next_after` is copied unchanged into the next request. The terminal failure and
leak counts cover this page, while `runner_count` covers the statuses on this
page. A terminal null cursor proves that traversal reached the end. A daemon
with several enrolled runners needs no new response vocabulary. Each status
carries issued request, runner, enrollment, and registration identities,
connection state (`connected`, `suspect`, or `lost`), registration revision,
advertised capability classes, tool names, credential-profile names, repository
entries — each naming its repository key and optional credential-profile name,
with the configuration meaning of absence owned by
[runner credential lifecycle](configuration-and-credentials.md#runner-credential-lifecycle)
— workspace capabilities, and sandbox profiles. Each `runner_operation_failure`
names its runner, the refused operation's correlation, one closed
daemon-actionable `category` (`credential_unavailable`,
`repository_unavailable`, `sandbox_unavailable`, `workspace_conflict`,
`workspace_cleanup_failed`, or `lease_admission_refused`), and the
runner-authored `detail` object carrying its bounded `code`, `message`, and
structured `payload` verbatim. The daemon bounds that detail and reproduces it
without interpretation; the runner applies the same exact-value redaction it
applies to tool output before sending it, so the detail carries no credential
value, and it carries no absolute host path or configured repository URL. That
`category` set is exactly the closed daemon-actionable set the runner wire
carries ([runner protocol and placement](runner-protocol.md)), member for
member, so every retained failure is serializable here and the projection never
has to choose between omitting one that `failure_count` counts and rejecting the
response. Runner status inspection is therefore the surface on which the
complete runner-specific failure is readable even though daemon logic keys only
off the category. `pending_runner_status` also carries the literal authority
state `provisioning_only`; it is never presented as dispatch-capable. Each leak
names its exact runner id and carries the exact closed `kind`, bounded
runner-root-relative `locator`, lowercase manifest-or-entry SHA-256 digest, and
nullable `session_id` and positive `placement_revision` admitted by that runner
final acknowledged wire report. It never carries an absolute host path,
repository URL, or credential fact. Evidence-kind order is failures before
leaks. A failure key compares the runner identity first, then the correlation
discriminator in the fixed order `workspace_provision`, `workspace_release`,
`lease_offer`, then only that arm's fields in this fixed sequence: provisioning
authorization identity; retired session identity, placement revision,
workspace-manifest identity; or offered lease identity, generation. Each
identity, integer, and manifest identity uses its runner-protocol canonical wire
bytes; comparison is unsigned lexicographic-byte order, and a shorter value
precedes a longer value when one is a prefix. No absent field or field from
another arm participates. A leak key compares runner-identity canonical bytes,
then the complete runner-protocol fact tuple: locator, kind, digest, optional
session, optional placement revision, under that tuple's exact order. The closed
cursor object is therefore either `operation_failure { runner_id, correlation }`
or
`workspace_leak { runner_id, locator, kind, digest, session_id, placement_revision }`
and exactly reproduces the last emitted evidence key, including null optional
members. Runner report admission permits each complete fact tuple at most once,
so runner plus tuple is unique within the published snapshot and an exclusive
cursor never straddles equal leak keys. The daemon fetches at most
`page_size + 1` checked evidence rows, using the extra row only to determine
`next_after`; it never materializes the retained history. Each page is
authoritative for the read that produced it. Concurrent durable additions that
sort at or before a cursor need not appear in that traversal, so a caller that
needs a fresh observation starts again with a null cursor. Why:
operation-failure evidence is append-only and therefore unbounded; a singular
unpaged status read would eventually make durable diagnostics unreadable or
require unbounded memory.

Followers additionally admit
`provider_text_delta { session_id, turn_id, model_call_id, part_index, content }`
only on a `follow_session` response. The three identities correlate the provider
observation to its active session, turn, and model call; `part_index` is the
provider part position as a canonical decimal string; and `content` is one
bounded text fragment. When one adapter delta would exceed the protocol's
fragment bound, signalboxd emits consecutive messages with the same identities
and part index whose contents concatenate to the exact already-redacted delta.
The message has no outbox `cursor`: it is a process-local presentation event,
not a `session_event`, transcript entry, or terminal-evidence fact. The native
client retains at most 8 MiB of concatenated provider text for one active turn
and model call. Crossing that presentation-only bound discards the ephemeral
overlay and requests authoritative synchronization recovery; it does not alter
durable transcript state.

A replayed metadata receipt remains the exact snapshot installed by its original
handling even if a later command has replaced the current metadata. A caller
that needs current state issues `read_session_metadata`.

In the server shapes below, notation such as `queued` or
`terminal { disposition }` means a closed JSON object with `"type":"queued"` or
`"type":"terminal"` plus exactly the named members.

A session summary contains `session_id`, `defaults_version`, `model_selection`,
positive `placement_version`, the complete current session `placement`, and a
required `runner` member. The runner member is either null or a complete object
carrying selector, the current or lost exact runner id when the state names one,
placement revision, sandbox profile, credential-profile name, repository key,
working directory, and state (`unpinned`, `pinned`, `runner_lost_before_pin`,
`runner_lost`, or `runner_abandoned`). The credential-profile name, repository
key, and working directory are each present or JSON null independently, because
the composition axes they project are independent
([runner protocol and placement](runner-protocol.md)). This exact object is the
`runner_projection` below; no listing defines a reduced runner shape. The
selected profile is therefore visible even before execution, and `ambient` is
always printed as ambient. A successful `list_sessions` response is
`sessions_start`, one `session_summary` per result in session-identity order,
then `sessions_end { session_count }`. The summaries are read in one read-only
repeatable-read transaction and spooled from one decoded row at a time before
client output. A slow client therefore retains temporary disk rather than the
complete session catalog in request heap or an open database transaction. The
sequence becomes authoritative only after the end message and count validate.
This avoids an aggregate frame-size limit.

A successful `read_operator_status` response consists of `operator_status`
messages: `kind=start`, zero or more rows from each section in this fixed order,
then `kind=end` with one count per section. The emitted row kinds are
`lifecycle_week` and `lifecycle_deadline_violation`. The daemon reads the two
session-lifecycle metric views in one read-only repeatable-read transaction. It
streams their rows through server-side cursors into a temporary-file spool
before writing the first response frame, so a database or encoding failure
produces no partial successful snapshot and the request retains neither an
unbounded row inventory nor a database transaction while the client reads.

A `lifecycle_week` row carries one calendar week's session-lifecycle metrics:
the week's UTC start as an ISO-8601 date, and each metric as its exact numerator
and denominator rather than as a ratio, so a week whose population is empty
carries no rate at all instead of a zero. The pairs are the completion failure
rate over the trimmed weekly terminal cohort, the `failed_unknown` count inside
that numerator, overflow incidence over the untrimmed cohort, the finished share
of exactly those overflow sessions, and the wall rate over the week's dispatch
cohort, the walls recorded in the week whatever cohort they belong to, and the
two cause-completeness axes. Every numerator is at most its own denominator, the
trimmed cohort is at most the untrimmed one, and `failed_unknown` is at most the
completion-failure numerator, because each is a subset relation the definitions
establish rather than a coincidence of one read.

A `lifecycle_deadline_violation` row names one owned non-terminal session whose
armed deadline obligation is unmet: its identity, the non-terminal state it
holds, whether the deadline record is missing outright, and how long the armed
expiry has been past. Exactly one of those last two is present — a session with
no armed record has no expiry to be past — and the section's count is the
`nonterminal_past_deadline` alarm value, whose target is zero.

A held-slot row carries dispatch, repository, dispatch origin, rule, singleton,
ordered session, whole-second held duration, and the independently failing
release clauses. The origin is a tagged choice rather than a number that may be
absent: a rule matching branch workflow-run completion holds its slot from a
branch fact, which names a branch, and every other admitted origin names a pull
request. A branch fact carries no pull request for a singleton to be keyed by,
so a branch origin accompanies only the rule and repository scopes; the
pull-request and stack scopes accompany only a pull-request origin. A singleton
that carries a repository names the row's own repository, and a pull-request
singleton names the same pull request its origin names, because the projection
keys both from the one event the dispatch was admitted from. A stack singleton
instead names the root of the open component that pull request belongs to, which
is a different pull request whenever the origin is not itself that root, so the
stack axis carries no such equality. A queued-obligation row carries obligation,
rule, singleton, first and latest event, collapsed match count, whole-second
wait duration, occupying dispatch and sessions, positive remaining cooldown when
any, and the view's ready decision. The count and the two events move together:
an obligation opens naming one evaluated event as both endpoints with a count of
one, and each later coalesced evaluation replaces the latest event with a
distinct one and increments the count, so the count stands at one exactly while
the two endpoints name the same event. The occupying dispatch is optional
independently of the sessions it would name: a watch dispatch names its identity
and its whole admitted session inventory, while an obligation blocked by an
independently commissioned live session lists exactly that one session and no
dispatch, because the obligation retains a single external blocker. Readiness is
the view's whole decision — excluding a dispatch or external session holding the
target, a parked obligation, and a spent attempt budget — narrowed only so that
a cooldown expiring mid-read cannot report readiness alongside a positive
remaining cooldown. An infinite eligibility timestamp is represented as a
never-eligible cooldown rather than a numeric duration. A convergence row
carries repository and pull request, head and base revisions, base branch,
mergeable state, review decision, unresolved-thread and gating-check counts,
sorted non-green check names, verdict, optional durable seal, and whole-second
assessment age. The verdict agrees with the evidence carried beside it: an
assessment settles unconverged exactly when the pull request carries any
blocker, so a converged verdict — internally converged or merge ready — carries
no unresolved thread, no non-green check, a mergeable provider state, a positive
gating-check count, and no requested change. Exactly one durable blocker, an
unsettled provider snapshot, is not carried on the wire, so an unconverged
verdict remains admissible beside wholly clean carried evidence. The base branch
is what separates the two converged verdicts: a merge-ready verdict is settled
only against `main`, an internally-converged verdict only against another
branch, and an unconverged verdict against either. The seal takes no such
pairing, because it is retained from the assessment that produced it and
outlives later ones, so a pull request retargeted after sealing carries it
beside a base branch that verdict could not have been settled against. Each
non-green check name is canonical padded base64 of its exact UTF-8 bytes on the
wire, keeping the complete admitted 10,000-name inventory below the frame cap
even under worst-case JSON escaping. A pending-clearance row carries repository
and pull request, current and reviewed heads, review identity, reviewer, and
whole-second pending duration. The structured identifiers these rows carry are
admitted by grammar rather than by width alone, each mirroring the domain
constructor and durable check that produced it: a repository is a canonical
lowercase `namespace/name` slug whose segments are neither empty nor a bare dot
or double dot, a rule identity is ASCII letters, digits, hyphens, underscores,
and dots, a branch name follows git's ref-name rules, and a reviewer is a
lowercase login with an optional App-bot suffix. A check name and a review node
identity remain unstructured text, which is all their durable counterparts
require. Every duration is clamped nonnegative and sampled against the database
transaction timestamp, not a client clock.

Identifiers are canonical UUID strings. Request identities, ordinal versions,
indices, counts, and outbox cursors are canonical decimal strings, preserving
their full unsigned 64-bit range without JSON-number precision loss.

The metadata list is a bounded sequence:

1. `session_metadata_page_start`;
2. zero through the requested page size of `session_metadata_summary` messages
   in strictly increasing session-identity order; and
3. `session_metadata_page_end { session_count, next_after_session_id }`.

Each summary carries `session_id`, current `defaults_version`,
`model_selection`, `dangerous_tool_auto_approval`, `title`, sorted `tags`,
`archived`, and `last_writer`; the runner proposal also requires the exact
`runner_projection`. `dangerous_tool_auto_approval` is a JSON boolean: `false`
encodes domain `Disabled` and `true` encodes domain `ApproveAll`. Tags are
strictly increasing by lexicographic UTF-8 byte sequence. Each summary applies
the deployment's tag-count policy and the metadata object's 262,144-byte
aggregate UTF-8 bound across its title and tags, not merely to each member
independently. Attributes are intentionally absent from the list projection. The
end cursor is null when no later match existed in the page snapshot; otherwise
it equals the last emitted session identity. The page sequence is spooled before
output and becomes authoritative only after its count, ordering, and cursor
validate.

The unified conversation list is the same bounded sequence shape:

1. `conversation_page_start`;
2. zero through the requested page size of `conversation_summary` messages in
   strictly increasing unified cursor order; and
3. `conversation_page_end { conversation_count, next_after }`.

Each summary carries one closed `conversation` object tagged by `origin`. A
`native_session` summary carries `session_id`, the optional exact metadata
`title`, `archived`, and the current `defaults_version`; the runner proposal
also requires the exact `runner_projection`. An `imported_conversation` summary
carries `imported_conversation_id`, the optional exact source-derived display
`title` ([conversation-import](conversation-import.md) owns the derivation), the
total normalized `entry_count` — the greatest `through_position` an imported
continuation may select — and the exact stored `source_format`
(`claude_code_session_jsonl_v1`, `claude_code_session_jsonl_v2`, or
`codex_rollout_jsonl_v1`). Neither summary materializes transcript, entry, or
raw-record content; the per-entry read surfaces retain that authority. The end
cursor is null when no later match existed in the page snapshot; otherwise it
names the last emitted summary's origin and identity. The page sequence is
spooled before output and becomes authoritative only after its count, ordering,
and cursor validate.

The deployment model-alias catalog is one ordered sequence:

1. `model_aliases_start`;
2. zero through 10,000 `model_alias_summary { alias_id, selection_id }` messages
   in strictly increasing alias-identity order; and
3. `model_aliases_end { alias_count }`.

Both identities are canonical UUID strings. The catalog is a current
deployment-configuration read, not durable session state: an alias summary
states the direct selection that accepting a new alias request would freeze at
that moment. The read exposes no provider credential, provider-native model
identifier, or mutable configuration operation. Existing sessions and accepted
inputs retain their previously frozen model-selection semantics when the
deployment later changes an alias.

The daemon rejects a deployment configuration containing more than 10,000
aliases, and the native client enforces that same terminal catalog bound before
presenting it.

The model-capability catalog is the parallel ordered sequence
`model_capabilities_start`, zero through 10,000 `model_capability_item` messages
in direct-selection identity order, and
`model_capabilities_end { capability_count }`. The item contains only the
client-visible direct selection identity, ordered reasoning-level and
provider-tagged service-tier sets, fast-mode support Boolean, and the ordered
nonempty `input_modalities` set. Modalities use exactly `text`, `image`, or
`document`; `text` is always present, duplicates are invalid, and projection
uses that closed order regardless of configuration order, as owned by the
[static model capability catalog](configuration-and-credentials.md#the-static-model-alias-and-web-fetch-catalog).
The exact settings vocabulary and the prohibition on exposing an alternate fast
serving identity are owned by
[model/session settings](model-session-settings.md). Reasoning-level and
service-tier overlay members admit `inherit`, `provider_default`, or `value`;
fast-mode overlay members admit only `inherit` or a `value` of `disabled` or
`enabled`. The same vocabulary carries the closed `unsupported_reasoning_level`,
`unsupported_fast_mode`, and `unsupported_service_tier` rejection details; each
names the direct selection, and the value-bearing forms retain the unsupported
value.

`session_metadata` is the successful single-session read and
`session_metadata_replaced` is the successful write receipt. Both carry
`session_id`, the complete metadata object, and `last_writer`. The initial
unwritten snapshot has the empty non-archived metadata object and a null
`last_writer`; an applied replacement always has a non-null last writer. A
last-writer object has `updated_at_unix_micros` (canonical nonnegative decimal
microseconds since the Unix epoch) and one closed actor object: `user`, `core`,
`model { turn_id }`, `recovery`, or `tool { tool_request_id }`. That inventory
is exactly the durable actor inventory, because the projection is total over
what storage admits: `replace_session_metadata` on this boundary always writes
the user actor, but the tool-facing replacement constructor writes a tool actor
for the session-status tool, and a writer with no wire form would fail an
otherwise valid read as an encode invariant rather than degrade a field. Actor
is provenance, not wire authentication or authorization.

`session_defaults_replaced` is the successful defaults write receipt. It echoes
the complete installed defaults and names the exact successor epoch. An equal
command replay returns that original receipt even after later epochs exist;
current state is observed through metadata or defaults reads.

`session_defaults` is the successful `read_session_defaults` response, with
`session_id`, the read `defaults_version`, complete `model_selection` and
`model_settings` snapshot, `dangerous_tool_auto_approval`, and the exact
`system_prompt` (string or null). A null request version reads the epoch named
by the session's current pointer; a named version reads exactly that immutable
epoch, so the response is stable under later replacements. The `not_found` error
covers both an absent session and a named epoch that was never installed.

A successful `read_imported_conversation` response is a bounded sequence:

1. `imported_conversation_start { imported_conversation_id }`;
2. one `imported_conversation_entry` per normalized entry, in imported position
   order; and
3. `imported_conversation_end { imported_conversation_id, entry_count }`.

Each entry carries its one-based `position`, `imported_entry_id`, the exact
`source_speaker` attestation, a `content_kind` discriminator over the closed
normalized content vocabulary, and a required `text_preview` member. Positions
are the contiguous sequence `1..=entry_count`, so `entry_count` is also the
greatest position `create_session_from_imported_frontier` admits. The client
validates that contiguity and the terminal count before presenting any row. The
daemon reads the complete checked aggregate through the same repository load the
continuation command performs, spools the whole sequence, and streams it, so a
slow client retains temporary disk rather than the aggregate.

`content_kind` names the entry's normalized content variant: `source_event`,
`source_message_block`, `text`, `tool_call`, `tool_result`, `thinking`,
`redacted_thinking`, `document`, or `message_content_absent`. A transcript
snapshot reaches its `text` arm only for absent or unattested text, because
attested text takes `transcript_text_entry` there; an inspection row has no such
split and uses `text` for every `Text` content.

`text_preview` is JSON `null` for every entry carrying no exact attested text —
every non-`Text` content, and `Text` whose value is unattested or explicitly
absent. A present preview on any `content_kind` other than `text` is a
contradictory frame and is rejected rather than presented. Otherwise it is
`{ preview, truncated }`, where `preview` is the entry's exact leading Unicode
scalar sequence cut at a scalar boundary within 256 UTF-8 bytes and `truncated`
states whether exact text remains beyond it. Attested empty text therefore
previews as exact empty text, which the null member cannot be confused with. A
`truncated = true` preview is nonempty, because the cut always keeps at least
one scalar of nonempty text. The projection exposes no imported content a
transcript snapshot does not already carry: it bounds exactly the attested text
that snapshot carries in full and adds nothing for any other content. The
immutable imported aggregate remains the authority for complete normalized
content and verbatim raw source. This read creates nothing, seeds no session,
and performs no durable write.

An application rejection is an `error` with `code = "rejected"` and a required
`detail` object whose variants are closed. The default input treatment admits
`session_not_found { session_id }`,
`active_turn_present { session_id, active_turn_id }`,
`defaults_version_mismatch { session_id, expected, current }`,
`unknown_model_alias { session_id, alias_id }`, and
`acceptance_position_exhausted { session_id, last }`. Explicit or omitted
`start_when_idle` retains that set. `steer` admits `session_not_found`,
`acceptance_position_exhausted`,
`no_active_turn { session_id, expected_active_turn_id }`, and
`active_turn_mismatch { session_id, expected_active_turn_id, active_turn_id }`;
a stopping turn accepts steering, and
`safe_point_unavailable_while_stopping { session_id, active_turn_id, existing_command_id }`
is returned only on replay of a rejection recorded before it did. `queue` admits
the first four steering details plus `defaults_version_mismatch` and
`unknown_model_alias`. A `replace_session_metadata` rejection admits exactly
`session_not_found { session_id }`. A `replace_session_defaults` rejection
admits `session_not_found { session_id }`,
`defaults_version_mismatch { session_id, expected, current }`, and
`defaults_version_exhausted { session_id, current }`. A `reconcile_turn`
rejection admits `session_not_found`, `defaults_version_mismatch`,
`unknown_model_alias`, and `acceptance_position_exhausted` as above, plus
`active_turn_mismatch { session_id, expected_active_turn_id, active_turn_id }`
and `no_active_turn { session_id, expected_active_turn_id }` for a decision that
lost its race, and `turn_not_awaiting_reconciliation { session_id, turn_id }`
for the refused precondition. A `stop_turn` rejection admits
`session_not_found`, `defaults_version_mismatch`, `unknown_model_alias`, and
`acceptance_position_exhausted` as above, plus `no_active_turn`,
`active_turn_mismatch`,
`interrupt_already_applied { session_id, active_turn_id, existing_command_id }`,
and
`interrupt_unavailable_while_awaiting_approval { session_id, active_turn_id }`.
The three content-bearing input mutations additionally admit
`attachment_blob_not_found { digest }` and
`attachment_byte_budget_exceeded { maximum_bytes }`; the maximum is the
configured canonical decimal-u64 `max_blob_bytes`. These current-catalog checks
run only after an unseen command identity is claimed, so either detail is stored
as the command's terminal typed rejection and equal replay returns it unchanged.
A `decide_tool_request` rejection admits
`tool_request_not_found { tool_request_id }`,
`tool_request_already_resolved { tool_request_id }`,
`tool_request_not_earliest_undecided { tool_request_id, earliest_tool_request_id }`,
and `tool_request_not_in_session { session_id, tool_request_id }`. An
`override_denied_tool_request` rejection admits `tool_request_not_found` and
`tool_request_not_in_session` with those same shapes, plus
`tool_request_not_delegate_denied { tool_request_id }`,
`tool_request_not_terminally_denied { tool_request_id }`, and
`tool_denial_already_overridden { tool_request_id }`. A delegation request
admits `session_not_found`, `tool_request_not_found`, and
`tool_request_not_in_session` with those same shapes, plus
`delegation_request_not_in_turn { session_id, turn_id, tool_request_id }` when
the named request belongs to another turn, and
`delegation_tool_request_not_executable { tool_request_id, state }` when a first
execution names a request whose state is `awaiting_approval`, `denied`,
`approved`, `prepared`, `closed`, or `attempt_ended`. `approved` means the
proposal-ordered request has approval but no physical attempt yet. Durable equal
replay is checked first against the exact stored operation and returns its
original receipt without requiring a still-live execution attempt. The
credential-administration operations extend that same closed inventory, as
`rejected` details rather than new top-level codes: a
`clear_credential_exclusion` rejection admits
`unknown_credential_exclusion { target }`, carrying the exact closed target
object the request named, and
`stale_generation { target, current_record_generation }`, carrying that same
target and the newer active generation at the target's own scope as a positive
canonical decimal string. A `read_credential_pool_policy` rejection admits
`unknown_pool_policy { session_id, turn_id, pool_policy_id }`. Each is a
rejection detail because each names a refused precondition on an otherwise
well-formed request, which is exactly what that closed set is for; none is a
transport or framing fault, so none has a top-level code.

`spawn_session` additionally admits
`delegation_spawn_conflict { tool_request_id }` for a non-equal replay and
`delegated_child_identity_collision { child_session_id }` when the generated
child identity is already occupied. It has no fixed active-child-count
rejection: admission checks the complete locked parent relationship inventory
only for request and child uniqueness. `await_session` additionally admits
`delegation_relation_not_found { session_id, peer_session_id }` and
`delegation_await_conflict { tool_request_id }`; `send_session_message` admits
that same relation detail, `delegation_message_conflict { tool_request_id }`,
and `delegation_message_identity_collision { message_id }` when a concurrent
operation already claimed the daemon-minted message identity. An exhausted
relation event ordinal admits
`delegation_event_ordinal_exhausted { spawning_request_id, last }`. Exhausting
the independent recipient-wide delivery counter admits
`delegation_delivery_sequence_exhausted { recipient_session_id, last }`. Either
`last` is the maximum unsigned 64-bit value. These delegation details are
closed; request-purpose, carried-argument, and bounded content failures occur
while constructing the application input and therefore map to `invalid_request`,
not `rejected`. A `create_session_from_imported_frontier` rejection admits
`imported_conversation_not_found { imported_conversation_id }` and
`imported_frontier_position_out_of_range { imported_conversation_id, requested_position, last_position }`.
The first names an imported conversation, never a session, as the absent target;
the second states that the identity was valid and only the ordinal was outside
`1..=last_position`. Because imported positions are that contiguous sequence, a
`last_position` of zero or a `requested_position` inside the stated range is a
contradictory frame and is rejected rather than presented. The two imported
details leave the command identity unclaimed — the daemon refuses them before
the creation service runs, and the service's own misses likewise claim nothing,
as
[sessions-and-transcript](sessions-and-transcript.md#create-from-an-imported-frontier)
states — so the same command identity remains available for a corrected
conversation or position rather than becoming a conflicting reuse.

An `update_session_placement` rejection is one of
`session_not_found { session_id }`,
`session_placement_current_version_mismatch { session_id, expected_placement_version, current_placement_version }`,
or
`session_placement_version_exhausted { session_id, current_placement_version }`.
Placement versions in these details are positive. A current-version mismatch
additionally requires distinct expected and current versions; equality is a
contradictory frame rather than durable mismatch evidence. Version-exhausted
evidence requires the maximum placement version. Each is the durable typed
result of handling the exact update command; equal replay returns the same
result and conflicting command reuse remains distinct. The terminal client
accepts one only when its variant, session, expected version, and current
version cohere with the submitted update; contradictory evidence is an ambiguous
mutation result.

The proposed `replace_lost_runner` rejection admits
`session_not_found { session_id }`, `runner_placement_not_found { session_id }`,
`placement_revision_mismatch { session_id, expected, current }`,
`placement_not_lost { session_id, placement_revision, state }`,
`replacement_same_runner { session_id, runner_id }`,
`replacement_target_unavailable { session_id, target, reason }`, and
`replacement_provisioning_failed { session_id, repository, failure_class, runner_detail }`.
Target reason is `not_connected`, `not_current`, `not_advertised`,
`pending_request_mismatch`, or `pending_request_disconnected`; provisioning
failure class is `credential_unavailable`, `repository_unavailable`,
`sandbox_unavailable`, or `workspace_conflict`. `repository` is JSON null when
the refused operation required none, and `runner_detail` is JSON null or the
bounded runner-authored `{ code, message, payload }` object the runner reported
with its failure. The failure class is the layer the daemon and this rejection's
readers branch on; the detail is data the runner may extend freely and the
daemon never interprets. The proposed `abandon_lost_runner` rejection admits
`session_not_found`, `runner_placement_not_found`,
`placement_revision_mismatch`, `placement_not_lost`, and
`active_turn_requires_existing_control { session_id, active_turn_id }` with
those same shapes. The proposed `promote_pending_runner` rejection names no
session and admits `no_pending_runner_enrollment {}`,
`pending_request_mismatch { pending_request_id }`,
`pending_request_disconnected { pending_request_id }`, and
`active_runner_not_lost { runner_id, connection_state }` for a daemon whose
active runner is still connected or only suspect. Every admitted runner
rejection is a recorded durable result; equal replay returns it even after
runner state changes.

The `turn_not_awaiting_reconciliation`, `tool_request_not_in_session`,
`imported_conversation_not_found`, and `imported_frontier_position_out_of_range`
details report refusals made before command recording, so unlike every other
`rejected` detail they name no durable command result and have no replay
projection; a caller that repeats the request observes the current state, not a
recorded outcome. An equal replay returns the same success or rejection
projection as the first handling.

Conversation-import refusals instead use `code = "invalid_request"` with one
required typed `detail`: `conversation_import_already_in_progress {}`;
`conversation_import_not_in_progress {}`;
`conversation_import_source_too_large { limit_bytes, declared_size_bytes, actual_size_bytes }`,
where actual size is null at begin and exact at append or commit;
`conversation_import_source_size_mismatch { declared_size_bytes, actual_size_bytes }`;
or `conversation_import_conversion_failed { class, record_ordinal }`, where the
one-based physical-record ordinal is null only when the converter has none. The
closed classes are `empty_source`, `blank_line`, `invalid_utf8`, `invalid_json`,
`json_depth_exceeded`, `top_level_not_object`, `invalid_record_type`,
`raw_record_count_exceeded`, `invalid_source_metadata`,
`invalid_message_envelope`, `invalid_message_role`, `message_role_mismatch`,
`invalid_message_content`, `invalid_content_block`, `invalid_tool_result_block`,
`invalid_reasoning`, `invalid_tool_call`, and `invalid_tool_result`.

Before either type-specific list applies, a cross-kind bulk-ingest request on a
connection that owns the process-wide permit uses
`bulk_ingest_already_in_progress { active_kind }`, where `active_kind` is
exactly `conversation_import` or `blob_upload`. It leaves the owning chunked
operation available for append, commit, or abort and never enters the permit
waiter queue.

Blob refusals use `code = "invalid_request"` with exactly one of these required
typed details: `blob_upload_already_in_progress {}`;
`blob_upload_not_in_progress {}`;
`blob_upload_length_out_of_range { min_length_bytes, max_length_bytes, declared_length_bytes }`;
`blob_upload_size_exceeded { expected_length_bytes, actual_length_bytes }`;
`blob_upload_length_mismatch { expected_length_bytes, actual_length_bytes }`;
`blob_upload_digest_mismatch { expected_digest, actual_digest }`;
`blob_read_length_out_of_range { min_length_bytes, max_length_bytes, requested_length_bytes }`;
or
`blob_read_range_out_of_bounds { blob_length_bytes, offset_bytes, length_bytes }`.
All byte counts use canonical decimal strings and both digest fields use the
canonical blob spelling. The range detail also represents checked-add overflow.
The request type identifies which append, commit, abort, or read operation
failed, so state details carry no duplicate operation discriminator. With blob
storage enabled, Begin checks active connection state before its declared-length
range and routed-store lookup. Append checks cumulative size after state; commit
checks state, then actual length, then digest. Read checks requested length,
catalog existence, and then checked range. Each request reports only the first
applicable failure in that order.

Conversation-import evidence carries no source bytes, text, paths, identifiers
taken from source, or parser excerpts; blob evidence carries no blob bytes,
object key, store name, or locator. Error codes other than `rejected` and these
conversation-import and blob `invalid_request` mappings have no `detail`.

The protocol error-code set is:

| Code                    | Meaning                                                                                                                                |
| ----------------------- | -------------------------------------------------------------------------------------------------------------------------------------- |
| `malformed_frame`       | JSON, UTF-8, framing, field, or size validation failed.                                                                                |
| `unsupported_version`   | The frame version is unsupported.                                                                                                      |
| `invalid_request`       | A boundary value cannot construct the requested application input.                                                                     |
| `not_found`             | The selected session, named defaults epoch, imported conversation or frontier, review aggregate, or blob does not exist.               |
| `blob_missing`          | The blob exists in the catalog, but every recorded replica is definitively absent.                                                     |
| `blob_corrupt`          | The blob exists in the catalog, no candidate is unavailable, and at least one recorded replica fails length or digest verification.    |
| `conflicting_reuse`     | A durable command identity already names different intent.                                                                             |
| `rejected`              | The canonical command was durably rejected by current typed state, or a request-specific precondition refused it before recording one. |
| `resync_required`       | A follower fell behind the bounded process-local event fan-out.                                                                        |
| `unavailable`           | Infrastructure failed; no requested mutation may have committed.                                                                       |
| `publication_ambiguous` | S3 may have accepted deterministic blob bytes, but reconciliation could not prove publication or nonacceptance.                        |
| `commit_ambiguous`      | Infrastructure obscured whether the requested mutation committed.                                                                      |
| `internal`              | Fail-closed corruption or a daemon defect stopped the request.                                                                         |

A `commission_session` request, and a pursuit-starting `attach_goal`,
`resume_goal`, or `supersede_goal` request for a pull-request-commissioned
session, additionally admits the transient rejection
`commission_target_busy { session_id }` when another live commissioned session
already owns the same target. `session_id` identifies that authoritative session
so callers can wait and retry the exact same command identity and payload after
the competing session becomes terminal.

For `create_session`, `create_session_from_template`, `commission_session`,
`create_session_from_imported_frontier`, `submit_input`, `compact_session`,
`reconcile_turn`, `stop_turn`, `decide_tool_request`,
`override_denied_tool_request`, `replace_session_metadata`,
`replace_session_defaults`, `replace_lost_runner`, `abandon_lost_runner`,
`promote_pending_runner`, and every review mutation, a lost commit response maps
to `commit_ambiguous`; the client retries the exact command identity and payload
to discover the recorded outcome. A `reconcile_turn`, `decide_tool_request`,
`override_denied_tool_request`, `replace_lost_runner`, `abandon_lost_runner`, or
`promote_pending_runner` retry reaches that recorded outcome or resumes its
exact claimed pending effect unconditionally, because a claimed command identity
bypasses the precondition the first handling already satisfied. Replacement
recovery reuses only its recorded workspace authorization and manifest receipt;
it never starts another clone under the same claim. Once a review aggregate
effect has been applied or recovered, any database failure during post-effect
verification, typed-receipt insertion, or claim commit is likewise
`commit_ambiguous`. A definitely pre-commit infrastructure failure maps to
`unavailable`.

Conversation import carries no durable command identity because exact
format-and-source replay already resolves through the import digest. Both the
single-shot request and chunked commit pass the same complete format and source
to that idempotent operation. The typed, content-silent conversion and size
refusals above map to `invalid_request`. The repository error does not retain
the failing database phase, so every import database error maps conservatively
to `commit_ambiguous`; retrying the exact format and bytes returns either the
first inserted identity or the existing identity. Import assembly allocation
exhaustion maps to `unavailable`; integrity failures map to `internal`.

Errors contain no database URL, socket path, credential path or value, SQL,
caller content, or provider payload.

An oversized outbound frame terminates only its connection. Other encoding
failures remain fatal evidence that the runtime cannot satisfy the closed wire
contract.

## Transcript snapshots

A transcript snapshot is read in one PostgreSQL repeatable-read, read-only
transaction. The transaction observes all of:

- the global last committed outbox sequence, returned as `cursor`; and
- the selected session's latest authoritative semantic frontier: the tip of
  persisted turn-start predecessor lineage when one exists, otherwise the
  checked `ImportedSessionSeed` frontier for imported ancestry, and otherwise no
  frontier; and
- every turn in acceptance order with its authoritative lifecycle state.

Selecting the imported fallback is a purpose-specific semantic-context read. It
fully validates the immutable seed and imported prefix under the same
repeatable-read snapshot; ordinary bounded `load_session` calls do not
materialize that prefix. A queued but not yet started first native turn does not
hide the fallback. Once a native turn-start frontier exists, normal persisted
predecessor lineage is authoritative.

One logical snapshot is a bounded message sequence sharing the request identity:

1. `transcript_snapshot_start { session_id, cursor, runner }`, where `runner` is
   the same complete nullable runner object as the session summary. The object
   carries the exact selector, current-or-lost runner, positive placement
   revision, sandbox, independent credential/repository/directory axes, closed
   placement state, and required-nullable `connection_health`. Connection health
   is present exactly for a pinned placement and is `connected`, `suspect`,
   `shutdown`, or `lost`; the snapshot therefore authenticates a health event at
   or before its cursor instead of suppressing that current fact as an already
   observed delta. A `runner_lost_before_pin` state requires an exact runner
   selector naming the lost runner, never a capability selector;
2. one `transcript_turn` per turn, with canonical decimal `acceptance_position`
   and required-nullable `model_settings`; a settings-aware turn carries the
   complete owning turn, accepted input, defaults epoch, requested and selected
   model, per-call override, resolved settings, and adjustment provenance, while
   a turn committed before settings evidence existed carries null;
3. one `transcript_model_call_usage` per terminal model call followed by one
   `transcript_model_calls_end`;
4. the entry messages below in frontier-member order; and
5. `transcript_snapshot_end { session_id, cursor, turn_count, entry_count }`.

Usage rows are ordered first by the owning turn's acceptance position and then
by model-call UUID. The native client models both usage rows and the mandatory
model-calls-end boundary, validates their contiguous indices, identities, and
count, and retains usage rows in its bounded snapshot record set. Each row
carries contiguous zero-based `model_call_index`, `turn_id`, `model_call_id`,
closed `usage_provenance` (`reported` or `estimated`), and a required `usage`
object with required-nullable `input_tokens`, `output_tokens`,
`cache_creation_input_tokens`, and `cache_read_input_tokens`. A null field means
that axis was not supplied; a present zero is the canonical decimal string
`"0"`. The required-nullable `cost` member is null when no derivation is
available. Otherwise it carries canonical nonnegative decimal `amount_usd`, the
exact `rate_window { provider, provider_model, channel, effective_from }`
identity that priced it, and label `real` or `metered_equivalent`. Because no
read-time derivation exists without evidence, a nonnull `cost` is rejected when
all four usage axes are null. `amount_usd` admits exactly the decimal
representation used for derivation: at most 28 fractional digits and a
coefficient no greater than 79,228,162,514,264,337,593,543,950,335. The daemon
derives that value at read time under the
[configuration-and-credentials](configuration-and-credentials.md) contract; no
bare dollar amount is durable model-call evidence.
`transcript_model_calls_end { model_call_count }` acknowledges the exact row
count before any entry message. Every terminal call has one row, including
historical, cancellation, and recovery calls whose four fields are all null.

The daemon builds that complete sequence in a secure unnamed temporary file
before writing its first snapshot frame to the connection. Persistence validates
the execution lineage in PostgreSQL and yields one turn or frontier member at a
time from the same read-only repeatable-read transaction; signalboxd encodes
each item directly to the spool, commits the transaction after the final item,
rewinds, and streams the completed file. A slow client therefore holds neither a
PostgreSQL snapshot nor transcript-sized heap state. Per request, heap retention
is bounded by one decoded row, one protocol frame, and fixed I/O buffers;
temporary disk usage follows the complete encoded transcript size. Projection or
spool failure before transmission returns `unavailable` and exposes no partial
snapshot sequence. Once transmission starts, peer-write failure closes only that
connection, while an unexpected read failure from the completed spool is fatal
runtime evidence because a valid snapshot has already begun. A follow request
closes the spool immediately after transmitting the snapshot, before waiting for
live events.

Every read that holds a pooled connection across more than one statement shares
one bounded admission that reserves application-pool capacity for non-snapshot
work. That is session-list, session-metadata-list, session-metadata-read,
transcript-read, follow-snapshot, goal-read, imported-conversation-read, and
conversation-list construction; the review target, run, finding, and
finding-list reads, each of which spans a repeatable-read transaction; and the
coherent review-orchestration snapshot, which spans one such transaction too and
draws the same single unit. The session-metadata read is admitted on the same
ground as the rest and not for its result size: it opens a transaction, fixes a
repeatable-read snapshot, selects, and commits. The session-defaults read is the
single-statement case; it returns its connection immediately and takes no
admission. The [snapshot-reader capacity budget](#snapshot-reader-capacity) owns
the admission formula. Every request states its admission class before dispatch,
so no read verb reaches the pool by omission.

Each `transcript_turn` has `turn_id` and one of these closed `state` objects:

- `queued { accepted_input_id, content }`;
- `queued_delegated { spawning_request_id, parent_session_id, parent_turn_id, content }`,
  whose identifiers bind the checked delegated-task origin rather than
  fabricating an accepted input;
- `queued_delegation_wake { first_delivery_sequence, through_delivery_sequence }`,
  whose positive ordered recipient-wide range identifies the delivered
  delegation content that will wake an otherwise idle session;
- `delegation_terminated { spawning_request_id, outcome, reason, provenance }`,
  whose outcome is exactly `stopped` or `cancelled` naming the bound-child
  action, whose reason is exactly `parent_stopped` or `parent_cancelled` naming
  independently whether the parent stopped or was cancelled — both cross-action
  combinations are valid — and whose parent turn- or goal-command provenance
  carries `parent_and_descendants`; this delivered logical state exposes no
  child transcript and does not erase retained physical execution evidence;
- `active_running { current_attempt_id, current_model_call }`, where
  `current_model_call` is null before preparation or `{ model_call_id, state }`
  with state exactly `prepared`, `in_flight`, or `cancellation_requested`;
- `active_awaiting_model_call_recovery { ended_attempt_id, recovery_model_call_id, automatic_reconciliation_attempts, operator_action_required }`,
  where the canonical nonnegative attempt count is the durable number already
  claimed and `operator_action_required` is false while automatic work is
  scheduled or attempting and true only after its five-attempt budget is
  exhausted;
- `active_awaiting_child { await_request_id, spawning_request_id, child_session_id }`,
  which names the exact foreground wait and delegated relationship retaining the
  parent turn's progressing slot;
- `failed { terminal_frontier_id, terminal_attempt_id, terminal_model_call }`,
  where `terminal_attempt_id` is null only for an evidence-free recovery
  failure, and `terminal_model_call` is null when that failure or physical
  attempt owns no call; otherwise it is `{ model_call_id, disposition, cause? }`
  with disposition exactly `known_failed` or `cancelled`. `cause` is absent for
  legacy and non-provider failures; when present it is one of
  `credential_rejected`, `permission_denied`, `invalid_request`,
  `target_not_found`, `request_too_large`, `rate_limited`, `quota_exhausted`,
  `overloaded`, `provider_internal`, or `unrecognized`, and classifies only a
  `known_failed` provider response. It never carries provider-authored prose. A
  nonnull `terminal_model_call` requires a nonnull `terminal_attempt_id`;
- `completed { terminal_frontier_id, terminal_attempt_id, terminal_model_call_id }`;
- `refused { terminal_frontier_id, terminal_attempt_id, terminal_model_call_id }`;
- `cancelled { terminal_frontier_id, terminal_attempt_id, terminal_model_call_id }`,
  where `terminal_model_call_id` is null when cancellation closed the turn
  before a call was prepared; or
- `reconciliation_required { terminal_frontier_id, terminal_attempt_id, terminal_model_call_id }`.

The tool-bearing vocabulary adds
`active_awaiting_tool_approval { tool_request_id }`,
`active_awaiting_tool_recovery { ended_attempt_id, recovery_tool_attempt_id, automatic_reconciliation_attempts, operator_action_required }`,
where the attempt count and operator flag have the same durable five-attempt
meaning as the model-call recovery variant, and
`tool_reconciliation_required { terminal_frontier_id, terminal_attempt_id, terminal_tool_attempt_id }`.
The runner-bearing vocabulary additionally admits
`active_awaiting_runner_recovery { runner_id, placement_revision, tool_attempt_id }`,
where `tool_attempt_id` is null when no physical tool attempt owns the loss. The
snapshot-level runner object remains authoritative for queued and otherwise
non-active sessions.

Each non-text native frontier member is one `transcript_entry` with
`entry_index`, `source_session_id`, `entry_id`, and one closed `entry` object:
`turn_completed { turn_id }`, `turn_failed { turn_id }`, or
`turn_cancelled { turn_id }`. The tool-bearing vocabulary also admits
`assistant_tool_use { turn_id, model_call_id, tool_request_id, tool_name, arguments, approval? }`,
`tool_execution_result { tool_request_id, tool_attempt_id, content }`,
`tool_denied { tool_request_id, content }`, and
`tool_closed { tool_request_id, content }`. The vocabulary also admits
`model_identity_changed { turn_id, defaults_version, selected_model_id }`,
naming the first started turn bound to a changed frozen direct selection. The
optional `approval` object repeats the exact `decision`, `decider`, and nullable
`rationale` shape of `tool_approval_decided`; it is absent while the request is
pending and when automatic policy decided without an explicit event. This makes
explicit provenance available from an authoritative snapshot after its event
cursor has passed. The vocabulary additionally admits the text-bearing
`context_summary { model_call_id, first_source_session_id, first_entry_id, through_source_session_id, through_entry_id }`;
its content follows through the ordinary `transcript_content` sequence, and its
source-qualified endpoints and call identify the exact recorded provenance. The
runner proposal additionally admits
`runner_placement_changed { prior_runner_id, new_runner_id, placement_revision, sandbox_profile }`.
It is the reference-only semantic boundary injected before work resumes on the
successor placement. A native accepted-input member is the single
`transcript_user_entry { entry_index, source_session_id, entry_id, accepted_input_id, turn_id, content }`
message, where `content` is the canonical ordered parts array. It therefore
retains attachment metadata and interleaving without emitting blob bytes. A
native assistant text member begins with
`transcript_text_entry { entry_index, source_session_id, entry_id, entry }`,
whose `entry` is exactly `assistant { turn_id, model_call_id }`. It is followed
by one or more `transcript_content` messages carrying the same `entry_index`, a
zero-based `fragment_index`, `final_fragment`, and `content_fragment`. Fragment
indices start at zero and are contiguous: each fragment index is exactly its
predecessor plus one. Exactly the last fragment carries `final_fragment = true`;
every earlier fragment carries `false`. The content is split only at UTF-8
scalar boundaries into fragments of at most 1 MiB of UTF-8; even empty content
has one final empty fragment. The 1 MiB content bound leaves room below the 8
MiB frame limit even when every byte requires worst-case JSON escaping.

The tool-entry `arguments` and `content` members are JSON strings, never nested
untyped JSON values. `arguments` contains the exact normalized JSON text or
credential-scrubbed undecodable text stored on the request. `content` contains
the exact provider-visible result string: admitted success text, or the compact
closed error object serialized as text by the provider bridge. Tool entry
discriminators and identifiers determine the semantic arm; clients never infer
it by reparsing either string. The runner proposal adds a required `execution`
object to every `tool_execution_result`, tagged by `type` and closed against
unknown members in both arms. A daemon-local result carries exactly
`{"type":"daemon"}`. A runner-produced result carries exactly `type` `runner`
plus `runner_id` and `lease_id` as canonical UUID strings, `placement_revision`
and `lease_generation` as positive canonical decimal strings,
`working_directory` as the bounded directory the dispatch executed in or JSON
null when the placement selected the runner default, `sandbox_profile` as
exactly `workspace-restricted` or `ambient`, and `outcome` as exactly
`succeeded` or `known_failed`. The discriminator is what lets a client tell the
arms apart instead of inferring the shape from which members are present, and
`outcome` is the closed classification the durable attempt reached rather than a
restatement of the result content. The set has no ambiguous member because a
physically ambiguous attempt never becomes an execution result: it stays the
turn's ambiguity set and projects as `tool_closed`, which names only the tool
request and therefore carries no attempt and no `execution` object
([tool-loop](tool-loop.md#serialized-staged-execution)). Admitting an
`ambiguous` outcome here would have obliged clients to accept an
execution-result state no domain transition can produce. The relocation members
are the same ones every other relocation-bearing projection carries, so a
working-directory move is visible in tool evidence exactly as a runner move is.
The profile label is evidence metadata, not text that a tool can forge.

The process projection resolves the domain's reference-only tool entries before
crossing the wire. Tool use carries the exact checked name and exact
normalized-or-scrubbed-undecodable arguments. Execution, denial, and closure
carry the same provider-neutral success text or compact typed failure JSON
defined by [tool-loop](tool-loop.md#provider-bridge-and-daemon-catalog). A
client therefore never needs private storage access to reconstruct tool-bearing
conversation history.

The following imported-entry variants exist in the protocol. An imported
semantic entry always identifies its source with `imported_conversation_id` and
`imported_entry_id` and carries the exact `source_speaker` attestation. That
attestation is one closed object: `not_attested`, `attested_absent`, or
`attested { speaker }`, where `speaker` is exactly `user` or `assistant`.

An imported `Text` whose value is attested begins with
`transcript_text_entry { entry_index, source_session_id, entry_id, entry }`. Its
`entry` is
`imported { imported_conversation_id, imported_entry_id, source_speaker }`, and
its exact text follows in the same `transcript_content` fragment sequence used
for native text, including one final empty fragment for attested empty text.

Every other imported content value, including unattested or explicitly absent
`Text`, is one `transcript_entry` whose `entry` is
`imported { imported_conversation_id, imported_entry_id, source_speaker, content_kind }`.
`content_kind` is one closed string discriminator: `source_event`,
`source_message_block`, `text`, `tool_call`, `tool_result`, `thinking`,
`redacted_thinking`, `document`, or `message_content_absent`. This conservative
projection carries no imported tool fields, results, thinking, media,
source-event payload, absence detail, or raw record. The complete normalized
imported content and verbatim raw source remain authoritative only in the
immutable imported-conversation aggregate; the wire snapshot neither fabricates
native evidence nor replaces that authority.

**Session-delegation transcript entries.** The process surface emits three
non-text `transcript_entry` arms:

- `delegated_task { spawning_request_id, parent_session_id, parent_turn_id, content }`;
- `delegation_message { spawning_request_id, message_id, sender_session_id, recipient_session_id, ordinal, delivery_sequence, content }`;
- `delegation_result { await_request_id, spawning_request_id, child_session_id, mode, delivery_sequence, outcome, content, reason, provenance }`.

The task arm resolves the immutable spawn request and relationship, requires
`source_session_id` to equal the child, and carries the exact checked task
argument. It is a distinct delegated-task origin, never a user accepted-input
entry. The message arm resolves the immutable message record referenced by the
semantic entry and requires `source_session_id` to equal its recipient. The
result arm resolves both the immutable child-result record and the exact
delegation wait named by `await_request_id`; that wait must name the same
spawning request and child, and `source_session_id` must equal its parent. Thus
separate waits for one child retain distinct model-visible tool-result
correlation. `mode` equals that wait's `foreground` or `background`;
`delivery_sequence` is null for foreground and is the positive canonical decimal
recipient-wide sequence for background. Message entries likewise resolve their
positive recipient-wide delivery sequence even though the wire retains the
relationship-local ordinal. `content` is required for a returned result and null
for every other closed outcome; `reason` and `provenance` use the same closed
shapes as the corresponding `session_event`. A missing record, mismatched wait,
relationship, endpoint, delivery sequence, or incompatible outcome fails the
snapshot before transmission.

Snapshot deduplication uses the complete `(source_session_id, entry_id)`
semantic identity. Neither `message_id` nor `spawning_request_id` is a
substitute for that source-qualified key, and a second occurrence of the same
key fails the snapshot. Each delegation arm increments `entry_count` exactly
once and consumes one contiguous `entry_index`; its inline content emits no
`transcript_content` frames. Distinct source-qualified entries remain distinct
even if they resolve through one relationship.

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
the process protocol explicitly maps them.

## Session-delegation process surface

The session-follow event shapes, internal-wake exclusion, closed mutation
request and receipt frame vocabulary, terminal-client verbs, and daemon
`await_session`/`send_session_message` execution in this section are
implemented. The composed process surface admits only exact already-issued model
work for terminal operation and recovery; it never lets a client fabricate model
provenance. The model-facing tool names and arguments are owned by
[tool-loop](tool-loop.md#session-delegation-tool-family). Each request carries
the invoking session, turn, and `tool_request_id`, which must reconstitute one
matching logical request before any mutation occurs.

Logical-request reconstitution alone is not execution authority. Before a first
mutation, the daemon must also reconstitute the exact authorized, executable
attempt for that request and prove it is neither awaiting approval, denied,
closed, nor already ended. This check uses the ordinary tool-execution authority
boundary; a process client cannot promote a merely issued request. An exact
durable replay is the sole exception: it returns the immutable stored receipt
before consulting current attempt state and cannot create another effect.

`await_session` carries the related child and wait mode, and returns either
`session_await_registered { tool_request_id, child_session_id, mode }` for
background or the delivered child outcome for foreground. A foreground request
subscribes before registering its durable wait, queries durable delivery before
blocking, and queries again after a matching wake or subscriber lag; completion
therefore cannot be lost between registration and subscription. Disconnect or
daemon shutdown abandons only the socket wait, never the durable child wait.
`send_session_message` carries the related peer and bounded content, and returns
`session_message_sent { tool_request_id, message_id, direction, ordinal, delivery_sequence }`.
For a sent-message receipt or update, `direction` is the closed string
`parent_to_child` or `child_to_parent`. For a result that predates the wait,
background records delivery and returns `session_await_registered`, while
foreground returns the child outcome. Equal replay returns that same
mode-specific receipt or outcome. Message strings must fit both the
delegation-content ceiling and their complete normalized JSON argument envelope;
the 1 MiB ceiling is exact only for standalone returned-result content.

**Committed unimplemented functionality.** `spawn_session` carries bounded
`task` and the closed relationship object and is reserved to return
`session_spawned { tool_request_id, child_session_id, relationship }`, but no
present process execution surface creates the child. Until the placement-owned
creation transaction implements the parent-directory default, the daemon rejects
this request without mutation. That future transaction must preserve the
exact-request and authority rules above. Its task string must fit both the
delegation-content ceiling and its complete normalized JSON argument envelope.

Session-follow updates add these closed event shapes:

- `child_spawned { spawning_request_id, child_session_id, relationship }`;
- `child_waiting { await_request_id, spawning_request_id, child_session_id, mode }`;
- `session_message { spawning_request_id, message_id, sender_session_id, recipient_session_id, ordinal, delivery_sequence, content }`;
- `child_result { spawning_request_id, child_session_id, outcome, content, reason, provenance }`;
- `child_lifecycle_disposition { spawning_request_id, child_session_id, outcome, reason, provenance }`.

The two outcome events use one closed nested union. `outcome` is `returned`,
`failed`, `stopped`, `cancelled`, `already_terminal`, or `continue_running`;
`reason` is `child_completed`, `child_execution_failed`,
`child_result_unavailable`, `child_cancelled`, `parent_stopped`, or
`parent_cancelled`. `provenance` is
`child_turn { child_session_id, child_turn_id }`,
`parent_turn_command { parent_session_id, parent_turn_id, command_id, descendant_scope }`,
or
`parent_goal_command { parent_session_id, goal_generation, command_id, descendant_scope }`,
or
`parent_lifecycle_command { parent_session_id, command_id, descendant_scope }`,
where `goal_generation` is a positive canonical decimal string and
`descendant_scope` uses the request spelling above. Goal-stop provenance never
carries or fabricates a parent turn. The admitted correlations are exhaustive:

| Outcome            | Reason                                                 | Provenance     | Content      |
| ------------------ | ------------------------------------------------------ | -------------- | ------------ |
| `returned`         | `child_completed`                                      | `child_turn`   | exact string |
| `failed`           | `child_execution_failed` or `child_result_unavailable` | `child_turn`   | null         |
| `cancelled`        | `child_cancelled`                                      | `child_turn`   | null         |
| `stopped`          | `parent_stopped` or `parent_cancelled`                 | parent command | null         |
| `cancelled`        | `parent_stopped` or `parent_cancelled`                 | parent command | null         |
| `already_terminal` | `parent_stopped` or `parent_cancelled`                 | parent command | null         |
| `continue_running` | `parent_stopped` or `parent_cancelled`                 | parent command | null         |

`child_result` admits every row except `already_terminal` and
`continue_running`; `child_lifecycle_disposition` admits only parent-command
`stopped`, `cancelled`, `already_terminal`, and `continue_running` rows. A
parent-command outcome admits `parent_turn_command`, `parent_goal_command`, or
`parent_lifecycle_command` provenance and requires `parent_and_descendants` for
`stopped`, parent-caused `cancelled`, and `continue_running`. An
`already_terminal` row additionally requires the relationship's pre-existing
immutable child result and never creates or replaces it; traversal continues
through the child's outgoing edges. For a nonterminal child, the outcome names
the bound-child action while the reason independently names whether the parent
stopped or was cancelled; both cross-action combinations are valid.
`already_terminal` names no new child action. A `continue_running` row is the
explicit evaluated no-change disposition for an edge included by the
caller-selected cascade. Any other outcome/reason/provenance/content combination
is a contradictory frame and is rejected. Thus every lifecycle result names both
why it happened and the exact child turn or parent command that caused it.

`delivery_sequence` is a positive canonical decimal string allocated under the
recipient session lock. It is required on every `session_message` update and on
the transcript's background `delegation_result`, null on its foreground
`delegation_result`, and unique and gap-free per recipient across messages and
background-result deliveries. The result-availability `child_result` update is
not an inbox delivery and carries no sequence. Relationship-local `ordinal`
never breaks a cross-relationship tie.

The internal `delegation_wake` outbox event is a scheduler signal, not a
session-follow update. Clients observe the durable result or message update that
caused it, never the wake itself. Wake emission cardinality and transaction
ownership belong to the
[transactional-outbox persistence contract](persistence-protocol.md#transactional-outbox).

Each typed delegation update has one recipient stream except a stopped or
cancelled `child_lifecycle_disposition` caused by a parent cascade, which is
emitted on both the parent and child streams. Spawn, waiting, other lifecycle,
and result updates go to the parent; messages go to their payload recipient.
Each event's own `session_id` identifies that stream. Cursor ordering,
snapshot-first follow, deduplication, and resync rules apply to these events as
to every other session event. No event embeds or links the child transcript.

`descendant_scope` is required on both `stop_goal` and `stop_turn`. The terminal
client spells omission as `parent_alone` and `--descendants` as
`parent_and_descendants`; it never guesses from the relationship policy. A
successful command records the selected scope as durable intent.

The scope is part of the durable command intent, not receipt-only metadata.
Domain `GoalUserAction::Stop { descendant_scope }` and
`DeliveryRequest::Interrupt { descendant_scope, .. }` retain it; command
storage, comparison, and reconstitution carry the same closed value. Reusing
either durable command identity with another scope is `conflicting_reuse`.

Committed unimplemented functionality: no present process or client receipt
surface reports cascade metadata. A future successful cascade receipt must
include the selected scope and the exact count of recorded descendant
dispositions, so a zero-child choice and an unperformed cascade cannot be
confused. An equal durable-command retry must return those stored values without
reevaluating the cascade.

## Durable update dispatch

`DATABASE_URL` must name a direct or otherwise session-affine PostgreSQL
endpoint. Transaction- and statement-pooled proxy modes are unsupported because
the guard and generation fences below use locks owned by one PostgreSQL server
session.

Before migration or recovery, `signalboxd` acquires
`pg_try_advisory_lock(1396856881, 1213547057)` on one dedicated database
connection and retains that connection—and therefore the session-level
lock—until shutdown. Failure to acquire the fixed database-scoped guard fails
startup. The two integer keys are the ASCII namespaces `SBX1` and `HUB1`.

The singleton `hub_fence_state` stores a positive generation. Every application
pool connection acquires and retains a shared session advisory lock keyed by the
ASCII namespace `SBF1` (`1396852273`) and this daemon's generation, then
requires the durable singleton still to equal that generation before the
connection becomes usable. A mismatch rejects the connection. A successor
holding the singleton guard takes and retains the exclusive prior-generation
fence, then transactionally advances the row before constructing its fenced
pool. That exclusive request waits for all prior pooled sessions and prevents
the old process from opening another usable connection: an older generation that
tries again after a failed intermediate successor can acquire only its old
shared lock, then fails the current-generation check. Pool construction requires
a non-cloneable capability borrowing the still-live fence session; the copyable
generation value is observational and cannot construct work after guard release.
The first migration creates and initializes the row for a database that cannot
have a prior fenced daemon; later startups fence before running any newer
migration. Importing or upgrading a pre-fence database is unsupported.
Exhaustion or corruption fails startup rather than wrapping.

Together these guards enforce one active daemon process—and therefore one
dispatcher and its process-local fan-outs—for a database, while preventing a
successor's migration or recovery from overlapping an old daemon's authoritative
work. Guard-session monitoring and fatal-loss behavior are owned by
[Daemon runtime: startup order and shutdown](turn-lifecycle-and-scheduling.md#daemon-runtime-startup-order-and-shutdown).
For each attempt, the dispatcher:

1. starts a PostgreSQL transaction and locks the `process_protocol` row in
   `outbox_consumer_cursor`;
2. loads exactly `delivered_through + 1` and its one typed record;
3. maps the storage record to a distinct process-update value and offers it to
   the in-process fan-out;
4. only after that offer is accepted, advances that consumer's
   `delivered_through` to the same sequence and commits.

An idle dispatcher polls again after 50 ms. It never skips a sequence for the
process-protocol consumer and never dispatches two events concurrently. Delivery
failure, task cancellation, or a crash before the cursor commit leaves that
consumer's prefix unchanged, so the same event is offered again after recovery.
A crash after the offer but before commit may therefore duplicate that cursor;
delivery is at least once and globally ordered (INV-032). Consumers deduplicate
by cursor.

The process-local durable-only fan-out and delta-admitting composite fan-out
each retain 64 update events. The dispatcher offers every durable update to
both; the provider bridge offers deltas only to the composite fan-out. Wire
followers use the composite fan-out, which preserves one send order across
deltas and durable updates; the durable-only fan-out remains available to
internal consumers that need no transient traffic. One immutable text allocation
backs every clone of a delta delivered to concurrent followers, so fan-out count
does not multiply provider-sized text allocations. Having no connected followers
does not block durable cursor advancement: reconnecting clients use a fresh
authoritative snapshot. A follower that overruns its selected bounded fan-out
receives `resync_required` and reconnects for another snapshot.

Each `session_event` message carries `cursor`, `session_id`, and exactly one
closed `event` object. The protocol admits these event shapes:

| Event                            | Additional members                                                                                                                                                                                               |
| -------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `session_created`                | none                                                                                                                                                                                                             |
| `session_model_settings_changed` | `command_id`, prior and installed defaults versions, models and complete settings, `caller_override`, and ordered `adjustments`                                                                                  |
| `turn_model_settings_resolved`   | `accepted_input_id`, `turn_id`, `defaults_version`, requested and selected model identities, `per_call_override`, complete `settings`, optional distinct `adjusted_from_selection_id`, and ordered `adjustments` |
| `input_accepted`                 | `accepted_input_id`, `turn_id`, `acceptance_position`, and `content`                                                                                                                                             |
| `goal_turn_retired`              | `turn_id`                                                                                                                                                                                                        |
| `turn_activated`                 | `turn_id` and `current_attempt_id`                                                                                                                                                                               |
| `model_call_transition`          | `turn_id`, `model_call_id`, and `state`                                                                                                                                                                          |
| `tool_approval_decided`          | `turn_id`, `tool_request_id`, `decision`, `decider`, and nullable `rationale`                                                                                                                                    |
| `turn_completed`                 | `turn_id`, `model_call_id`, `completion_entry_id`, and `terminal_frontier_id`                                                                                                                                    |
| `turn_failed`                    | `turn_id`, `failure_entry_id`, and `terminal_frontier_id`                                                                                                                                                        |
| `turn_refused`                   | `turn_id`, `model_call_id`, and `terminal_frontier_id`                                                                                                                                                           |
| `turn_cancelled`                 | `turn_id`, `cancellation_entry_id`, and `terminal_frontier_id`                                                                                                                                                   |
| `turn_reconciliation_required`   | `turn_id`, `model_call_id`, and `terminal_frontier_id`                                                                                                                                                           |
| `child_spawned`                  | `spawning_request_id`, `child_session_id`, and `relationship`                                                                                                                                                    |
| `child_waiting`                  | `await_request_id`, `spawning_request_id`, `child_session_id`, and `mode`                                                                                                                                        |
| `child_lifecycle_disposition`    | `spawning_request_id`, `child_session_id`, `outcome`, `reason`, and `provenance`                                                                                                                                 |
| `child_result`                   | `spawning_request_id`, `child_session_id`, `outcome`, `content`, `reason`, and `provenance`                                                                                                                      |
| `session_message`                | `spawning_request_id`, `message_id`, `sender_session_id`, `recipient_session_id`, `ordinal`, `delivery_sequence`, and `content`                                                                                  |

A `goal_turn_retired` event clears only the exact queued turn it names; an
unmatched or already-active identity leaves local turn controls unchanged. A
superseding transaction publishes this event before its replacement
`input_accepted`, so a live follower cannot retain the obsolete queued identity
and ignore the replacement activation.

The `tool_approval_decided` decision is exactly `approve {}` or
`deny { reason }`, where `reason` is required-nullable: a user denial may
decline to give one. Its decider is exactly `user { command_id }`,
`delegate { model_selection_id, model_call_id }`, or
`user_override { command_id, overridden_tool_request_id }`; `rationale` is
required-nullable and present only for a delegate decision. A delegate rationale
is 1 through 4,096 UTF-8 bytes and contains no U+0000; a delegate denial's
`reason` is derived deterministically from that rationale (control characters
become spaces, forbidden edge spaces are trimmed, the text is cut to 1,024 bytes
on a character boundary) and is null exactly when the rationale sanitizes to
nothing. Every present denial reason — user-authored or derived — is nonempty,
at most 1,024 UTF-8 bytes, contains no Unicode control scalar, and has no
surrounding POSIX whitespace. A `user_override` decider is approve-only with a
null `rationale`: it records the consumption of one recorded override, naming
the override command and the overridden delegate-denied request.

The protocol additionally admits
`context_compacted { context_compaction_id, model_call_id, through_position, summary_entry_id, result_frontier_id }`.
The event is appended atomically with the completed dedicated call, summary
entry, and projected frontier. A follower receives the event even when its
initial snapshot predates the compaction commit.

The protocol additionally admits
`tool_batch_transition { turn_id, model_call_id, state }`, where `state` is
exactly `proposed { frontier_id }`, `results_projected { frontier_id }`, or
`recovery_required { tool_attempt_id }`, and
`turn_tool_reconciliation_required { turn_id, tool_attempt_id, terminal_frontier_id }`.

The runner proposal additionally admits
`runner_state_transition { runner_id, placement_revision, sandbox_profile, working_directory, state }`,
where `working_directory` is the placement's bounded directory or JSON null for
the runner default and state is `pinned`, `suspect`, `connected`,
`runner_lost_before_pin`, `runner_lost`, `replaced`,
`working_directory_changed`, or `abandoned`. `suspect` is emitted on the first
missed heartbeat. `connected` is emitted when a later acknowledgement clears
that same suspect epoch before durable loss and when a newly established epoch
supersedes a suspect predecessor. `replaced` and `working_directory_changed` are
the relocation states, and this family is the only surface on which a follower
learns that its session lost, changed, or moved on its runner. The family is
also the extension point for later runner facts: a further relocation shape, or
runner metadata and attributes, adds a state and its members to this one event
kind rather than a second kind. The event carries no runner-discovered host
path, credential fact, or arbitrary runner text; the working directory is the
user-selected placement value the client itself supplied. The runner proposal
includes its own representability gate before a runner-aware client subscribes.

The model-call `state` object is exactly `prepared`, `in_flight`,
`cancellation_requested`, or `terminal { disposition }`; terminal disposition is
one of `completed`, `known_failed`, `refused`, `cancelled`, or `ambiguous`.
Storage-version columns are not exposed as wire-version fields.

## Follow synchronization

For `follow_session`, the server subscribes to process-local fan-out before
reading the repeatable-read transcript snapshot. It sends that snapshot first,
then discards subscribed events at or below its cursor and sends matching
session events above it in cursor order. Every follower subscribes to the
ordered composite fan-out, which interleaves provider-text deltas with those
same durable updates in their process send order.

When the repeatable-read snapshot completes, the server records the exact count
of updates already queued on that follower subscription. It discards deltas in
that fixed prefix while continuing to apply the ordinary durable-cursor filter,
then forwards later deltas. An initial snapshot that already contains terminal
reply truth therefore cannot be followed by stale fragments for that reply.
Losing pre-snapshot deltas is part of their ephemeral presentation semantics;
the snapshot remains authoritative.

This ordering closes the snapshot/subscription race: every listed client-visible
transition committed before the snapshot is represented by its durable queued
content, turn state, and current model-call projection even when it adds no
semantic transcript entry. Each settings-aware turn projection also carries the
complete frozen settings fact that its at-or-below-cursor
`turn_model_settings_resolved` event established, while a compaction commit
retains its durable `context_compacted` observation behavior. A transition
committed after the snapshot has a greater cursor and was observed by the
preexisting subscription. A refused turn is therefore terminal in the initial
snapshot and cannot leave `send` waiting for an event at or below the snapshot
cursor. Previously seen transient display state may always be replaced by the
new snapshot (INV-032).

Followers forward durable transition events and additionally forward a correctly
correlated `TextDelta` emitted while the selected session turn is active. The
HTTP adapter has already applied the credential-redaction boundary before that
fact leaves the runtime (INV-035); the bridge and daemon copy its text unchanged
and do not apply their own redaction. Deltas remain ephemeral
process-incarnation presentation events: they are not appended to the
transactional outbox, do not advance the follow cursor, do not enter the
transcript, and do not alter the observation or terminal-evidence paths. The
durable transcript remains the sole reply truth.

An overrun of the selected composite fan-out produces `resync_required`. A
lagging or reconnecting delta-admitting follower loses any unreceived deltas; it
reads the new authoritative snapshot and continues from that snapshot's durable
cursor. Deltas are never replayed from storage. Resynchronization replaces
transient presentation state with the complete durable transcript rather than
making token delivery another source of authority (INV-032).

A delivery-admitting `send --queue` follows the exact origin turn returned by
`input_submitted`; it waits while any active-slot holder blocks activation and
then through its own queued turn until that turn terminalizes. A
delivery-admitting `steer` returns a `steering_submitted` receipt immediately
because pending steering creates no turn to follow. Neither behavior changes the
follow ordering or broadcast semantics.

The terminal `send` command follows the submitted turn, accepts terminal state
from the initial snapshot or waits for its durable terminal event, rereads the
authoritative transcript, and prints the committed assistant text. Its terminal
waiter accepts and ignores provider-text deltas for the selected session and
rejects a cross-wired delta. The client keeps following while an authoritative
`active_awaiting_model_call_recovery` state has
`operator_action_required = false`; a bounded cancellation-safe reread makes an
exhaustion-only projection change visible even when no session event is emitted.
It exits with a typed nonzero recovery-required diagnostic only after the
authoritative state has `operator_action_required = true`. A live terminal
`ambiguous` model-call transition triggers an immediate authoritative reread but
does not by itself require operator action.

The client applies the same behavior to `active_awaiting_tool_recovery` and to
`tool_batch_transition { recovery_required }` followed by that state. An
`active_awaiting_runner_recovery` turn likewise ends the follow with its typed
lost-runner diagnostic naming replacement or `stop_turn` before abandonment. A
model-call recovery wait is completed by bounded daemon reconciliation using the
same terminal transition as `reconcile_turn`; the operator verb remains
available to win that race and becomes required only when the projected
`operator_action_required` field is true. A tool recovery wait uses the same
durable budget and terminalizes through its proposal-ordered tool-reconciliation
boundary; it becomes an operator park only after exhaustion. A runner recovery
wait has `stop_turn`, which terminalizes the parked turn as cancelled or
reconciliation-required while preserving any tool ambiguity. An
`active_awaiting_tool_approval` turn remains an ordinary nonterminal wait that
`send` keeps waiting through; `decide_tool_request` is its resolving writer,
issued from a second connection while the waiting client's transcript names the
pending request and its proposing tool. A client disconnect never cancels model
or tool work.

An `active_awaiting_child` turn is likewise a nonterminal wait. Its three
identifiers are required together, and clients keep waiting until the delivered
foreground delegation result resumes or terminalizes that exact parent turn.

The client rereads after each `tool_batch_transition { proposed }` and
`tool_batch_transition { results_projected }`; the client rereads after every
terminal turn event. `follow` uses a separate connection to read and validate a
fresh authoritative transcript before it resumes printing later followed events.
That side reread does not advance the follow connection's observed cursor: only
events consumed from the subscribed connection do so, and every buffered event
remains eligible for ordered presentation. Although the reread may have a cursor
later than the triggering event, it makes presentation eligible only the
previously undisplayed semantic material attributable to that exact event:
assistant text and tool-use entries for the named producing call at `proposed`;
proposal-correlated tool-result entries at `results_projected`; assistant text
from the terminal event's named turn and model call plus the exact completion
marker for `turn_completed`; the exact failure marker and any immediately
preceding terminal tool-result suffix for `turn_failed`; the exact cancellation
marker and any immediately preceding terminal tool-result suffix for
`turn_cancelled`; and the exact terminal tool-result suffix for
`turn_tool_reconciliation_required`. It presents no semantic material for
`turn_refused`, model-call `turn_reconciliation_required`, or
`recovery_required`. It does not present material introduced by any later
cursor. Such material remains ordered behind its buffered followed event, or
behind a new authoritative snapshot after `resync_required`. Final durable
content is deduplicated by source-qualified semantic-entry identity while
transition-only events remain visible instead of being suppressed by a newer
side snapshot.

## Program-run cancellation

**Committed unimplemented functionality.** No present wire surface names program
runs. Protocol version `1` adds the `cancel_program_run` request and
`program_run_cancellation_receipt` server message cataloged above; both carry
the required top-level version `1`, and a later incompatible shape requires a
new process-protocol version under the ordinary version rules. The request names
canonical UUID `run_id` and durable canonical UUID `command_id`. Its receipt
answers from a closed outcome vocabulary: applied (the run is now terminally
cancelled), `not_found` (no such run), or `already_terminal` naming the standing
terminal state and result the command found. The run-state semantics — that a
cancel never overwrites a terminal outcome and that an applied cancel is
journaled and replayed — are owned by the substrate page; this contract owns the
message pair, its versioned encoding, and the closed receipt algebra, which
client and daemon must implement together.

## Terminal client

The `signalbox` binary uses the single admitted version. Single-session metadata
read and replacement are core protocol and daemon capabilities without
terminal-client UX, while the paginated metadata list is the `search` verb
below. The client accepts a global `--socket <path>` override or reads
`SIGNALBOX_SOCKET_PATH`, and provides:

- `create ((--model <selection-uuid> | --alias <alias-uuid>) [--system-prompt-file <path>] | --template <name>) [--placement <dotted-path> [--root-global-read]] [--runner <uuid> | --runner-class <name>] [--working-directory <path>] [--repository <key>] [--credential-profile <name>] [--sandbox-profile <workspace-restricted|ambient>] [--tool-auto <name>]... [--tool-confirm <name>]... [--command-id <uuid>]`;
- `place <session-uuid> (--placement <dotted-path> [--root-global-read] | --pathless) --expected-placement-version <positive-decimal> [--command-id <uuid>]`;
- `continue <imported-conversation-uuid> --through-position <positive-decimal|latest> --relationship <resume|fork> (--model <selection-uuid> | --alias <alias-uuid>) [--runner <uuid> | --runner-class <name>] [--working-directory <path>] [--repository <key>] [--credential-profile <name>] [--sandbox-profile <workspace-restricted|ambient>] [--tool-auto <name>]... [--tool-confirm <name>]... [--command-id <uuid>]`;
- `imported <imported-conversation-uuid>`;
- `list`;
- `templates`;
- `search [--title <substring>] [--tag <tag>]... [--include-archived] [--limit <decimal>] [--after <session-uuid>]`;
- `conversations [--title <substring>] [--origin <native|imported|all>] [--include-archived] [--limit <decimal>] [--after <native|imported>:<uuid>]`;
- `compact <session-uuid> [--through-position <positive-decimal>] [--command-id <uuid>]`;
- `goal attach <session-uuid> (--statement <text> | --statement-file <path>) [--command-id <uuid>]`;
- `goal show <session-uuid>`;
- `goal resume <session-uuid> [--guidance <text> | --guidance-file <path>] [--command-id <uuid>]`;
- `goal stop <session-uuid> [--descendants] [--command-id <uuid>]`;
- `goal supersede <session-uuid> (--statement <text> | --statement-file <path>) [--command-id <uuid>]`;
- `send <session-uuid> [--parts-file <path>] [--command-id <uuid> --defaults-version <decimal>]`;
- `send <session-uuid> --queue [--parts-file <path>] [--command-id <uuid> --defaults-version <decimal> --turn <uuid>]`;
- `steer <session-uuid> [--parts-file <path>] [--command-id <uuid> --turn <uuid>]`;
- `model <session-uuid> (--model <selection-uuid> | --alias <alias-uuid>) [--system-prompt-file <path> | --clear-system-prompt] [--command-id <uuid> --defaults-version <decimal> --dangerous-tool-auto-approval <disabled|approve-all>]`;
- `transcript <session-uuid>`;
- `follow <session-uuid>`;
- conversation import operations described by the
  [conversation-import operational surface](conversation-import.md);
- `blob upload <file>`;
- `blob metadata <sha256-digest>`;
- `blob read <sha256-digest> --offset <decimal> --length <decimal> --output <file>`;
- `reconcile <session-uuid> <turn-uuid> [--parts-file <path>] [--command-id <uuid> --defaults-version <decimal>]`;
- `stop <session-uuid> [--descendants] [--parts-file <path>] [--command-id <uuid> --defaults-version <decimal> --turn <uuid>]`;
- `approve <session-uuid> <tool-request-uuid> [--command-id <uuid>]`;
- `deny <session-uuid> <tool-request-uuid> --reason <text> [--command-id <uuid>]`;
- `runner status`;
- `runner replace <session-uuid> (--new-runner <runner-uuid> | --pending-enrollment <request-uuid> | --same-runner <runner-uuid>) --placement-revision <positive-decimal> [--command-id <uuid>]`;
- `runner abandon <session-uuid> --placement-revision <positive-decimal> [--command-id <uuid>]`;
- `runner promote --pending-enrollment <request-uuid> [--command-id <uuid>]`;
  and
- `chat <session-uuid>`.

For the five content-authoring mutations above, `--parts-file` names a file
whose complete UTF-8 contents are exactly one nonempty JSON array of closed text
or attachment part objects; array order is content order. The client reads it
with the same bounded-input and owner-private regular-file checks as other
content-bearing file options. Supplying the option makes standard input
unavailable for that mutation; omitting it preserves the existing single text
part read from standard input. Conversation content therefore never appears in
process arguments. Chat accepts the same closed object through `:part JSON` by
appending it to a pending sequence without submitting it; `:send` submits the
nonempty pending sequence only when the loop awaits neither a queued nor active
reply. It clears the sequence only after validating the resulting
`input_submitted` receipt; any rejection, connection loss, cancellation, or
ambiguous outcome retains the exact sequence. While either reply is pending,
`:send` reports a local busy error and likewise retains it. `:clear` discards it
locally. A malformed part clears the sequence and reports a local parse error.
An ordinary input line keeps its immediate one-text-part meaning only while no
part is pending; otherwise the client refuses it and requires `:send` or
`:clear`.

The terminal client provides these delegation commands for exact already-issued
tool requests:

- `session spawn <parent-session-uuid> <parent-turn-uuid> <tool-request-uuid> (--task <text> | --task-file <path>) (--background | --bound --on-parent-stopped <keep_running|stop|cancel> --on-parent-cancelled <keep_running|stop|cancel>)`;
- `session await <parent-session-uuid> <parent-turn-uuid> <tool-request-uuid> <child-session-uuid> --mode <foreground|background>`;
  and
- `session message <sender-session-uuid> <sender-turn-uuid> <tool-request-uuid> <peer-session-uuid> (--content <text> | --content-file <path>)`.

Delegation mutations print the exact spawning or awaiting request, child or peer
session, closed mode or policy, and recorded message or result identity. Follow
and chat render child results as delivered content labeled with the child
session; they never inline the child transcript. Lifecycle lines always show
outcome, typed reason, and provenance, including `continue_running`. Background
result wakes are labeled separately from foreground tool-result continuation so
a user can see whether an old turn resumed or a new parent turn became eligible.
Before presenting success, the terminal client requires every child or peer to
be distinct from the invoking session in spawn, both await-result modes, and
message receipts.

`chat` is the plain line-oriented interactive surface for one live session. It
opens one long-lived `follow_session` connection before accepting input and
keeps that connection dedicated to ordered snapshots, provider-text deltas, and
durable events. Submissions and in-loop control operations use a second
connection, opened through the one-request connection path; the client does not
multiplex requests onto the follow connection. The initial and every
resynchronized follow snapshot replace transient display state with the durable
transcript, and later provider-text deltas remain ephemeral presentation exactly
as they do for `follow`. Snapshot reconciliation selects the active turn or,
when there is none, the first acceptance-ordered queued turn; queued work is not
presented as an idle loop.

A line without the `:` prefix submits exact nonempty line content only while the
loop awaits neither a queued nor active reply and no multipart sequence is
pending. Line termination removes LF or CRLF only, retaining a bare trailing
carriage return at standard-input EOF. The returned `input_submitted` receipt
marks that turn queued; only its durable `turn_activated` event enables
active-turn controls and changes the displayed state to streaming. The closed
in-loop command set is `:part JSON`, `:send`, `:clear`, `:stop TEXT`,
`:steer TEXT`, `:approve ID`, `:deny ID REASON`, `:transcript`,
`:model ALIAS-UUID`, and `:quit`. Multipart commands map to one start-when-idle
`submit_input`; the remaining commands map to `stop_turn`, configuration-free
steering `submit_input`, `decide_tool_request`, `read_transcript`, and
`replace_session_defaults` requests, or local exit; ordinary input maps to
start-when-idle `submit_input`. `:stop` requires successor text because the
interrupt request cannot represent a standalone cancellation. `:deny` applies
the denial contract's POSIX edge-whitespace rule without rejecting other Unicode
whitespace. `:steer` requires an active turn, binds the exact turn currently
observed by the loop, and prints the typed steering receipt without waiting for
a successor turn. `:model` changes only the alias selection and copies the
observed dangerous-tool posture and system prompt into the forward-only
successor defaults epoch. Tool proposals and projected results are reread and
presented at their durable transition, and an approval wait prints its exact
request identity. Each successful decision rereads authoritative state and
immediately announces the next approval identity when the batch has one. All
process-derived text, including live deltas and tool content, uses the same
terminal-safe escaping as the other client verbs unless the invocation selected
`--raw-output`.

While a stoppable turn is active, the first Ctrl-C leaves the daemon turn
running and prints the `:stop TEXT` choice. During an approval wait, that first
interrupt instead names the exact `:approve` or `:deny` decision the turn
requires; active-only `:stop` and `:steer` are unavailable in that phase. A
second Ctrl-C exits and explicitly reports that the turn remains running. The
offer is bound to the exact observed turn phase and is reset by
resynchronization. Every follow resubscription and in-loop side request
continues polling Ctrl-C. Exiting while a mutation request is in flight reports
its potentially ambiguous outcome and terminates the loop, so the loop cannot
retry with a fresh command identity; the printed recovery values remain the
standalone exact-retry path.

`:quit` and standard-input EOF use the same exit report. While a turn is queued,
Ctrl-C, `:quit`, or standard-input EOF exits immediately and reports that the
turn remains queued; active-only `:stop` and `:steer` remain unavailable until
activation. Once the followed turn terminalizes, the client presents its exact
durable terminal material and accepts another ordinary input line.

`status` sends exactly one `read_operator_status` request through the configured
owner-only daemon socket; it never opens the database itself. It validates the
fixed section order and all six terminal counts before printing anything. The
first output line names those counts, followed by one human-scannable line per
row with `held`, `queued`, `convergence`, `stale_review_clearance`,
`lifecycle_week`, or `nonterminal_past_deadline` as its kind. A `lifecycle_week`
line prints each metric as `numerator/denominator` and, where the denominator is
not zero, the derived rate in parts per million, so an absent rate is visibly
absent rather than printed as zero. A held line prints its dispatch origin as
`origin=pull_request#<number>` or `origin=branch:<branch>`, naming the fact the
slot was taken from under one field whichever shape it has. A queued line prints
an occupant blocked by an independently commissioned live session as
`occupying=external:<sessions>`, distinguishing it from a watch dispatch, which
prints its identity ahead of its sessions. A convergence line prints
`non_green_count` beside the comma-joined `non_green` field, so an empty
inventory cannot collide with a check literally named `none`. Durations use
compact day, hour, minute, and second units. Process-derived text uses
terminal-safe field escaping unless `--raw-output` is selected. The final
`model_usage=omitted` line states that no cheap status aggregate is available:
model usage crosses this protocol only inside each complete session transcript,
and `status` does not issue one transcript read per session.

`list` is the complete unfiltered summary sequence. `search` is the separate
verb for `list_session_metadata`, whose filters, bounded page, and keyset cursor
are distinct: each invocation sends exactly one request and prints exactly one
page. `--title` is the exact case-sensitive substring query, each `--tag` adds
one required tag to the exact AND-filter, `--include-archived` selects the
archived-inclusive view, `--limit` is the page size and defaults to 50, and
`--after` is the exclusive session-identity cursor. Empty filter text, filter
text carrying U+0000, a repeated `--tag` value, a limit outside one through 100,
more than 256 required tags, a tag beyond 1,024 UTF-8 bytes, and a title query
plus tags beyond 262,144 aggregate UTF-8 bytes are all rejected as usage errors
before socket I/O, so every metadata-filter bound this page states reaches the
user as a named diagnostic rather than a generic local encode failure. Each
result is one line carrying the summary's session identity, archive state,
defaults version, model selection, dangerous-tool posture, last-writer actor and
timestamp, sorted comma-joined tags, and title. The actor prints as its wire
kind — `user`, `core`, `model`, `recovery`, or `tool` — without the reference
the kind carries, which the line has no field for. An unwritten metadata
snapshot prints `last_writer=none`, `updated_at_unix_micros=none`, and empty tag
and title values, which a present tag or title never is. A tag may itself
contain the space that ends its field, the comma that separates it from a
sibling, or the backslash that introduces an escape, so all three are escaped
inside a tag exactly as a control code point is; every backslash in the tag
field therefore opens an escape the client wrote, and the field decodes back to
the exact tag set. The title is the line's last field, keeps its spaces, and is
rendered to be read rather than decoded. When the page end names a continuation
cursor, the client prints `next_after_session_id=<uuid>` to standard error after
the results; a page is therefore never silently truncated, and that value is the
next invocation's `--after`. The client also validates that a page never exceeds
its requested limit.

`conversations` is the separate verb for `list_conversations` and follows the
same one-request, one-page discipline as `search`. `--title` is the exact
case-sensitive substring query, `--origin` selects native sessions, imported
conversations, or both and defaults to `all`, `--include-archived` selects the
archived-inclusive native view, `--limit` is the page size and defaults to 50,
and `--after` is the exclusive origin-qualified cursor spelled exactly as a
prior page printed it. Empty filter text, filter text carrying U+0000, a title
query beyond 262,144 UTF-8 bytes, a limit outside one through 100, and a cursor
that is not `native:<uuid>` or `imported:<uuid>` are rejected as usage errors
before socket I/O. Each result is one origin-tagged line whose title is the
line's last, terminal-safely escaped field: a native line carries
`origin=native session_id=<uuid> archived=<bool> defaults_version=<decimal> title=<title>`,
and an imported line carries
`origin=imported imported_conversation_id=<uuid> format=<format> entry_count=<decimal> title=<title>`.
A listed session identity is directly usable by `transcript`, `follow`, and
`send`; a listed imported conversation is directly usable by `continue`, whose
greatest `--through-position` is the listed entry count. When the page end names
a continuation cursor, the client prints `next_after=<origin>:<uuid>` to
standard error after the results, and it validates ordering, the requested
bound, and the terminal count and cursor exactly as `search` does.

The `review` command adds these headless workflow verbs:

- `create-target <target> --provider <key> --repository <key> --change-request <decimal> --head-revision <revision> --base-revision <revision>`;
- `start-run <target> <run> <pass> --workflow <kind> --session-id <session> --accepted-input-id <input>`;
- `activate-pass <run> <pass> --turn-id <turn>`;
- `complete-pass <run> <pass> --outcome <succeeded|failed|blocked|cancelled> [--turn-id <turn>] [--output-frontier-id <frontier>]`;
- `record-finding <run> <pass> --turn-id <turn> --output-frontier-id <frontier> --finding-id <finding> --file-path <path> --title <text> --body <text> --severity <severity> --is-real-confidence <basis-points> --severity-label-confidence <basis-points> --category <key>`;
- `record-findings <run> <pass> --turn-id <turn> --output-frontier-id <frontier> --findings-file <path>`;
- `record-finding-event <run> <pass> --turn-id <turn> [--output-frontier-id <frontier>] --finding-id <finding> --event-ordinal <decimal> --event <accepted|rejected|duplicate|superseded|stale|fixed|blocked-with-reason>`;
- `start-orchestration <attempt> <target> --concern-set-version <key> --import-template-name <name> --judgment-template-name <name> --repair-template-name <name> --publication-template-name <name> --concerns-file <path>`;
- `record-import-outcome <attempt> --outcome <succeeded|failed|blocked|cancelled> [--pass-id <pass>] [--external-link-id <link>] [--context-digest <digest>]`;
- `record-concern-outcome <attempt> <concern> --outcome <succeeded|failed|blocked|cancelled> [--pass-id <pass>]`;
- `record-judgment-plan <attempt> <analysis-pass> --members-file <path>`;
- `record-judgment-effect <attempt> <finding> --outcome <applied|failed|blocked|cancelled> [--event-pass-id <pass>]`;
- `record-repair-outcomes <attempt> --outcomes-file <path>`;
- `record-publication-outcomes <attempt> --outcomes-file <path>`;
- `reserve-external-link <finding> <link> --provider <key> --object-kind <review|review-thread|review-comment|change-request-comment>`;
- `attach-external-link <link> <run> <pass> --turn-id <turn> --output-frontier-id <frontier> --external-object <opaque-key> --event-ordinal <decimal>`;
- `read-orchestration <attempt>`;
- `list-findings <run>`;
- `read-target <target>`;
- `read-run <run>`; and
- `read-finding <finding>`.

Every review mutation accepts `--command-id <uuid>`. Target creation accepts an
optional `--stack-parent-target-id`; finding recording accepts an optional
paired line range, diff side, and recommended fix. Finding-event variants use
ordinary closed flags: rejected requires only `--reason`, duplicate and
superseded require only `--referenced-finding-id`, and blocked-with-reason
requires `--reason` and optionally `--external-link-id`. Accepted, stale, and
fixed admit none of those variant fields. Blocked-with-reason forbids
`--output-frontier-id`; every other event requires it. The terminal rejects
variant fields or pass/frontier/link/digest evidence that contradicts the
selected closed outcome before socket I/O. External-link reservation precedes
the provider write; attachment follows a successful provider write and carries
the exact producing pass, turn, frontier, opaque provider object, and finding
event ordinal.

Only bounded inventories use JSON files. Their exact top-level wrappers are
`{"findings":[...]}` for `--findings-file`, `{"concerns":[...]}` for
`--concerns-file`, `{"members":[...]}` for `--members-file`, and
`{"outcomes":[...]}` for either outcomes file. The findings inventory may be
empty and seals the read-only pass exactly once; `record-finding` remains the
one-finding convenience form. Every wrapper and nested protocol object denies
unknown members; canonical UUID, decimal, digest, tag, nullable-member,
uniqueness, and outcome-correlation rules remain those of the wire request. The
terminal reads at most three quarters of the 8 MiB frame cap plus one byte and
refuses a file reaching that extra byte before printing a command identity. The
reserved quarter covers the request envelope and re-encoding overhead; exact
frame encoding remains authoritative. Ordinary request validation then enforces
the 32-concern and 1,024-member bounds before connecting. Exact ambiguous retry
therefore needs the same file bytes semantically decoded to the same closed
inventory, as well as the same scalar arguments and printed command identity.

Orchestration reads validate the selected attempt and the protocol-level frozen
inventory correlations before output. The first line reports attempt, target,
state, concern count, and all five progress counts; subsequent lines report the
concern-set key, four frozen stage-template digests, then every concern's key,
template digest, status, and optional pass. Process-derived keys remain
terminal-safely escaped.

Runner creation accepts either an exact runner or a capability class, never
both. Beyond the selector every placement flag is independent: `--repository`,
`--credential-profile`, and `--working-directory` may each be supplied or
omitted in any combination, a selector requires none of them, and an omitted
flag encodes that explicit absence instead of letting the client or daemon
choose a value. A credential profile with no repository, a repository with no
credential profile, and a runner with neither are all ordinary invocations
([runner protocol and placement](runner-protocol.md)). Omitting
`--sandbox-profile` selects `workspace-restricted` before the request is
encoded, so `ambient` requires that exact flag, and either profile is admissible
with or without a repository. Tool overrides require a runner selector,
duplicate or cross-listed names fail locally, and every selected fact is printed
with the created session. `runner status` starts with page size 100 and a null
cursor, validates and prints each status, failure, and leak page without
buffering prior pages, copies a nonnull terminal cursor into the next request,
and stops only at a null cursor. It prints profile availability, runner-reported
failure detail, and leaks without host paths. Replace, abandon, and promote
print command identity before socket I/O; replace and abandon also print the
expected placement revision, and each requires the complete recovery set on
retry. Replace accepts a different live runner, the exact pending enrollment
request reported by `runner status`, or — for a registration-triggered loss —
the same runner; the three targets are mutually exclusive. Promote names only
the pending enrollment request and no session. Abandon creates no successor
input.

`send` without `--parts-file` reads the exact input text from standard input
through EOF; with `--parts-file` it reads only that file's ordered part array
and does not read standard input. Neither form accepts conversation content in
process arguments. Empty or oversized input fails before socket I/O. Without
`--queue` it uses start-when-idle behavior. With `--queue`, a fresh invocation
reads the authoritative transcript, names the active turn, submits `queue`, and
follows the returned origin turn through its own terminal outcome; it therefore
waits while the predecessor finishes and while the queued turn runs. Exact
queued-send recovery supplies command identity, defaults version, and expected
turn together. While the returned turn is still queued, a model-call or tool
recovery wait on the turn currently holding the active slot and blocking its
activation returns the existing recovery-required diagnostic instead of waiting
for successor activation.

`steer` reads content the same way. A fresh invocation observes and prints the
active turn, submits configuration-free `steer`, validates the typed receipt,
prints `accepted_input`, acceptance `position`, and `source_turn`, then exits
without waiting because no new turn exists. It fails locally when its initial
transcript has no active turn; a race after observation reaches the daemon's
typed `no_active_turn` or `active_turn_mismatch` rejection. Exact recovery
supplies command identity and expected turn together. A distinct verb makes the
no-successor, receipt-only completion semantics explicit; queueing remains a
`send` option because it still creates and waits for a separate reply-bearing
turn.

`create --template NAME` sends the name, the command identity, and the selected
session path placement, and validates the ordinary `session_created` receipt.
The optional `--placement` is admitted only as a validated non-root path unless
`--root-global-read` explicitly acknowledges a one-segment global-read root.
Without it, creation sends the pathless compatibility value. Clap rejects
combining it with `--model`, `--alias`, or `--system-prompt-file` before socket
I/O; explicit flags never override template values. The choice keeps one
invocation's complete creation defaults under either client control or daemon
configuration, not both. The runner flags are outside that alternative and
remain admissible with `--template`, because a template supplies defaults and
neither placement axis: the choices compose, and a selected session path or
runner placement is carried into the request rather than dropped. The
`templates` verb sends one `list_templates` request, validates strict name order
and the terminal count, and prints one `name=<name> version=<decimal>` line per
summary. Template prompt text, model selection, approval posture, and digest do
not cross this listing surface.

`place` sends the exact caller-observed positive placement version and one
complete replacement. Its receipt prints the successor version and replacement;
`list` prints those same current facts for every session. `--pathless` restores
legacy unrestricted reads explicitly. Root placement again requires the explicit
`--root-global-read` acknowledgement, so no client syntax can silently turn a
one-segment path into global read.

`--system-prompt-file` likewise carries a path, never prompt content in a
process argument: the client reads one bounded file snapshot before socket I/O
and rejects an empty, oversized, non-UTF-8, or U+0000-bearing prompt locally,
then sends the exact text.

`blob upload` opens the named regular file once, streams it with a bounded
buffer to determine its digest and length, rewinds the same descriptor, and uses
begin, bounded appends, and commit on one connection. It validates every
cumulative acknowledgement and the final identity; an already-present receipt
revalidates the same descriptor before sending no append. `blob metadata` prints
only digest, byte length, and replica count. `blob read` makes one bounded
exact-range request, validates its digest, offset, and decoded length, then
writes only those bytes to the named output. Local paths never cross the wire or
appear in daemon logs.

**Implemented behavior.** The `goal` verbs expose only commission, inspection,
resume, stop, and supersession. Mutations print a generated command identity
before socket I/O and validate the applied event ordinal and generation receipt.
`goal show` validates and spools the complete history before printing the
current projection and every event with terminal-safe statement, need, guidance,
and report text. Guidance that does not replace scope remains a resume or the
`steer` verb; no edit-in-place command exists.

`reconcile` reads its successor content the same way and names the parked turn
the operator observed in the session transcript. It prints the same command and
defaults recovery values as an ordinary `send`, then follows the accepted
successor turn to its own terminal, so one invocation both records the
reconciliation decision and continues the conversation.

`imported` prints one imported conversation's selectable positions. Each result
is one line carrying the position, imported entry identity, speaker attestation,
content kind, and — for an entry with exact attested text — its truncation
marker and bounded preview. An entry carrying no exact attested text omits both
preview fields rather than printing a placeholder that empty attested text could
not be told apart from. The preview is the line's last field and its truncation
marker precedes it, so preview text cannot forge either. A final
`entry_count=<decimal>` line names the total, which is also the greatest
position `continue` admits. The client validates the complete sequence, its
position contiguity, and its terminal count into a spool before presenting any
line.

`continue` requires the imported position and relationship explicitly; it never
treats resume as an implicit default and has no implicit position. Its position
is either a positive decimal or the exact sentinel `latest`. `latest` is
resolved client-side against `read_imported_conversation`'s entry count before
the durable command is constructed, and the resolved ordinal is printed as the
recovery value `through_position=<decimal>` on standard error; the wire request
therefore always carries a concrete position. An imported conversation is
immutable, so that resolution is stable and an exact replay names the same
boundary. Success prints the created session identity, which is immediately
usable by `send`, `transcript`, and `follow`. The command identity, imported
conversation, resolved position, relationship, and model selection are the
complete replay inputs.

`stop` reads its successor content the same way. When `--turn` is absent it
reads the authoritative transcript, selects the single turn holding the active
slot, and fails with a typed local error when no turn is active; the selected
turn is printed as a recovery value before the mutation. It then prints command
identity, defaults version, and expected turn and follows the accepted successor
turn to its own terminal, so one invocation both records the stop and continues
the conversation.

`approve` and `deny` name the pending request printed by `transcript` or
`follow` — the awaiting turn line names the request identity and the
`assistant_tool_use` entry names its tool and arguments. Each verb validates
that the receipt echoes the exact request and decision it sent and prints one
`tool_request=<uuid> decision=<approve|deny>` line.

When `--command-id` is absent, the client generates a fresh UUIDv7 identity and
prints it to standard error before any socket I/O. Fresh queued sends and
steering also print the exact active turn they observe. `send` and `stop` first
read the session summary and use its defaults version, then print that expected
version to standard error before sending the mutation. `model` issues one
`read_session_defaults` for the current epoch; it copies the defaults version,
dangerous-tool posture, and — when no prompt option was given — the exact
current system prompt, prints the version and posture, and changes only the
requested fields. Thus every client-generated or server-discovered scalar
recovery value is visible before its commit can become ambiguous; the
content-sized prompt is never echoed. Exact replay also requires the original
selection, template name, imported-conversation, or session arguments and, for
`send`, `steer`, `reconcile`, and `stop`, the exact standard-input content; the
client does not echo that potentially sensitive input or synthesize a shell
command. Template recovery needs no digest or copied values: equal replay uses
the original command and name. Its ambiguity diagnostic directs the user to
retry the original command with those arguments and input plus any printed
recovery values. For recovery, the user supplies the printed command identity.
An ordinary `send` and `reconcile` also require the exact `--defaults-version`;
`send --queue` requires defaults version and the exact expected `--turn`;
`steer` requires that turn but no defaults version; and `stop` requires command
identity, defaults version, and the exact expected `--turn`, because a stopped
turn cannot be rediscovered once the first handling terminalizes it. `model`
instead requires all three printed facts — command identity, defaults version,
and dangerous-tool posture — plus the original prompt option: a re-supplied
`--system-prompt-file` or `--clear-system-prompt` is re-read or re-applied
exactly, while a copied-forward prompt is re-read from the immutable epoch the
printed defaults version names, so the retried payload is byte-exact under
concurrent replacements without printing megabyte content. Each recovery set is
all-or-none. The client never silently substitutes a new command identity for an
ambiguous attempt. It uses a fresh nonzero request identity per connection,
validates that a defaults receipt is the exact successor carrying the requested
selection, copied posture, and exact replacement prompt, validates that a
decision receipt echoes the exact request and decision it sent, renders only
known messages, and exits nonzero on protocol or application errors other than
the follow-specific `resync_required` control case, which reconnects for a fresh
snapshot.

Review mutations print a generated command identity before socket I/O and an
ambiguous diagnostic directs the operator to repeat the same verb, identifiers,
scalar options, and JSON inventory with that identity. An invalid or oversized
JSON file fails before identity generation. Mutation receipts must echo the
selected run/pass, finding/status, or attempt/state relation exactly; a
well-formed but incoherent receipt is a protocol failure. Review reads validate
selected identities and a run response's pass presence, pass identity, run
ancestry, and target ancestry before writing output; finding lists additionally
validate their start marker, strict identity order, maximum 32-item inventory,
terminal count, and end marker before success. `record-finding` rejects a zero
or greater-than-32-bit line number, a line end before its start, and either
confidence above 10,000 basis points as a usage error before socket I/O.
`record-finding-event` likewise rejects zero or greater-than-32-bit event
ordinals and every event/frontier or variant-field contradiction locally. Every
process-derived review text field follows the same terminal-safe escaping and
`--raw-output` opt-in below. Target output distinguishes an absent base revision
from every present value, run output carries its complete frozen policy, finding
output carries every immutable content field, location, severity, confidence,
category, optional-repair presence, ancestry identity, status, and event count,
and orchestration output carries the complete frozen concern inventory and
progress projection.

The client validates each complete snapshot and its terminal counts into an
owner-private anonymous temporary-file spool before replay or presentation.
Turn, model-call, and source-qualified entry identity indexes are disk-backed
too, so the wire's unbounded aggregate snapshot size does not become unbounded
client memory. Before adopting an initial or resynchronized snapshot cursor,
`follow` presents its acceptance-ordered turn projections, including queued user
content, active attempt and current-call state, recovery waits, and terminal
state. It validates but does not print the snapshot's usage section; follow
updates remain durable transitions rather than provider evidence. A transition
committed at or below that cursor therefore remains visible even when it has not
added a semantic transcript entry.

`transcript` replays the validated usage rows after ordinary turn and frontier
output. It prints separate `reported` and `estimated` token subtotals per turn
that has terminal calls and at session scope, including `terminal_calls=0` with
`0/0` field coverage. The two provenances are never silently summed. Each of the
four token fields carries both its subtotal and `present_calls/terminal_calls`
coverage. Zero is printed as zero; a field with no supplied calls is printed as
`unreported`, and partial coverage is never silently treated as complete.
Snapshot validation rejects noncontiguous indices, unknown turn identities,
repeated model-call identities, or usage rows outside strict turn-acceptance and
per-turn model-call-UUID order. Currency presentation aggregates only per-call
derived figures sharing the same usage provenance, billing label, and rate
window; each line states that labeled triple and its costed-call count. These
client totals are presentation arithmetic over exact per-call read evidence and
use an anonymous temporary-file index so distinct rate windows do not grow
client heap. The client scans that index once for output; totals are never
persisted, and an addition that cannot retain both operands exactly rejects the
snapshot instead of reporting a rounded total.

The unbounded aggregate session-summary sequence is bounded the same way. `list`
validates ordering and the terminal count while spooling summary frames to an
anonymous temporary file, then presents them only after the complete sequence
validates. `send` validates the whole sequence with constant memory and retains
only the selected session's defaults version. `search` and `conversations` each
spool one bounded page the same way and present it only after that page's
ordering, count, and cursor validate; `model` reads the same validated metadata
pages while retaining only the selected session's defaults facts.

After completion, `send` rereads and prints only authoritative committed
assistant text produced for its exact turn. A failed or refused turn produces a
typed diagnostic and a nonzero exit without reply text; when a failed terminal
call carries a provider cause, both CLI and native client render the closed
classification (for example, credential rejection versus retry-later overload)
without provider prose. Cancelled and reconciliation-required turns use their
distinct typed diagnostics. The native synchronization surface reports an
unrecognized frame as a nonfatal decoding diagnostic, while a known frame that
fails its closed shape is named as a malformed-known protocol violation.
`follow` prints the initial transcript, ephemeral provider-text deltas, and
subsequent typed durable updates until interrupted. Each delta is flushed as one
line:
`provider_text_delta session=<session> turn=<turn> call=<call> part=<index> content=<text>`.
Accepted `transcript_user_entry` members use the terminal line shape owned by
[blob storage](blob-storage.md), preserving their ordered part JSON without
rendering attachment bytes. That JSON's default terminal serialization
additionally renders DEL and C1 characters inside string values as lowercase
four-hex-digit JSON escapes; `--raw-output` is the explicit opt-in to ordinary
compact JSON that may carry those characters literally. By default the
provider-delta trailing text field escapes line feed and every other C0 code
point, DEL, and C1 code point, so provider output cannot forge another event
line or execute terminal controls; `--raw-output` remains the explicit opt-in to
unchanged text. Snapshots render a model boundary as `model_identity_changed`
with its turn, defaults version, selected model, source session, and entry
identity. By default every process-derived text field written to a terminal
preserves line feed but renders every other C0 code point, DEL, and C1 code
points as visible `\u{...}` escapes, preventing ESC/OSC execution. A metadata
title or tag shares its output line with named neighbors, so `search` escapes
line feed in those two fields as well, and a tag additionally escapes its own
delimiters and escape introducer, using the same `\u{...}` vocabulary; no
metadata value can forge another result row, field, or tag. `--raw-output` is
the explicit opt-in that writes those fields unchanged; the same safe-rendering
choice covers assistant text, typed diagnostics, and durable updates. Each
complete raw text value is flushed before the client awaits another frame,
without adding a delimiter.

The `signalbox-debug` binary is a development harness, not a protocol client.

### Credential-pool preparation failure

**Committed unimplemented functionality.** No present event or transcript state
admits this shape. This section owns the wire projection of the `pre-call fail`
and `wait-transition fail (no call)` endings of
[the credential-availability machine](credential-availability.md), which share
one wire projection; that page states which endings project to a terminal state
and which to an active one. The wire projection must add
`failed_credential_pool_exhausted { terminal_frontier_id, terminal_attempt_id, failure_entry_id, pool_policy_id, policy_members, members }`
as a distinct `transcript_turn.state` variant and
`turn_credential_pool_exhausted { turn_id, terminal_attempt_id, failure_entry_id, terminal_frontier_id, pool_policy_id, policy_members, members }`
as its live event. It must also add
`read_credential_pool_policy { session_id, turn_id, pool_policy_id }`. That read
is admitted only when the caller may read the named session and its named turn
references that exact immutable policy; mismatch is `unknown_pool_policy`. Its
`credential_pool_policy { pool_policy_id, policy_members }` response loads and
reconstitutes the policy header and ordered membership rows directly, rather
than copying either failure projection. The two failure `members` arrays have
identical nonempty content in frozen policy order. Both shapes additionally
carry `policy_members`, the immutable policy's complete nonempty ordered array
of profile references. It has the same length as `members`, and each evidence
item's `profile` must equal the same-ordinal `policy_members` value. Each
evidence item carries `profile`, required-nullable `reset_at_unix_ms`, and one
closed `exclusion` object:

- `profile_quarantine { record_generation }`;
- `membership_exclusion { record_generation }`;
- `session_displacement { record_generation }`; or
- `chain_exclusion { predecessor_model_call_id }`.

One member can satisfy several of these at once. The producer selects exactly
one by the fixed precedence in which they are listed above, which is
widest-scope-first: a profile-wide `profile_quarantine` outranks a
`membership_exclusion` covering one membership across every session, which
outranks a `session_displacement` covering one session, which outranks a
`chain_exclusion` covering one successor chain within one turn. Two producers
therefore cannot describe one exhaustion differently. `reset_at_unix_ms` is
present only when every exclusion active for that member at the failure commit
is of a kind that *expires* at the reset it reports, and is then the latest of
them; any exclusion with no reset, and any whose kind clears by something other
than time passing, makes it null. Reporting a reset is not sufficient, by the
same rule and for the same reason the wait deadline uses
([credential availability](credential-availability.md)) — publishing a time no
wake honors would name a recovery moment that never arrives. A wake can
consequently never be scheduled while an indefinite condition still bars the
member. The narrower correlations the selected item omits are not lost: each
remains an active durable record that
[credential-exclusion administration](#credential-exclusion-administration)
lists and clears by its own exact target.

Generations and reset instants are positive canonical decimal strings;
`pool_policy_id` uses the `PoolPolicyId` UUID spelling above, and every other
identity uses its already-owned bounded wire spelling. The snapshot and event
carry no credential bytes, path, provider prose, or current-configuration
lookup. The client validates the nonempty `policy_members` inventory and its
one-to-one order and identity equality with `members`, then requires the
session-correlated policy read to return that same complete ordered inventory,
before exposing the terminal state. A producer that omits or reorders evidence
therefore cannot make its second event-local copy authoritative. Reconnect and
live follow project the same typed cause rather than a generic failed turn.
Configuration admission limits each profile and pool name to 256 UTF-8 bytes and
each pool to 1,024 members, reserving enough of the 8 MiB frame for the complete
duplicated failure evidence under worst-case JSON escaping; this projection is
never paginated or truncated. Version one rejects both new variants and the
policy read until the daemon and client implement them together, and no present
producer may terminalize a turn for this pre-call cause.

**Committed unimplemented functionality — credential-availability projection.**
No present request, event, transcript message, or closed turn-state object
exposes an availability-successor chain or credential-availability wait — the
`successor`, `contended-wait`, and `exhausted-wait` endings of
[the credential-availability machine](credential-availability.md), whose wire
projection this section owns. The predecessor, authorizing cause, selected
profile, and wait evidence are themselves committed future storage — no present
migration, repository operation, or reconstitution path supplies them — so the
wire cannot project them until that storage exists, and must then add a
version-one shape together with its daemon and client consumers. The wait must
be projected as an active state retaining the same turn and session slot.
Nothing is committed about a client-visible successor relation: whether the
predecessor, cause, and successor chain are exposed, and how, is the open
question at
[model fallback and provenance](../open-questions.md#model-fallback-and-provenance);
this section states the wait compatibility constraint only. Whatever first makes
either wait reachable must include this coordinated wire projection; admitting
`park` in static configuration alone does not make the state reachable. Until
then, transcript snapshots expose per-call usage rows and final turn state only;
no client-visible claim is made for the committed storage evidence.

## Open edges

Deferred transport, compatibility, update-stream, retention, and operation
questions are cataloged under
[Protocols and persistence](../open-questions.md#protocols-and-persistence);
later client-form choices are cataloged under
[Client scope](../open-questions.md#client-scope). Richer metadata query
language and creation-derived visibility are cataloged under
[Session organization, visibility, and retention](../open-questions.md#session-organization-visibility-and-retention).
Wire and projection data for future graded approval judgments are cataloged
under [Graded approval judging](../open-questions.md#graded-approval-judging).

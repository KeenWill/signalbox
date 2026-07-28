# Process protocol

The baseline Signalbox process protocol version one and the terminal client that
consumes it were verified through PR #177 (`agent/terminal-client`); the
`signalboxd` binary name this page states for the serving process was verified
through PR #258 (`agent/signalboxd-rename`). The conversation-import stack adds
protocol version two for the conservative imported transcript-snapshot
projection described here. The tool-loop stack adds protocol version three for
tool-bearing projection; versions one and two retain their closed message
vocabularies unchanged. The session-metadata stack adds protocol version four
for paginated metadata listing, single-session metadata reads, and durable
complete-snapshot replacement; versions one through three retain their closed
request and message vocabularies unchanged. The one-file conversation-import
surface adds protocol version five; versions one through four remain unchanged,
verified through PR #252 (`agent/import-surfaces`). The mid-session
model-selection stack adds protocol version six for complete, forward-only
defaults replacement and its model-identity frontier entry; versions one through
five remain unchanged, verified through PR #272 (`agent/mid-session-model`). The
owner turn-reconciliation stack adds protocol version seven for the single
`reconcile_turn` request; versions one through six retain their closed request
and message vocabularies unchanged, verified through PR #281
(`agent/turn-reconciliation-recovery`). The turn-control stack adds protocol
version eight for the `stop_turn` and `decide_tool_request` requests, taken
while version seven was still reserved by the then-open turn-reconciliation
stack; versions one through seven retain their closed request and message
vocabularies unchanged, verified through PR #291 (`agent/turn-control-verbs`).
The session system-prompt stack adds protocol version nine for the required
system-prompt member on session creation and defaults replacement, the
single-session defaults read, and their receipts; versions one through eight
retain their closed request and message vocabularies unchanged, verified through
PR #286 (`agent/session-system-prompt`). The imported-continuation stack adds
protocol version ten for the single `create_session_from_imported_frontier`
request, taken while nine was still reserved by that then-open stack, verified
through PR #294 (`agent/continue-imported-conversation`). The review-workflow
surface adds protocol version eleven, verified through PR #295
(`agent/review-workflow-surface`). When the provider-text streaming branch
began, nine remained reserved by open PR #286 and no open pull request numbered
#298 or later reserved another protocol version. The ephemeral provider-text
surface therefore takes version twelve, verified through PR #300
(`agent/token-level-streaming`). Versions thirteen and fourteen are reserved by
the then-open steering and token-usage stacks, and fifteen by the then-open
imported-conversation inspection stack. The unified conversation-listing surface
therefore takes version sixteen for the single read-only `list_conversations`
request, verified through PR #304 (`agent/unified-conversation-listing`). The
implementation in this stack speaks versions one through twelve and sixteen
while thirteen, fourteen, and fifteen remain unsupported, and its terminal
client selects version sixteen. Its `search` verb over version four's metadata
list was verified through PR #283 (`agent/session-search-cli`; terminal client
surface only). This page's version-four last-writer member spelling was verified
through PR #288 (`agent/audit-fix-docs-coherence`). This page is the normative
boundary between a local client process and `signalboxd`; domain values,
PostgreSQL records, and wire messages remain distinct representations.

Invariant law lives in [docs/invariants.md](../invariants.md), cited here by
tag. Durable update storage and the delivered-through cursor are owned by
[persistence-protocol](persistence-protocol.md).

## Transport and trust boundary

Every admitted version uses one Unix domain stream socket. The daemon requires
its path in `SIGNALBOX_SOCKET_PATH`; the terminal client uses its
`--socket <path>` override when present and otherwise requires that environment
value. `signalboxd` binds the socket with owner-only `0600` permissions. The
configured path must be absolute and must end in an explicit filename component;
a trailing separator, `/.`, or `/..` is rejected rather than normalized. The
daemon canonicalizes its existing parent once and uses that resolved parent for
the socket lifetime; the parent must be a directory owned by the daemon's
effective user with traditional permission mode exactly `0700`. This
owner-private immediate parent is required even when the socket node itself has
mode `0600`; version one does not rely on every supported Unix implementation
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
reserved `<socket-path>.identity` name by an abrupt prior exit only when the
public and reserved names still identify the same owned socket. An orphaned or
differently paired entry at the reserved name fails startup without
modification. It then handles the final path as follows:

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
still names this daemon's socket and removes that path, then releases the
identity link and path lock.

The transport is local-machine and single-user only. The process protocol's lack
of authentication is provisional; none of the versions has an authorization
exchange or remote transport. Socket filesystem access is the deployment
boundary; it is not represented as application-level owner proof.

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
acceptance reuses the decoded allocation. Conversation-import source bytes
likewise move directly into a dedicated import admission path. At most one
conversion-and-store operation runs at a time. A decoded import waiting for that
permit retains its inbound-frame permit, so queued source bytes remain inside
the existing 64 MiB aggregate frame budget; only the admitted import can retain
the expanded aggregate or use repository work. The admitted service runs on the
blocking pool so synchronous conversion does not occupy an asynchronous runtime
worker. Its source and aggregate are dropped and its permit is released before
response output. A peer that stops reading responses therefore cannot retain
rejected input or completed import content.

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

- `version`: JSON integer `1` through `12`, or `16`;
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
spellings decode to the same name. A version other than one through twelve or
sixteen produces an `unsupported_version` error naming the supported versions,
then the server closes the connection. Every response uses the request's
admitted version; when no version can be admitted, the server error uses version
one as the pre-admission fallback. A client speaking a version above one admits
that version-one fallback only for `malformed_frame` or `unsupported_version`,
then applies the ordinary request-identity check; every other response-version
mismatch fails locally. A server error uses `request_id = "0"` only when the
incoming frame prevents recovery of a valid nonzero identity; zero is never a
valid client identity or success-response identity. Leading zeroes, a plus sign,
whitespace, and any spelling other than the shortest ASCII decimal form are
invalid.

The server may close a connection after any error. Clients never reinterpret an
unknown message as a known one.

Why: a required version on every independent line makes captured traffic and
errors self-describing without connection-global negotiation state.

## Client requests

Request objects carry a required string `type` and reject fields not admitted by
that variant.

| Type                                    | Version | Additional required members                                                                                                                                                                                                                                          | Meaning                                                                                                                                                                                                                                           |
| --------------------------------------- | ------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `create_session`                        | 1+      | `command_id` (canonical UUID string), `initial_model_selection` (selection object); version nine and above also require `system_prompt` (string or null)                                                                                                             | Create an owner-initiated session with no ancestry and establish defaults version one.                                                                                                                                                            |
| `list_sessions`                         | 1+      | none                                                                                                                                                                                                                                                                 | Read all current sessions as legacy summaries, ordered by session identity.                                                                                                                                                                       |
| `submit_input`                          | 1+      | `command_id` and `session_id` (canonical UUID strings), `content` (string), `expected_defaults_version` (canonical decimal string)                                                                                                                                   | Submit exact owner text as `StartWhenNoActiveTurn`, using the caller-observed defaults version and no per-input model override.                                                                                                                   |
| `read_transcript`                       | 1+      | `session_id` (canonical UUID string)                                                                                                                                                                                                                                 | Read one authoritative durable transcript snapshot and its observation cursor.                                                                                                                                                                    |
| `follow_session`                        | 1+      | `session_id` (canonical UUID string)                                                                                                                                                                                                                                 | Receive an initial authoritative snapshot, then this process incarnation's ordered durable update events committed after the snapshot cursor for the same session; versions twelve and above additionally receive ephemeral provider-text deltas. |
| `list_session_metadata`                 | 4       | `required_tags` (string array), `title_contains` (string or null), `include_archived` (boolean), `page_size` (canonical decimal string), `after_session_id` (canonical UUID string or null)                                                                          | Read one filtered metadata-summary page in session-identity order.                                                                                                                                                                                |
| `read_session_metadata`                 | 4       | `session_id` (canonical UUID string)                                                                                                                                                                                                                                 | Read one complete current metadata snapshot.                                                                                                                                                                                                      |
| `replace_session_metadata`              | 4       | `command_id` and `session_id` (canonical UUID strings), `metadata` (the complete metadata object below)                                                                                                                                                              | Durably replace one complete metadata snapshot as the owner actor.                                                                                                                                                                                |
| `import_conversation`                   | 5       | `format` (`claude_code_session_jsonl_v2` or `codex_rollout_jsonl_v1`), `source` (canonical padded base64 string)                                                                                                                                                     | Convert and idempotently resolve or insert one complete external conversation snapshot.                                                                                                                                                           |
| `create_session_from_imported_frontier` | 10      | `command_id` and `imported_conversation_id` (canonical UUID strings), `through_position` (positive canonical decimal string), `relationship` (`resume` or `fork`), `initial_model_selection` (selection object)                                                      | Create an independent live session seeded through the selected inclusive imported position.                                                                                                                                                       |
| `replace_session_defaults`              | 6       | `command_id` and `session_id` (canonical UUID strings), `expected_defaults_version` (canonical decimal string), `model_selection` (selection object), `dangerous_tool_auto_approval` (boolean); version nine and above also require `system_prompt` (string or null) | Install one complete immutable defaults epoch as the owner actor, conditional on the exact current epoch.                                                                                                                                         |
| `reconcile_turn`                        | 7       | `command_id`, `session_id`, and `expected_active_turn_id` (canonical UUID strings), `content` (string), `expected_defaults_version` (canonical decimal string)                                                                                                       | Supply the owner reconciliation decision for the named turn parked on an ambiguous model call, accepting `content` as its immediate successor origin.                                                                                             |
| `stop_turn`                             | 8       | `command_id`, `session_id`, and `expected_active_turn_id` (canonical UUID strings), `content` (string), `expected_defaults_version` (canonical decimal string)                                                                                                       | Apply the accepted interrupt treatment to the named active turn, accepting `content` as its immediate-successor origin.                                                                                                                           |
| `decide_tool_request`                   | 8       | `command_id`, `session_id`, and `tool_request_id` (canonical UUID strings), `decision` (a decision object below)                                                                                                                                                     | Supply the owner decision for one pending tool request through the canonical decision command.                                                                                                                                                    |
| `read_session_defaults`                 | 9       | `session_id` (canonical UUID string), `defaults_version` (canonical decimal string or null)                                                                                                                                                                          | Read one complete immutable defaults epoch: the current one for null, otherwise exactly the named one.                                                                                                                                            |
| `list_conversations`                    | 16      | `title_contains` (string or null), `origin` (`native`, `imported`, or `all`), `include_archived` (boolean), `page_size` (canonical decimal string), `after` (cursor object or null)                                                                                  | Read one filtered unified conversation-summary page across native sessions and imported conversations in unified keyset order.                                                                                                                    |

Version eleven adds these review-workflow requests. Every `*_id` is a canonical
UUID string, ordinal and count values are canonical decimal strings, and every
nullable member is required with either its value or JSON `null`.

| Type                                | Additional required members                                                                                                | Meaning                                                                  |
| ----------------------------------- | -------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------ |
| `create_review_target`              | `command_id`, `target_id`, `provider`, `repository`, `subject`, `head_revision`, `base_revision`, `stack_parent_target_id` | Register one immutable external target snapshot.                         |
| `start_review_run`                  | `command_id`, `target_id`, `run_id`, `pass_id`, `workflow`, `session_id`, `accepted_input_id`                              | Admit one run and its sole session-backed pass.                          |
| `activate_review_pass`              | `command_id`, `run_id`, `pass_id`, `turn_id`                                                                               | Bind the queued run and pass to their canonical active turn.             |
| `record_review_findings`            | `command_id`, `run_id`, `pass_id`, `turn_id`, `output_frontier_id`, `findings`                                             | Atomically succeed a read-only pass with its complete finding inventory. |
| `record_review_finding_disposition` | `command_id`, `run_id`, `pass_id`, `turn_id`, `output_frontier_id`, `finding_id`, `event_ordinal`, `disposition`           | Atomically conclude a judgment pass and append its finding event.        |
| `reserve_review_external_link`      | `command_id`, `external_link_id`, `finding_id`, `provider`, `object_kind`                                                  | Reserve one provider object identity before an external write.           |
| `attach_review_external_link`       | `command_id`, `external_link_id`, `run_id`, `pass_id`, `turn_id`, `output_frontier_id`, `external_object`, `event_ordinal` | Atomically bind an external object and its exact publication result.     |
| `read_review_target`                | `target_id`                                                                                                                | Read one immutable target snapshot.                                      |
| `read_review_run`                   | `run_id`                                                                                                                   | Read one run and its optional admitted pass.                             |
| `read_review_finding`               | `finding_id`                                                                                                               | Read one complete finding aggregate.                                     |
| `list_review_findings`              | `run_id`                                                                                                                   | List one run's findings in finding-identity order.                       |

Target subjects, workflows, pass and finding state, finding content,
disposition, and external-link vocabularies are the distinct wire
representations of the [review-workflow domain](review-workflows.md). The daemon
constructs domain values before mutation and rejects an incompatible target
shape, workflow/pass pair, session/input/turn binding, terminal frontier,
finding inventory, event, or attachment without normalizing it.

A selection object is exactly one of:

- `{"kind":"direct","selection_id":"<canonical UUID>"}`;
- `{"kind":"alias","alias_id":"<canonical UUID>"}`.

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
command retransmission therefore reaches the existing durable replay boundary; a
new request identity does not change command meaning (INV-012). The expected
defaults version is part of the canonical submit payload. A caller retries an
ambiguous submission with the same command identity, session, content, expected
version, and treatment; changing any of them is a conflicting reuse, not
recovery. The expected defaults version is likewise part of a replacement's
canonical payload; exact recovery preserves it together with the complete model
selection and dangerous-tool auto-approval posture.

Review mutations use the same owner-global command namespace. Equality is the
closed operation kind plus SHA-256 of the validated semantic request object;
frame version and request identity are excluded. Before hashing, the daemon
canonicalizes a complete `record_review_findings` request into finding-identity
order, so array order does not distinguish the same semantic inventory. The
typed append-only receipt stores the complete stable success response. A
recorded receipt is inspected before mutable aggregate-state preconditions, so
an equal retry returns that response even after the operation changed the
aggregate state. Each aggregate effect uses its owning store transaction. Fresh
run admission creates its run and sole pass in one transaction; recovery also
recognizes and completes a matching run-only intermediate committed by the
earlier multi-transaction implementation. The
[atomic run-admission decision](../decisions.md#2026-07-26--admit-review-run-and-pass-roots-atomically)
owns this refinement. If an effect commits before the receipt and the process
exits, an equal retry recognizes the exact complete effect, records the missing
receipt, and returns the stable result. Reusing the command identity for a
different digest, operation kind, aggregate payload, complete inventory, event,
or attachment fails closed. The
[durable review-command decision](../decisions.md#2026-07-26--recover-review-commands-from-their-exact-aggregate-effects)
owns this representation choice.

The daemon admits one review mutation at a time and retains that admission
through claim inspection, aggregate effect recovery, and receipt recording. A
decoded review mutation retains its inbound-frame budget slot while it waits for
that admission, so queued maximum-size requests remain inside the same 64 MiB
aggregate frame budget; the frame slot is released after the review permit is
acquired and before application handling. Review reads remain concurrent. This
bound composes with the snapshot-reader reservation so an open claim cannot form
a circular pool wait with its nested aggregate transaction; the
[review-command admission decision](../decisions.md#2026-07-26--serialize-durable-review-command-claims)
owns the capacity choice.

Before application construction, `replace_session_defaults` validates the
requested direct selection or alias against the process's immutable model
catalog. An unknown catalog identity is `invalid_request` and claims no command
identity. This check is read-only: the protocol does not register models or
change the catalog.

The `system_prompt` member exists at version nine and above, where it is
required on `create_session` and `replace_session_defaults`: JSON null states
explicitly that the complete defaults carry no prompt, and a string carries the
exact prompt. A present member under any version below nine, or an absent member
at version nine and above, is a `malformed_frame`. A present prompt is nonempty
exact Unicode text that rejects U+0000 and carries at most 1,048,576 UTF-8 bytes
— the accepted-input content bound, restated by the wire constant
`MAX_SYSTEM_PROMPT_UTF8_BYTES` — leaving response-envelope and worst-case
JSON-escaping headroom below the 8 MiB frame limit when the same prompt is
echoed by a receipt or defaults read. Bound, placement, and capacity reasoning
are recorded in the
[bound-and-placement decision](../decisions.md#2026-07-26--bound-the-session-system-prompt-as-a-defaults-epoch-value).

A `replace_session_defaults` below version nine cannot represent a prompt, so on
a session whose current defaults epoch carries a present one it would install a
complete successor that silently cleared a fact its version cannot state. The
atomic replacement transaction therefore refuses an unstated-member replacement
whose expected current epoch carries a prompt: the check runs after the
expected-version compare-and-set, under that row lock and against the immutable
expected epoch, so no concurrent replacement can interleave, and the refusal
rolls the whole transaction back — nothing, not even the command identity, is
recorded — and returns `unsupported_version` naming version nine. A command
identity that already names durable intent replays its recorded result
unconditionally (INV-012), and an absent session is the transaction's recorded
`session_not_found`. A below-nine replacement on a promptless current epoch
remains admitted and installs a promptless successor.

A metadata object has exactly `title` (string or null), `tags` (string array),
`attributes` (an object whose values are strings), and `archived` (boolean).
Present titles, tags, and attribute keys are nonempty; every metadata string
rejects U+0000. Attribute values may be empty. Duplicate tags produce
`malformed_frame`. Repeating a decoded attribute member name also produces
`malformed_frame` under the frame-wide duplicate-object-member rule above. Tag
order and attribute member order do not affect durable command equality. Wire
validation enforces the domain capacity contract: at most 262,144 total UTF-8
bytes across the object, at most 256 tags, at most 256 attributes, and at most
1,024 UTF-8 bytes in each tag or attribute key. Those bounds leave
response-envelope and worst-case JSON-escaping headroom below the 8 MiB frame
limit while bounding normalized satellite work when a complete accepted object
is echoed by a read or replacement receipt. The exact capacity choice is
recorded in the
[metadata-bound decision](../decisions.md#2026-07-25--bound-session-metadata-for-storage-and-process-frames).

`list_session_metadata` admits one through 100 results. `required_tags` is an
exact AND-filter, a present `title_contains` is nonempty and applies an exact
case-sensitive substring filter, `include_archived = false` selects the default
all-non-archived view, and `after_session_id` is an exclusive keyset cursor. An
empty tag array, null title query, false archive switch, page size 50, and null
cursor form the ordinary default request; the wire carries every field
explicitly. At most 256 required tags are admitted. They are nonempty, reject
U+0000, and carry at most 1,024 UTF-8 bytes each; a title query rejects U+0000;
and all required tags plus the title query carry at most 262,144 UTF-8 bytes.
Every metadata-object and metadata-filter string, shape, cardinality, and byte
rule in these two paragraphs is client-frame field or size validation. A
violation returns `malformed_frame` before application construction.
`invalid_request` is reserved for the fail-closed case where an admitted wire
value cannot construct the corresponding application input; no currently valid
metadata frame is intended to reach that mapping error.

`list_conversations` is the unified read surface over both conversation record
classes and mirrors the metadata list's pagination discipline exactly: one
bounded page per request, admitting one through 100 results, with no silent
truncation. It is a plain keyset read over the authoritative session,
current-defaults, metadata, and imported-conversation tables in one
repeatable-read, read-only transaction — no materialized view, cache, or
analytical artifact stands between the caller and committed state, so every
listed row is transactionally fresh; the
[unified-listing decision](../decisions.md#2026-07-27--serve-the-unified-conversation-listing-from-authoritative-tables)
records this stance and its scaling ladder. The unified order is by conversation
identity UUID value, with a native session ordered before an imported
conversation carrying a theoretical equal identity value. A cursor object has
exactly `origin` (`native_session` or `imported_conversation`) and
`conversation_id` (canonical UUID string); `after` is the exclusive keyset
cursor at that total position, so no row can be skipped at a page boundary. A
present `title_contains` is nonempty, rejects U+0000, carries at most 262,144
UTF-8 bytes, and applies the same exact case-sensitive substring filter to a
present native metadata title or imported display title; an absent title matches
no title query, and a transitional pending imported title survives every title
filter so the read fails closed on it
([conversation-import](conversation-import.md#derived-display-titles)) rather
than silently omitting an unresolved row. `origin` selects native rows, imported
rows, or both; `include_archived = false` selects the default view excluding
archived native sessions, and imported conversations carry no archive state, so
the switch never affects them. Every bound in this paragraph is client-frame
field or size validation returning `malformed_frame` before application
construction, exactly as for the metadata list.

`submit_input` deliberately exposes only the daily sequential-conversation
treatment in every admitted version. If a turn is already active, the normal
typed application result is returned as a rejection; the protocol does not guess
an interrupt, steering, or after-current treatment.

`reconcile_turn` is the one request that names a treatment explicitly, and it is
narrow by construction. The daemon reads whether the named turn is the session's
active turn parked in the `awaiting_model_call_recovery` phase and refuses
anything else with `rejected` and a `turn_not_awaiting_reconciliation` detail,
before any durable command is recorded. That precondition is skipped in exactly
the two cases the durable boundary owns the answer to: a command identity that
already names durable intent replays its recorded result unconditionally
(INV-012), because the first handling already released the wait it would now be
refused for; and an absent session is left to the transaction's recorded
`session_not_found`. Every other request reaches the authoritative transaction,
which applies the accepted `Interrupt` delivery in
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
invoking the existing application service. For a claimed command, it first
compares the recorded canonical frontier, relationship, and defaults and returns
equal replay or conflicting reuse without resolving the wire address again. An
absent conversation or position returns `not_found` without claiming the
command. Resume and fork are explicit and have the semantics owned by
[sessions-and-transcript](sessions-and-transcript.md#create-from-an-imported-frontier).

`stop_turn` is the explicit stop verb, and it is the accepted `Interrupt`
delivery in
[turn-lifecycle-and-scheduling](turn-lifecycle-and-scheduling.md#occupied-slot-input-handling)
on the wire — no standalone active-turn cancellation command exists (INV-029).
The request names the exact turn the caller observed active and carries the
successor content the interrupt algebra requires: an applied interrupt is the
only cancellation authority, and it binds an immediate-successor origin in the
same transaction. Terminalization flows through the existing lifecycle — a
running turn with no prepared call, or a prepared call, cancels directly, while
an issued call first enters its durable `cancellation_requested` state and the
turn terminalizes when physical cancellation resolves. The authoritative
transaction validates the expected active turn under the session lock and
records every refusal as a typed rejection: `no_active_turn` when no turn holds
the slot, `active_turn_mismatch` for a stale expected turn,
`interrupt_already_applied` when a distinct earlier stop already carries the
proof, and `interrupt_unavailable_while_awaiting_approval` when the active turn
is parked on a tool-approval wait, which a stop can neither decide nor bypass —
the caller denies the pending request through `decide_tool_request` first, then
stops ([tool-loop](tool-loop.md#approval-policy-and-decision-sources) owns the
deny-first caller protocol).

`decide_tool_request` carries the canonical owner decision command for one
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

Versions two and three admit the same request vocabulary as version one and add
no new mutation authority. Version four retains that vocabulary and adds only
the three metadata requests. Version five retains all earlier requests and adds
only `import_conversation`. Version six retains all earlier requests and adds
only `replace_session_defaults`. Version seven retains all earlier requests and
adds only `reconcile_turn`. Version eight retains all earlier requests and adds
only `stop_turn` and `decide_tool_request`. Version nine retains every earlier
request, requires the system-prompt member on the two defaults-bearing
mutations, and adds only the read-only `read_session_defaults`. Version ten
retains every earlier request and adds only
`create_session_from_imported_frontier`. Version eleven retains every earlier
admitted request and adds only the review-workflow requests above. Version
twelve retains every earlier admitted request and adds no request variant.
Version sixteen retains every earlier admitted request and adds only the
read-only `list_conversations`; versions thirteen, fourteen, and fifteen remain
reserved by concurrent stacks and unsupported here. A metadata request carried
under version one, two, or three, an import request carried under version one
through four, a defaults-replacement request carried under version one through
five, a reconciliation request carried under version one through six, a
turn-control request carried under version one through seven, a defaults read
carried under version one through eight, an imported-frontier creation request
carried under any version one through nine, a review request carried under any
version one through ten, or a unified-listing request carried under any version
one through twelve, is classified as `malformed_frame` because its supported
version does not admit that request variant; it never reaches application
construction. A version-one `submit_input`, `read_transcript`, or
`follow_session` request that selects imported ancestry returns a version-one
`unsupported_version` error naming version two before mutation or snapshot
construction.

Versions four and above also inherit every transcript, turn-state, entry, and
event shape admitted by version three, including the imported representations
introduced by version two and the tool-bearing representations introduced by
version three. A `read_transcript`, `follow_session`, or `submit_input` under
version four and above therefore never requires a downgrade or a newer version
for a representation already admitted by version three.

Tool-free native sessions remain readable and mutable through every admitted
version. A version-one or version-two `read_transcript` or `follow_session`
request whose snapshot requires a tool-only state or entry returns an error in
its admitted version with `code = "unsupported_version"` naming version three
before any snapshot frame. If an already-followed tool-free session first
commits a tool-only event, a version-one or version-two follower receives that
same error and the connection closes before the event is emitted. A version-one
or version-two `submit_input` request targeting a session whose existing history
contains a tool-only state or entry returns that same error naming version three
before mutation. This gate lets an upgraded daemon continue serving old clients
without sending a tagged variant their accepted version requires them to reject.
Version three adds tool observation but no approval, cancellation, or other
mutation request; version eight adds exactly the two turn-control mutations
above.

A version-one through version-five `read_transcript`, `follow_session`, or
`submit_input` request targeting a session whose history contains a
model-identity boundary returns `unsupported_version` naming version six before
snapshot construction or mutation. A follower admitted before that boundary may
receive only older-version-compatible transition events; its next authoritative
snapshot request encounters this same gate. Versions six and above preserve
every earlier shape and admit the new entry. A session system prompt adds no
transcript entry and therefore raises no read or follow gate: transcripts of
prompted sessions remain representable in every admitted version.

Submitted `content` is limited to 1 MiB of UTF-8. The daemon applies that
boundary before application construction or mutation and returns
`invalid_request` when it is exceeded. This leaves enough space for worst-case
JSON escaping when the same accepted content is projected in a queued turn or
durable update event. The exact capacity choice is recorded in the
[input-bound decision](../decisions.md#2026-07-23--bound-process-protocol-input-at-1-mib).

An import `source` is the complete exact byte sequence encoded with RFC 4648
standard-alphabet padded base64. A noncanonical spelling is a malformed frame.
The server validates canonical padding and trailing bits in the same decode that
constructs the source bytes under the existing inbound-frame permit; validation
does not construct a second full-size canonical encoding. There is no
independent source-size admission rule in this slice: the existing 8 MiB
encoded-frame limit determines whether one complete request can cross the
boundary. Before socket I/O, the terminal's bounded reader takes at most one
byte beyond three quarters of the frame cap, the greatest decoded byte count
that base64 could possibly fit. It rejects a source reaching that extra byte;
exact request encoding happens before socket I/O and remains authoritative for
smaller inputs. The source path is client-local and never appears in the
request.

## Server messages

Message objects carry a required string `type` and reject fields not admitted by
that variant. Every accepted non-review mutation request — `create_session`,
`create_session_from_imported_frontier`, `submit_input`, `reconcile_turn`,
`stop_turn`, `decide_tool_request`, `replace_session_metadata`,
`replace_session_defaults`, or `import_conversation` — produces exactly one of:

- `session_created` with `session_id`;
- `input_submitted` with `session_id`, `accepted_input_id`,
  `acceptance_position`, and `turn_id`; a `stop_turn` acceptance names the
  accepted immediate successor;
- `tool_request_decided` with `tool_request_id` and the exact recorded
  `decision` object; the receipt mirrors the recorded applied result and
  intentionally echoes no session, because the session is not part of the
  canonical decision payload;
- `session_metadata_replaced` with `session_id`, the complete `metadata`
  snapshot installed by that recorded handling, and its non-null `last_writer`;
- `session_defaults_replaced` with `session_id`, the newly installed
  `defaults_version`, complete `model_selection`, and
  `dangerous_tool_auto_approval`; at version nine and above it also requires the
  installed `system_prompt` (string or null), a member absent below nine;
- `conversation_import_inserted` with `imported_conversation_id`;
- `conversation_import_already_imported` with `imported_conversation_id`; or
- `error` with a stable `code` and a non-sensitive `message`.

Version-eleven review mutations return exactly one stable acknowledgement:

- `review_target_created { target_id }`;
- `review_run_started { run_id, pass_id }`;
- `review_pass_activated { run_id, pass_id }`;
- `review_findings_recorded { run_id, pass_id, finding_count }`;
- `review_finding_disposition_recorded { finding_id, status }`;
- `review_external_link_reserved { external_link_id }`; or
- `review_external_link_attached { external_link_id, external_object }`.

Single-aggregate reads return `review_target { target }`,
`review_run { run, pass }`, or `review_finding { finding }`; an absent identity
returns `not_found`. Target snapshots carry the immutable subject and revisions.
Run snapshots carry frozen workflow, policy values, lifecycle, and optional pass
identity; the nullable `pass` carries its exact session/input/origin-turn,
lifecycle, optional turn, and optional successful frontier. Finding snapshots
carry immutable content, derived status, and event count.

A successful `list_review_findings` response is
`review_findings_start { run_id }`, zero or more
`review_finding_item { finding }` messages in strictly increasing
finding-identity order, then `review_findings_end { finding_count }`. The client
validates the selected run, ordering, and terminal count before presenting the
list.

Versions twelve and above additionally admit
`provider_text_delta { session_id, turn_id, model_call_id, part_index, content }`
only on a `follow_session` response. The three identities correlate the provider
observation to its active session, turn, and model call; `part_index` is the
provider part position as a canonical decimal string; and `content` is one
bounded text fragment. When one adapter delta would exceed the protocol's
fragment bound, signalboxd emits consecutive messages with the same identities
and part index whose contents concatenate to the exact already-redacted delta.
The message has no outbox `cursor`: it is a process-local presentation event,
not a `session_event`, transcript entry, or terminal-evidence fact. Versions one
through eleven never receive this message.

A replayed metadata receipt remains the exact snapshot installed by its original
handling even if a later command has replaced the current metadata. A caller
that needs current state issues `read_session_metadata`.

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

Version four's metadata list is a bounded sequence:

1. `session_metadata_page_start`;
2. zero through 100 `session_metadata_summary` messages in strictly increasing
   session-identity order; and
3. `session_metadata_page_end { session_count, next_after_session_id }`.

Each summary carries `session_id`, current `defaults_version`,
`model_selection`, `dangerous_tool_auto_approval`, `title`, sorted `tags`,
`archived`, and `last_writer`. `dangerous_tool_auto_approval` is a JSON boolean:
`false` encodes domain `Disabled` and `true` encodes domain `ApproveAll`. Tags
are strictly increasing by lexicographic UTF-8 byte sequence. Each summary
admits at most 256 tags and applies the metadata object's 262,144-byte aggregate
UTF-8 bound across its title and tags, not merely to each member independently.
Attributes are intentionally absent from the list projection. The end cursor is
null when no later match existed in the page snapshot; otherwise it equals the
last emitted session identity. The page sequence is spooled before output and
becomes authoritative only after its count, ordering, and cursor validate.

Version sixteen's unified conversation list is the same bounded sequence shape:

1. `conversation_page_start`;
2. zero through 100 `conversation_summary` messages in strictly increasing
   unified cursor order; and
3. `conversation_page_end { conversation_count, next_after }`.

Each summary carries one closed `conversation` object tagged by `origin`. A
`native_session` summary carries `session_id`, the optional exact metadata
`title`, `archived`, and the current `defaults_version`. An
`imported_conversation` summary carries `imported_conversation_id`, the optional
exact source-derived display `title`
([conversation-import](conversation-import.md#derived-display-titles) owns the
derivation), the total normalized `entry_count` — the greatest
`through_position` an imported continuation may select — and the exact stored
`source_format` (`claude_code_session_jsonl_v1`, `claude_code_session_jsonl_v2`,
or `codex_rollout_jsonl_v1`). Neither summary materializes transcript, entry, or
raw-record content; the per-entry read surfaces retain that authority. The end
cursor is null when no later match existed in the page snapshot; otherwise it
names the last emitted summary's origin and identity. The page sequence is
spooled before output and becomes authoritative only after its count, ordering,
and cursor validate.

`session_metadata` is the successful single-session read and
`session_metadata_replaced` is the successful write receipt. Both carry
`session_id`, the complete metadata object, and `last_writer`. The initial
unwritten snapshot has the empty non-archived metadata object and a null
`last_writer`; an applied replacement always has a non-null last writer. A
last-writer object has `updated_at_unix_micros` (canonical nonnegative decimal
microseconds since the Unix epoch) and the closed actor object `owner`. No
non-owner metadata writer is constructible through this boundary; additional
actor variants require the later slice that introduces their constructing
authority. Actor is provenance, not wire authentication or authorization.

`session_defaults_replaced` is the successful defaults write receipt. It echoes
the complete installed defaults and names the exact successor epoch. An equal
command replay returns that original receipt even after later epochs exist;
current state is observed through metadata reads or the defaults read version
nine introduced.

`session_defaults` is the successful `read_session_defaults` response, admitted
from version nine, with `session_id`, the read `defaults_version`, complete
`model_selection`, `dangerous_tool_auto_approval`, and the exact `system_prompt`
(string or null). A null request version reads the epoch named by the session's
current pointer; a named version reads exactly that immutable epoch, so the
response is stable under later replacements. The `not_found` error covers both
an absent session and a named epoch that was never installed.

An application rejection is an `error` with `code = "rejected"` and a required
`detail` object whose variants are closed. The version-one input treatment
admits `session_not_found { session_id }`,
`active_turn_present { session_id, active_turn_id }`,
`defaults_version_mismatch { session_id, expected, current }`,
`unknown_model_alias { session_id, alias_id }`, and
`acceptance_position_exhausted { session_id, last }`. A version-four
`replace_session_metadata` rejection admits exactly
`session_not_found { session_id }`. A version-six `replace_session_defaults`
rejection admits `session_not_found { session_id }`,
`defaults_version_mismatch { session_id, expected, current }`, and
`defaults_version_exhausted { session_id, current }`. A version-seven
`reconcile_turn` rejection admits `session_not_found`,
`defaults_version_mismatch`, `unknown_model_alias`, and
`acceptance_position_exhausted` as above, plus
`active_turn_mismatch { session_id, expected_active_turn_id, active_turn_id }`
and `no_active_turn { session_id, expected_active_turn_id }` for a decision that
lost its race, and `turn_not_awaiting_reconciliation { session_id, turn_id }`
for the refused precondition. A version-eight `stop_turn` rejection admits
`session_not_found`, `defaults_version_mismatch`, `unknown_model_alias`, and
`acceptance_position_exhausted` as above, plus `no_active_turn`,
`active_turn_mismatch`,
`interrupt_already_applied { session_id, active_turn_id, existing_command_id }`,
and
`interrupt_unavailable_while_awaiting_approval { session_id, active_turn_id }`.
A version-eight `decide_tool_request` rejection admits
`tool_request_not_found { tool_request_id }`,
`tool_request_already_resolved { tool_request_id }`,
`tool_request_not_earliest_undecided { tool_request_id, earliest_tool_request_id }`,
and `tool_request_not_in_session { session_id, tool_request_id }`. The
`turn_not_awaiting_reconciliation` and `tool_request_not_in_session` details
report refusals made before command recording, so unlike every other `rejected`
detail they name no durable command result and have no replay projection; a
caller that repeats the request observes the current state, not a recorded
outcome. Other error codes have no `detail`. An equal replay returns the same
success or rejection projection as the first handling.

The error-code set in all admitted versions is:

| Code                  | Meaning                                                                                                                                |
| --------------------- | -------------------------------------------------------------------------------------------------------------------------------------- |
| `malformed_frame`     | JSON, UTF-8, framing, field, or size validation failed.                                                                                |
| `unsupported_version` | The frame version is unsupported, or the selected representation requires a newer supported version.                                   |
| `invalid_request`     | A boundary value cannot construct the requested application input.                                                                     |
| `not_found`           | The selected session, named defaults epoch, imported conversation, imported frontier, or review aggregate does not exist.              |
| `conflicting_reuse`   | A durable command identity already names different intent.                                                                             |
| `rejected`            | The canonical command was durably rejected by current typed state, or a request-specific precondition refused it before recording one. |
| `resync_required`     | A follower fell behind the bounded process-local event fan-out.                                                                        |
| `unavailable`         | Infrastructure failed; no requested mutation may have committed.                                                                       |
| `commit_ambiguous`    | Infrastructure obscured whether the requested mutation committed.                                                                      |
| `internal`            | Fail-closed corruption or a daemon defect stopped the request.                                                                         |

For `create_session`, `create_session_from_imported_frontier`, `submit_input`,
`reconcile_turn`, `stop_turn`, `decide_tool_request`,
`replace_session_metadata`, `replace_session_defaults`, and every review
mutation, a lost commit response maps to `commit_ambiguous`; the client retries
the exact command identity and payload to discover the recorded outcome. A
`reconcile_turn` or `decide_tool_request` retry reaches that recorded outcome
unconditionally, because a claimed command identity bypasses the precondition
the first handling already satisfied. Once a review aggregate effect has been
applied or recovered, any database failure during post-effect verification,
typed-receipt insertion, or claim commit is likewise `commit_ambiguous`. A
definitely pre-commit infrastructure failure maps to `unavailable`.

Conversation import carries no durable command identity because exact
format-and-source replay already resolves through the import digest. A selected
converter's content-silent rejection maps to `invalid_request`. The current
repository error does not retain the failing database phase, so every import
database error maps conservatively to `commit_ambiguous`; retrying the exact
format and bytes returns either the first inserted identity or the existing
identity. Import integrity failures map to `internal`.

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

1. `transcript_snapshot_start { session_id, cursor }`;
2. one `transcript_turn` per turn, with canonical decimal `acceptance_position`;
3. the entry messages below in frontier-member order; and
4. `transcript_snapshot_end { session_id, cursor, turn_count, entry_count }`.

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

Session-list, transcript-read, and follow-snapshot construction share bounded
admission that reserves application-pool capacity for non-snapshot work. The
exact reservation is owned by the
[snapshot-resource decision](../decisions.md#2026-07-23--bound-process-snapshot-construction-resources).

In versions one and two, each `transcript_turn` has `turn_id` and one of these
closed `state` objects:

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

Version three preserves all of those model-call shapes unchanged and adds
`active_awaiting_tool_approval { tool_request_id }`,
`active_awaiting_tool_recovery { ended_attempt_id, recovery_tool_attempt_id }`,
and
`tool_reconciliation_required { terminal_frontier_id, terminal_attempt_id, terminal_tool_attempt_id }`.
The distinct tool variant avoids changing the older `reconciliation_required`
object.

Across their admitted representations, each non-text native frontier member is
one `transcript_entry` with `entry_index`, `source_session_id`, `entry_id`, and
one closed `entry` object: `turn_completed { turn_id }`,
`turn_failed { turn_id }`, or `turn_cancelled { turn_id }`. Version three also
admits
`assistant_tool_use { turn_id, model_call_id, tool_request_id, tool_name, arguments }`,
`tool_execution_result { tool_request_id, tool_attempt_id, content }`,
`tool_denied { tool_request_id, content }`, and
`tool_closed { tool_request_id, content }`. Versions six and above additionally
admit `model_identity_changed { turn_id, defaults_version, selected_model_id }`,
naming the first started turn bound to a changed frozen direct selection. A
native text member begins with
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

In version three, the tool-entry `arguments` and `content` members are JSON
strings, never nested untyped JSON values. `arguments` contains the exact
normalized JSON text or credential-scrubbed undecodable text stored on the
request. `content` contains the exact provider-visible result string: admitted
success text, or the compact closed error object serialized as text by the
provider bridge. Tool entry discriminators and identifiers determine the
semantic arm; clients never infer it by reparsing either string.

The version-three process projection resolves the domain's reference-only tool
entries before crossing the wire. Tool use carries the exact checked name and
exact normalized-or-scrubbed-undecodable arguments. Execution, denial, and
closure carry the same provider-neutral success text or compact typed failure
JSON defined by [tool-loop](tool-loop.md#provider-bridge-and-daemon-catalog). A
client therefore never needs private storage access to reconstruct tool-bearing
conversation history.

The following imported-entry variants exist in protocol versions two and three.
An imported semantic entry always identifies its source with
`imported_conversation_id` and `imported_entry_id` and carries the exact
`source_speaker` attestation. That attestation is one closed object:
`not_attested`, `attested_absent`, or `attested { speaker }`, where `speaker` is
exactly `user` or `assistant`.

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
migration. This fence migration belongs to Signalbox's initial deployment: the
owner confirms that no deployed database or daemon predates it, so there is no
legacy unfenced writer to drain during the first installation. Importing or
upgrading a pre-fence database is unsupported. Exhaustion or corruption fails
startup rather than wrapping.

Together these guards enforce one active daemon process—and therefore one
dispatcher and its process-local fan-outs—for a database, while preventing a
successor's migration or recovery from overlapping an old daemon's authoritative
work. Guard-session monitoring and fatal-loss behavior are owned by
[Daemon runtime: startup order and shutdown](turn-lifecycle-and-scheduling.md#daemon-runtime-startup-order-and-shutdown).
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

The process-local durable-only fan-out and delta-admitting composite fan-out
each retain 64 update events. The dispatcher offers every durable update to
both; the provider bridge offers deltas only to the composite fan-out. Versions
one through eleven therefore cannot lag because of delta volume, while a
follower whose version admits deltas — twelve and above — preserves one send
order across deltas and durable updates. One immutable text allocation backs
every clone of a delta delivered to concurrent followers, so fan-out count does
not multiply provider-sized text allocations. Having no connected followers does
not block durable cursor advancement: reconnecting clients use a fresh
authoritative snapshot. A follower that overruns its selected bounded fan-out
receives `resync_required` and reconnects for another snapshot.

Each `session_event` message carries `cursor`, `session_id`, and exactly one
closed `event` object. Every version admits these unchanged event shapes:

| Event                          | Additional members                                                            |
| ------------------------------ | ----------------------------------------------------------------------------- |
| `session_created`              | none                                                                          |
| `input_accepted`               | `accepted_input_id`, `turn_id`, `acceptance_position`, and `content`          |
| `turn_activated`               | `turn_id` and `current_attempt_id`                                            |
| `model_call_transition`        | `turn_id`, `model_call_id`, and `state`                                       |
| `turn_completed`               | `turn_id`, `model_call_id`, `completion_entry_id`, and `terminal_frontier_id` |
| `turn_failed`                  | `turn_id`, `failure_entry_id`, and `terminal_frontier_id`                     |
| `turn_refused`                 | `turn_id`, `model_call_id`, and `terminal_frontier_id`                        |
| `turn_cancelled`               | `turn_id`, `cancellation_entry_id`, and `terminal_frontier_id`                |
| `turn_reconciliation_required` | `turn_id`, `model_call_id`, and `terminal_frontier_id`                        |

Version three additionally admits
`tool_batch_transition { turn_id, model_call_id, state }`, where `state` is
exactly `proposed { frontier_id }`, `results_projected { frontier_id }`, or
`recovery_required { tool_attempt_id }`, and
`turn_tool_reconciliation_required { turn_id, tool_attempt_id, terminal_frontier_id }`.

The model-call `state` object is exactly `prepared`, `in_flight`,
`cancellation_requested`, or `terminal { disposition }`; terminal disposition is
one of `completed`, `known_failed`, `refused`, `cancelled`, or `ambiguous`.
Storage-version columns are not exposed as wire-version fields.

## Follow synchronization

For `follow_session`, the server subscribes to process-local fan-out before
reading the repeatable-read transcript snapshot. It sends that snapshot first,
then discards subscribed events at or below its cursor and sends matching
session events above it in cursor order. Versions one through eleven subscribe
to the durable-only fan-out. Version twelve subscribes to the ordered composite
fan-out, which interleaves provider-text deltas with those same durable updates
in their process send order.

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
semantic transcript entry, while a transition committed after the snapshot has a
greater cursor and was observed by the preexisting subscription. A refused turn
is therefore terminal in the initial snapshot and cannot leave `send` waiting
for an event at or below the snapshot cursor. Previously seen transient display
state may always be replaced by the new snapshot (INV-032).

Versions one through eleven forward durable transition events only. Version
twelve additionally forwards a correctly correlated `TextDelta` emitted while
the selected session's turn is active. The HTTP adapter has already applied the
credential-redaction boundary before that fact leaves the runtime (INV-035); the
bridge and daemon copy its text unchanged and do not re-invent redaction. Deltas
remain ephemeral process-incarnation presentation events: they are not appended
to the transactional outbox, do not advance the follow cursor, do not enter the
transcript, and do not alter the observation or terminal-evidence paths. The
durable transcript remains the sole reply truth.

An overrun of either selected fan-out produces the existing `resync_required`. A
lagging or reconnecting delta-admitting follower loses any unreceived deltas; it
reads the new authoritative snapshot and continues from that snapshot's durable
cursor. Deltas are never replayed from storage. This is intentional:
resynchronization replaces transient presentation state with the complete
durable transcript rather than making token delivery another source of authority
(INV-032).

The terminal `send` command follows the submitted turn, accepts terminal state
from the initial snapshot or waits for its durable terminal event, rereads the
authoritative transcript, and prints the committed assistant text. Its internal
terminal waiter accepts and ignores ephemeral provider-text deltas for the
selected session and rejects a cross-wired delta. Every version exits with a
typed nonzero recovery-required diagnostic after observing
`active_awaiting_model_call_recovery` or a live terminal `ambiguous` model-call
transition followed by that authoritative state.

Version three applies the same behavior to `active_awaiting_tool_recovery` and
to `tool_batch_transition { recovery_required }` followed by that state. A
model-call recovery wait has one process-protocol writer that completes it —
version seven's `reconcile_turn`, which the diagnostic's operator runs next; the
tool recovery wait still has none. An `active_awaiting_tool_approval` turn
remains an ordinary nonterminal wait that `send` keeps waiting through; version
eight's `decide_tool_request` is its resolving writer, issued from a second
connection while the waiting client's transcript names the pending request and
its proposing tool. A client disconnect never cancels model or tool work.

Version three rereads after each `tool_batch_transition { proposed }` and
`tool_batch_transition { results_projected }`; every version rereads after a
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

## Terminal client

The `signalbox` binary in this stack uses version sixteen; version four's
single-session metadata read and metadata replacement remain core protocol and
daemon capabilities without terminal-client UX, while its paginated metadata
list is the `search` verb below. Older clients remain supported for
representations admitted by their declared version as described above. The
client accepts a global `--socket <path>` override or reads
`SIGNALBOX_SOCKET_PATH`, and provides:

- `create (--model <selection-uuid> | --alias <alias-uuid>) [--system-prompt-file <path>] [--command-id <uuid>]`;
- `continue <imported-conversation-uuid> --through-position <positive-decimal> --relationship <resume|fork> (--model <selection-uuid> | --alias <alias-uuid>) [--command-id <uuid>]`;
- `list`;
- `search [--title <substring>] [--tag <tag>]... [--include-archived] [--limit <decimal>] [--after <session-uuid>]`;
- `conversations [--title <substring>] [--origin <native|imported|all>] [--include-archived] [--limit <decimal>] [--after <native|imported>:<uuid>]`;
- `send <session-uuid> [--command-id <uuid> --defaults-version <decimal>]`;
- `model <session-uuid> (--model <selection-uuid> | --alias <alias-uuid>) [--system-prompt-file <path> | --clear-system-prompt] [--command-id <uuid> --defaults-version <decimal> --dangerous-tool-auto-approval <disabled|approve-all>]`;
- `transcript <session-uuid>`;
- `follow <session-uuid>`;
- conversation import operations described by the
  [conversation-import operational surface](conversation-import.md#operational-surface);
- `reconcile <session-uuid> <turn-uuid> [--command-id <uuid> --defaults-version <decimal>]`;
- `stop <session-uuid> [--command-id <uuid> --defaults-version <decimal> --turn <uuid>]`;
- `approve <session-uuid> <tool-request-uuid> [--command-id <uuid>]`;
- `deny <session-uuid> <tool-request-uuid> --reason <text> [--command-id <uuid>]`;
- `chat <session-uuid>`.

`chat` is the plain line-oriented interactive surface for one live session. It
opens one long-lived `follow_session` connection before accepting input and
keeps that connection dedicated to ordered snapshots, provider-text deltas, and
durable events. Submissions and in-loop control operations use a second
connection, opened through the existing one-request connection path; the client
does not multiplex requests onto the follow connection. The initial and every
resynchronized follow snapshot replace transient display state with the durable
transcript, and later provider-text deltas remain ephemeral presentation exactly
as they do for `follow`.

A line without the `:` prefix submits exact nonempty line content only while no
turn is active. The closed in-loop command set is `:stop TEXT`, `:approve ID`,
`:deny ID REASON`, `:transcript`, `:model ALIAS-UUID`, and `:quit`. These map,
respectively, to `stop_turn`, `decide_tool_request`, `read_transcript`,
`replace_session_defaults`, or local exit; ordinary input maps to
`submit_input`. `:stop` requires successor text because the interrupt request
cannot represent a standalone cancellation. `:model` changes only the alias
selection and copies the observed dangerous-tool posture and system prompt into
the forward-only successor defaults epoch. Tool proposals and projected results
are reread and presented at their durable transition, and an approval wait
prints its exact request identity. All process-derived text, including live
deltas and tool content, uses the same terminal-safe escaping as the other
client verbs unless the invocation selected `--raw-output`.

While a turn is active, the first Ctrl-C leaves the daemon turn running and
prints the `:stop TEXT` choice. A second Ctrl-C exits the client and explicitly
reports that the turn remains running. `:quit` and standard-input EOF use the
same honest exit report. Once the followed turn terminalizes, the client
presents its exact durable terminal material and accepts another ordinary input
line.

`list` remains the complete unfiltered version-one summary sequence. `search` is
the separate verb for version four's `list_session_metadata`, whose filters,
bounded page, and keyset cursor have no version-one counterpart: each invocation
sends exactly one request and prints exactly one page. `--title` is the exact
case-sensitive substring query, each `--tag` adds one required tag to the exact
AND-filter, `--include-archived` selects the archived-inclusive view, `--limit`
is the page size and defaults to 50, and `--after` is the exclusive
session-identity cursor. Empty filter text, filter text carrying U+0000, a
repeated `--tag` value, a limit outside one through 100, more than 256 required
tags, a tag beyond 1,024 UTF-8 bytes, and a title query plus tags beyond 262,144
aggregate UTF-8 bytes are all rejected as usage errors before socket I/O, so
every metadata-filter bound this page states reaches the user as a named
diagnostic rather than a generic local encode failure. Each result is one line
carrying the summary's session identity, archive state, defaults version, model
selection, dangerous-tool posture, last-writer actor and timestamp, sorted
comma-joined tags, and title. An unwritten metadata snapshot prints
`last_writer=none`, `updated_at_unix_micros=none`, and empty tag and title
values, which a present tag or title never is. A tag may itself contain the
space that ends its field, the comma that separates it from a sibling, or the
backslash that introduces an escape, so all three are escaped inside a tag
exactly as a control code point is; every backslash in the tag field therefore
opens an escape the client wrote, and the field decodes back to the exact tag
set. The title is the line's last field, keeps its spaces, and is rendered to be
read rather than decoded. When the page end names a continuation cursor, the
client prints `next_after_session_id=<uuid>` to standard error after the
results; a page is therefore never silently truncated, and that value is the
next invocation's `--after`. The client also validates that a page never exceeds
its requested limit.

`conversations` is the separate verb for version sixteen's `list_conversations`
and follows the same one-request, one-page discipline as `search`. `--title` is
the exact case-sensitive substring query, `--origin` selects native sessions,
imported conversations, or both and defaults to `all`, `--include-archived`
selects the archived-inclusive native view, `--limit` is the page size and
defaults to 50, and `--after` is the exclusive origin-qualified cursor spelled
exactly as a prior page printed it. Empty filter text, filter text carrying
U+0000, a title query beyond 262,144 UTF-8 bytes, a limit outside one through
100, and a cursor that is not `native:<uuid>` or `imported:<uuid>` are rejected
as usage errors before socket I/O. Each result is one origin-tagged line whose
title is the line's last, terminal-safely escaped field: a native line carries
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
- `record-finding <run> <pass> --turn-id <turn> --output-frontier-id <frontier> --finding-id <finding> --file-path <path> --title <text> --body <text> --severity <severity> --confidence <basis-points> --category <key>`;
- `list-findings <run>`;
- `read-target <target>`;
- `read-run <run>`; and
- `read-finding <finding>`.

Each review mutation also accepts `--command-id <uuid>`. Target creation accepts
an optional `--stack-parent-target-id`; finding recording accepts an optional
paired line range, diff side, and recommended fix.

`send` reads the exact input text from standard input through EOF and never
accepts conversation content in process arguments. Empty or oversized input
fails before socket I/O. `--system-prompt-file` likewise carries a path, never
prompt content in a process argument: the client reads one bounded file snapshot
before socket I/O and rejects an empty, oversized, non-UTF-8, or U+0000-bearing
prompt locally, then sends the exact text.

`reconcile` reads its successor content the same way and names the parked turn
the operator observed in the session transcript. It prints the same recovery
values as `send`, then follows the accepted successor turn to its own terminal,
so one invocation both records the reconciliation decision and continues the
conversation.

`continue` requires the imported position and relationship explicitly; it never
selects the last frontier or treats resume as an implicit default. Success
prints the created session identity, which is immediately usable by `send`,
`transcript`, and `follow`. The command identity, imported conversation,
position, relationship, and model selection are the complete replay inputs.

`stop` reads its successor content the same way. When `--turn` is absent it
reads the authoritative transcript, selects the single turn holding the active
slot, and fails with a typed local error when no turn is active; the selected
turn is printed as a recovery value before the mutation. It then prints the same
recovery values as `send` and follows the accepted successor turn to its own
terminal, so one invocation both records the stop and continues the
conversation.

`approve` and `deny` name the pending request printed by `transcript` or
`follow` — the awaiting turn line names the request identity and the
`assistant_tool_use` entry names its tool and arguments. Each verb validates
that the receipt echoes the exact request and decision it sent and prints one
`tool_request=<uuid> decision=<approve|deny>` line.

When `--command-id` is absent, the client generates a fresh UUIDv7 identity and
prints it to standard error before any socket I/O. `send` and `stop` first read
the session summary and use its defaults version, then print that expected
version to standard error before sending the mutation. `model` issues one
`read_session_defaults` for the current epoch; it copies the defaults version,
dangerous-tool posture, and — when no prompt option was given — the exact
current system prompt, prints the version and posture, and changes only the
requested fields. Thus every client-generated or server-discovered scalar
recovery value is visible before its commit can become ambiguous; the
content-sized prompt is deliberately never echoed. Exact replay also requires
the original selection, imported-conversation, or session arguments and, for
`send`, `reconcile`, and `stop`, the exact standard-input content; the client
does not echo that potentially sensitive input or synthesize a shell command.
Its ambiguity diagnostic directs the user to retry the original command with
those arguments and input plus any printed recovery values. For recovery, the
user supplies the printed command identity; `send` and `reconcile` then also
require the exact `--defaults-version`, and `stop` requires command identity,
defaults version, and the exact expected `--turn`, because a stopped turn cannot
be rediscovered once the first handling terminalizes it. `model` instead
requires all three printed facts — command identity, defaults version, and
dangerous-tool posture — plus the original prompt option: a re-supplied
`--system-prompt-file` or `--clear-system-prompt` is re-read or re-applied
exactly, while a copied-forward prompt is re-read from the immutable epoch the
printed defaults version names, so the retried payload is byte-exact under
concurrent replacements without printing megabyte content. Each recovery set is
all-or-none. The client never silently substitutes a new command identity for an
ambiguous attempt. It uses a fresh nonzero request identity per connection,
validates that a defaults receipt is the exact successor carrying the requested
selection, copied posture, and exact replacement prompt, validates that a
decision receipt echoes the exact request and decision it sent, renders only
known version-sixteen messages, and exits nonzero on protocol or application
errors other than the follow-specific `resync_required` control case, which
reconnects for a fresh snapshot.

Review mutations print a generated command identity before socket I/O and an
ambiguous diagnostic directs the operator to repeat the same verb, identifiers,
and content with that identity. Review reads validate selected identities and a
run response's pass presence, pass identity, run ancestry, and target ancestry
before writing output; finding lists additionally validate their start marker,
strict identity order, maximum 32-item inventory, terminal count, and end marker
before success. `record-finding` rejects a zero or greater-than-32-bit line
number, a line end before its start, and confidence above 10,000 basis points as
a usage error before socket I/O. Every process-derived review text field follows
the same terminal-safe escaping and `--raw-output` opt-in below. Target output
distinguishes an absent base revision from every present value, run output
carries its complete frozen policy, and finding output carries every immutable
content field, location, severity, confidence, category, optional-repair
presence, ancestry identity, status, and event count.

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
only the selected session's defaults version. `search` and `conversations` each
spool one bounded page the same way and present it only after that page's
ordering, count, and cursor validate; `model` reads the same validated metadata
pages while retaining only the selected session's defaults facts.

After completion, `send` rereads and prints only authoritative committed
assistant text produced for its exact turn. A failed or refused turn produces a
typed diagnostic and a nonzero exit without reply text; cancelled and
reconciliation-required turns do the same with their distinct typed diagnostics.
`follow` prints the initial transcript, ephemeral provider-text deltas, and
subsequent typed durable updates until interrupted. Each delta is flushed as one
line:
`provider_text_delta session=<session> turn=<turn> call=<call> part=<index> content=<text>`.
By default its trailing text field escapes line feed and every other C0 code
point, DEL, and C1 code point, so provider output cannot forge another event
line or execute terminal controls; `--raw-output` remains the explicit opt-in to
unchanged text. Version six and later snapshots render a model boundary as
`model_identity_changed` with its turn, defaults version, selected model, source
session, and entry identity. By default every process-derived text field written
to a terminal preserves line feed but renders every other C0 code point, DEL,
and C1 code points as visible `\u{...}` escapes, preventing ESC/OSC execution. A
metadata title or tag shares its output line with named neighbors, so `search`
escapes line feed in those two fields as well, and a tag additionally escapes
its own delimiters and escape introducer, using the same `\u{...}` vocabulary;
no metadata value can forge another result row, field, or tag. `--raw-output` is
the explicit opt-in that writes those fields unchanged; the same safe-rendering
choice covers assistant text, typed diagnostics, and durable updates. Each
complete raw text value is flushed before the client awaits another frame,
without adding a delimiter.

The existing `signalbox-debug` binary is unchanged and remains a development
harness, not a protocol client.

## Open edges

Deferred transport, compatibility, update-stream, retention, and operation
questions are cataloged under
[Protocols and persistence](../open-questions.md#protocols-and-persistence);
later client-form choices are cataloged under
[Client scope](../open-questions.md#client-scope). Richer metadata query
language and creation-derived visibility are cataloged under
[Session organization, visibility, and retention](../open-questions.md#session-organization-visibility-and-retention).

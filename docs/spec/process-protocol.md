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
dedicated connection before migrations run and held until shutdown. A committed
credential-wait change sends `resync_required` to that session's followers; the
database notification listener reconnecting requires all followers to resync.

## Design decisions

Wire version 1 is edited in place until the first durable deployment, a client
that cannot be rebuilt at will, such as an owner-operated remote daemon or
installed app; after that, every incompatible change allocates a permanent new
version number.

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
live successor's socket. The daemon admits only socket peers whose uid equals
its effective uid. Remote access needs an authenticated identity and revocation
design that does not exist, recorded in
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
Storage-version columns follow [identity and commands](identity-and-commands.md)
and are not exposed as wire-version fields. An opaque provider-compaction
semantic entry projects only a non-text marker carrying its turn and
producing-call identities; its provider replay bytes never cross protocol
output.

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
diagnostic evidence, including local workspace-configuration logs.

The transport is local-machine and single-user; beyond the socket peer uid
check, the protocol has no authentication or authorization exchange and no
remote transport.

Domain values, PostgreSQL records, and wire messages are distinct
representations. The daemon constructs domain and application values from a
request before any mutation and rejects an incompatible shape without
normalizing it. A wire snapshot is a presentation projection, not a domain
session, a storage record, or a provider prompt; a stored variant the projection
does not recognize fails closed.

Version admission is the one centralized wire gate: an unknown version produces
`unsupported_version` and the server closes the connection. The server may close
a connection after any error, and a client never reinterprets an unknown message
as a known one. Outbound encoding failures close and log only the affected
connection. Recoverable listener accept errors retry with a bounded delay;
permanent listener errors remain fatal.

A connection processes one request at a time, and a follow request consumes its
connection until it closes. Inbound admission is bounded globally by an
active-connection ceiling, a bounded pre-admission read-ahead, and an aggregate
buffered-frame budget that reserves one slot for an active import. One
process-wide bulk-ingest permit admits at most one chunked or single-shot
conversation import or blob upload at a time. The daemon admits one review
mutation at a time and releases the inbound frame slot after acquiring that
permit and before application handling.

The daemon exports an active-client-connection gauge and warns once on entering
each saturation episode; it pauses acceptance at the ceiling. The required
`client_frame_deadline` bounds frame completion from the first received byte,
including admission wait and buffered input awaiting earlier request handling,
and `client_write_progress_deadline` bounds a write that makes no progress.
Read-ahead remains bounded and does not defer expiry when full. Expiry closes
the connection. Neither bound limits idle connections or idle follow streams;
`"none"` disables the corresponding bound.

SIGINT or SIGTERM stops admission and drains runtime work under the configured
shutdown grace window. A subsequent termination signal interrupts that drain,
aborts remaining runtime tasks, and proceeds immediately to cleanup.

Every accepted non-review mutation, import transport request, or blob transport
request produces exactly one receipt message or an error, except when a
connection deadline expires or an import or blob request crosses its
non-resetting deadline and closes the connection. A mutation whose commit
outcome is unknown returns `commit_ambiguous`; an infrastructure failure known
to precede the commit returns `unavailable`.

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

A decision on the earliest delegated request without terminal judge evidence
records an `awaiting_approval_judge` rejection and returns
`tool_request_awaiting_approval_judge`.

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

Except for immutable credential-pool policy validation, every read that holds a
pooled connection across more than one statement takes one snapshot-reader
admission. Single-statement defaults and blob metadata reads take no
snapshot-reader permit before dispatch; the blob metadata handler acquires one
before its database read. Pool-policy validation takes no admission so a
follower can validate its snapshot while holding one. Every request states its
admission class before dispatch, so no read verb reaches the pool by omission.
The reader budget leaves at least two pool connections outside snapshot work.

The imported seed frontier is selected only when no persisted turn-start lineage
exists; a queued but unstarted first native turn does not hide it.

Operator status includes one `unavailable_component` record with a stable cause
for each process-local store, adapter, or credential member unavailable at
startup, and its end message carries the count. A component name admits the
credential-member prefix plus the full configured credential-profile name. It
includes one `repository_ingestion` record per configured watched repository and
a `repository_ingestion_count` in its end message. A repository attempt that
exhausts its request budget or leaves failed targeted observations reports
`partial`.

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
carrying the tool request identity, closure content, and required Boolean
`approved_before_close`, and omitting the attempt identity.
`approved_before_close` is true exactly when the request had a recorded approval
before closure; an undecided request closed by turn end carries false. A
`tool_denied` entry carries required Boolean `override_recorded`, true when that
denial has its one permitted user override, including after retirement.
`operator_action_required` is false while automatic recovery is scheduled or
attempting and true only after the recovery budget in
[turn-lifecycle-and-scheduling.md](turn-lifecycle-and-scheduling.md) is
exhausted.

A successful `stop_goal` or `stop_turn` receipt carries `termination` with the
selected `descendant_scope` and the recorded `descendant_count`; parent-alone
has count zero. An equal command retry reads those immutable facts without
re-evaluating the cascade. Other goal and input receipts omit `termination`.
Input commands retain the first handling's receipt kind across equal replay
through either input surface.

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

`spawn_session` carries a bounded `task` and the closed relationship object and
returns `session_spawned { tool_request_id, child_session_id, relationship }`.
The placement-owned creation transaction that implements the parent-directory
default creates the child; it preserves the exact-request and authority rules of
the delegation contracts in
[sessions-and-transcript](sessions-and-transcript.md), and the task string fits
both the delegation-content ceiling and its complete normalized JSON argument
envelope.

Configuration reload is `reload_configuration { command_id }`, with a
user-global command identity. Equal retries report busy while pending or replay
the stored result without reading configuration again. Reloads serialize from
file read through the terminal result. Before installation, core commits the
complete checked replacement and prior reloadable snapshots and the checked
rule-set digest as durable intent. Template snapshots contain accepted
prompt-file contents.

Startup reload replay shares the serial request-reload boundary. Reload pauses
sweep admission and stops and joins active sweep attempts and affected ingestion
tasks before rule activation and event-tail capture. Only after that activation
commits does core reconcile convergence targets from the retained intent. On a
stale or conflicting rule-revision rejection, recovery first validates the prior
snapshot against the current startup-only sections. If invalid, it terminalizes
the intent with `configuration_reload_failed` without resuming workers, and
startup recovery fails. Otherwise it resumes ingestion and sweep admission under
the prior snapshot before terminalizing the intent with
`configuration_reload_failed`, without replacing the running configuration.
Other failures after either stops leave the intent pending until recovery
installs the replacement snapshot and resumes them before terminalizing the
claim. The [reload-intent input](session-ownership.md) delivers rule activation
only, and the module activates the rules atomically and idempotently by command
identity and digest. The activation transaction captures each repository's
current event tail and retains it for idempotent replay. Before replay activates
any retained effects, startup validates the retained reloadable snapshot
together with the on-disk startup-only sections; incompatibility fails startup
and leaves the intent pending. Startup replays any undelivered intent from its
retained payload even if the configuration files changed, before terminalizing
its claim.

Success returns `configuration_reloaded { command_id, reloaded_sections }`, with
exactly `model_catalog`, `session_templates`, and `repo_watch` in that order.
Failure returns `configuration_reload_failed { command_id, phase, reason }`;
`phase` is `read`, `validate`, `activate`, `reconcile`, or `install`, and
`reason` is sanitized as startup logs are, with 1 through 1,024 UTF-8 bytes and
no control characters. A pending reload returns `unavailable` with the message
`configuration reload is busy`. The terminal client exposes
`reload-configuration` with optional `--command-id` for retries and prints the
installed sections or failure phase and reason.

Runner placement inspection is `read_runner_status { page_size, after }`, with
`page_size` 1 through 100; other sizes reject the request. Each page opens with
`runner_status_start`, returns `runner_status`, then `runner_operation_failure`
and `runner_workspace_leak` in that order, and closes with
`runner_status_end { runner_count, failure_count, leak_count, next_after }`. The
counts name messages on this page; `page_size` bounds runner facts, failures,
and leaks together. `after` and `next_after` are null or one tagged cursor
naming the last emitted fact, exclusive on continuation; `next_after` is null at
the end.

`runner_status` carries one enrollment or current session placement. Enrollment
facts include the runner, its enrollment-request identity, current authority
(`pending`, `active`, or `revoked`), and nullable connection health. Session
placement facts carry the snapshot's complete runner object. Enrollment facts
sort by runner UUID before placements sorted by session UUID, with
`enrollment { runner_id }` and `placement { session_id }` cursors respectively.
A pending provisioning-only successor is visible by the identity
`promote_pending_runner` accepts. The terminal client exposes
`runner status [--page-size N] [--after JSON]` and verifies fact ordering,
counts, and continuation before rendering the page.

The failure projection's `operation_kind` selects the refused operation's
complete correlation arm, including its runner. Its category set is exactly the
runner wire's closed daemon-actionable set. The detail carries bounded `code`,
`message`, and structured `payload`; storage retains the runner-authored text
unchanged. The diagnostic projection replaces the message with `[redacted]` and
payload strings with empty strings, preserving the checked code and nontext
structure. A workspace leak carries runner, fact kind, relative locator, entry
digest, and nullable session and placement revision. No retained leak producer
exists, so the read returns no leak messages and `leak_count` is zero. Retained
failure traversal belongs to [persistence-protocol](persistence-protocol.md).

`runner_state_transition` notifies followers of live transitions above the
snapshot cursor; reconnect snapshots and session summaries carry the current
runner object, with connection health present exactly for a pinned placement. A
new runner fact extends this event kind with its state and members rather than
adding another kind.

`replace_lost_runner` and `abandon_lost_runner` carry the command and session
identities; replacement also carries a nullable complete checkout revision.
`promote_pending_runner` carries the command identity and pending enrollment
request identity. Each returns a correlated closed terminal receipt, and equal
pending replacement replay waits on the same durable operation. The terminal
client exposes `runner replace <session> [--revision <sha>]`,
`runner abandon <session>`, and `runner promote <enrollment-request-id>`, with
an optional `--command-id` for replay.

OAuth administration has three requests, `provision_oauth_credential`,
`reprovision_oauth_credential`, and `delete_oauth_credential`, each carrying
`profile` and a user-global `command_id`. The terminal client exposes them as
`credential provision|reprovision|delete <profile>`. The progress message is
`oauth_credential_authorization { command_id, profile, user_code, verification_uri }`:
`user_code` is 1 through 256 printable ASCII bytes and `verification_uri` is an
absolute HTTPS URI without user information or fragment, at most 4,096 UTF-8
bytes. The terminal receipt is
`oauth_credential_receipt { command_id, profile, outcome }`; its closed outcome
and failure vocabulary live in `crates/process-protocol/src/response.rs`. For
provisioning and re-provisioning, an undeclared profile records
`failed { reason: unknown_profile }`; a declared non-OAuth profile records
`failed { reason: non_oauth_profile }`. Neither starts an exchange or mutates
credential state. An equal command replays its stored receipt before current
registration is evaluated, and conflicting reuse is rejected. The terminal
client verifies the command and profile correlations and prints authorization
details before the receipt.

OAuth exchange claims retain validated authorization instructions before
emission; invalid progress fields yield `device_endpoint_rejected`. Equal
pending commands replay available instructions and report busy; startup records
`abandoned` for pending exchanges; an ambiguous claim commit or failure to
retain or send authorization progress or commit exchange completion raises the
daemon's recovery signal. Authorization and its terminal receipt commit
atomically after generation and current-registration checks; stale generations
yield `superseded`, and changed registrations yield
`failed { reason: registration_changed }`. Initial provisioning with stored
authorization returns `already_provisioned` without an exchange; re-provisioning
without authorization returns `not_provisioned`.

Deletion uses OAuth registration or administration history even without a
current declaration; a pool-lock row alone does not qualify. Without retained
identity it records the same unknown-profile or non-OAuth failure. A new
deletion holds the dispatch profile lock, advances the generation, removes
authorization and cached access, and returns `deleted` or `already_deleted` when
no authorization was stored; equal replay changes nothing. Current registration
and referenced history remain, and a child already holding a copied token
finishes. Successful re-provisioning replaces authorization and clears every
OAuth delivery-origin quarantine and cached access; failed re-provisioning
clears none.

Workspace operator commands are `register_workspace`, `mint_git_remote`, and
`withdraw_git_remote`. Each carries a command identifier and returns the
corresponding immutable workspace, mint, or withdrawal identity. Registration
resolves the supplied root once in the daemon filesystem before constructing the
canonical payload. The client exposes them as `workspace register`,
`workspace mint-remote`, and `workspace withdraw-remote`. Registration retains
the original request path at storage version 2 for settled replay without
filesystem access. Version 1 registrations without that field replay by their
stored canonical root; version 2 requires it. Workspace and remote
state-constraint rejections return `invalid_request`; stored corruption returns
`internal`.

Credential-exclusion administration is one `list_credential_exclusions` read
carrying `page_size` and `after`, and one `clear_credential_exclusion` mutation
carrying a user-global `command_id` and one closed `target` object. The target
is `profile_quarantine { profile, record_generation }`,
`membership_exclusion { pool_policy_id, profile, record_generation }`, or
`session_displacement { session_id, pool_policy_id, profile, record_generation }`;
chain exclusions remain insert-only and turn-local. The read lists every active
exclusion the mutation admits, as its exact target object, and omits exactly the
records the mutation rejects; the filter turns on the exclusion's origin, never
on the profile's delivery. The origin filter rejects `oauth_refresh` quarantines
and accepts `codex_home` quarantines. `page_size` is 1 through 100; `after` is
null or one complete target object and is an exclusive keyset cursor. Results
sort by target tag in the order above, then by each field's canonical order:
UTF-8 bytes for configured names, UUID bytes for durable identities, numeric
order for generations. The read opens with `credential_exclusion_start`, then
one `credential_exclusion` per row, then
`credential_exclusion_end { exclusion_count, next_after }` with a null
`next_after` only at the end. The mutation marks exactly the named active
generation cleared. A newer active generation at the target's own exact scope
returns `stale_generation` before the named older generation is considered; the
scope is the profile and origin for a profile quarantine, the pool policy and
profile for a membership exclusion, and the session, pool policy, and profile
for a session displacement. A target with no exactly matching retained record is
`unknown_credential_exclusion`; an exact record an earlier command already
cleared is `already_cleared`. Success returns
`credential_exclusion_cleared { target, outcome }` with outcome `cleared` or
`already_cleared`, and the inactive record is retained so the second answer is
durable. An equal `command_id` replay returns its stored receipt before current
state is evaluated. Both operations are authorized as every other request is:
reaching the owner-private socket is the authority.

Program commands use ordinary user authority from the owner-private socket.
`register_program { registration_id, registration }` accepts name, revision,
grants and a JavaScript source/artifact pair or compiled native entry/revision;
the daemon supplies the native binary digest. It returns
`program_registered { registration_id }`. NUL bytes in registration name,
revision, artifact text or native entry/revision return `invalid_request`.
`start_program_run { run_id, registration_id, input }` retains exact encoded
input bytes and returns `program_run_started { run_id, registration_id }`. A
failed runner wake after admission returns `commit_ambiguous`. Equal
registration and start retries return the same admission; conflicting identity
reuse returns `conflicting_reuse`, and an unavailable native key returns
`invalid_request`. `read_program_run { run_id }` returns
`program_run_read { run_id, run }`, with registration identity, `input` bytes,
`input_extent`, and an outcome tagged by `state`: `running`, `cancelled`,
`faulted`, or `succeeded` with `result` bytes and `result_extent`. Each extent
is `{ kind: "complete" }` or `{ kind: "truncated", total_bytes }`. Reads fit the
frame byte budget by returning byte prefixes, input first and then result;
truncation leaves stored data intact. Missing reads return `not_found`.

Program-run cancellation is the request
`cancel_program_run { run_id, command_id }` and the receipt
`program_run_cancellation_receipt { command_id, run_id, outcome }`. The outcome
is `applied { terminal_state: "cancelled", result: null }`, `not_found`, or
`already_terminal { terminal_state, result }` naming the standing terminal state
and result the command found. The terminal states are `cancelled`, `faulted` and
`succeeded`; success carries a frame-bounded `result` byte prefix and a
`result_extent` using the same complete/truncated markers as run reads. The
prefix budget reserves the longest request identity so cancellation retries
return identical outcomes. Other states carry null. The result field is required
for every terminal state. An identical request bearing the same `command_id`
replays its stored receipt even if the run's standing state later changes; the
same identity with a different payload is conflicting reuse. Run-state semantics
belong to [workflows.md](../spec/workflows.md); this pair, its version-1
encoding, and the closed receipt algebra belong here, and a later incompatible
shape requires a new protocol version.

Transcript snapshot starts include nullable `repository_watch` provenance
resolved from the retained dispatch ledger.

If counted-activation revalidation selects pre-call exhaustion failure,
activation and terminalization commit together before execution resumes.

When no pool member is admissible, pre-call exhaustion projects
`failed_credential_pool_exhausted { terminal_frontier_id, terminal_attempt_id, failure_entry_id, pool_policy_id, policy_members, members }`
as a `transcript_turn` state variant,
`turn_credential_pool_exhausted { turn_id, terminal_attempt_id, failure_entry_id, terminal_frontier_id, pool_policy_id, policy_members, members }`
as its live event, and the read
`read_credential_pool_policy { session_id, turn_id, pool_policy_id }` answered
by `credential_pool_policy { pool_policy_id, policy_members }`. The read is
admitted only when the caller may read the named session and its named turn
references that exact immutable policy; a mismatch is `unknown_pool_policy`, and
the response reconstitutes the policy header and membership rows directly rather
than copying either failure projection. `policy_members` is the immutable
policy's complete ordered array of profile references; `members` has the same
length, and each evidence item's `profile` equals the same-ordinal
`policy_members` value. Each item carries `profile`, a nullable
`reset_at_unix_ms`, and one closed `exclusion`:
`profile_quarantine { record_generation }`,
`membership_exclusion { record_generation }`,
`session_displacement { record_generation }`,
`chain_exclusion { predecessor_model_call_id }`,
`transient_exclusion { observation_model_call_id }`, or
`headroom_reserve { observed_headroom_percent, reserve_percent }` without a
generation. A member satisfying several exclusions reports exactly one, chosen
in that order, widest scope first, so two producers cannot describe one
exhaustion differently. `reset_at_unix_ms` is present only when every exclusion
active for the member at the failure commit expires at the reset it reports, and
is then the latest of them. The snapshot and event carry no credential bytes,
path, provider prose, or current-configuration lookup, and the projection is
never paginated or truncated; configuration admission bounds each profile and
pool name to 256 UTF-8 bytes and each pool to 1,024 members so the duplicated
evidence fits one frame under worst-case JSON escaping. The non-null
`record_generation` is zero, the oldest generation, for an active action without
a projection generation or for a member operationally unavailable in the
captured selection snapshot. OAuth quarantine writes retain a profile-quarantine
exclusion tied to the authorization generation; reauthorization retires its
active state.

In the exhaustion projection, `members` and `policy_members` are equal in length
and order, the snapshot state and the live event carry identical `members`, the
policy read returns the same inventory, and the client exposes the terminal
state only after those checks pass.

Credential admission waits project the active turn state
`active_awaiting_credential_availability`, carrying the call-free ended
`wait_attempt_id` and the closed `contended` or `exhausted` cause. The terminal
client keeps following that turn.

A terminal credential wait release after a provider call projects
`failed_after_credential_wait`, naming the fresh call-free terminal attempt and
its known-failed predecessor with the retained provider cause. Its live terminal
event is `turn_failed`; it emits no pool-exhaustion event.

## Planned

- Listener probing before stale socket identity-pin cleanup; see
  [daemon survival design](../design/daemon-survival.md).

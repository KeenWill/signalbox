# Blob storage

This page is the foundation proposal introduced by PR #553
(`agent/blob-storage-foundation`). Its implemented-behavior statements become
verified with its implementing child stack and take effect with the full stack.
A paragraph that names itself unimplemented is committed unimplemented
functionality and carries only its stated compatibility constraint.

The blob digest and external spelling, deterministic object-key producer,
streaming whole-object and ranged store contract, filesystem publication,
verification, and corrupt-object repair behavior, and shared store conformance
suite are verified against PR #555 (`agent/blob-storage-substrate`).

The path-style S3 adapter, explicit rotating credential-file reads, streaming
multipart publication, namespace and lifecycle authentication, bounded
transport, and opt-in live conformance suite are verified against this
implementing change (`agent/blob-storage-s3`).

The append-only identity, store-binding, and verified-replica catalog and its
transactional registration behavior are verified against PR #581
(`agent/blob-storage-catalog`).

The configuration catalog, route validation, filesystem namespace admission, and
startup store registry are verified against this implementing change
(`agent/blob-storage-registry`).

The disk-backed upload lifecycle, routed-store-only live deduplication,
publication-before-registration ordering, process-protocol messages, and shared
bulk-ingest resource bounds are verified against this implementing change
(`agent/blob-storage-upload`).

The terminal upload command and PostgreSQL/socket lifecycle proofs are verified
against this implementing change (`agent/blob-storage-upload-terminal`).

Bounded direct metadata and range reads, replica fallback classification,
non-waiting process admission, and the terminal read commands are verified
against this implementing change (`agent/blob-storage-read-wire`).

The canonical multipart domain algebra and ordered process-protocol vocabulary
are verified against this implementing change
(`agent/blob-storage-multipart-algebra`).

Registry-first attachment catalog admission, distinct-digest byte accounting,
and durable typed rejection replay are verified against this implementing change
(`agent/blob-storage-attachment-admission`).

Prospective rendered-frontier attachment accounting and affected queued-input
revalidation are verified against this implementing change
(`agent/blob-storage-attachment-frontier-admission`).

Provider-neutral ordered attachment stubs and immutable catalog-length
projection are verified against this implementing change
(`agent/blob-storage-attachment-rendering`).

The text-only blob-read family, frontier-derived digest authorization, and
durable per-request and per-turn admission counters are verified against this
implementing change (`agent/blob-storage-read-tools`).

It owns one thing: how Signalbox stores, identifies, references, and reads
immutable binary content — blob identity, the durable replica catalog, store
configuration and routing, the ingest and read lifecycle, the blob wire
vocabulary, multipart user content with attachment parts, and what a model is
shown when accepted content carries an attachment. The session aggregate and
transcript projections are owned by
[sessions-and-transcript](sessions-and-transcript.md); command payload storage
and replay equality by [identity-and-commands](identity-and-commands.md); the
relational baseline and migration discipline by
[persistence-protocol](persistence-protocol.md); framing and the client request
vocabulary by [process-protocol](process-protocol.md); the Layer-1 runtime
boundary by [runtime-substrate](runtime-substrate.md); model-call preparation
and authorization by [model-call-execution](model-call-execution.md); the
configuration catalog and credential delivery by
[configuration-and-credentials](configuration-and-credentials.md); imported
source records by [conversation-import](conversation-import.md); tool result
authority by [tool-loop](tool-loop.md).

## Identity and the blob value

A blob is an immutable, nonempty byte sequence identified by the SHA-256 of
exactly those bytes. The domain value is a 32-byte digest newtype in the shape
of the existing digest family (`ImportedRawRecordHash` and kin); the external
spelling is `sha256:` followed by 64 lowercase hexadecimal characters. Why: the
tag lives in the spelling, where format evolution actually happens, while the
domain algebra stays as small as every other digest the repository carries.

The digest covers raw bytes only. Filename, declared media type, purpose,
producing session, and storage placement are properties of a use of the blob —
an attachment part, a future tool artifact record — never of the blob itself.
One byte sequence uploaded under two names, or produced independently by two
sessions, is one blob. Blob identity is global to the installation; the
single-user authorization model makes global deduplication safe, and any future
multi-principal boundary must revisit that assumption before sharing the
namespace.

This substrate is general: user attachments, tool artifacts, imported source
material, and generated assets are all uses of the same immutable-byte layer.
Nothing in it may depend on which use referenced a blob first.

## The replica catalog

PostgreSQL is canonical for which blobs exist and where their bytes durably
live. A `blob` row records the digest, its byte length, and creation time; a
`blob_replica` row records that one named store durably holds one verified
object for that digest under one recorded object key, and is inserted only after
that store's publication has been verified. Deleting a blob row that a replica
references is rejected, and no surface in this stack deletes either row: the
catalog is append-only.

The blob digest is the `blob` primary key. A `blob_store_binding` row records
one store name and its deployment-supplied canonical UUID `namespace_id`; both
are unique, append-only durable facts. Replica registration first inserts or
reloads that binding and rejects any disagreement as typed catalog corruption. A
replica is unique both by digest plus store name and by store name plus object
key. Concurrent registration reloads the winning catalog state: matching
namespace, length, and replica facts are idempotent success, while any
disagreement is typed catalog corruption rather than a raw uniqueness error.

Placement is durable fact, not configuration lookup. Routing configuration
decides where new writes go; reads resolve through recorded replicas, so a
configuration change never reinterprets or orphans existing content. A store
name plus namespace identity is the durable deployment identity. Startup
compares every recorded binding with configuration and fails before socket work
if a name or namespace is absent or disagrees. Moving one namespace to another
locator preserves its UUID; assigning a locator to another namespace requires a
fresh store name and UUID. Version one has no replica-retirement state, so a
configured binding cannot be removed while any `blob_replica` row names it, even
after another replica has been added elsewhere. Why: the alternative — deriving
identity from current location configuration — silently changes the meaning of
every old durable record on each configuration edit.

Object keys are deterministic and content-derived (`sha256/ab/cd/<hex>`), carry
no filename, extension, or session identity, and are recorded per replica so a
store's key layout can evolve without reinterpreting history.

## Stores, routing, and configuration

The daemon configuration catalog gains an optional `[blob_storage]` table. Its
absence preserves startup compatibility only while the blob catalog is empty;
blob and conversation-import operations are then unavailable rather than
inventing a storage location. Once the catalog is nonempty, omission is a
startup error because every recorded store must remain resolvable. When present,
the table requires an absolute `staging_directory`, a positive decimal-u64
`max_blob_bytes`, one through 32 `[[blob_storage.stores]]` entries with distinct
`name` values matching `[a-z][a-z0-9_-]{0,63}` and distinct canonical UUID
`namespace_id` values, and a `[blob_storage.routes]` table containing exactly
`user_attachment`, `tool_artifact`, `imported_source`, and `generated_artifact`.
Every route names a declared store. When conversation import is enabled,
`max_blob_bytes` must be at least `conversation_import.max_source_bytes`,
including that table's default. The table follows the version-one catalog
grammar and rejects unknown or kind-inapplicable fields. Every catalog query and
in-memory traversal orders store names by unsigned ASCII bytes; persistence uses
bytewise `C` collation for that order rather than a deployment locale.

Configured store entries must also name distinct physical namespaces. Before
initializing filesystem roots, startup resolves each opened directory's
canonical path, `(st_dev, st_ino)` identity, and bounded Linux mount-inventory
ancestry and rejects equality or ancestry overlap on those facts, so symlink,
relative-component, and bind-mount aliases cannot manufacture replica diversity
or place one store's control or deterministic object namespace inside another's.
An identity that cannot be proved distinct fails startup. For S3, the namespace
locator is the parsed endpoint's canonical URL serialization with default-port
and empty-path variance removed, paired with the exact bucket; startup rejects a
duplicate locator even when store names, namespace UUIDs, regions, or
credentials differ. One physical namespace is represented by one store binding.
The bucket marker below additionally detects physical aliases whose canonical
locators differ. The canonical staging-directory path must be disjoint from
every filesystem-store root: neither path may equal, contain, or be contained by
the other, including through a bind mount. This also excludes every store's
reserved `.publish-v1` subtree from staging ownership and prevents either
startup sweep from encountering the other's files.

Each filesystem root also owns a private exact-mode-0600
`.signalbox-blob-namespace-v1` marker whose complete bytes are the configured
canonical `namespace_id` plus one LF. Startup loads recorded store bindings
before initializing roots. If a binding already exists, the marker must already
exist as a no-follow regular file with exact ownership, mode, and bytes; absence
or disagreement fails startup before socket admission, and startup never creates
it. With no recorded binding, initialization atomically creates the marker
without clobbering, syncs the file and root directory, or validates an existing
exact marker. Why: if a configured mount is absent at restart, the underlying
directory must not be admitted as the recorded namespace merely because its
current path and device identity are locally unique.

Each S3 bucket likewise owns the reserved object key
`.signalbox-blob-namespace-v1`, whose complete body is the configured canonical
`namespace_id` plus one LF. Startup reads recorded bindings before accessing the
bucket. These authenticated checks run after the database connects and the
configuration-independent recovery scan completes, but before socket admission
or scheduling. For a new binding in a currently routed store, it conditionally
creates the marker with `If-None-Match: *`, then performs a bounded exact read;
precondition loss reads and verifies the winner. A currently routed existing
binding requires that exact read and never creates a missing marker. Absence,
disagreement, a body larger than 128 bytes, or an unavailable read or
conditional write fails startup before socket admission. An unrouted historical
S3 binding performs no startup I/O; before its first read operation in an
incarnation, one shared lazy check reads and verifies the marker, with
concurrent callers sharing the outcome. Failure makes that store candidate
unavailable and falls through under ordinary replica selection rather than
stopping the daemon. This backend-resident proof makes two locator strings that
alias one physical bucket disagree on their distinct configured namespace UUIDs
instead of being admitted as independent replicas.

A `filesystem` store entry contains exactly `name`, `namespace_id`,
`kind = "filesystem"`, and an absolute `root_directory`. An `s3` entry contains
exactly `name`, `namespace_id`, `kind = "s3"`, an absolute HTTP(S) `endpoint`,
nonempty `region` and `bucket` strings of at most 255 ASCII bytes each, and an
absolute `credentials_file`. Endpoints reject user information, query, and
fragment components; HTTP is admitted only for a literal loopback host, and
version one always uses path-style bucket addressing. The credentials file is at
most 16,384 bytes of strict TOML containing exactly `version = 1`, a nonempty
`access_key_id` of at most 256 bytes, and a nonempty `secret_access_key` of at
most 4,096 bytes. It satisfies the configuration contract's regular-file,
ownership, and mode checks and is read once per logical store operation so
rotation does not require daemon restart. No environment, provider profile,
metadata service, or other ambient source is consulted.

Version one ships two store kinds. `filesystem` is a production-supported store
only on a filesystem the host can positively classify as local, non-network, and
non-userspace storage. Startup rejects network, userspace, and unclassified
mounts rather than admitting an uninterruptible remote operation into the
daemon. Native network-filesystem support therefore remains outside this version
until it has an isolatable, bounded-operation contract. Publication writes
through a create-new file directly beneath a reserved `.publish-v1` child of the
store root, syncs the complete temporary file, atomically renames it to the
final content-addressed path, syncs affected directory metadata, and then
completely verifies the final bytes before catalog registration; a failed
durability or verification operation makes the store unavailable and records no
replica. Before socket admission and after acquiring the exclusive daemon lock,
startup removes every regular file from each `.publish-v1` child. A symlink,
subdirectory, entry with a different UID, or otherwise unprovable occupant fails
startup rather than being followed or removed. The configured root,
`.publish-v1`, and every created directory are owned by the daemon's effective
UID with exact mode `0700`; a symlink, another UID, or any group or other
permission fails startup. Store-local temporary and final blob files are
create-new regular files owned by that UID with exact mode `0600`, are opened
without following links, and retain that mode across publication. `s3` speaks
the S3-compatible API against an explicit endpoint with explicit file-delivered
static credentials; ambient credential discovery (process environment, provider
configuration files, instance metadata) is rejected by construction, and an
object store's own integrity metadata is never treated as content identity.
Multiple stores are enabled simultaneously and routed by class; routing by media
type or filename is inexpressible. Why: class is a classification Signalbox
itself made, while media type and filename are caller-supplied strings, and a
caller-supplied string must not select which infrastructure gains authority over
bytes.

**Committed unimplemented functionality.** No present surface stores program
frame payloads. The closed routing-class vocabulary gains one `program_journal`
class for over-threshold journal payloads written by the
[program substrate](program-substrate.md)'s host: derived by the daemon from the
writing surface exactly as every class is, never operation-selected, and added
to the routes table's required set when the substrate lands. This constrains
present change: the class vocabulary and route-validation surface must stay
extensible to that addition without loosening the closed-set rejection of
unknown classes.

Blobs are large: the substrate supports multi-gigabyte objects, so every daemon
path — ingest, verification, replica copy, read — streams and none materializes
a whole blob in memory. Bounded in-memory materialization exists only at
explicitly bounded consumers, and each such consumer names its bound.

The configured staging directory is a daemon-owned private directory on storage
the host positively classifies as local, non-network, and non-userspace by the
same admission rule as a filesystem-store root; an unclassified staging mount
fails startup. The daemon creates one `uploads-v1` child with mode `0700` and
holds the installation's exclusive daemon lock while using it; upload spools are
create-new mode `0600` regular files directly beneath that child. Before socket
admission, startup removes every regular spool in that child. A symlink,
subdirectory, entry whose UID differs from the daemon's effective UID, or
otherwise unprovable occupant fails startup rather than being followed or
removed. Clean shutdown cancels active uploads and performs the same sweep. This
reclaims crash leftovers without treating unrelated paths as Signalbox-owned.

Every S3 logical operation has a 10-second connect timeout, a 60-second
no-progress read/write timeout, and a 24-hour whole-operation deadline. The
caller's cancellation signal aborts transport work and best-effort aborts an
open multipart upload; a model-call attachment check binds that signal to the
call's authoritative cancellation, while upload work binds it to connection loss
and daemon shutdown. A timeout after a publication that might have been accepted
is not success. When cancellation has not won, the adapter gets one fresh
24-hour reconciliation deadline, with the same connect and idle bounds, to
perform a complete read-back and registers only exact verified bytes. A
read-back timeout or cancellation returns unavailable when nonacceptance is
proved and ambiguous publication otherwise, releases the bulk-ingest permit, and
leaves at most an unregistered orphan; retry live-verifies the deterministic key
before completing registration.

One direct read or one model-call attachment-preparation pass also owns one
24-hour monotonic aggregate deadline across its complete ordered traversal of
all attachment digests and replica candidates. Every adapter operation receives
only the remaining allowance; moving to another candidate never restarts it.
Expiry cancels the active adapter operation, releases traversal resources, and
uses the ordinary unavailable outcome rather than delaying a later candidate or
caller beyond the aggregate bound.

Direct blob reads and checked imported-aggregate loads share a separate
non-waiting process-wide admission bound of 16 active traversals. A request that
cannot acquire one of those permits immediately returns the ordinary unavailable
outcome; it never occupies a connection task while queued. Thus even 16 reads
that retain their complete 24-hour allowance leave 112 of the process protocol's
128 connection tasks available to control traffic.

A model-originated `blob_read` that acquires one of those permits releases its
scheduler-pass slot before store traversal while its physical attempt and
per-session dispatch gate remain in flight. It reacquires scheduler capacity
before committing either the correlated result evidence or a crash-loss
classification. A request that cannot acquire a direct-read permit returns the
ordinary unavailable result without relinquishing its pass. At most 16 such
tasks can wait to reacquire scheduler capacity, independent of the configured
scheduler-pass capacity. Because each task relinquishes its scheduler slot
during store traversal, slow reads cannot occupy every configured scheduler-pass
slot, and the fixed direct-read bound keeps the handoff's waiter inventory
bounded.

Attachment-preparation store traversal is bounded independently from scheduler
passes: at most eight such traversals are active process-wide. A model-call pass
tries to acquire this permit without waiting. If none is immediately available,
the pass releases its scheduler-pass slot, ends its in-flight work, and leaves
only the durable `Prepared` call for a later sweep. A pass that acquires the
permit releases its scheduler-pass slot before performing store I/O while
remaining in flight for per-session deduplication. Successful preparation
reacquires scheduler capacity before send authorization, whose guarded
transaction revalidates the call; unavailable preparation releases all capacity
and leaves the call `Prepared`. The zero-attachment path acquires no blob permit
and never relinquishes its ordinary pass slot. Thus slow 24-hour traversals
cannot occupy the scheduler's bounded pass inventory, and attachment preparation
creates no unbounded waiter inventory.

An S3 store currently named by at least one route is admitted for publication
only when its bucket lifecycle configuration contains an enabled rule covering
the complete `sha256/` object-key prefix (or the whole bucket) that aborts
incomplete multipart uploads after one day. Startup reads a bounded 65,536-byte
lifecycle response with the configured static credential and fails closed when
the rule cannot be proved; the credential therefore needs that read permission.
An unrouted historical S3 binding remains configured but is not
lifecycle-queried at startup; its read failures use the ordinary runtime
`unavailable` candidate outcome. The external bucket rule is the crash and
credential-loss bound for uploaded parts that never became a final object.

All namespace-marker and lifecycle operations for every currently routed S3
store share one non-resetting five-minute monotonic startup deadline. Each
operation receives only the remaining allowance while retaining the 10-second
connect and 60-second no-progress bounds. Exhaustion fails startup before socket
admission; changing stores or probe kinds never restarts the aggregate deadline.

## Ingest and the transaction boundary

Ingest streams caller bytes to a staging file while hashing and counting,
enforces the stored-size ceiling, verifies the caller's expected digest and
length, publishes the object to the routed store, verifies publication, and only
then records `blob` and `blob_replica` rows in one PostgreSQL transaction. No
database transaction is ever open across store input/output. A crash between
publication and registration leaves an unregistered orphan object. That failure
is catalog-safe — it never creates a dangling reference — but it is not
capacity-free: no surface in this stack inventories or removes the orphan, and
retention and garbage collection remain outside this contract. Re-ingest
rediscovers the object; an acknowledged reference always has verified durable
bytes behind it. Because the key is the digest, retrying an ambiguous store
outcome is idempotent: read back the final key, verify, and finish registration.
Ingest validates the expected length against any catalogued identity before an
already-present response. It short-circuits only when that identity has a
live-verified replica in the store selected by the current semantic use. A
missing or corrupt object behind an existing routed replica record accepts the
upload and atomically replaces that deterministic object only after verifying
the staged source; a successful repair retains the matching replica fact.
Otherwise ingest publishes and registers an additional replica in the routed
store rather than minting a second identity. Deduplication across other stores
is a future optimization, not a version-one upload path.

## Wire vocabulary

Blob upload copies the chunked conversation-import lifecycle over the local
process protocol: `begin_blob_upload` declares only the expected digest and
expected byte length. The request kind is the fixed user-attachment operation,
from which the daemon derives the `user_attachment` storage class; no operation
or client-controlled class field selects a route. After validating any known
length, begin short-circuits only for a verified replica that a live read
verifies in that routed store; a missing or corrupt recorded object proceeds to
upload for repair. `append_blob_upload` carries nonempty padded-base64 chunks
with at most 4,194,304 decoded bytes, spooled to staging and never assembled in
memory; `commit_blob_upload` returns the verified digest and length;
`abort_blob_upload` discards the staging state.

Reads are `read_blob_metadata { digest }`, returning the digest, byte length,
and bounded replica count, and
`read_blob_chunk { digest, offset_bytes, length_bytes }`. Offset and length are
canonical decimal-u64 strings; length is from 1 through 4,194,304 bytes, checked
addition must not overflow, and the exact half-open range must lie within the
blob. A request at or beyond end-of-blob, or one crossing it, is a typed range
rejection rather than a short or empty read. The response echoes digest and
offset and carries exactly the requested bytes as padded base64. A connection or
model turn considers recorded replicas in canonical store-name order and
live-verifies each candidate by streaming its full length and SHA-256 before
returning that digest's first range, retaining only the requested range in
memory. Missing, corrupt, or unavailable candidates fall through to the next
recorded replica; typed failure with no bytes is returned only when none can
satisfy the read. After all candidates fail, any unavailable candidate makes the
result `unavailable` because the daemon cannot prove the blob unusable;
otherwise any digest or length mismatch makes it `blob_corrupt`, and an
all-missing set makes it `blob_missing`. An absent catalog identity remains
`not_found`. The scope may reuse verification for later ranges only while the
adapter pins an immutable object generation and makes every range conditional on
that exact generation. A retained filesystem handle pins an inode but not its
contents and is never sufficient: filesystem replicas are completely reverified
before every range. An S3 adapter without a usable immutable version token
likewise reverifies before every range. The least-recently-used inventory of
generation-pinned verifications holds at most eight digests; eviction makes a
later range verify again, and the inventory is discarded at scope end or on any
candidate failure. An inventory entry is only a bounded immutable-generation
token of at most 1,024 bytes and never retains a file descriptor, socket,
request, or store client; conditional reads acquire and release their operation
resources independently. Bytes flow only through the daemon: no client receives
a store credential, bucket name, filesystem path, or presigned URL.
Client-facing blob messages and content-part blob references expose only the
digest spelling, never placement; catalog rows separately retain byte length,
creation time, store name, and object key, while content parts retain their
attachment metadata.

## Multipart user content

`UserContent` becomes an ordered, nonempty sequence of parts, each either exact
text (the existing checked text value) or an attachment: a blob digest plus a
closed attachment kind (`image`, `document`, `file`), a declared media type, and
an optional bounded display filename admitted as a basename and redacted in logs
like other content-bearing values. Construction admits at most 256 parts,
rejects adjacent text parts, bounds the aggregate UTF-8 bytes of all text parts
at 1,048,576, bounds each declared media type at 255 visible ASCII bytes, and
bounds each optional display filename at 255 UTF-8 bytes while rejecting empty,
`.`/`..`, slash, backslash, and U+0000. These structural and resource checks
happen before typed command construction. There is exactly one canonical
representation — single-text content is a one-part sequence, and no second
spelling of equivalent content exists. Why: the durable command reuse check
compares caller-supplied payloads structurally, and two spellings of one meaning
would turn equal resubmission into conflicting reuse.

Attachment metadata is caller-supplied semantic input, so part order, digests,
kinds, media types, and filenames all participate in command replay equality.
Acceptance requires every referenced digest to be catalogued with at least one
verified replica. The sum of catalogued byte lengths for distinct referenced
digests in one input must not exceed `blob_storage.max_blob_bytes`; this names
the aggregate full-verification work bound even when that input contains 256
parts. Before recording any accepted-input effect, the same checked sum is
applied to the distinct attachment digests in the complete prospective rendered
frontier after the new content and its delivery transition. The same acceptance
transaction also recomputes the eventual prospective rendered frontier of every
already-queued accepted input whose predecessor can change under that
transition, in canonical queue order, and applies the bound to each result. Any
oversized sum uses the same typed terminal rejection, so acknowledged content
cannot make the new or an already-queued input's future prepared call exceed its
attachment bound. Catalog existence and all sums are current-state validation,
so an unseen command identifier is claimed first under the registry-first
protocol; an unknown digest or oversized aggregate then commits the typed
payload and terminal rejection with no accepted-input effect. Equal replay
returns that rejection and corrected content uses a new command identity.
Command and accepted-input rows carry mirrored ordered content-part satellites
under the existing command/effect correlation discipline. The command-side
satellite retains a pre-admission unknown digest for exact rejection replay;
only an accepted-side attachment has a catalog foreign key. The wire
`submit_input`, `reconcile_turn`, and `stop_turn` content fields all become the
same ordered parts array. The process protocol's version-one in-place editing
window is why this lands as the canonical shape rather than a compatibility
variant beside the string form.

An input and prospective rendered frontier containing no attachment digest have
both attachment sums equal to zero and bypass blob configuration, catalog, and
store access. Text-only submission therefore remains available when
`[blob_storage]` is omitted under the empty-catalog startup rule.

The one-time storage migrations insert exactly one ordinal-zero text part for
every pre-migration command and accepted-input row, verify one complete ordered
sequence per parent row, update every `SubmitInput` record to final storage
version 4, and remove the `content_text` columns. Their inserts are idempotent
on parent row plus ordinal, disagreement aborts the migration, and runtime code
accepts only version 4 and reconstructs only the satellites. Command-side and
accepted-side parts remain separate mirrored records rather than shared mutable
authority.

The terminal client renders one accepted user entry as exactly one line:
`user_content source_session=<uuid> entry=<uuid> accepted_input=<uuid> turn=<uuid> parts=<json>`.
Here `<json>` is the canonical compact ordered parts array from the wire
contract, including its fixed object-member order. Default terminal
serialization uses ordinary JSON escaping and additionally emits DEL and every
C1 code point in string values as the lowercase four-hex-digit escapes `\u007f`
and `\u0080` through `\u009f`. Parsing the JSON therefore preserves the ordered
part values while text and filenames cannot forge another terminal line or
execute controls. Attachment metadata and interleaving remain visible, and
transcript, follow, and chat never render blob bytes. `--raw-output` uses the
ordinary compact JSON spelling without the added DEL/C1 escapes and is the
explicit opt-in to those literal code points; neither mode renders blob bytes.

## Attachment visibility and model reads

Transcript presence is distinct from model-context inclusion. When a frontier
renders an accepted input whose content carries attachments, the model sees each
attachment as a bounded textual stub — kind, media type, filename when present,
byte length, and digest — never the bytes. Each attachment part becomes one
provider-neutral text part containing compact JSON whose members appear in the
exact order `signalbox_attachment`, then within that object `kind`,
`media_type`, `display_filename`, `byte_length`, and `digest`; byte length is a
canonical decimal string and an absent filename is JSON null. Ordinary JSON
string escaping makes caller-supplied metadata data rather than stub syntax. No
provider call automatically materializes attachment content, whatever its size.
Why: a durable attachment may be orders of magnitude larger than any context
window, and silently replaying large media into every subsequent call converts
one upload into an unbounded per-turn cost.

Models reach attachment content the same way they reach every other effect:
through tools, explicitly, within declared bounds. This stack ships a
daemon-registered blob-read tool family over the catalog. `blob_metadata`
accepts exactly `{ digest }` and returns text containing compact JSON with
`digest`, canonical-decimal-string `byte_length`, and canonical-decimal-string
`replica_count`. `blob_read` accepts exactly
`{ digest, offset_bytes, length_bytes }`, with both numeric values expressed as
canonical decimal-u64 strings, and returns text containing compact JSON with
`digest`, `offset_bytes`, and canonical padded `bytes_base64`. The stub's stated
length is the model's sizing information. Each read admits 1 through 524,288
decoded bytes, each turn admits at most 2,097,152 decoded bytes across admitted
read requests, and each turn admits at most 64 distinct `blob_read` logical
requests. Both durable counters charge once by tool-request identity before
authorization and replay never charges twice. The request-count cap bounds
complete replica reverification work even when the model repeatedly requests a
tiny range from a store without generation-pinned reuse. The existing
tool-result and target-context caps further limit the encoded result. At
preparation the daemon derives an allow-set from attachment stubs in the
rendered frontier; a catalogued digest outside that set is unauthorized. Results
use the existing text-only tool-result arm and never enter a provider message as
image or document media.

Content-type-aware readers are committed unimplemented functionality: no present
surface provides one, and neither its exact inventory nor the formats it
supports are decided. The compatibility constraint is that attachment stubs and
the generic read family remain sufficient to add such readers without
re-deciding visibility.

Content-interpreting processor isolation is committed unimplemented
functionality: no present decoder, parser, or renderer surface exists. The
compatibility constraint is that every future content-interpreting reader
executes inside strong process isolation and treats input validation as
best-effort defense in depth. The concrete sandbox mechanism is selected by that
implementation without weakening this posture. Why: parser hardening is an
unending surface — a malicious payload exploiting a decoder defect must be
contained by isolation rather than entrusted to an ever-growing validator.

## Model-call preparation and modalities

Before a prepared model call can cross durable send authorization, preparation
streams and verifies the length and SHA-256 of at least one recorded replica for
every distinct attachment whose stub enters the rendered request. The complete
rendered request first checked-sums the catalogued lengths of its distinct
digests. More than `blob_storage.max_blob_bytes` closes the unsent call through
`AttachmentPreparationFailure::TooLarge { maximum_bytes }` before any store I/O
or durable send authorization. Repeated occurrences of one digest do not
multiply the sum or verification work. Preparation holds no database transaction
during store I/O and retains no attachment bytes. A rendered request with no
distinct attachment digest bypasses blob configuration, catalog, and store
access, preserving the existing text-only preparation path. A digest with no
recorded matching replica closes the unsent call through the typed
missing-attachment preparation failure. When every recorded candidate can be
read but all fail length or digest verification, preparation closes it through
the typed corrupt-attachment failure. Neither path permits provider interaction
or tool authorization. When no candidate verifies and at least one candidate
remains temporarily unavailable, preparation releases its store and preparation
resources, leaves the call `Prepared`, records no turn outcome, and returns the
sanitized typed unavailable operator failure so a later pass can retry the same
unsent call. The eligibility sweep includes an active turn whose current model
call remains `Prepared`, so this retryable durable shape receives another pass;
the per-session in-flight deduplication prevents concurrent execution.
Authoritative cancellation aborts store I/O without relabeling cancellation as
an attachment failure. Successful verification seeds the turn-scoped bounded
verification inventory used by later blob reads.

The model and serving-target modality grammar, defaults, effective selection,
and client projection are owned by
[configuration and credentials](configuration-and-credentials.md#the-static-model-alias-and-web-fetch-catalog).
The attachment-specific compatibility constraint is that version-one rendered
messages in this stack carry only text, attachment stubs, and text-only
blob-read results; no present surface materializes attachment bytes into a
prepared call.

Modality-unsupported preparation failure is committed unimplemented
functionality: no present surface can trigger it because the current request
contains only text, attachment stubs, and text-only blob-read results. The
compatibility constraint is that a future typed media result fails preparation
before durable authorization when its target lacks that modality; media must
never be silently dropped. Rich image/file result arms and their carrier remain
outside this stack. Missing and corrupt attachment failures are implemented by
the preceding verification boundary and are not deferred with media carriers.

## Import convergence

Imported raw source records are the substrate's proof producer: their existing
global SHA-256 deduplication converges onto the blob catalog, with the import
satellite's content hash becoming an ordinary blob reference and the stored
bytes living in a routed store rather than a relational column. Import semantics
— record identity, conversion digests, snapshot immutability — are unchanged and
remain owned by [conversation-import](conversation-import.md).

The one-time storage-layer SQL migration produces the final blob-reference-only
`imported_raw_source_record` schema. Runtime code accepts only that shape, and
new imports write only blob references.

## Open edges

- Retention, purge, a future marked-deleted state, and garbage collection are
  deferred with
  [session organization, visibility, and retention](../open-questions.md#session-organization-visibility-and-retention)
  and the artifact lifecycle bullets in
  [general-purpose artifacts](../open-questions.md#general-purpose-artifacts);
  this page's append-only catalog is the constraint they design against.
- The content-type-aware read-tool inventory and the concrete isolation
  mechanism its processors use are recorded in
  [general-purpose artifacts](../open-questions.md#general-purpose-artifacts).
- How a tool family's admitted result references a blob rather than embedding
  bytes, and rich image/file result-content arms, remain with
  [tool safety](../open-questions.md#tool-safety).
- Ingest paths that do not cross the local socket — daemon-local file adoption
  and runner-produced artifact ingest — are recorded in
  [general-purpose artifacts](../open-questions.md#general-purpose-artifacts).
- A native network-filesystem store kind, replica-set routing, and replica
  retirement are recorded in
  [general-purpose artifacts](../open-questions.md#general-purpose-artifacts).
- Remote client access to blob content rides the open remote-transport decisions
  in
  [protocols and persistence](../open-questions.md#protocols-and-persistence);
  presigned or direct store access is inexpressible until they are decided.

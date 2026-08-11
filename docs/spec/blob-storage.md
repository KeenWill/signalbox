# Blob storage

This page is the foundation contract introduced and verified through PR #553
(`agent/blob-storage-foundation`). Its implemented-behavior statements take
effect with the full implementing stack. A paragraph that names itself
unimplemented is committed unimplemented functionality and carries only its
stated compatibility constraint.

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
absence preserves startup compatibility only while both the blob catalog and the
legacy imported-raw-source backlog are empty; blob and conversation-import
operations are then unavailable rather than inventing a storage location. Once
either durable inventory is nonempty, omission is a startup error because every
recorded store must remain resolvable and every legacy source must be
convergeable. When present, the table requires an absolute `staging_directory`,
a positive decimal-u64 `max_blob_bytes`, one through 32
`[[blob_storage.stores]]` entries with distinct validated `name` values and
distinct canonical UUID `namespace_id` values, and a `[blob_storage.routes]`
table containing exactly `user_attachment`, `tool_artifact`, `imported_source`,
and `generated_artifact`. Every route names a declared store. When conversation
import is enabled, `max_blob_bytes` must be at least
`conversation_import.max_source_bytes`, including that table's default. The
table follows the version-one catalog grammar and rejects unknown or
kind-inapplicable fields.

Configured store entries must also name distinct physical namespaces. After
initializing filesystem roots, startup resolves each opened directory's
canonical path and `(st_dev, st_ino)` identity and rejects equality on either,
so symlink, relative-component, and bind-mount aliases cannot manufacture
replica diversity; an identity that cannot be proved distinct fails startup. For
S3, the namespace locator is the parsed endpoint's canonical URL serialization
with default-port and empty-path variance removed, paired with the exact bucket;
startup rejects a duplicate locator even when store names, namespace UUIDs,
regions, or credentials differ. One physical namespace is represented by one
store binding.

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
— including over network mounts that honor same-directory atomic rename and file
and directory synchronization — writing through a same-filesystem temporary
file. Publication syncs the complete temporary file, atomically renames it to
the final content-addressed path, syncs affected directory metadata, and then
completely verifies the final bytes before catalog registration; a failed
durability or verification operation makes the store unavailable and records no
replica. `s3` speaks the S3-compatible API against an explicit endpoint with
explicit file-delivered static credentials; ambient credential discovery
(process environment, provider configuration files, instance metadata) is
rejected by construction, and an object store's own integrity metadata is never
treated as content identity. Multiple stores are enabled simultaneously and
routed by class; routing by media type or filename is inexpressible. Why: class
is a classification Signalbox itself made, while media type and filename are
caller-supplied strings, and a caller-supplied string must not select which
infrastructure gains authority over bytes.

Blobs are large: the substrate supports multi-gigabyte objects, so every daemon
path — ingest, verification, replica copy, read — streams and none materializes
a whole blob in memory. Bounded in-memory materialization exists only at
explicitly bounded consumers, and each such consumer names its bound.

The configured staging directory is a daemon-owned private directory. The daemon
creates one `uploads-v1` child with mode `0700` and holds the installation's
exclusive daemon lock while using it; upload spools are create-new mode `0600`
regular files directly beneath that child. Before socket admission, startup
removes every regular spool in that child. A symlink, subdirectory, entry whose
UID differs from the daemon's effective UID, or otherwise unprovable occupant
fails startup rather than being followed or removed. Clean shutdown cancels
active uploads and performs the same sweep. This reclaims crash leftovers
without treating unrelated paths as Signalbox-owned.

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
process protocol: `begin_blob_upload` declares the expected digest, expected
byte length, and the user-attachment operation from which the daemon derives the
`user_attachment` storage class. No client-controlled class selects a route.
After validating any known length, begin short-circuits only for a verified
replica that a live read verifies in that routed store; a missing or corrupt
recorded object proceeds to upload for repair. `append_blob_upload` carries
nonempty padded-base64 chunks with at most 4,194,304 decoded bytes, spooled to
staging and never assembled in memory; `commit_blob_upload` returns the verified
digest and length; `abort_blob_upload` discards the staging state.

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
satisfy the read. The scope may reuse verification for later ranges only while
the adapter pins the exact verified object instance: a retained open handle, an
opaque version with conditional range reads, or an equivalent stable-instance
proof. An adapter that cannot prove the same instance re-verifies before each
range. Its least-recently-used verification inventory holds at most eight
digests; eviction makes a later range verify again, and the inventory is
discarded at scope end or on any candidate failure. Bytes flow only through the
daemon: no client receives a store credential, bucket name, filesystem path, or
presigned URL. Client-facing blob messages and content-part blob references
expose only the digest spelling, never placement; catalog rows separately retain
byte length, creation time, store name, and object key, while content parts
retain their attachment metadata.

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
digests must not exceed `blob_storage.max_blob_bytes`; this names the aggregate
full-verification work bound for one accepted input even when it contains 256
parts. Catalog existence and the aggregate are current-state validation, so an
unseen command identifier is claimed first under the registry-first protocol; an
unknown digest or oversized aggregate then commits the typed payload and
terminal rejection with no accepted-input effect. Equal replay returns that
rejection and corrected content uses a new command identity. Command and
accepted-input rows carry mirrored ordered content-part satellites under the
existing command/effect correlation discipline, and the wire `submit_input`,
`reconcile_turn`, and `stop_turn` content fields all become the same ordered
parts array. The process protocol's version-one in-place editing window is why
this lands as the canonical shape rather than a compatibility variant beside the
string form.

The satellite migration raises the owning storage versions, inserts exactly one
ordinal-zero text part for every legacy command and accepted-input row, verifies
one complete ordered sequence per parent row, and only then removes the legacy
`content_text` columns from read authority. Its inserts are idempotent on parent
row plus ordinal, disagreement aborts the migration, and new code reconstructs
and compares only the satellites. Command-side and accepted-side parts remain
separate mirrored records rather than shared mutable authority.

The terminal client renders one accepted user entry as exactly one line:
`user_content source_session=<uuid> entry=<uuid> accepted_input=<uuid> turn=<uuid> parts=<json>`.
Here `<json>` is the canonical compact ordered parts array from the wire
contract, including its fixed object-member order and ordinary JSON escaping.
Text cannot forge another terminal line, attachment metadata and interleaving
remain visible, and transcript, follow, and chat never render blob bytes.
`--raw-output` does not decode or otherwise alter this structural JSON.

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
`digest`, canonical-decimal-string `byte_length`, and numeric `replica_count`.
`blob_read` accepts exactly `{ digest, offset_bytes, length_bytes }`, with both
numeric values expressed as canonical decimal-u64 strings, and returns text
containing compact JSON with `digest`, `offset_bytes`, and canonical padded
`bytes_base64`. The stub's stated length is the model's sizing information. Each
read admits 1 through 524,288 decoded bytes, each turn admits at most 2,097,152
decoded bytes across admitted read requests, and the existing tool-result and
target-context caps further limit the encoded result. At preparation the daemon
derives an allow-set from attachment stubs in the rendered frontier; a
catalogued digest outside that set is unauthorized. Results use the existing
text-only tool-result arm and never enter a provider message as image or
document media.

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
every distinct attachment whose stub enters the rendered request. The accepted
input's distinct attachment lengths were already bounded in aggregate by
`blob_storage.max_blob_bytes`; repeated occurrences of one digest do not
multiply this verification work. Preparation holds no database transaction
during store I/O and retains no attachment bytes. A digest with no recorded
matching replica closes the unsent call through the typed missing-attachment
preparation failure. When every recorded candidate can be read but all fail
length or digest verification, preparation closes it through the typed
corrupt-attachment failure. Neither path permits provider interaction or tool
authorization. When no candidate verifies and at least one candidate remains
temporarily unavailable, preparation releases its store and preparation
resources, leaves the call `Prepared`, records no turn outcome, and returns the
sanitized typed unavailable operator failure so a later pass can retry the same
unsent call. Authoritative cancellation aborts store I/O without relabeling
cancellation as an attachment failure. Successful verification seeds the
turn-scoped bounded verification inventory used by later blob reads.

Model capability records gain an input-modality axis (`text`, `image`,
`document`) on the same closed-set shape as the existing capability axes,
declared per target and projected to clients like the rest of the capability
record. Omission from an existing configuration means exactly `text`, and the
projection materializes that default explicitly. Version-one rendered messages
in this stack carry only text, attachment stubs, and text-only blob-read
results; no present surface materializes attachment bytes into a prepared call.

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

The schema transition first admits exactly one of legacy `raw_bytes` or a blob
digest on each `imported_raw_source_record`. Before accepting socket work, a
restart-safe barrier processes one legacy row at a time: it checks the stored
content hash and configured blob-size bound, publishes and registers the bytes
under the `imported_source` route without an open database transaction, then
locks and rechecks the row before atomically recording the digest and clearing
`raw_bytes`. Publication without registration may leave the ordinary orphan;
failure before the row transition leaves legacy authority intact. Restart skips
transitioned rows, re-verifies an existing routed replica, and resumes remaining
rows. A legacy row larger than `max_blob_bytes` makes startup fail until the
operator raises that ceiling; changing the current new-import admission bound
never invalidates acknowledged legacy bytes. Startup fails closed until no
legacy row remains, after which all reads use the blob reference and the
nullable legacy column contains no bytes. New imports write only blob
references.

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

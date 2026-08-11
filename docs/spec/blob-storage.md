# Blob storage

This page was introduced and verified through PR #553
(`agent/blob-storage-foundation`); it is the foundation proposal at the bottom
of that implementing stack. The implementing child pull requests make these
paragraphs current behavior before the stack merges; a section that names
itself unimplemented at merge time is committed unimplemented functionality
and carries only its stated compatibility constraint.

It owns one thing: how Signalbox stores, identifies, references, and reads
immutable binary content — blob identity, the durable replica catalog, store
configuration and routing, the ingest and read lifecycle, the blob wire
vocabulary, multipart user content with attachment parts, and what a model is
shown when accepted content carries an attachment. The session aggregate and
transcript projections are owned by
[sessions-and-transcript](sessions-and-transcript.md); command payload storage
and replay equality by [identity-and-commands](identity-and-commands.md); the
relational baseline and migration discipline by
[persistence-protocol](persistence-protocol.md); framing and the client
request vocabulary by [process-protocol](process-protocol.md); the Layer-1
runtime boundary by [runtime-substrate](runtime-substrate.md); model-call
preparation and authorization by
[model-call-execution](model-call-execution.md); the configuration catalog and
credential delivery by
[configuration-and-credentials](configuration-and-credentials.md); imported
source records by [conversation-import](conversation-import.md); tool result
authority by [tool-loop](tool-loop.md).

## Identity and the blob value

A blob is an immutable, nonempty byte sequence identified by the SHA-256 of
exactly those bytes. The domain value is a 32-byte digest newtype in the shape
of the existing digest family (`ImportedRawRecordHash` and kin); the external
spelling is `sha256:` followed by 64 lowercase hexadecimal characters. Why:
the tag lives in the spelling, where format evolution actually happens, while
the domain algebra stays as small as every other digest the repository
carries.

The digest covers raw bytes only. Filename, declared media type, purpose,
producing session, and storage placement are properties of a use of the blob —
an attachment part, a future tool artifact record — never of the blob itself.
One byte sequence uploaded under two names, or produced independently by two
sessions, is one blob. Blob identity is global to the installation; the
single-user authorization model makes global deduplication safe, and any
future multi-principal boundary must revisit that assumption before sharing
the namespace.

This substrate is general: user attachments, tool artifacts, imported source
material, and generated assets are all uses of the same immutable-byte layer.
Nothing in it may depend on which use referenced a blob first.

## The replica catalog

PostgreSQL is canonical for which blobs exist and where their bytes durably
live. A `blob` row records the digest, its byte length, and creation time; a
`blob_replica` row records that one named store durably holds one verified
object for that digest under one recorded object key, and is inserted only
after that store's publication has been verified. Deleting a blob row that a
replica references is rejected, and no surface in this stack deletes either
row: the catalog is append-only.

Placement is durable fact, not configuration lookup. Routing configuration
decides where new writes go; reads resolve through recorded replicas, so a
configuration change never reinterprets or orphans existing content. Store
names are durable deployment identities — a name the catalog references must
keep meaning the same storage namespace until every replica it holds has been
migrated by adding replicas elsewhere. Why: the alternative — deriving
location from current configuration — silently changes the meaning of every
old durable record on each configuration edit.

Object keys are deterministic and content-derived (`sha256/ab/cd/<hex>`),
carry no filename, extension, or session identity, and are recorded per
replica so a store's key layout can evolve without reinterpreting history.

## Stores, routing, and configuration

The daemon configuration catalog gains a `[blob_storage]` table: a staging
directory, a deployment-configurable stored-size ceiling,
`[[blob_storage.stores]]` entries — each a validated unique name plus a kind —
and a `[blob_storage.routes]` table mapping each semantic blob class
(`user_attachment`, `tool_artifact`, `imported_source`, `generated_artifact`)
to a store name. It follows the catalog's grammar: versioned, unknown fields
rejected, named entries as arrays of tables.

Version one ships two store kinds. `filesystem` is a production-supported
store — including over network mounts — writing through a same-filesystem
temporary file with an atomic rename so a final content-addressed path is
never partially visible. `s3` speaks the S3-compatible API against an
explicit endpoint with explicit file-delivered static credentials; ambient
credential discovery (process environment, provider configuration files,
instance metadata) is rejected by construction, and an object store's own
integrity metadata is never treated as content identity. Multiple stores are
enabled simultaneously and routed by class; routing by media type or filename
is inexpressible. Why: class is a classification Signalbox itself made, while
media type and filename are caller-supplied strings, and a caller-supplied
string must not select which infrastructure gains authority over bytes.

Blobs are large: the substrate supports multi-gigabyte objects, so every
daemon path — ingest, verification, replica copy, read — streams and none
materializes a whole blob in memory. Bounded in-memory materialization exists
only at explicitly bounded consumers, and each such consumer names its bound.

## Ingest and the transaction boundary

Ingest streams caller bytes to a staging file while hashing and counting,
enforces the stored-size ceiling, verifies the caller's expected digest and
length, publishes the object to the routed store, verifies publication, and
only then records `blob` and `blob_replica` rows in one PostgreSQL
transaction. No database transaction is ever open across store input/output.
A crash between publication and registration leaves an unregistered orphan
object — harmless, rediscovered by re-ingest, never the reverse: an
acknowledged reference always has verified durable bytes behind it. Because
the key is the digest, retrying an ambiguous store outcome is idempotent:
read back the final key, verify, and finish registration. Ingesting a digest
the catalog already knows verifies and reports the existing identity; if its
class routes to a store with no replica, ingest publishes the additional
replica rather than minting a second identity.

## Wire vocabulary

Blob upload copies the chunked conversation-import lifecycle over the local
process protocol: `begin_blob_upload` declares the expected digest, expected
byte length, and blob class, and short-circuits with an already-present
response when the catalog knows the digest; `append_blob_upload` carries
bounded base64 chunks under the import lifecycle's half-frame bound, spooled
to staging and never assembled in memory; `commit_blob_upload` returns the
verified digest and length; `abort_blob_upload` discards the staging state.
Reads are `read_blob_metadata`, returning length and catalog facts for a
digest, and `read_blob_chunk`, returning a bounded byte range so clients can
render and download attachments. Bytes flow only through the daemon: no
client receives a store credential, bucket name, filesystem path, or
presigned URL, and durable records carry only the digest spelling.

## Multipart user content

`UserContent` becomes an ordered, nonempty sequence of parts, each either
exact text (the existing checked text value) or an attachment: a blob digest
plus a closed attachment kind (`image`, `document`, `file`), a declared media
type, and an optional bounded display filename admitted as a basename and
redacted in logs like other content-bearing values. There is exactly one
canonical representation — single-text content is a one-part sequence, and no
second spelling of equivalent content exists. Why: the durable command reuse
check compares caller-supplied payloads structurally, and two spellings of
one meaning would turn equal resubmission into conflicting reuse.

Attachment metadata is caller-supplied semantic input, so part order,
digests, kinds, media types, and filenames all participate in command replay
equality. Acceptance requires every referenced digest to be catalogued with
at least one verified replica; content that references unknown bytes is
rejected before any durable command claim. Command and accepted-input rows
carry mirrored ordered content-part satellites under the existing
command/effect correlation discipline, and the wire `submit_input` content
becomes the same ordered parts array. The process protocol's version-one
in-place editing window is why this lands as the canonical shape rather than
a compatibility variant beside the string form.

## Attachment visibility and model reads

Transcript presence is distinct from model-context inclusion. When a frontier
renders an accepted input whose content carries attachments, the model sees
each attachment as a bounded textual stub — kind, media type, filename when
present, byte length, and digest — never the bytes. No provider call
automatically materializes attachment content, whatever its size. Why: a
durable attachment may be orders of magnitude larger than any context window,
and silently replaying large media into every subsequent call converts one
upload into an unbounded per-turn cost.

Models reach attachment content the same way they reach every other effect:
through tools, explicitly, within declared bounds. This stack ships a
daemon-registered blob-read tool family over the catalog — bounded ranged
reads with the stub's stated length as the model's sizing information — with
per-read and per-turn byte bounds that compose with the target model's
context capacity. Content-type-aware readers — structured walks over
markdown/JSON/YAML/TOML, page-rendering for paginated document formats,
downscaled raster views for large images handed to vision-capable targets —
are committed unimplemented functionality: no present surface provides them,
and the constraint they impose now is that attachment stubs and the read
family must stay sufficient to host them without re-deciding visibility.

Any decoder, parser, or renderer that interprets attachment bytes (document
rendering, image scaling, structured-format walking) executes inside strong
process isolation, and its input validation is deliberately best-effort.
Why: parser hardening is an unending surface — the containment boundary is
the sandbox, so a malicious payload exploiting a decoder defect is contained
by isolation rather than prevented by an ever-growing validator, and review
of these children should reject validator accretion in favor of isolation
strength.

## Model-call preparation and modalities

Model capability records gain an input-modality axis (text, image, document)
on the same closed-set shape as the existing capability axes, declared per
target and projected to clients like the rest of the capability record. A
prepared model call whose rendered messages would carry media a target cannot
consume, or whose referenced blob is missing or fails digest verification at
materialization, fails preparation with a typed error before durable
authorization — attachment content is never silently dropped from a call.
In version one, rendered user-role messages carry only text and attachment
stubs, so the modality gate binds where media actually enters a call: the
bounded read family's media-bearing results, and any later rich result
content.

## Import convergence

Imported raw source records are the substrate's proof producer: their
existing global SHA-256 deduplication converges onto the blob catalog, with
the import satellite's content hash becoming an ordinary blob reference and
the stored bytes living in a routed store rather than a relational column.
Import semantics — record identity, conversion digests, snapshot immutability
— are unchanged and remain owned by
[conversation-import](conversation-import.md).

## Implementation stack

The child slices, in review order: the substrate (digest value, catalog
tables and repository, store contract with filesystem kind, configuration,
conformance suite shared by every store kind); the wire lifecycle (upload and
read operations plus terminal-client commands); multipart user content
(domain algebra, mirrored satellites, replay equality, wire parts, stub
rendering); the S3 store kind over the same conformance suite; the blob-read
tool family with its bounds; import convergence. Each slice is independently
testable against the substrate contract: identical bytes yield one identity,
routed stores satisfy one conformance suite, configuration change moves only
new writes, and a registration failure after publication leaves an orphan and
never a dangling reference.

## Open edges

- Retention, purge, a future marked-deleted state, and garbage collection are
  deferred with
  [session organization, visibility, and retention](../open-questions.md#session-organization-visibility-and-retention)
  and the artifact lifecycle bullets in
  [general-purpose artifacts](../open-questions.md#general-purpose-artifacts);
  this page's append-only catalog is the constraint they design against.
- The content-type-aware read-tool inventory and the isolation substrate its
  processors run in are recorded in
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
- Remote client access to blob content rides the open remote-transport
  decisions in
  [protocols and persistence](../open-questions.md#protocols-and-persistence);
  presigned or direct store access is inexpressible until they are decided.

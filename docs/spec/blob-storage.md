# Blob storage

Blob storage keeps immutable byte content under its SHA-256 digest, records
which stores hold each blob, and delivers verified bytes to clients, browsers,
and models.

## Overview

This page owns blob identity, the durable replica catalog, store configuration
and routing, the ingest and read lifecycle, the blob wire vocabulary, multipart
user content with attachment parts, and what a model sees when accepted content
carries an attachment. A blob is an immutable, nonempty byte sequence identified
by the SHA-256 of exactly those bytes. `BlobDigest` is the domain value, and its
external spelling is the hexadecimal digest after a `sha256:` prefix.

PostgreSQL holds the catalog: one `blob` row per digest, one `blob_replica` row
for every store that holds a verified copy, and one `blob_store_binding` row
tying each store name to its namespace UUID. `BlobCatalogRepository` writes
those rows and no surface deletes them. Routing configuration decides only where
a new write goes; the catalog is the record of where bytes are.

The daemon configuration carries an optional `[blob_storage]` table. It names a
staging directory, a maximum blob size, the stores, and one route for every
storage class; each store has a name, a namespace UUID, and a kind, either
`filesystem` or `s3`. Every store holds a namespace marker: a filesystem root
holds a private `.signalbox-blob-namespace-v1` file, an S3 bucket holds an
object of the same name, and in both the complete content is the configured
namespace UUID followed by one line feed. At startup `BlobStoreRegistry` checks
the configuration against the recorded bindings, verifies the marker of every
filesystem store and every routed S3 store, and hands out the adapters, which
implement the `BlobStore` trait. An S3 binding that no route names verifies its
marker on first use instead.

Ingest arrives over the process protocol as a chunked upload. The daemon spools
the bytes to the staging directory while hashing them, publishes the object to
the store the route selects, verifies the published object, then records the
catalog rows. Reads reach the daemon over the process protocol, over the
same-origin HTTP surface the browser client uses, and through the blob-read tool
family a model calls. All three serve content through one runtime that walks the
recorded replicas in order and returns a verified byte range; a metadata read
answers from the catalog and opens no store. The request and response shapes
live in `crates/process-protocol` and on
[process-protocol](process-protocol.md).

The browser client asks for a descriptor of one use of a blob. The descriptor
lists the views the server admits: download, a browser-native view for common
image formats, and thumbnail and preview views that an isolated worker produces
on demand. Each production is recorded as a `BlobDerivation`, an immutable
relation from input digests to output digests that names its producer.

Accepted user content is `UserContent`, an ordered sequence of parts; each part
is exact text or an attachment naming a blob digest with a kind, a media type,
and an optional filename. Transcript presence is distinct from model-context
inclusion: the transcript and the terminal show an attachment as metadata, the
model sees a textual stub, and the model reaches the bytes only through the
blob-read tools. Before a model call is authorized to send, attachment
preparation verifies a replica of every attachment the rendered request names.

## Design decisions

Blob identity is global to the installation, so one byte sequence is one blob
whoever uploads it. Why: the single-user authorization model makes global
deduplication safe, and a multi-principal boundary must revisit this before
sharing the namespace.

A store's durable identity is its name plus its namespace UUID, never its
current locator. Why: identity derived from the locator would silently change
the meaning of every old durable record on each configuration edit.

The namespace marker lives inside the store rather than in the daemon's
configuration or database. Why: path and device identity are only locally
unique, so without the marker an absent mount would be admitted as the recorded
namespace, and two locator strings that alias one bucket would be admitted as
two replicas instead of disagreeing on their namespace UUIDs.

There is no replica-retirement state, so a configured binding cannot be removed
while any `blob_replica` row names it, even after another replica exists
elsewhere.

A `filesystem` store is admitted only on storage the host positively classifies
as local, non-network, and non-userspace; network, userspace, and unclassified
mounts fail startup. Why: a remote filesystem operation cannot be interrupted or
bounded from inside the daemon.

Several stores are enabled at once and routed by storage class; routing by media
type or filename is inexpressible. Why: the daemon assigns the class, and a
caller-supplied string must not select which infrastructure gains authority over
bytes.

S3 credentials come only from the configured credentials file; no environment
variable, provider profile, metadata service, or other ambient source supplies
them.

Object keys are derived from the digest, so retrying an ambiguous store outcome
is idempotent: the daemon reads back the final key, verifies it, and finishes
registration.

The browser renderer loads the preview view, then the thumbnail view, in
capability order, and loads a browser-native original only after an explicit
action.

Content responses send an exact content length, advertise byte ranges, forbid
media-type sniffing, and use the quoted digest as the ETag with immutable
year-long caching. Why: the bytes behind a digest never change, so an immutable
cache lifetime is safe.

By default the terminal client escapes DEL and every C1 control code point
inside the part JSON it prints, so the ordered part values stay parseable while
text and filenames cannot forge another terminal line or execute terminal
controls. `--raw-output` is the explicit opt-in that leaves those characters
literal.

The terminal transcript, follow, and chat views show attachment metadata and
interleaving but never render blob bytes, in either the default or the raw
output mode.

The attachment stub shown to the model is compact JSON, because ordinary JSON
string escaping makes caller-supplied metadata data rather than stub syntax.

A prepared model call carries only text, attachment stubs, and text-only
blob-read results; no provider call materializes attachment bytes, whatever
their size, and a blob-read result never enters a provider message as image or
document media. Why: a durable attachment may exceed any context window, and
replaying media into every call converts one upload into an unbounded per-turn
cost.

Models reach attachment content the way they reach every other effect: through
tools, explicitly, within declared bounds.

The per-turn cap on blob-read requests exists alongside the byte budget because
it bounds complete replica reverification work even when the model repeatedly
requests a tiny range from a store without generation-pinned reuse.

## Boundary contracts

The database records which stores hold each blob; a read uses those records, not
configuration, to find the blob. A replica row is written only after the upload
was verified. Every content read checks the length and hash of the bytes against
the recorded replica row before it returns them. Clients never receive a store
credential, bucket name, path, or presigned URL. The daemon relays every blob
byte between a client and a store.

The digest covers raw bytes only. Filename, media type, purpose, producing
session, and placement are properties of a use of the blob, never of the blob.
User attachments, tool artifacts, imported source, and generated assets are uses
of one immutable-byte layer, and nothing depends on which use referenced a blob
first.

Startup compares every recorded store binding with the configuration and fails
before socket work when a store name or namespace UUID is absent or disagrees.
Moving one namespace to another locator preserves its UUID; assigning a locator
to another namespace requires a fresh store name and UUID.

Startup succeeds without the `[blob_storage]` table only while the blob catalog
is empty. With the table absent, blob and conversation-import operations are
unavailable rather than inventing a storage location.

Configured stores name distinct physical namespaces. Startup compares each
filesystem root's canonical path, device and inode identity, and mount ancestry
and rejects equality or overlap. For S3 the locator is the canonical endpoint
URL plus the exact bucket, and a duplicate locator is rejected even when names,
UUIDs, regions, or credentials differ.

Every catalog query, in-memory traversal, and replica walk orders store names by
unsigned ASCII bytes; persistence uses bytewise `C` collation for that order,
not a deployment locale.

The S3 credentials file passes the regular-file, ownership, and mode checks of
[configuration-and-credentials](configuration-and-credentials.md) and is read
once per logical store operation, so rotating it needs no restart.

A failed durability or verification operation makes the store unavailable and
records no replica, and a timeout after a publication that might have been
accepted is not success. An object store's own integrity metadata is never
treated as content identity.

A routed S3 store is admitted for publication only when its bucket lifecycle has
an enabled rule covering the `sha256/` prefix that aborts incomplete multipart
uploads after one day; that rule is the bound on parts orphaned by a crash or
credential loss. An S3 binding that no route names is not lifecycle-queried at
startup, and its read failures use the ordinary unavailable candidate outcome.

All namespace-marker and lifecycle operations for routed S3 stores share one
five-minute monotonic startup deadline; changing stores or probe kinds never
restarts it.

Blobs may be multiple gigabytes, so every daemon path streams; bounded in-memory
materialization exists only at consumers that name their bound.

One direct read or one attachment-preparation pass has one 24-hour monotonic
aggregate deadline across its whole ordered traversal of digests and replica
candidates; moving to another candidate never restarts it. Direct blob reads and
checked imported-aggregate loads share a process-wide bound of 16 active
traversals, and a request that cannot acquire one returns unavailable at once.
How a model-originated read or a preparation pass hands off its scheduler slot
around store I/O is owned by
[turn-lifecycle-and-scheduling](turn-lifecycle-and-scheduling.md).

A model-call attachment check binds its cancellation to the call's authoritative
cancellation; upload work binds cancellation to connection loss and daemon
shutdown. Authoritative cancellation aborts store I/O without relabeling the
cancellation as an attachment failure.

Ingest publishes and verifies the object before it records the catalog rows,
with no database transaction open across store I/O, as
[persistence-protocol](persistence-protocol.md) requires. A crash between
publication and registration leaves an unregistered orphan object, which is
catalog-safe but not capacity-free; re-ingest rediscovers it, and an
acknowledged reference always has verified durable bytes behind it. Ingest
short-circuits only when the catalogued identity has a live-verified replica in
the store the current use routes to. A missing or corrupt object behind an
existing routed replica record accepts the upload and atomically replaces that
object only after the staged source verifies; otherwise ingest publishes and
registers an additional replica in the routed store rather than creating a
second identity. The daemon derives the storage class from the fixed request
kind; no operation or client field selects a route.

A read whose range starts at or beyond the end of the blob, or crosses it, is a
typed range rejection, never a short or empty read. When every candidate fails,
any unavailable candidate makes the result `unavailable`; otherwise any length
or digest mismatch makes it `blob_corrupt`, an all-missing set makes it
`blob_missing`, and an absent catalog identity is `not_found`. A retained
filesystem handle pins an inode but not its contents, so filesystem replicas are
completely reverified before every range.

Client-facing blob messages and content-part references expose only the digest
spelling, never placement; the catalog rows alone retain byte length, creation
time, store name, and object key.

A descriptor repeats the canonical digest and catalogued byte length and lists
only the views the server admits; the browser selects a renderer from a view's
capability kind and never derives a capability from the media type. Image uses
receive a browser-native view whose URL copies no caller-controlled media type.
Thumbnail and preview views exist only after their exact output is present and
carry their complete derivation record.

A recorded derivation key is reused without running the producer only while its
output is still retrievable. Missing or unverifiable output re-runs the producer
and republishes the same output digests under the unchanged key, so the append
resolves back to the existing record and writes no second one. Publication to
the generated-artifact route and catalog registration precede the derivation
append.

The thumbnail and preview producer runs the current daemon executable through
the configured filesystem-confined supervisor with no network and fixed deadline
and concurrency bounds, and the digest of that executable is its implementation
provenance. This worker and every content-interpreting reader of
[file-and-media](file-and-media.md) run inside strong process isolation and
treat input validation as defense in depth, because parser hardening is never
complete and a payload that exploits a decoder defect must be contained by
isolation.

User content has exactly one canonical representation: single-text content is a
one-part sequence, and no second spelling of equivalent content exists. The
durable command reuse check compares payloads structurally, so a second spelling
would turn equal resubmission into conflicting reuse. Attachment metadata is
caller-supplied semantic input, so part order, digests, kinds, media types, and
filenames all participate in command replay equality.

Acceptance requires every referenced digest to be catalogued with at least one
verified replica. The sum of catalogued byte lengths over the distinct digests
in one input must not exceed `blob_storage.max_blob_bytes`. The same checked sum
is applied to the complete prospective rendered frontier after the new content
and its delivery transition, and the acceptance transaction recomputes the
eventual frontier of every already-queued input whose predecessor can change, in
canonical queue order, and bounds each result before any accepted-input effect.
Catalog existence and these sums are current-state validation, so an unseen
command identifier is claimed first under the command protocol of
[identity-and-commands](identity-and-commands.md). An input and frontier with no
attachment digest have both sums zero and touch no blob configuration, catalog,
or store, so text-only submission works with `[blob_storage]` omitted.

Command-side and accepted-side parts are separate mirrored records, never shared
mutable authority. The terminal client renders one accepted user entry as
exactly one line ending in `parts=<json>`, the canonical compact ordered parts
array with its fixed member order.

A rendered accepted input shows the model each attachment as a bounded textual
stub naming kind, media type, filename, byte length, and digest, never the
bytes. At preparation the daemon derives an allow-set from the attachment stubs
in the rendered frontier; a catalogued digest outside that set is unauthorized.
A digest absent from the frontier, a turn byte reservation past 2,097,152, or a
turn read reservation past 64 closes the prepared attempt as a known failure
with an exact fixed detail. Both durable counters charge once by tool-request
identity before authorization, and replay never charges twice.

Before a prepared call crosses durable send authorization, preparation streams
and verifies the length and SHA-256 of at least one recorded replica for every
distinct attachment in the rendered request. The request is first checked-summed
over the catalogued lengths of its distinct digests, and an oversized sum closes
the unsent call as too large before any store I/O or send authorization. A
digest with no recorded replica, or one whose every candidate reads but fails
verification, closes the unsent call, and neither path permits provider
interaction or tool authorization. When no candidate verifies and one remains
temporarily unavailable, preparation leaves the call prepared, records no turn
outcome, and returns a sanitized unavailable failure so a later pass can retry
the same call.

Imported raw source records of [conversation-import](conversation-import.md)
converge onto the blob catalog: the import satellite's content hash is an
ordinary blob reference and the bytes live in a routed store.

## Planned

- A `program_journal` storage class for over-threshold program journal payloads;
  see [blob storage design](../design/blob-storage.md).
- Generation-pinned verification reuse across ranges and the connection- or
  turn-scoped verification inventory, seeded by attachment preparation; see
  [blob storage design](../design/blob-storage.md).
- Transcript projections that carry blob descriptors and URLs; see
  [blob storage design](../design/blob-storage.md).
- A modality-unsupported attachment preparation failure for typed media results;
  see [blob storage design](../design/blob-storage.md).

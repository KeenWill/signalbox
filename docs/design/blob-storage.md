# Blob storage design

This design is not built; it extends [blob-storage](../spec/blob-storage.md)
with four committed capabilities.

## Goal

A `program_journal` storage class lets the program substrate's host store
over-threshold journal payloads in a routed store and reference them from the
catalog. A generation-pinned verification inventory lets one read scope verify a
blob once and serve later ranges without a full reverification, and attachment
preparation seeds that inventory for the turn. Transcript projections carry blob
descriptors and URLs so a browser can render attachments from the transcript
alone. A modality-unsupported preparation failure closes a call whose rendered
request carries media its target cannot accept.

## Design

The routing-class vocabulary gains `program_journal`, and the routes table
requires it alongside the four present classes. The daemon derives the class
from the writing surface, the journal write of the
[program-substrate](../spec/program-substrate.md) host whose payload exceeds the
journal threshold, exactly as every class is derived; no operation or client
field selects it. Unknown classes are still rejected as a closed set.

A read scope is one process-protocol connection or one model turn; a turn's
attachment-preparation passes and blob-read tool calls share that one scope. A
connection's scope spans every blob read on that connection, so a later range
request reuses an earlier request's verification. The same-origin HTTP surface
has no scope, and every content request verifies in full. Each scope owns a
least-recently-used inventory of at most eight generation-pinned verifications
keyed by digest, store name, and object key, so a token applies only to the
replica whose verification produced it. An entry is a bounded
immutable-generation token of at most 1,024 bytes; it never retains a file
descriptor, socket, request, or store client, and each conditional read acquires
and releases its own resources. Eviction makes a later range verify again, and
the whole inventory is discarded at scope end or on any candidate failure. A
range read that reuses an entry targets that entry's replica and is conditional
on the exact generation the token names; a failed condition discards the entry
and reverifies the replica in full. The `BlobStore` trait returns a generation
token from a full verification only when the adapter can pin an immutable object
generation and make later ranges conditional on it; the filesystem adapter never
can, and the present S3 adapter returns none. Attachment preparation records
into the turn's inventory any generation token a replica verification returns,
so a blob-read tool call in that turn reuses that verification.

Transcript data transfer objects in `crates/web-contract` carry, for each
attachment part, the blob descriptor the same-origin HTTP surface would return:
the canonical digest, the catalogued byte length, and the admitted views with
their URLs. The projection never embeds blob bytes.

`AttachmentPreparationFailure` gains a modality-unsupported variant. When a
rendered request contains a typed media result, an image or document result arm
rather than a text part or an attachment stub, and the selected model or serving
target's input modalities from
[configuration-and-credentials](../spec/configuration-and-credentials.md) do not
include that modality, preparation closes the unsent call with that variant
before durable send authorization. The call records the failure as a terminal
preparation outcome; media is never dropped from the request to make it
sendable.

## Compatibility constraints

The class vocabulary and route-validation surface stay extensible to one added
class without loosening the closed-set rejection of unknown classes.

Verification is reused for a later range only while an adapter pins an immutable
object generation and makes every range conditional on that exact generation;
without such a token every range reverifies in full, which is the present
behavior for both adapters.

Any verification cache is bounded in entry count and entry size and holds no
handle, socket, request, or store client.

Transcript projections never embed blob bytes.

A typed media result fails preparation before durable authorization when its
target lacks that modality; no present or future surface silently drops media
from a prepared call.

## Acceptance criteria

A routes table naming `program_journal` parses, a routes table missing it fails
startup, and a table naming an unknown class still fails startup. An
over-threshold journal payload written by the program host lands in the store
`program_journal` routes to and is catalogued with a verified replica.

With an adapter that pins a generation, a second range request for a digest on
one connection reads the verified replica and performs no full reverification, a
changed object generation fails the conditional read and forces a full
reverification, the inventory never holds more than eight entries, and it is
empty after scope end and after any candidate failure. With the filesystem
adapter every range still reverifies in full. After attachment preparation
verifies a replica and receives a generation token, a blob-read tool call in the
same turn reuses that verification.

A transcript projection of an accepted input with attachments carries a
descriptor and view URLs for each attachment part and contains no blob bytes.

A prepared request carrying a typed media result against a text-only target
closes the call with the modality-unsupported failure before send authorization,
records that failure, and sends no provider request.

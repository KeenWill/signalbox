# File and media interpretation

The file and media layer gives a model typed, bounded views of attached blob
bytes; every parser runs in an isolated worker process outside the daemon.

## Overview

The layer sits above [blob storage](blob-storage.md), which owns blob identity,
catalog placement, replica verification, raw reads, attachment visibility, and
generated-artifact ingest. This layer owns everything typed: which reader a
blob's bytes belong to, which views that reader offers, and the bounded text or
structure a view returns. Its two agent tools are declared through the
[tool loop](tool-loop.md).

The same bytes have three descriptions. A blob digest names immutable bytes and
carries no type fact. A file use is a caller's declaration about one use of
those bytes: their length, the attachment intent, and a media type. A validated
file is one reader's byte-derived evidence about those bytes, together with the
ordered views the reader offers for them.

`signalbox-file-media-runtime` is the provider-neutral core: the checked
declaration and value types, the registry, the detection and validation
algorithm, and the untrusted-processor boundary. It depends on no domain,
application, persistence, daemon, parser, image, audio, or provider crate. The
daemon-side `FileMediaRegistry` holds checked reader declarations and calls only
the `FileMediaProcessor` port; it never runs adapter code in its own process.
Inspection probes every registered reader, resolves the claims to one type and
reader, and validates through that reader. It ends as a validated file, as
unknown bytes with no views, or as a typed failure. A read selects one declared
view and returns bounded UTF-8 or JSON with a completeness or continuation fact.

Archive ZIP parsing bounds cumulative bytes visited, including rescans and
nested-content detection; exhausted scan work is a limit failure.

Each format family is one adapter crate implementing `FileMediaProvider`,
compiled into its own worker executable.
`signalbox-file-media-processor-runtime` implements the processor port by
launching a worker for each operation. The worker holds no source; it asks the
daemon for byte ranges, which the daemon checks against the declared envelope
before serving them from a `VerifiedBlobSource`. A worker's response is
untrusted until the registry has reparsed and cross-checked it.

With `file_media = true` and blob storage configured, the daemon registers
`file_inspect` and `file_read` as external-effect tools. Startup verifies the
compiled text, image and PDF workers beside the daemon executable through
`/usr/bin/bwrap` and the delegated `SIGNALBOX_FILE_MEDIA_CGROUP_ROOT`. The
resolver reuses `blob_read`'s projected-frontier attachment proof and completes
catalog work before source or worker I/O. A digest outside that frontier is
unauthorized; a repeated digest requires its rendered selector, the semantic
entry identity and zero-based part ordinal. The registry recognizes no format in
the daemon. Authorization and catalog repository failures retain their operator
failure class through tool execution. Verified-source integrity violations take
the fail-closed operator path.

`file_read` takes an exact provider-owned view and either object options or an
authenticated restart-ephemeral continuation. Continuations bind the original
digest, selector, reader, view, options, and provider section state; each page
reauthorizes the selected use against its issuing frontier and repeats
inspection. Text inspection validates a bounded prefix, and text reads return
bounded UTF-8 sections without splitting scalars. JSON and CSV views retain
bounded structured results and reject sources outside their whole-decode
envelopes. Sources are range capabilities; their total length never sizes a
materialization. Source range reads reject ranges extending beyond the
catalogued length, including offset-plus-length overflow, before store access.

PDF preflight charges recursive length-carrier decoding against the aggregate
object-stream budget. Text reads charge page content and font CMaps against one
read-wide decoding budget and accept bounded indirect content arrays.

## Design decisions

View names and their meanings are provider-owned; the core fixes only the closed
output kind. Why: adding a format adds an adapter and a declaration, not a
central vocabulary change.

Format adapters add no MIME branch to the tool executor, the bridge, or the
daemon, for the same reason.

Registry construction admits image views with a declared direct or generated
kind and a finite set of output media types. Audio and general-file views are
rejected.

An empty registry is valid, so the daemon boots with no adapters.

Configuration can disable a provider or lower a bound; it cannot add a
media-type mapping, an alias, an executable, or a precedence rule. Why:
configuration must never become a source of type authority or executable code.

The daemon derives probe byte counts from brokered reads. The strongest probe
candidate is validated; equal-strength candidates and unsuccessful validation
return unknown bytes. Registration order never settles probe claims.

The service repeats inspection for every read, and `file_read` accepts no
model-supplied media type or reader identity. Why: no classification from an
earlier call is trusted. A registered streaming-text reader is selected through
bounded prefix validation even for declared `text/plain`. JSON container-entry
ceilings are enforced while parsing, before constructing an excessive tree.
Image metadata views use the canonical metadata from that inspection; absent
image fields fail without a second decode.

Validation and read requests carry effective `maximum_image_axis` and
`maximum_decoded_image_pixels` ceilings. Image decoding clamps both to the
compiled maxima.

The raw processor output types carry strings and JSON text rather than checked
registry values, and the registry reparses and cross-checks every claim against
the declaration it invoked before admitting it. Why: a worker is untrusted.

The processor runtime starts one fresh local process for every probe,
validation, or read. Why: a compromised worker cannot carry bytes or state into
another request.

On Linux the processor launches the exact worker through bubblewrap.

A worker receives one digest and length and the byte ranges it requests. It
never receives a store locator, a source path, the catalog, a database
connection or open transaction, the daemon socket, configuration, a credential,
a home directory, or a network namespace.

Image output bytes use a separate bounded binary channel on worker stderr; other
stderr is drained and discarded. Diagnostics are never parser evidence,
telemetry content, or model-visible output.

No adapter renders, executes active content, follows links, extracts embedded
files, fetches external resources, or recurses into embedded containers.
Recognized encrypted or locked content is a terminal outcome, and no password
channel exists.

A reader revision is immutable. An earlier durable tool result keeps what the
model saw while a later request may use a newer revision. Why: a durable result
is never reclassified.

## Boundary contracts

Blob identity, catalog records, replica verification, and byte relay follow the
contract on [blob storage](blob-storage.md). Both file tools are tool executors
under the contract on [tool loop](tool-loop.md).

A processor response that is oversized, malformed, injection-shaped, from
another reader, carries an unregistered reason code, has the wrong output kind,
contradicts its own continuation, or nests too deep collapses to one sanitized
processor failure with no partial success. The registry sanitizer in
`FileMediaRegistry` enforces this for the tested cases; the rule binds every
future adapter.

The daemon owns three deadlines: one wall deadline for each worker invocation,
one across all serial reader probes of an inspection, and one across the
isolation probes of every configured worker. No test covers the set.

Archive validation fits the effective source-byte and range ceilings; entry
decoding uses the remaining aggregate expansion allowance, with one byte to
detect exhaustion.

A single probe candidate above the effective validation envelope may return a
typed malformed result from bounded validation, but cannot become a validated
file.

A stored source may be larger than a view's envelope. A streaming view requests
it in bounded frames within its declared source work; a whole-decode view may
reject it without changing the blob.

A continuation position is semantic, such as a page, row, section, frame, or
time span, never a parser offset. The cursor sanitizer checks only bounds, so
this binds every adapter.

A recognized malformed file receives no permissive parser recovery unless that
recovery is registered as part of the reader's validation.

## Planned

- Audio and general-file views, whose derived bytes publish and register before
  the read's result commits and leave no dangling result on failure. See the
  [design](../design/file-and-media.md).

## Image presentation

PNG, JPEG and WebP readers offer direct image, downscale/compress and crop
views. Inspection reads at most a 64 KiB prefix. A presentation read validates
the raster through bounded source ranges inside the worker. Crop coordinates
`x`, `y`, `width`, `height` address the full-resolution source; optional `scale`
is in `(0, 1]`. Downscale preserves aspect ratio and never upscales. Both
transformations produce deterministic PNG bytes under the decoded-pixel and
allocation ceilings.

The reference result retains distinct presented and source digests, canonical
types, validating reader revisions and content-silent evidence. Direct image
presentation requires equal identities and an encoded source fitting the view,
process and target bounds. An oversized direct image returns dimensions, source
bytes and available derived views as bounded structured data.

Generated output is limited to the view's declared type set and eight MiB. The
registry computes its identity and independently detects and fully validates its
bytes through the ordinary sandboxed worker before publication. It rejects an
ambiguous result, wrong type, wrong output reader or exceeded bound. Valid
output publishes and verifies, registers as a generated artifact, then commits
its durable tool result. A failure before commit leaves no rich result; an
unreferenced blob may remain after publication. Direct reads publish and
register nothing.

Codex CLI presents image parts through `turn/start` RPC inputs; Claude Code CLI
uses base64 image blocks in stream-JSON input. Adapters encode authenticated
bytes without format detection. Their capability records bound accepted types,
one image and the complete encoded request. Daemon
`max_image_presentation_bytes` and `max_image_request_bytes` lower these limits;
`"none"` leaves the adapter limits. The process admits at most 16 image
references, eight MiB each and 32 MiB in aggregate. Encoding and request framing
count against the request bound.

Preparation authenticates each rendered reference against its terminal tool
attempt and catalog length before source I/O. It materializes only admitted
bounded images and runs no reader. Unsupported presentations fail before send
authorization. Database unavailability retains infrastructure failure, absent
replicas retain missing-blob failure, and inconsistent authority or blob-store
integrity remains fail-closed corruption. Ordinary JSON cannot issue image
authority.

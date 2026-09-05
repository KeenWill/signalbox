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

Each format family is one adapter crate implementing `FileMediaProvider`,
compiled into its own worker executable.
`signalbox-file-media-processor-runtime` implements the processor port by
launching a worker for each operation. The worker holds no source; it asks the
daemon for byte ranges, which the daemon checks against the declared envelope
before serving them from a `VerifiedBlobSource`. A worker's response is
untrusted until the registry has reparsed and cross-checked it.

Two effect-free tool declarations are compiled, and no daemon catalog registers
either one. `file_inspect` takes a canonical digest and an optional selector for
a repeated visible use. `file_read` adds an exact provider-owned view and
exactly one input: object options for a first read, or the cursor a truncated
result returned. `signalbox-file-media-provider-runtime` supplies the
registry-backed service behind both and authorizes each request through an
injected `FileUseResolver`.

## Design decisions

View names and their meanings are provider-owned; the core fixes only the closed
output kind. Why: adding a format adds an adapter and a declaration, not a
central vocabulary change.

Format adapters add no MIME branch to the tool executor, the bridge, or the
daemon, for the same reason.

Registry construction rejects a view whose output kind is image, audio, or
general file. Why: no durable result path can carry such output.

An empty registry is valid, so the daemon boots with no adapters.

Configuration can disable a provider or lower a bound; it cannot add a
media-type mapping, an alias, an executable, or a precedence rule. Why:
configuration must never become a source of type authority or executable code.

Registration order never settles conflicting probe claims; incompatible claims
return ambiguity. Why: detection must give the same answer for any adapter set
and any probe completion order.

The service repeats inspection for every read, and `file_read` accepts no
model-supplied media type or reader identity. Why: no classification from an
earlier call is trusted.

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

Bounded worker stderr is drained and discarded; it is never parser evidence,
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

A stored source may be larger than a view's envelope. A streaming view requests
it in bounded frames within its declared source work; a whole-decode view may
reject it without changing the blob.

A continuation position is semantic, such as a page, row, section, frame, or
time span, never a parser offset. The cursor sanitizer checks only bounds, so
this binds every adapter.

A recognized malformed file receives no permissive parser recovery unless that
recovery is registered as part of the reader's validation.

## Planned

- Daemon composition of `file_inspect` and `file_read` behind a
  `FileUseResolver` that reuses the rendered-frontier visibility proof; the
  daemon registry recognizes no format. See the
  [design](../design/file-and-media.md).
- Image, audio, and general-file views, whose derived bytes publish and register
  before the read's result commits and leave no dangling result on failure. See
  the [design](../design/file-and-media.md).

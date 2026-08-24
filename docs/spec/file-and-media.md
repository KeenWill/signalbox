# File and media interpretation

The provider-neutral core is verified against PR #898 (`agent/file-media-core`).
Its isolated processor implementation is
verified against PR #900 (`agent/file-media-worker`). Together they include the
type model, declaration and registry checks, detection and validation algorithm,
untrusted processor-response boundary, stable agent tool contracts,
visibility-authorizing application bridge, and fresh daemon-supervised worker
runtime.

The text-family adapter coverage is verified against PR #903
(`agent/file-media-text-family`).

This page owns typed interpretation above immutable blob bytes. Blob identity,
catalog placement, replica verification, raw reads, attachment visibility, and
generated-artifact ingest remain owned by [blob storage](blob-storage.md). Tool
attempt authority and result durability remain owned by
[the tool loop](tool-loop.md).

## Frames and checked values

`FileDigest` is the provider-neutral spelling of the same exact SHA-256 bytes as
domain `BlobDigest`; it carries no type fact. `FileUse` carries one digest,
positive catalogued length, caller attachment intent, exact bounded declared
media type, and optional checked display basename. `ValidatedFile` adds only
byte-derived canonical media type, exact provider/reader/revision identity,
closed content-silent validation evidence, bounded canonical metadata, and a
nonempty ordered view inventory. Caller declaration and filename never select a
reader without byte validation.

Canonical media types are lowercase ASCII `type/subtype` essences with no
parameters. Provider, reader, view, and reason names are checked bounded
lowercase ASCII tokens. View argument schemas are bounded canonical JSON objects
whose root declares `type: object`; processor metadata is separately parsed and
bounded as a canonical JSON object before it reaches a tool result.

The common output vocabulary is closed as text, structure, image, audio, or
general file. The provider-neutral core admits only text and structured views;
rich view registration remains unavailable until the durable media-reference
result path lands. View names and meanings remain provider-owned. Every view
declares an object options schema, streaming or finite-range access posture,
cumulative source work, and output-specific finite bounds. Text and structured
views bound body bytes; structure also bounds depth, nodes, and strings. Image,
audio, and general-file views carry the corresponding dimension, pixel, channel,
sample, duration, and byte bounds.

## Registry and adapter boundary

`signalbox-file-media-runtime` depends on no domain, application, persistence,
daemon, parser, image, audio, or provider crate. A format-family adapter depends
on that runtime and implements `FileMediaProvider` inside the processor worker.
The daemon-side registry stores checked declarations and calls only
`FileMediaProcessor`; it never invokes adapter code in its own process.

Registry construction sorts unsigned-ASCII provider/reader identities and
rejects duplicate providers or readers, duplicate exact media-type claims,
duplicate per-reader types, views, or reason codes, absent or excessive probe
and output bounds, read source work or range fan-out above their compiled
lowerable ceilings, contradictory image bounds, ambiguous streaming-text
fallback, unavailable isolation when any provider is present, and any effective
ceiling above the compiled version-one value. An empty registry is valid.
Configuration can therefore disable providers or lower bounds but cannot add a
media-type mapping, alias, executable, or precedence rule.

An adapter author supplies one provider declaration with exact owned canonical
types, probe budget, view schemas and resource envelopes, registered sanitized
reason codes, and immutable reader revision. Probe, inspect, and read methods
receive only a placement-free `VerifiedBlobSource`, cooperative cancellation,
and their checked request. An adapter may report only adapter execution failure;
the daemon supervisor originates process availability, timeout, cancellation,
and framing failures. Validation requests carry effective lowerable source-byte
and exact-range ceilings for broker enforcement. They return raw processor
outputs: the registry reparses and cross-checks every type, evidence claim,
reason, metadata object, body, JSON tree, continuation, and bound before
admitting it. Structured output node and per-container entry ceilings stop
duplicate-aware deserialization before structural excess is materialized.

## Detection and validation

Inspection first requires the verified source digest and length to equal the
selected `FileUse`. It probes every reader in canonical identity order; this
orders bounded work and telemetry only. A processor claim is admitted only when
its canonical type belongs to that exact reader. Registration order never
settles conflicting claims.

Incompatible strong claims return ambiguity. Compatible strong claims resolve to
their sole type and reader and require strong-signature validation. With no
strong claim, a sole compatible structural candidate receives structural
validation. This ordering is the simplest interpretation of the accepted
design's otherwise unplaced `StructuralCandidate` strength. With neither, a
syntactically canonical declaration may nominate its exact reader for
independent structural validation. Finally, the sole registered text fallback
may claim only through complete streaming validation. No successful path returns
an ordinary declaration as evidence.

A recognized-malformed probe is terminal; incompatible recognized types are
ambiguous. Strong or structural validation cannot quietly return no-match and
fall through. A declared or streaming candidate that does not validate becomes
ordinary unknown. Successful detection that disagrees with a syntactically
canonical caller declaration becomes `DeclaredTypeMismatch`, blocking typed
reads without changing the blob or its metadata. Recognized encrypted or locked
content is terminal `EncryptedOrLocked`; version one has no password channel.

## Agent tools

The stable `file_inspect` contract accepts exactly a canonical `digest` and an
optional bounded visible-part selector. `file_read` adds an exact provider-owned
`view` and object `options`; the serialized options object is limited to 65,536
bytes before adapter validation, and the request accepts no model-supplied type
or reader. Both are effect-free tool declarations. The generic executor projects
only compact JSON or bounded typed failure evidence.

`signalbox-file-media-provider-runtime` supplies their registry-backed service.
Its injected `FileUseResolver` is the sole authorization boundary: it must reuse
the rendered-frontier allow-set, disambiguate repeated digest uses, finish
catalog work, and return exact use metadata plus a placement-free verified
source. The bridge rejects a resolver that returns another digest, repeats
inspection for every read, and never exposes a store locator, path, credential,
or open database transaction to a processor.

Unknown inspection is successful and has no views. Malformed, ambiguous,
declared-mismatch, and encrypted outcomes are known typed failures. Reads admit
only a declared view; options remain structured data for adapter validation. A
complete `file_read` argument document admits a maximum nesting depth of 256
JSON object and array containers; the outer argument and options objects count
toward that depth. Text must be bounded valid UTF-8 without U+0000. Structured
output must parse as bounded JSON within its declared depth, node, string, and
byte limits. A cursor is absent on complete output and is a bounded control-free
opaque value on a truncated result. A continuation read sends that cursor
instead of initial view options through the same checked service and processor
request contracts.

**Committed unimplemented functionality.** No present daemon catalog composes
these tools because the concrete rendered-frontier attachment resolver is not
yet on `main`; the empty daemon registry recognizes no format. Dedicated adapter
workers may register compiled providers with their worker catalogs, but the
compatibility constraint is that future daemon composition supplies the existing
visibility proof to `FileUseResolver`, not a weaker catalog-presence check.
Format adapters add no MIME branch to the executor, bridge, or daemon.

## Adapter coverage

Each listed adapter is compiled into a dedicated worker and registered there as
one provider declaration. Inputs remain whole-source bounded; adapter output is
untrusted until the daemon-side registry sanitizer admits it.

| Family | Canonical types                              | Detection and validation                                                                                           | Views                                                                                 | Decoder choice                                                                                     |
| ------ | -------------------------------------------- | ------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------- |
| Text   | `text/plain`, `application/json`, `text/csv` | Complete NUL-free UTF-8 fallback; structural JSON parse; strict rectangular CSV parse with row and column ceilings | Exact `text`; bounded JSON `structured`; bounded CSV headers and rows as `structured` | Standard-library UTF-8, `serde_json`, and pure-Rust `csv`; all execute only in the isolated worker |

## Processor and durable media boundary

The raw processor enums deliberately carry strings and JSON text rather than
checked registry values. Oversized, malformed, injection-shaped, cross-reader,
unregistered-reason, wrong-output-kind, contradictory continuation, and
excessively nested responses collapse to sanitized processor failure without
partial success (INV-076). Detection uses generated synthetic bytes and
byte-derived evidence independent of caller declaration (INV-075).

`signalbox-file-media-processor-runtime` implements `FileMediaProcessor` with
one fresh local process for every probe, validation, or read. A provider maps to
one checked absolute worker executable. The executable builds a `WorkerCatalog`
from its compiled `FileMediaProvider` implementations and calls `serve_one`; the
daemon registers the same declaration through `WorkerBinding`. An exact profile
probe must return `ProcessorIsolation::Available` before the corresponding
nonempty registry is constructed.

On Linux the processor launches the exact worker through bubblewrap. It unshares
every supported namespace, explicitly unshares and then disables further user
namespaces, drops every capability, clears the environment, creates private
`/proc`, `/dev`, `/tmp`, and `/run`, places `/dev/shm` under its own bounded
tmpfs mount, and mounts only the exact worker plus the host's dynamic-runtime
library trees read-only. It exposes no source path, catalog, database, daemon
socket, configuration, credential, home directory, or network namespace. An
architecture-checked seccomp filter returns `ENOSYS` for `clone3`, permits
fallback `clone` only with `CLONE_THREAD`, denies process creation, and denies
unbudgeted inotify instance and watch allocation. Decoder threads remain
available while worker descendants stay zero.

Before releasing a dedicated startup gate, the daemon applies hard
address-space, CPU, core-dump, and descriptor limits to the sandbox process and
all inherited worker threads. Each invocation first enters its own child of an
explicitly configured writable delegated cgroup-v2 root, with `pids.max` set to
the compiled task ceiling and `memory.max` set to the configured worker-memory
ceiling before bubblewrap can fork. The memory controller accounts tmpfs data,
inode slab, and other cgroup-charged kernel memory. Construction fails closed
when either delegated controller cannot be validated. The configured memory
ceiling is one combined budget: half is reserved for address space and half is
split between the three writable tmpfs mounts, so their maxima cannot add to
more than the configured value; `memory.max` independently bounds their charged
memory and metadata. The daemon independently owns one wall deadline per
invocation, one inspection-wide deadline across all serial reader probes, and
one verification-wide deadline across all configured worker probes. It kills the
isolated process group on timeout or authoritative cancellation. Bounded stderr
is drained and discarded; it is never parser evidence, telemetry content, or
model-visible output.

The worker receives one digest and positive length, then requests exact byte
ranges over length-delimited standard I/O. The daemon checks every request for
positive checked arithmetic, source length, per-frame size, access posture,
range count, cumulative source work, and cancellation before calling
`VerifiedBlobSource`. Probing uses the reader's declared probe envelope.
Validation uses its independent effective random-access source-byte and
exact-range ceilings, up to the compiled 1 GiB and 4,096-range maxima. A typed
read uses its selected view's streaming or random-access envelope. Streaming
access is monotonic; no range can exceed half the effective frame ceiling, so
encoded source replies remain bounded.

A worker result is eligible for the existing durable tool-result commit path
only after one matching final frame, exact EOF with no trailing byte, successful
worker exit, and complete bounded stderr drain. EOF before a final frame, crash,
signal, timeout, cancellation, malformed or oversized framing, extra output,
source failure, or limit excess discards the whole result. Thus the isolation
slice can leave neither a partial durable result nor parser output that bypasses
the registry sanitizer (INV-081 through INV-086). Authoritative cancellation
terminates the in-flight worker and admits no result (INV-087).

The version-one compiled ceilings are:

| Resource                              | Maximum                 |
| ------------------------------------- | ----------------------- |
| Probe prefix and suffix               | 65,536 bytes each       |
| Probe ranges / cumulative bytes       | 16 / 262,144            |
| Processor frame                       | 1,048,576 bytes         |
| Text body / JSON body                 | 174,000 / 500,000 bytes |
| Serialized read options               | 65,536 bytes            |
| Structured depth / nodes              | 64 / 100,000            |
| Observed container entries            | 10,000                  |
| Image axis / decoded pixels           | 8,192 / 16,777,216      |
| Presented image bytes                 | 8,388,608               |
| Audio channels / sample rate          | 8 / 192,000 Hz          |
| Audio duration / presented bytes      | 60 s / 8,388,608        |
| Presented general-file bytes          | 8,388,608               |
| References / aggregate media per call | 16 / 33,554,432 bytes   |
| Worker memory budget / CPU / wall     | 512 MiB / 60 s / 120 s  |
| Worker descriptors / retained stderr  | 32 / 16,384 bytes       |
| Minimum worker descriptor override    | 16 descriptors          |
| One / aggregate executable snapshots  | 64 MiB / 64 MiB         |
| Worker bindings per processor         | 256                     |
| Worker tasks                          | 64                      |
| Worker descendants                    | 0                       |

`FileMediaCeilings` admits only positive effective values at or below its
compiled maxima. `FileMediaProcessCeilings` keeps the protocol frame fixed at
1,048,576 bytes while admitting only positive resource values at or below their
compiled maxima; descendants are fixed at zero (INV-072). Each worker snapshot
is additionally capped by its effective worker address-space budget; bubblewrap
and all distinct worker snapshots together cannot exceed the 64 MiB aggregate
snapshot ceiling. The text body ceiling reserves enough frame space for
worst-case JSON escaping and envelope fields. A stored source may be larger. A
streaming view must request it in bounded frames under its finite declared
source-work envelope; a whole-decode view may reject it without changing the
blob.

**Committed unimplemented functionality.** No worker is composed into
`signalboxd` because no concrete rendered-frontier resolver is present. No
image, audio, or general-file producer exists, so this slice adds no rich
`BlobReference` result arm: the accepted design forbids that arm until one
producer-to-provider path proves publication, registration, preparation, and
failure behavior. When such an adapter lands, generated bytes must publish and
verify before catalog registration, and registration must precede durable result
commit; a failed path may leave an unreferenced orphan but never a dangling
result.

No classification cache exists. Earlier durable tool results preserve what a
model saw while a later request may use a newer immutable reader revision. OCR
and transcription are absent in version one.

## Open edges

- Remaining concrete format families and their independently reviewed
  dependencies remain with
  [general-purpose artifacts](../open-questions.md#general-purpose-artifacts).
- Cumulative per-turn typed-read and source-work budgets wait for the first
  adapter benchmarks under
  [identity, credentials, and resource governance](../open-questions.md#identity-credentials-and-resource-governance).
- Provider-native general-file inventories remain adapter-specific review work;
  no generic provider file capability admits unknown bytes.

# File and media layer over blob storage

Status: proposed for owner decision; design only.

## Placement and authority

This page records a decision for review, not implemented behavior. It changes no
page under [`docs/spec/`](../spec/). If accepted, its implementation stack must
update the owning specification pages as behavior becomes true, add public API
shapes to [`docs/domain-spine.md`](../domain-spine.md), and put enforced
invariants in INV-tagged tests.

The immutable blob catalog, attachment stubs, bounded raw reads,
preparation-time attachment verification, and model input-modality catalog are
fixed inputs from [blob storage](../spec/blob-storage.md). Blob identity remains
the SHA-256 of raw bytes only, placement remains catalog fact, reads stream, and
no database transaction spans store I/O.

## Outcome and boundaries

Add a typed interpretation layer above blobs. Agents can inspect an attachment,
learn its validated content type and available views, extract bounded text or
structure, request a bounded image or audio representation, and receive a typed
reference that a capable model adapter can materialize. Images, audio,
documents, and general files use one provider registry. Adding a type adds an
adapter and registry entry, never a content-type `match` in the daemon,
application service, tool executor, or model-provider bridge.

This proposal decides the common types, registry, agent reads, failure algebra,
resource envelope, durable result references, and rollout. It leaves the first
format inventory, parser libraries, and concrete process sandbox to the owner
rulings at the end.

It does not change blob lifecycle, routing, remote access, or provider-native
storage, and adds no mutable aliases, implicit inference, archive extraction,
network-fetching parsers, or automatic attachment materialization.

## Proposed decisions

Keep three frames distinct:

1. `BlobDigest` identifies immutable bytes and says nothing about type.
2. `FileUse` holds caller-supplied metadata for one use of those bytes.
3. `ValidatedFile` is one bounded reader's evidence about verified bytes under
   an immutable registry snapshot.

The daemon composes a process-lifetime `FileMediaRegistry` from independently
owned providers. Each declares its probes, media types, views, access pattern,
bounds, and isolation needs. Construction rejects conflicting static ownership;
runtime detection rejects ambiguous bytes rather than using registration order.

Agents receive two stable tools, `file_inspect` and `file_read`. Inspection
returns type facts and provider-declared view names. Reading selects one view
and returns bounded text/JSON or a fixed-size blob reference. The selected
provider validates view options against its registered object schema.

Every byte interpreter runs out of process with hard memory, CPU, wall-time,
source-read, output, nesting, and expansion ceilings. A killed, timed-out,
over-limit, or malformed worker returns typed failure and no partial output.

Text and structured previews remain bounded text tool results. Image, audio, and
provider-native file presentation uses a durable `BlobReference` result arm.
Derived views are registered as `generated_artifact` blobs before result commit,
so replay names exact immutable bytes rather than rerunning a parser.

## Type model

These are proposed semantic shapes, not existing declarations:

```text
FileUse {
  digest: BlobDigest,
  byte_length: NonZeroU64,
  attachment_kind: AttachmentKind,
  declared_media_type: DeclaredMediaType,
  display_filename: Option<DisplayFilename>,
}

AttachmentKind = Image | Audio | Document | File

CanonicalMediaType {
  type_name: LowercaseAsciiToken,
  subtype_name: LowercaseAsciiToken,
}

ReaderIdentity {
  provider: FileReaderProviderName,
  reader: FileReaderName,
  revision: FileReaderRevision,
}

ValidatedFile {
  source: FileUse,
  detected_media_type: CanonicalMediaType,
  reader: ReaderIdentity,
  validation: ValidationEvidence,
  views: NonEmpty<ReadViewDeclaration>,
}
```

`AttachmentKind::Audio` extends the current closed attachment vocabulary when
implementation changes the domain, storage, and process wire in place. Kind is
user intent, not detection evidence. A declared `image/*` file containing other
bytes remains representable; typed inspection reports the disagreement.

`DeclaredMediaType` preserves the exact bounded caller value already held by an
attachment. `CanonicalMediaType` is a lowercase ASCII type/subtype essence with
no parameters. Parameters are separately validated metadata. Filename extensions
are diagnostic hints only. Neither declared type nor extension selects a reader
without byte validation.

`ValidationEvidence` is closed and content-silent:

```text
StrongSignature
| StructuralValidation
| DeclaredCandidateStructurallyValidated
| StreamingTextValidation
```

A reader declares common output semantics while owning its view vocabulary:

```text
ReadViewDeclaration {
  name: ReadViewName,
  description: BoundedText,
  arguments_schema: CanonicalJsonObjectSchema,
  output: Text | Structured | Image | Audio | File,
  bounds: ReadViewBounds,
}
```

Examples such as `pages_text`, `page_preview`, `region`, `waveform`, and `clip`
are provider data, not central enum arms. A genuinely new output kind is a
foundation change; it cannot be hidden inside a reader registration.

## Detection and validation

Detection operates on `VerifiedBlobSource`: length plus asynchronous prefix,
suffix, and exact-range reads backed directly by catalog-selected replicas and
the blob layer's full verification rules. It does not call the process protocol,
decode base64, expose paths, or hand store credentials to a worker.

Each provider registers bounded probes returning:

```text
NoMatch
| Candidate { media_type, strength }
| RecognizedMalformed { media_type, reason_code }

strength = Strong | StructuralCandidate | DeclaredCandidate
```

Detection follows one fixed algorithm:

1. Verify source digest and length through ordinary replica traversal.
2. Run relevant probes in isolation under their range and byte budgets.
3. Partition probe results by canonical media type and owner. A
   `RecognizedMalformed` claim conflicts with any candidate for another type or
   owner and returns `AmbiguousType`; multiple malformed claims for the same
   type and owner collapse to that type's deterministic registered reason code.
4. With no conflicting claim, any `RecognizedMalformed` result returns
   `Malformed` immediately. It never falls back to a candidate, declared type,
   or text.
5. Validate the sole strong candidate, or return `AmbiguousType` for
   incompatible strong claims. Registration order never breaks a tie. After
   successful validation, compare its canonical type with the syntactically
   valid declared type; disagreement returns `DeclaredTypeMismatch` rather than
   a validated file.
6. With no strong claim, a syntactically valid declared type may nominate its
   sole provider for structural validation. The declaration is not proof.
7. With no declared candidate, a text provider may claim only after complete
   streaming UTF-8 and control-policy validation. After successful validation,
   compare the canonical text type with any syntactically valid declared type;
   disagreement returns `DeclaredTypeMismatch` rather than a validated file.
8. Validation failure after a recognized signature is `Malformed`, never a
   fallback to text or a weaker candidate.
9. No successful candidate is ordinary `UnknownType`; the blob stays stored and
   raw-readable.

Polyglots are admitted only when every strong claim resolves to the same type
and owner. A container provider owns distinctions within its own family. Probe
order is unsigned-ASCII provider/reader order for deterministic work and
telemetry, never precedence.

Version one has no durable classification cache. Immutable tool results already
preserve what a model saw. A later tool request may use a newer reader revision,
but cannot rewrite the earlier result.

## Registry and adapter boundary

Mirror the model-runtime dependency posture:

- `signalbox-file-media-runtime` owns provider-neutral source, request, result,
  cancellation, registry, and conformance types. It depends on no domain,
  application, persistence, daemon, parser, image, audio, or provider crate.
- One narrow adapter crate owns each parser family and all parser dependencies.
- `signalbox-file-media-provider-runtime` bridges application ports and blob
  sources into the runtime; dependencies point into application and runtime.
- `signalboxd` composes a literal provider list. It never dispatches by MIME.

The provider contract is logically:

```text
trait FileMediaProvider {
  fn declaration(&self) -> FileMediaProviderDeclaration;
  async fn probe(
    request,
    verified_source,
    bounded_probe_reads,
    cancellation,
  ) -> ProbeOutcome;
  async fn validate(
    request,
    selected_probe,
    verified_source,
    cancellation,
  ) -> ValidateOutcome;
  async fn read(
    request,
    validated_file,
    verified_source,
    bounded_output,
    cancellation,
  ) -> ReadOutcome;
}
```

The registry collects every bounded `ProbeOutcome`, applies the fixed selection
algorithm, and invokes `validate` only on the selected provider. An
implementation may keep an opaque, request-bound capability between those
stages. The contract is that probing precedes registry selection, validation
precedes interpretation, every stage is bounded and cancellable, and truncated
worker output is never success.

Registry construction rejects duplicate identities or exact MIME owners,
duplicate view names, non-object schemas, unsupported output kinds, absent or
above-process limits, unbounded access/output, or unavailable isolation. Image
views must bound dimensions, pixels, and bytes; audio views duration, samples,
channels, and bytes; structured views nesting, nodes, strings, and bytes.

Configuration may disable compiled providers and lower bounds. It cannot add
aliases, choose precedence, raise compiled ceilings, or load executable plugins.
Runtime plugin loading would turn configuration into code authority.

### Registering a type

1. Add one adapter crate without daemon or persistence dependencies.
2. Implement the provider and declare exact media types, probes, views, schemas,
   output kinds, and limits.
3. Run the shared suite and malformed corpus under the real isolation harness.
4. Add its constructor to daemon composition and optional strict configuration.
5. Update owning specs in the implementation PR that exposes the provider.

No common tool schema, wire enum, application branch, or model adapter changes
when the provider uses existing output kinds.

## Processor isolation

All adapter probes and parsers run outside signalboxd. A fresh worker receives
only a read-only capability for one digest and a write-only bounded result
channel: no database, store locator, credential, daemon socket, network, ambient
environment, arbitrary host path, or unrelated descriptor.

The broker checks every range against source length, the provider declaration,
cumulative source bytes, range count, and cancellation before store I/O. The
supervisor enforces finite resident memory, CPU, wall time, descendants,
descriptors, and output. Worker pooling is deferred so compromise cannot cross
requests.

A result is length-delimited and accepted only after clean worker exit. EOF,
crash, signal, timeout, malformed or extra frame, and any limit breach discard
it. Stderr is bounded, scrubbed diagnostic material and never model-visible.
Parser messages collapse to registered reason codes.

The selected sandbox must prove process/address-space separation, no network or
ambient credentials, whole-descendant termination, exact one-digest authority,
and no durable partial output. Validation remains defense in depth rather than
the containment boundary.

## Agent read surfaces

### `file_inspect`

Every rendered attachment stub adds `part_selector` containing the immutable
semantic-entry identity and the attachment's zero-based part ordinal. The pair
is stable across replay and identifies one exact `FileUse`; digest alone never
chooses among uses. Arguments are exactly `{ digest, part_selector }`, and the
digest must equal the digest in that selected visible stub. Authorization reuses
the `blob_read` rendered-frontier allow-set and additionally verifies the exact
selector. The permission default is `Auto` and the effect class is
`ExternalEffect`, because inspection can issue an authenticated object-store
read visible to its operator. Success is compact JSON with:

- digest, decimal byte length, attachment kind, declared type, and filename;
- status `validated` or `unknown`;
- detected type and reader identity when validated;
- bounded provider metadata; and
- ordered view declarations available to `file_read`.

Unknown is successful inspection with no views. Malformed, ambiguity, and
mismatch are known failures using the exact closed application-facing variants
`Malformed`, `AmbiguousType`, and `DeclaredTypeMismatch`; they never persist the
success JSON schema. Outcomes expose no content beyond admitted metadata.

### `file_read`

Arguments are exactly `{ digest, part_selector, view, options, continuation }`.
The permission default is `Auto` and the effect class is `ExternalEffect`,
because a read can perform observable authenticated store reads and publish a
generated artifact. On an initial request, `options` is the provider object and
`continuation` is null. On a continuation request, `options` is null and
`continuation` is the cursor from the preceding result; the cursor carries and
authenticates the original normalized options and next semantic position. The
executor repeats inspection; it never trusts model-supplied type or reader
identity. The selected view must exist, the initial `options` must validate
against its registered object schema, and a continuation must bind the same
digest, selector, reader identity, and view.

- `Text` returns admitted UTF-8 with truncation and continuation facts.
- `Structured` returns canonical compact JSON with the same facts.
- `Image` and `Audio` return a durable reference to exact source or derived
  bytes.
- `File` returns a reference only when the selected model adapter supports a
  reviewed general-file input contract; it never silently becomes text.

Pagination uses a common opaque authenticated cursor binding digest, part
selector, reader identity, view, normalized initial options, and position. It is
at most 1,024 bytes and expires on restart; an expired cursor returns a typed
invalid-continuation failure and the model may issue a new initial request
rather than the executor guessing. Providers expose semantic positions such as
page, row, section, frame, or time span, not parser offsets.

Raw `blob_read` remains beside these tools as the unknown-format and diagnostic
escape hatch. A typed reader never falls back to raw bytes for recognized
malformed content.

## Durable results and model preparation

Add one provider-neutral result arm:

```text
BlobReference {
  digest: BlobDigest,
  byte_length: NonZeroU64,
  media_type: CanonicalMediaType,
  presentation: Image | Audio | File,
  source_digest: BlobDigest,
  reader: ReaderIdentity,
  validation: ValidationEvidence,
}
```

`source_digest` is provenance for a derived view and equals `digest` for direct
presentation. `validation` is the content-silent evidence admitted before result
commit and protected by durable-result integrity; because the digest fixes the
validated bytes, preparation authenticates this evidence instead of rerunning a
reader. Filename, parser text, store, and object key are absent. The enclosing
tool result keeps request correlation and order.

New output streams through generated-artifact ingest. Publication and
verification precede registration; registration precedes result commit. Before
processing or publication, the continuation transaction prospectively projects
the complete rendered frontier and durably reserves one reference slot and the
view's maximum presented bytes. It rejects the read before creating output when
that reservation would exceed the selected model call's reference-count or
aggregate media ceiling. Publication consumes no more than the reservation; the
transaction releases unused bytes and commits the reference atomically. A later
publication or commit failure may leave an unreferenced orphan, never a dangling
result, but capacity rejection registers nothing. Thus every committed reference
is admissible to its mandatory continuation call. Equal output bytes converge by
digest, and ambiguous publication cannot become tool success.

Rendering first emits a bounded textual stub. Preparation then:

1. rejects unsupported image/audio/file presentation through typed
   modality-unsupported failure before send authorization;
2. live-verifies a replica against the durable digest and authenticates the
   persisted canonical type and validation evidence without rerunning a reader;
3. maps missing, corrupt, unavailable, malformed, or oversized bytes to typed
   preparation outcomes without provider traffic; and
4. materializes only the bounded form required by the selected provider.

Direct adapters may encode bounded bytes; CLI adapters may receive an
owner-private temporary file removed after the call. Provider wire types remain
inside their adapter. Referenced bytes do not consume the text-result ceiling,
but fixed metadata does, and media gets separate per-call bounds. Visibility of
a later reference derives from the rendered durable result, not global catalog
presence.

## Hard ceilings

Every limit is a rejection boundary, not a preallocation target. Checked
arithmetic governs byte, pixel, sample, node, and expansion products. Readers
may lower but never raise these process ceilings:

| Resource                              | Ceiling                |
| ------------------------------------- | ---------------------- |
| Probe prefix and suffix               | 65,536 each            |
| Probe ranges / cumulative bytes       | 16 / 262,144           |
| Processor frame / text-or-JSON body   | 1,048,576 / 786,432    |
| Structured depth / nodes              | 64 / 100,000           |
| Observed container entries            | 10,000                 |
| Image axis / decoded pixels           | 8,192 / 16,777,216     |
| Presented image bytes                 | 8,388,608              |
| Audio channels / sample rate          | 8 / 192,000 Hz         |
| Audio clip duration / presented bytes | 60 s / 8,388,608       |
| Presented general-file bytes          | 8,388,608              |
| References / aggregate media per call | 16 / 33,554,432 bytes  |
| Worker memory / CPU / wall time       | 512 MiB / 60 s / 120 s |
| Worker descendants                    | 0                      |

A stored blob may remain multi-gigabyte. A compatible reader streams it under a
finite cumulative source-work limit; a whole-decode view may return
`SourceTooLarge` without invalidating the blob. Text/structure stops before the
first complete semantic unit that would cross output bounds; a single oversized
unit returns `OutputUnitTooLarge`, never partial UTF-8 or JSON. Image and audio
limits are checked before decode allocation.

Per-turn governance adds durable typed-read count and source-work reservations.
One tool request charges once before authorization and is never refunded or
recharged on replay. Initial cumulative values require the benchmark ruling
below; they cannot weaken any per-request or per-call ceiling here.

## Unknown, malformed, and failure handling

The application-facing algebra is closed and sanitized:

```text
BlobNotVisible | BlobMissing | BlobCorrupt | BlobUnavailable
| UnknownType | AmbiguousType
| DeclaredTypeMismatch { declared, detected }
| Malformed { media_type, reason_code }
| EncryptedOrLocked { media_type }
| UnsupportedView | InvalidViewArguments | InvalidContinuation
| SourceTooLarge { maximum_bytes } | ExpansionLimitExceeded { limit_kind }
| OutputUnitTooLarge | ProcessorUnavailable
| ProcessorFailed { reason_code } | ProcessorTimedOut | Cancelled
```

Unknown files remain valid blobs and attachments with stubs, metadata, and raw
reads, but no typed views. A recognized malformed file uses no permissive parser
recovery unless registered as part of validation. Locked files are distinct from
corrupt ones; this proposal adds no password channel.

Declared/detected mismatch blocks typed reads and provider materialization, but
does not delete the blob or reject an already-durable attachment. A corrected
use can reference the same digest. This avoids store I/O in accepted-input
transactions and keeps metadata correction out of blob identity.

Processor failure is operator failure, not malformed evidence unless a complete
authenticated failure frame arrived before exit. Telemetry may name digest,
reader, reason code, and numeric limits, never bytes, extracted text, filenames,
declared types, stderr, paths, or credentials.

## Existing blob wire and trust boundary

`read_blob_metadata`, `read_blob_chunk`, `blob metadata`, and `blob read` remain
unchanged byte surfaces. Typed readers use an internal verified-source port over
the same catalog and registry, reusing replica order, verification, generation
pinning, range checks, admission, deadlines, and failure classification. They do
not tunnel through process messages or internal base64. Catalog transactions end
before source I/O.

A later client-facing typed command needs new paged file-media messages and uses
the existing blob lifecycle for referenced output. It cannot widen blob chunks,
put media type in blob metadata, or expose routes. Derived outputs are ordinary
`generated_artifact` blobs; provenance stays in the tool result or a future
artifact aggregate, never the blob row.

All source and embedded metadata is attacker-controlled tool content. Readers
perform no network fetch, write no archive path, execute no active content, and
do not recursively interpret embedded files in version one. Parser dependencies
are adapter-confined, pinned, publicly sourced, and covered by malformed
fixtures. Broad or native dependencies remain ordinary owner gates.

## Conformance contract

One shared suite proves every provider and isolation implementation:

- finite declarations, enforced range/output bounds, and deterministic valid
  detection;
- unknown, malformed, truncated, ambiguous, mismatch, and bomb fixtures fail as
  specified, independent of registration order;
- crash, cancellation, timeout, memory/output kill, or framing defect produces
  no partial success;
- workers lack network, credentials, database, path, and second-digest access;
- text/JSON boundaries remain valid, and expansion/pixel/sample limits stop at
  the named value;
- derived publication failure commits no reference, while a later failure leaves
  at most an orphan and equal output deduplicates;
- durable replay does not rerun parsing or recharge the turn; and
- preparation rejects missing, corrupt, malformed, oversized, and
  modality-unsupported references before send authorization.

Each adapter adds public-spec boundary fixtures, signature collisions, trailing
data, malformed metadata, and parser regressions. Fuzzing supplements rather
than replaces fixed contract fixtures.

## Sliced rollout

Each child targets its predecessor and splits again rather than exceeding the
normal review budget.

1. **Runtime and registry:** neutral types, registry, bounded ports, failure
   algebra, scripted provider, and shared suite; compose an empty registry.
2. **Isolation and inspection:** ruled sandbox, verified-source broker,
   supervision, `file_inspect`, and one small pure-Rust text/structure adapter.
3. **Text and structure:** ruled document adapters, paged `file_read` results,
   and durable turn work accounting.
4. **Images:** ruled adapters, pixel validation, region/fit views, generated
   previews, and `BlobReference::Image`.
5. **Provider image input:** neutral image message parts, target gates, and each
   proven adapter projection.
6. **Audio:** attachment kind, ruled adapters, duration/sample checks, clip or
   waveform views, and audio projection only where supported.
7. **General files:** native `BlobReference::File` only for adapters with an
   exact reviewed contract; other files stay legible through typed readers.
8. **Further adapters:** one narrow family per PR. Archives, OCR, transcription,
   and network readers require separate proposals.

No rich result arm lands without one producer-to-provider path and its complete
preparation-failure proofs.

## Open questions requiring owner ruling

1. **Isolation substrate.** Choose a dedicated local worker, existing runner
   sandbox, or another mechanism proving this contract. Recommendation: a
   daemon-supervised local worker using already accepted platform sandbox
   primitives, because reads must work without a session runner. Blocks slice 2.
2. **First formats.** Recommendation: UTF-8 text, JSON, CSV, PDF, PNG, JPEG,
   WebP, GIF, WAV, MP3, FLAC, and Ogg/Opus; defer office containers, SVG, video,
   and archives. Blocks adapter slices, not registry work.
3. **Parser dependency budget.** Decide whether isolated native decoders are
   admissible. Recommendation: pure Rust first; approve native libraries per
   adapter only when coverage requires them and isolation is executable.
4. **OCR and transcription.** Choose explicit inference providers, local
   readers, or absence. Recommendation: exclude both; they introduce selection,
   credentials, cost, privacy, and nondeterministic replay beyond file reading.
5. **Provider-native general files.** Decide which adapters may receive them.
   Recommendation: require an exact per-adapter type inventory; never interpret
   a generic provider “file” surface as accepting unknown bytes.
6. **Encrypted files.** Decide whether a future credential reference may supply
   a password. Recommendation: keep `EncryptedOrLocked` terminal in version one;
   secrets must not enter tool arguments or results.
7. **Turn budgets.** Set cumulative source-work and request ceilings after first
   adapter benchmarks, while preserving every hard ceiling above. Blocks
   production enablement, not interface work.
8. **Classification cache.** Recommendation: omit it until measurement proves a
   need. Immutable results already stabilize replay; a cache adds invalidation
   and retirement law without improving correctness.

Acceptance settles only this common architecture. Every parser dependency and
new provider modality still receives a narrow implementation review and updates
its owning specification only when behavior exists.

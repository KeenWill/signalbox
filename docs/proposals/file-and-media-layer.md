# File and media layer over blob storage

Status: proposed for owner decision; design only.

## Placement and authority

This page records a decision for review, not implemented behavior. It changes no
page under [`docs/spec/`](../spec/). If accepted, its implementation stack must
update the owning specification pages as behavior becomes true, add public API
shapes to [`docs/domain-spine.md`](../domain-spine.md), and put enforced
invariants in INV-tagged tests. The missing owning records are explicit:
`AttachmentKind::Audio` belongs in the domain spine and blob-storage attachment
contract; attachment `part_selector` belongs in the blob-storage rendered-stub
contract; durable `BlobReference` results, media admission, and preparation
belong in the tool-loop contract; and later client-facing paged file-media
messages belong in the process-protocol contract. None is recorded as
implemented by this proposal.

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
suffix, and exact-range reads backed by one request-local immutable snapshot.
The broker creates that snapshot while performing the blob layer's initial full
verification traversal, makes it read-only before any probe runs, and serves all
later probe and reader ranges from that exact verified generation. It deletes
the snapshot after the request and never exposes its path to a worker. Before
traversal, the broker atomically reserves the authenticated exact byte length
from both per-request and process-wide snapshot pools. The effective per-request
maximum is the lesser of the configured per-request and process-wide bounds. An
authenticated length above that maximum returns
`SourceTooLarge { maximum_bytes }`; temporary exhaustion by other fitting
requests returns `ProcessorUnavailable`. Either rejection occurs without reading
source bytes or creating a snapshot. The reservation remains held until the
request-local file is removed. At startup, the broker removes abandoned files
from its owner-private snapshot directory before admitting work, so a crash
cannot permanently consume the pool. This preserves verification across range
reads even for filesystem replicas and S3 replicas without immutable version
tokens. Detection does not call the process protocol, decode base64, expose
paths, or hand store credentials to a worker.

Each provider registers bounded probes returning:

```text
NoMatch
| Candidate { media_type, strength }
| RecognizedMalformed { media_type, reason_code }

strength = Strong | StructuralCandidate | DeclaredCandidate
```

Detection follows one fixed algorithm:

01. Start the inspection-wide 120-second wall deadline before source
    verification. Reserve the authenticated exact byte length from the bounded
    snapshot pools, then verify source digest and length through ordinary
    replica traversal while materializing the immutable request-local snapshot.
    The executor first attempts the owning blob path's non-waiting direct-read
    admission while retaining its scheduler-pass slot. If admission is
    unavailable, it retains the pass and returns `BlobUnavailable`; it creates
    no waiter that would later need a pass merely to commit failure. Only after
    acquiring direct-read admission does the durable handoff release the
    scheduler-pass slot, immediately before store traversal. Completion,
    failure, and cancellation reacquire a pass only through that owning path's
    bounded queue; no store I/O begins while both capacities are held, and
    reacquisition cannot exceed the inspection-wide deadline. Verification may
    perform one complete traversal per recorded replica candidate, in the
    owning replica order, until one candidate verifies or the finite catalog
    snapshot is exhausted. Each traversal is bounded by the catalog's
    authenticated `byte_length`; the checked aggregate is bounded by that
    length times the recorded candidate count and remains under the same wall
    deadline and process-wide traversal admission. Corruption or unavailability
    of one candidate falls through to the next candidate exactly as the owning
    blob path requires. Those integrity bytes use the blob layer's traversal
    accounting, not the reader-work ceiling. Every later source range is served
    from the completed snapshot and cannot observe another replica generation.
    Exhausting the wall deadline returns `ProcessorTimedOut`. Ordinary blob
    traversal's longer deadline cannot extend the inspection deadline. Only
    exhaustion of the recorded candidates propagates `BlobCorrupt` or
    `BlobUnavailable`; missing or no-longer-visible sources propagate as
    `BlobMissing` or `BlobNotVisible` before candidate selection.
02. Run relevant probes in isolation under both their provider limits and the
    same inspection-wide deadline, plus the request-wide probe-count,
    concurrency, memory, CPU, range, and probe-source-byte budgets below.
    Exceeding any probe-specific request-wide budget is
    `ProcessorFailed { reason_code: probe_budget_exceeded }`; an inspection
    never schedules additional probes after that boundary. Failure to reserve or
    start a worker propagates as `ProcessorUnavailable`. Every scheduled
    relevant probe must complete successfully before candidate classification:
    timeout, cancellation, crash, signal, malformed or extra framing,
    output-limit breach, or other operational failure terminates inspection with
    its exact `ProcessorTimedOut`, `Cancelled`, or sanitized `ProcessorFailed`
    outcome. A surviving claim is never selected around a failed probe, because
    the failed probe might have supplied conflicting strong or malformed
    evidence. None of these source or processor outcomes becomes content
    evidence.
03. Partition byte-evidence results—`Strong`, `StructuralCandidate`, and
    `RecognizedMalformed`—by canonical media type and exact `ReaderIdentity`.
    Claims for different types or reader identities conflict and return
    `AmbiguousType`, including two `RecognizedMalformed` claims for the same
    type from different readers and a strong claim accompanied by a structural
    candidate for another type or reader. `DeclaredCandidate` is nomination from
    caller metadata, not byte evidence, and never participates in this conflict
    set. Multiple malformed claims for the same type and reader collapse to that
    reader's deterministic registered reason code.
04. With no conflicting claim, any `RecognizedMalformed` result returns
    `Malformed` immediately. It never falls back to a candidate, declared type,
    or text.
05. After step 3, all remaining strong claims necessarily name one canonical
    type and exact `ReaderIdentity`; collapse duplicate claims to that reader
    and validate it once. If no unique identity remains, return `AmbiguousType`;
    registration and probe order never break a tie. After successful validation,
    compare its canonical type with the syntactically valid declared type;
    disagreement returns `DeclaredTypeMismatch` rather than a validated file.
06. With no strong claim, one unambiguous structural candidate is validated by
    its exact reader. Successful validation compares its canonical type with the
    syntactically valid declared type and returns `DeclaredTypeMismatch` on
    disagreement. Authenticated structural-invalid evidence returns
    `UnknownType` because no recognized signature established malformed content,
    and does not fall back to text; operational, cancellation, and locked
    outcomes propagate unchanged.
07. With no byte-evidence candidate selected, a syntactically valid declared
    type may nominate its sole provider for structural validation. Only a
    `DeclaredCandidate` for that exact declared type and provider is considered.
    The declaration is not proof. Authenticated structural-invalid evidence from
    declared-candidate validation returns `UnknownType` and does not fall back
    to text or a weaker candidate; authenticated malformed evidence returns
    `Malformed`. Operational, cancellation, and locked outcomes propagate
    unchanged as `ProcessorTimedOut`, `ProcessorFailed`, `Cancelled`, or
    `EncryptedOrLocked`.
08. With no structural or declared candidate, a text provider may claim only
    after complete streaming UTF-8 and control-policy validation. After
    successful validation, compare the canonical text type with any
    syntactically valid declared type; disagreement returns
    `DeclaredTypeMismatch` rather than a validated file.
09. Authenticated structural-invalid evidence after a recognized signature is
    `Malformed`, never a fallback to text or a weaker candidate. Operational,
    cancellation, and locked outcomes propagate unchanged as
    `ProcessorTimedOut`, `ProcessorFailed`, `Cancelled`, or `EncryptedOrLocked`;
    they are never converted into content evidence.
10. No successful candidate is ordinary `UnknownType`; the blob stays stored and
    raw-readable.

Polyglots are admitted only when every byte-evidence claim resolves to the same
type and exact `ReaderIdentity`. Declaration-only candidates participate only in
the declared-versus-detected comparison. A container provider owns distinctions
within its own family. Probe order is unsigned-ASCII provider/reader order for
deterministic work and telemetry, never precedence.

Each declared probe has a stable `ProbeName`, and a claim's canonical key is
`{ ReaderIdentity, canonical media type, strength, ProbeName }`. A provider must
reduce all internal signals for one key to one canonical outcome and one opaque
request-bound capability before returning from `probe`; the registry never
merges opaque capabilities. Repeated outcomes for one key are accepted only when
their canonical outcome bytes and capability authenticator are identical, then
collapse to that one outcome. A differing duplicate is a provider contract
failure, `ProcessorFailed { reason_code: inconsistent_probe_state }`, not a
selection tie. After claim classification, more than one winning canonical key
for the selected reader, media type, and strength is
`ProcessorFailed { reason_code: inconsistent_probe_state }`, even when the keys
have distinct `ProbeName` values. The registry neither chooses among nor merges
their opaque capabilities. Exactly one winning key therefore supplies the
`selected_probe` passed to `validate`, independently of scheduling or return
order.

Whether validated classifications need a durable cache remains an owner decision
in
[`docs/open-questions.md`](../open-questions.md#file-and-media-interpretation).
This proposal requires immutable tool results to preserve what a model saw but
does not decide whether an implementation also caches classifications. A later
tool request may use a newer reader revision, but cannot rewrite the earlier
result.

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
implementation may keep the uniquely authenticated opaque, request-bound
capability between those stages. The contract is that probing precedes registry
selection, validation precedes interpretation, every stage is bounded and
cancellable, and truncated worker output is never success.

Registry construction rejects duplicate identities or exact MIME owners,
duplicate view names, non-object schemas, unsupported output kinds, absent or
above-process limits, unbounded access/output, or unavailable isolation. Every
view declares its maximum range count and cumulative reader-source bytes; the
registry rejects either value above the common per-read ceilings below. It also
computes the complete worst-case detection schedule for every possible declared
type and rejects a snapshot that could require more than 32 probes. No runtime
prefilter may omit a byte probe based on declared type or filename. Image views
must bound dimensions, pixels, and bytes; audio views duration, samples,
channels, and bytes; structured views nesting, nodes, strings, and bytes. Each
provider has at most 64 views, and registry construction encodes the worst-case
successful `file_inspect` projection, including provider metadata and all
ordered view declarations and argument schemas. It rejects a declaration whose
complete projection exceeds the 786,432-byte text-or-JSON body ceiling, so an
admitted provider's views are always reachable through inspection. Every image,
audio, or file view also declares the finite set of canonical media types it can
emit. Each model adapter declares its accepted canonical media types per
presentation kind and its maximum materialized and provider-wire bytes for one
reference of each accepted type. For every direct or uploaded representation,
the adapter also declares a deterministic checked worst-case wire projection
from materialized length to complete provider payload length, including base64,
multipart, JSON escaping, framing, and fixed metadata. Registry construction
rejects an absent, overflowing, or non-monotonic projection. Each adapter
additionally declares one aggregate provider-wire payload maximum per target:
the bound on the complete encoded request payload rather than on any single
part. Before processing or reserving a reference, `file_read` requires the
view's complete emitted-type set to be accepted by the selected adapter and
applies that projection to each declared presentation-byte maximum. Both the
materialized maximum and projected wire maximum must fit their target-specific
limits, and the sum of the checked worst-case wire projections across every
reference bound to one provider call must fit the declared aggregate payload
maximum, unless the view instead guarantees normalization to one accepted type
and bound. Admission first projects and reserves the complete encoded non-media
baseline for the pinned provider call, including text, tool schemas, history,
and all non-reference framing. It then accumulates every existing and newly
admitted reference's checked worst-case wire projection against the remaining
aggregate capacity. Thus the baseline plus all references, rather than the
references alone, must fit the complete-request maximum. A baseline that already
exceeds the maximum or a reference that would overflow the remainder returns
`SourceTooLarge` with the aggregate target maximum and publishes, reserves, and
commits nothing. An unsupported type returns typed modality-unsupported failure;
an oversized target projection returns `SourceTooLarge` with the target-specific
maximum. Neither path can publish, reserve, or commit a reference.

Registry construction's 786,432-byte body ceiling is only a declaration bound.
Before committing a successful `file_inspect`, the tool loop prospectively
projects its complete rendered result into the pinned target and current
frontier, including mandatory continuation-call framing and reserved output
capacity. If that exact result cannot fit the target's remaining input capacity,
the request returns `OutputUnitTooLarge` and commits no result; automatic
compaction is not an admission mechanism.

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

No common tool schema, wire enum, or application branch changes when the
provider uses existing output kinds. A model adapter changes only when its
reviewed canonical-media-type inventory is intentionally widened; registering a
reader does not imply that support.

## Processor isolation

All adapter probes and parsers run outside signalboxd. A fresh worker receives
only a read-only capability for one digest, a write-only bounded control-result
channel, and, for an image, audio, or file view, a separate write-only bounded
streaming binary channel into generated-artifact ingest: no database, store
locator, credential, daemon socket, network, ambient environment, arbitrary host
path, or unrelated descriptor. The binary channel admits at most the selected
view's declared presentation-byte ceiling, computes length and digest while
streaming, and is never interpreted as a control frame.

The broker checks every range against source length, the provider declaration,
the common per-read maximum of 4,096 ranges and 1,073,741,824 cumulative source
bytes, and cancellation before snapshot I/O. Exceeding the cumulative
source-byte bound returns `SourceTooLarge { maximum_bytes: 1073741824 }`.
Exhausting the range-count bound is a property of the reader's request pattern,
not of the source, so it returns `ExpansionLimitExceeded { limit_kind }` naming
the range-count limit. Either way the broker supplies no further bytes. The
supervisor enforces finite resident memory, CPU, wall time, descendants,
descriptors, and output. Before spawning, it atomically reserves one slot and
the declared memory limit from a process-wide pool of four workers and 2 GiB;
unavailable capacity returns `ProcessorUnavailable` without spawning or reading
the source. Reservations are released only after whole-process-tree termination.
Worker reuse is deferred so compromise cannot cross requests.

A control result is length-delimited and accepted only after clean worker exit.
Before classification, validation, publication, or commit, the supervisor
validates every control value against the exact immutable declaration invoked:
probe media type and strength, validation type and evidence class, read view and
output kind, emitted media type and registered output reader, and every reason
code must be declared members. A value outside that declaration is sanitized to
`ProcessorFailed { reason_code: undeclared_worker_outcome }`; it supplies no
candidate or evidence and discards all staged output. A clean frame and exit do
not weaken this declaration-membership check. Binary output is likewise
committed to ingest only after a valid terminal control frame, exact declared
length and digest agreement, clean worker exit, and independent validation of
the staged bytes as the emitted canonical type. The view declaration identifies
the registered output reader for every emitted type, and registry construction
rejects a rich view without one. EOF, crash, signal, timeout, malformed or extra
frame, disagreement, failed output validation, and any channel limit breach
discard the control result and staged binary output. Stderr is bounded, scrubbed
diagnostic material and never model-visible. Parser messages collapse to
registered reason codes.

The selected sandbox must prove process/address-space separation, no network or
ambient credentials, whole-descendant termination, exact one-digest authority,
and no durable partial output. Validation remains defense in depth rather than
the containment boundary.

A CLI parser is itself the fresh worker launched directly by the supervisor; no
wrapper worker launches it as a child. When the CLI requires a path, the broker
receives one owner-private path in a broker-backed, read-only metered filesystem
over the exact authorized snapshot; it never receives an ordinary materialized
host file. Every open, read, positioned read, mapping fault, and reread is
mediated and atomically charged against the selected operation's declared range
and cumulative source-byte limits before bytes are supplied. Cache hits are
charged again, and crossing either limit supplies no further bytes, terminates
the worker, and returns `SourceTooLarge`, so seeking or mapping cannot bypass
the broker's accounting. The mount and path disappear only after
whole-process-tree termination. The CLI inherits the same write-only
length-delimited control descriptor and optional bounded binary descriptor as
any other worker. Success requires one valid terminal frame, exact binary
length/digest agreement when present, clean exit, and no extra output;
cancellation, signal, timeout, nonzero exit, malformed framing, descendant
creation, or metering failure terminates the whole sandbox and discards staged
output. The sandbox applies the same network, credential, environment, path,
descriptor, and zero-descendant restrictions to this directly supervised
process.

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

Arguments are exactly
`{ digest, part_selector, reader, view, options, continuation }`. `reader` is
the exact `ReaderIdentity` returned by `file_inspect`. The permission default is
`Auto` and the effect class is `ExternalEffect`, because a read can perform
observable authenticated store reads and publish a generated artifact. On an
initial request, `options` is the provider object and `continuation` is null. On
a continuation request, `options` is null and `continuation` is the cursor from
the preceding result. The cursor is an opaque authenticated token that
references process-local state containing the original normalized options and
next semantic position; it does not embed either value. The permission check
reuses the `blob_read` rendered-frontier allow-set before the executor boundary.
An initial request must reauthorize the exact currently visible stub identified
by its digest and selector. A continuation request must present a live
authenticated cursor from the currently visible preceding result; its bound
digest and selector must still identify an exact stub in the current allow-set.
Falling out of the rendered frontier invalidates either authority; a remembered
digest, selector, or cursor grants no access. The executor repeats inspection
after authorization; it never trusts model-supplied type evidence. The result
must select the exact supplied reader identity and revision, or the initial
request returns `ReaderRevisionUnavailable` instead of executing a view with
changed semantics. The selected view must exist on that revision, the initial
`options` must validate against its registered object schema, and a continuation
must bind the same digest, selector, reader identity, and view.

- `Text` returns admitted UTF-8 with truncation and continuation facts.
- `Structured` returns canonical compact JSON with the same facts.
- `Image` and `Audio` return a durable reference to exact source or derived
  bytes.
- `File` returns a reference only when the selected model adapter supports a
  reviewed general-file input contract; it never silently becomes text.

Before any request in one tool batch executes worker or store I/O, one short
batch-admission transaction visits every request in stable request order,
including siblings for tools outside this file-media layer. It starts from the
pinned target and current frontier, reserves mandatory continuation-call framing
and output capacity once for the batch, and cumulatively reserves each sibling's
complete maximum rendered result. Existing tools use the finite durable-result
and rendered-projection bound from their registered tool contract; a tool with
no finite target projection cannot be admitted in the batch. File-media tools
use the registry-bounded inspection projection, the selected text or structured
view bound, or the rich-reference projection and media bounds below. A request
that cannot obtain its cumulative reservation returns `OutputUnitTooLarge`,
performs no external I/O, and commits no result or continuation state.
Reservations for admitted siblings remain charged until their completion
transactions consume the actual result and release unused capacity, so
independently fitting siblings can never overfill the combined continuation
call. Steering accepted while the batch executes uses the same pinned-target
capacity ledger: before a steering input becomes pending, a short transaction
projects its complete rendered bytes after the already reserved sibling results
and atomically consumes remaining continuation capacity. Steering that cannot
obtain that reservation is not accepted into this batch and remains eligible for
a later turn; it can never be appended as unreserved pending input. Its
reservation remains charged through continuation creation or terminal batch
failure. Automatic compaction, arrival time, and completion order are not
admission mechanisms.

Pagination uses a common opaque authenticated cursor of at most 1,024 bytes. The
cursor is only a random token for bounded process-local state; it does not embed
provider options or semantic positions. The state binds digest, part selector,
reader identity, view, normalized initial options, and position. Registry
construction requires normalized options to fit 16,384 canonical JSON bytes and
each provider's encoded semantic position to fit 4,096 bytes; creation rejects
larger state before returning a truncated result. At most one live continuation
entry exists per admitted result, at most 1,024 entries and 20 MiB exist per
turn, and at most 4,096 entries and 64 MiB exist process-wide. Admission
reserves both entry and byte capacity before returning truncated success. It
never evicts a live entry to admit another; exhausted capacity returns
`ContinuationCapacityExceeded`. Turn terminalization and restart expire all of
that turn's entries deterministically. A missing or expired token returns typed
invalid-continuation failure, and the model may issue a new initial request
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
  source_validation: ValidationEvidence,
  presentation_validation: ValidationEvidence,
}
```

`source_digest` is provenance for a derived view and equals `digest` for direct
presentation. `source_validation` authenticates the inspected source.
`presentation_validation` always authenticates the exact bytes named by
`digest`; it equals the source evidence for direct presentation. For a derived
artifact, after clean producer exit and before registration, the broker runs the
complete relevant-probe classification and selection algorithm over the
completed staged bytes under the ordinary output-validation isolation and
resource bounds. The result must uniquely select the declared emitted canonical
type and its registered output `ReaderIdentity`; ambiguity, another selected
type or reader, or any failed relevant probe discards the staged output and
commits no reference. The broker then validates once with that uniquely selected
output reader. Derived output admits only `StrongSignature` or
`StructuralValidation`; a producer's type claim, length, or digest is not
validation evidence. Both evidence values are content-silent and protected by
durable-result integrity, so preparation authenticates the evidence for the
referenced digest instead of rerunning a reader. Filename, parser text, store,
and object key are absent. The enclosing tool result keeps request correlation
and order.

New output streams through generated-artifact ingest. Publication and
verification precede registration; registration precedes result commit. After
authorization and before processor or store I/O, a separate short pre-execution
transaction prospectively projects the complete rendered frontier and durably
reserves one reference slot and the view's maximum presented bytes for this
request. The projection includes the pinned target's per-type materialized and
provider-wire byte limits and the mandatory continuation call's remaining input
capacity. It rejects the read before creating output when the reservation would
exceed any target-specific, reference-count, aggregate-media, or context-input
boundary, then ends before any worker or store I/O begins. Publication consumes
no more than the durable reservation. The byte reservation separately records
the maximum materialized length and the adapter's checked worst-case complete
provider-wire projection for that length; encoded expansion is therefore
reserved before execution rather than inferred from materialized bytes. A short
completion transaction atomically registers the verified blob, consumes the
reservation, releases unused bytes, and commits the reference; a known-failure
completion transaction releases the reservation without registering a blob.
Every crash-lost authorized `ExternalEffect` attempt remains ambiguous and keeps
its reservation through the owning tool loop's reconciliation lifecycle; it
cannot be retried. The bottom tool-loop specification diff in the implementation
stack must add an explicit, durable reconciliation closure before this
reservation mechanism can ship. A closure that proves success consumes the
reservation while committing the exact result; one that proves known failure or
records an operator's explicit terminal abandonment releases it. Abandonment is
irreversible, records that no later result may be committed for the attempt, and
is the only safe release when the external effect remains unknowable. Until one
of those terminal closures commits, the reservation has no timeout and remains
charged. This proposal adds no pre-effect checkpoint and does not treat daemon
restart or elapsed time as closure. A publication or completion failure may
leave an unreferenced orphan, never a dangling result, but capacity rejection
registers nothing. Thus every committed reference is admissible to its mandatory
continuation call, no transaction spans processor or store I/O, and crash
recovery never erases an external effect. Equal output bytes converge by digest,
and ambiguous publication cannot become tool success.

For a rich view that directly presents the verified source instead of producing
binary output, the broker checks the authenticated source `byte_length` before
committing the reference. The length must fit the view's declared presentation
maximum, the presentation kind's process ceiling, and the target-specific
materialized maximum. The adapter's checked worst-case wire projection of that
exact length must independently fit the target provider-wire maximum and the
wire portion of the durable reservation. Failure returns `SourceTooLarge` with
the effective maximum and commits no reference. A direct reference cannot bypass
a generated-output channel's bounds merely because its bytes already exist.

Rendering first emits a bounded textual stub. Preparation then:

1. rejects unsupported image/audio/file presentation through typed
   modality-unsupported failure before send authorization;
2. live-verifies a replica against the durable digest and authenticates the
   persisted canonical type and `presentation_validation` evidence for those
   exact bytes without rerunning a reader;
3. maps missing, corrupt, unavailable, malformed, or oversized bytes to typed
   preparation outcomes without provider traffic; and
4. materializes only the bounded form required by the selected provider.

Direct adapters may encode bounded bytes; CLI adapters may receive only the
owner-private metered filesystem path described above, removed after the call.
Provider wire types remain inside their adapter. Referenced bytes do not consume
the text-result ceiling, but fixed metadata does, and media gets separate
per-call bounds. Visibility of a later reference derives from the rendered
durable result, not global catalog presence.

## Hard ceilings

Every limit is a rejection boundary, not a preallocation target. Checked
arithmetic governs byte, pixel, sample, node, and expansion products. Readers
may lower but never raise these process ceilings:

| Resource                                | Ceiling                |
| --------------------------------------- | ---------------------- |
| Probe prefix and suffix                 | 65,536 each            |
| Per-probe ranges / cumulative bytes     | 16 / 262,144           |
| Probes / concurrent workers per inspect | 32 / 2                 |
| Aggregate probe ranges / source bytes   | 64 / 1,048,576         |
| Aggregate probe memory / CPU / wall     | 1 GiB / 120 s / 120 s  |
| Source verification traversals / bytes  | inventory / aggregate  |
| Snapshot bytes per request / process    | 64 GiB / 256 GiB       |
| Per-read ranges / cumulative source     | 4,096 / 1,073,741,824  |
| Control frame / text-or-JSON body       | 1,048,576 / 786,432    |
| Views / complete inspection JSON        | 64 / 786,432 bytes     |
| Structured depth / nodes                | 64 / 100,000           |
| Observed container entries              | 10,000                 |
| Image axis / decoded pixels             | 8,192 / 16,777,216     |
| Presented image bytes                   | 8,388,608              |
| Audio channels / sample rate            | 8 / 192,000 Hz         |
| Audio clip duration / presented bytes   | 60 s / 8,388,608       |
| Presented general-file bytes            | 8,388,608              |
| References / aggregate media per call   | 16 / 33,554,432 bytes  |
| Continuations per turn / process        | 1,024 / 4,096          |
| Continuation state per turn / process   | 20 MiB / 64 MiB        |
| Worker memory / CPU / wall time         | 512 MiB / 60 s / 120 s |
| Worker descendants                      | 0                      |
| Process worker slots / reserved memory  | 4 / 2 GiB              |

A stored blob may remain multi-gigabyte. Inspection may verify it into one
immutable request-local snapshot with a deadline-bounded full traversal whose
byte bound is its authenticated length, provided that length fits the effective
per-request snapshot ceiling and available process capacity. Deployments may
lower the compiled 64 GiB per-request and 256 GiB process bounds, but the
effective per-request maximum is the lesser of both configured bounds. A source
larger than that effective maximum returns `SourceTooLarge { maximum_bytes }`
because it can never fit; `ProcessorUnavailable` is reserved for capacity that
could fit the request but is currently occupied. Probe and reader work remain
under their smaller fixed ceilings. A compatible reader can stream bounded
regions from it, but no one read may consume more than 1,073,741,824 source
bytes; a whole-decode view may return `SourceTooLarge` without invalidating the
blob. Text/structure stops before the first complete semantic unit that would
cross output bounds; a single oversized unit returns `OutputUnitTooLarge`, never
partial UTF-8 or JSON. Image and audio limits are checked before decode
allocation.

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
| UnsupportedView | ModalityUnsupported { presentation, media_type }
| InvalidViewArguments | InvalidContinuation
| ContinuationCapacityExceeded | ReaderRevisionUnavailable
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
authenticated failure frame arrived before exit. Telemetry follows the owning
identity-and-commands contract exactly: it may use only daemon-minted aggregate
identifiers plus the closed tool-name and error-classification tokens. It never
records a blob digest, reader identity, bytes, extracted text, filename,
declared type, parser message, stderr, path, credential, or content-derived
identifier.

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

- finite declarations, enforced per-provider and request-wide range, worker,
  time, memory, and output bounds, and deterministic valid detection;
- unknown, malformed, truncated, ambiguous, mismatch, and bomb fixtures fail as
  specified, independent of registration order;
- fixtures cover malformed claims from different readers, a strong claim with a
  conflicting structural claim, a declaration-only candidate alongside a unique
  strong mismatch, a valid declared type with no byte-evidence candidate,
  duplicate same-reader claims with different probe state, distinct probe names
  producing multiple winning capabilities, duplicate compatible strong claims,
  successful and failed structural-candidate validation, and failed
  declared-candidate validation;
- an encrypted or locked fixture returns `EncryptedOrLocked` terminally without
  a password channel, permissive parser recovery, or text fallback;
- source visibility, missing, corruption, availability, worker-startup, and
  process-wide worker-capacity failures preserve their exact closed variants
  before candidate selection;
- filesystem and unversioned-S3 fixtures mutate the selected replica after the
  verification traversal and prove every probe/read range still observes only
  the completed verified snapshot; replica fixtures also corrupt or make
  unavailable an earlier recorded candidate and prove verification falls
  through in owning order to a later valid candidate under the per-candidate,
  checked aggregate, and inspection-deadline bounds;
- snapshot admission reserves exact bytes before traversal, rejects exhausted
  process capacity without source I/O, classifies a length above the effective
  configured per-request maximum as `SourceTooLarge` with that maximum, uses
  `ProcessorUnavailable` only when an otherwise fitting request is blocked by
  occupied capacity, releases capacity only after deletion, and removes crash
  leftovers before startup admission;
- declarations above 4,096 reader ranges or 1,073,741,824 reader-source bytes
  are rejected, and runtime exhaustion returns `SourceTooLarge` without another
  source byte;
- any scheduled relevant probe's crash, cancellation, timeout, memory/output
  kill, or framing defect terminates inspection with its exact operational
  outcome before candidate classification and produces no partial success;
- every probe, validation, and read control value is checked against its exact
  immutable declaration before use; undeclared media types, strengths, views,
  output kinds, output readers, evidence classes, or reason codes return
  sanitized `undeclared_worker_outcome` failure and commit no staged output;
- declared-candidate validation preserves timeout, processor failure,
  cancellation, and locked outcomes unchanged, and only authenticated
  structural-invalid evidence becomes `UnknownType`;
- workers lack network, credentials, database, arbitrary host-path, and
  second-digest access;
- path-based CLI fixtures prove seeks, positioned reads, mappings, and rereads
  are charged before delivery and cannot exceed the declared cumulative source
  limit;
- text/JSON boundaries remain valid, and expansion/pixel/sample limits stop at
  the named value;
- binary presentation can reach its declared ceiling without entering the
  control frame; derived bytes run the complete relevant-probe ambiguity
  algorithm and uniquely match the declared output type and reader before
  validation and commit; conflicting polyglot evidence or a failed relevant
  probe commits no reference; derived publication failure commits no reference,
  while a later failure leaves at most an orphan and equal output deduplicates;
- direct rich references compare the authenticated source length with the view,
  process, target materialized, target wire, and durable reservation maxima
  before commit, and an oversized source commits no reference;
- durable replay does not rerun parsing or recharge the turn;
- ambiguous rich-result reservations remain charged until durable reconciliation
  proves success or failure or records irreversible terminal abandonment, after
  which the attempt cannot later commit a result;
- continuation tokens authenticate and resolve only to their bound digest, part
  selector, reader identity, view, normalized initial options, and position;
- one pre-execution batch admission reserves target-input capacity cumulatively
  in stable request order for every sibling tool result, including non-file
  tools, plus one mandatory continuation frame; a tool without a finite rendered
  projection is not admitted, independently fitting results cannot overfill the
  combined call; steering accepted during execution atomically consumes that
  same ledger before becoming pending; and rich views are rejected before
  processing when either their materialized maximum or the adapter's checked
  worst-case complete encoded-wire projection exceeds its target-specific limit;
- snapshot traversal acquires non-waiting direct-read admission before releasing
  scheduler-pass ownership, retains the pass when admission fails, begins no
  store I/O while both capacities are held, and uses the owning blob path's
  bounded reacquisition; aggregate provider-wire fixtures reserve the complete
  encoded non-media baseline before existing and new reference projections and
  reject a call whose baseline-plus-references exceeds the target maximum; and
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
4. **Image groundwork:** ruled adapters, pixel validation, region/fit view
   declarations, and conformance fixtures; this slice exposes no rich result.
5. **Images and provider input:** generated previews, `BlobReference::Image`,
   neutral image message parts, target gates, and each proven adapter projection
   land together.
6. **Audio:** attachment kind, ruled adapters, duration/sample checks, clip or
   waveform views, and audio projection only where supported.
7. **General files:** native `BlobReference::File` only for adapters with an
   exact reviewed contract; other files stay legible through typed readers.
8. **Further adapters:** one narrow family per PR. Archives, OCR, transcription,
   and network readers require separate proposals.

No rich result arm lands without one producer-to-provider path and its complete
preparation-failure proofs.

## Open questions requiring owner ruling

The authoritative unresolved inventory is
[`docs/open-questions.md`](../open-questions.md#file-and-media-interpretation).
It owns the isolation substrate, first formats, parser dependency budget, OCR
and transcription, provider-native general files, encrypted-file credentials,
turn budgets, and classification-cache questions. This proposal does not settle
them.

Acceptance settles only this common architecture. Every parser dependency and
new provider modality still receives a narrow implementation review and updates
its owning specification only when behavior exists.

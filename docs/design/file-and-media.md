# File and media interpretation design

This design is not built. It extends
[file and media interpretation](../spec/file-and-media.md) with the rich media
result path.

## Goal

A reader may offer image, audio, and general-file views, and a read of such a
view returns a durable reference to immutable bytes that a capable model adapter
can present. A classification cache and OCR or transcription are outside this
design; both remain undecided in [open questions](../open-questions.md).

## Design

`FileReadResult` gains one provider-neutral reference arm carrying two complete
identities. The presented identity is the blob's digest and length, its
canonical media type, its presentation kind (image, audio, or general file), the
reader that validated those bytes, and content-silent validation evidence for
them. The source identity is the source digest, its detected media type, the
reader that validated it, and its own content-silent evidence. For direct
presentation both identities name the same digest, media type, and reader, and
the evidence is copied from the inspection; a derived view whose output reader
differs from the source reader keeps both. For a derived view the worker streams
bytes through a separate bounded binary channel into generated-artifact ingest
on [blob storage](../spec/blob-storage.md).

For a derived read, validated bytes publish and verify first, the blob registers
in the catalog second, and the durable tool result commits last; a direct read
publishes and registers nothing. Before publication the daemon re-runs detection
and validation over the completed staged bytes with the ordinary probe
algorithm. Each rich view's `ReadViewDeclaration` carries a finite set of
permitted canonical output types. The result must select one type in that set
and its registered output reader uniquely, and a result outside the set is
rejected; only strong-signature or structural validation counts, and a
producer's own type claim, length, or digest is never evidence. Invalid derived
bytes publish nothing, and any failure after publication and before the result
commits leaves no result and may leave an unreferenced blob.

Each target bound is the lower of the configured limit and the model adapter's
limit for an accepted media type and presentation kind: one bounds the
materialized bytes of one reference, the other the complete encoded
provider-wire payload, which counts encoding and framing. A reference is
admitted only when both hold, so emitted byte length alone is never the test. A
direct presentation requires the emitted type to equal the source's detected
type and the source length to fit the view, process, and target bounds; derived
bytes must fit those same target bounds, and bytes that exceed them fail the
read.

Preparation for a model call authenticates the persisted evidence for the
referenced bytes instead of running a reader again, and rejects an unsupported
presentation before send authorization. Visibility of a reference derives from
the rendered durable result, not from catalog presence.

## Compatibility constraints

Registry construction keeps rejecting image, audio, and general-file views, and
`FileReadResult` keeps only its text and structured arms, until one producer
path has proved publication, registration, preparation, and failure behavior end
to end. The rich arm changes no adapter and adds no MIME branch to the executor,
bridge, or daemon.

## Acceptance criteria

- A derived read publishes and verifies, registers, then commits, in that order;
  a fault injected at each step commits no result, and the only residue is an
  unreferenced blob. A direct read publishes and registers nothing, and commits
  its reference only after verifying the cataloged source.
- Derived bytes fail the read, publish nothing, and commit nothing when
  re-detection is ambiguous or selects a type outside the view's permitted set
  or another reader, or when they exceed the target bounds.
- Preparation rejects an unsupported presentation before send authorization and
  runs no reader.
- Text and structured reads behave exactly as they do today.

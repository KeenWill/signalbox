# Conversation import

Conversation import stores a transcript another program produced as an immutable
record, and a session can later be created from any of its entry boundaries.

## Overview

An imported conversation (`ImportedConversation`) is one snapshot of an external
transcript. The daemon mints its identity; its content identity is a digest over
the converter's format and the sequence of raw source records. One header holds
two sequences: the raw records and the normalized entries derived from them. A
raw record is one nonempty physical JSONL record of the source, preserved
verbatim as a content-addressed blob. Each normalized entry references the raw
record it came from, so every conversion decision is traceable to exact source
bytes.

Every source field on an entry is attested with a value, attested absent, or not
attested; a JSON null is attested absence and an omitted member is not attested.
The normalized content vocabulary (`ImportedTranscriptContent`) is closed: one
variant per recognized message content kind, one typed absence for a message
without content, and generic variants that retain a source-defined record or
block whose kind has no more specific variant.

Every normalized entry boundary is one immutable, addressable
`ImportedTranscriptFrontier` naming its conversation and the inclusive final
entry. The rest of Signalbox consumes frontiers:
[sessions-and-transcript](sessions-and-transcript.md) describes creating a
session that resumes or forks from one, and
[model-call-execution](model-call-execution.md) describes how imported entries
reach a model.

`ImportedConversationConverter` is the application-facing seam that turns source
bytes into a checked aggregate. A converter consumes the bytes, one
caller-supplied conversation identity, and a total lazy callback that supplies
entry identities; it declares a closed `ImportedConversationFormat` carrying
both the source family and the Signalbox converter version. Two converters
exist. `ClaudeCodeJsonlConverter` reads Claude Code session JSONL and produces
converter version 2; stored version-1 snapshots keep the version-1
interpretation. `CodexRolloutJsonlConverter` reads Codex rollout JSONL and
produces converter version 1.

`ImportConversationService` runs a converter and calls the append-only Postgres
import store once, after complete conversion. The store keeps raw bytes in the
blob store under their content hash ([blob-storage](blob-storage.md)) and the
header, raw-record occurrences, and normalized entries in relational tables.
Each header also records a display title derived once from the preserved
records, so the unified conversation listing in
[process-protocol](process-protocol.md) can show imported rows by name. When no
preserved record yields a title, the header records the underivable state and
carries none.

Three surfaces reach the store. The user terminal imports one named file or
every candidate file under a directory; a source that fits one frame is sent as
a single request, and a larger one is assembled on the daemon through a chunked
begin, append, and commit sequence. The one operator setting,
`conversation_import.max_source_bytes`, bounds the assembled source and defaults
to 256 MiB. An inspection read lists the normalized entries of one import with
their positions, so a user can choose the position a session continues from. A
browser read model lists imports, returns a descriptor for one, and windows its
entries around a position. The terminal and inspection wire shapes are owned by
[process-protocol](process-protocol.md). The browser DTOs are defined in the
web-contract crate; the daemon's web adapter serves the routes that carry them.

## Design decisions

External history is stored as its own aggregate. Why: replaying it as native
execution would fabricate the evidence chain the native lifecycle invariants
require.

The content identity is the digest of the converter format and the raw record
sequence, so reingesting the same source returns the existing identity and a
changed sequence is a new snapshot. Why: retrying or copying the same source
must not duplicate history, while an append or edit cannot mutate a snapshot an
existing session names.

The digest is not a session identifier or filename key: no source path, import
time, adoption choice, target session, or future-use policy participates in it.

Neither the importer nor a converter derives an identity, lineage evidence, or a
missing value from a filename, source path, neighboring record, another field,
wall clock, or import-time context.

The verified blob is the raw-byte authority; Postgres stores only the blob
digest and occurrence relationships, never a second `bytea` copy.

Imported structured values use a source-neutral JSON algebra rather than
`serde_json` or provider wire types, retaining scalars, array order, and every
member. Why: maximum-fidelity normalization makes later rendering choices
reversible, and a source-neutral algebra keeps provider JSON outside the domain.

Frontiers follow physical record order; the converter retains a source parent
identifier as attestation and does not follow, repair, or use it to reorder.
Why: a stable prefix boundary then exists for every observed entry even when
source ancestry is incomplete, and adjacency is not proof of external causality.

Converter versions describe Signalbox's interpretation, not a source
application's release.

A converter does not read files or choose paths; its caller supplies bytes, so
later formats add no filesystem types to the domain or application crates.

The Claude Code converter at version 2 does not reject a structurally valid
source-defined block whose discriminator has no more specific normalized
variant; it retains the block as a generic variant.

The Codex converter treats `response_item` as the semantic conversation stream;
every other top-level item kind, and every `response_item` payload type without
a specific mapping, produces one source event, including kinds introduced after
this converter version. Why: administrative and presentation events are retained
without reclassifying mirrored text or tool progress as duplicate conversation
entries.

No non-message Codex response item acquires a fabricated assistant speaker
merely because Codex produced it.

The terminal infers no source path from an environment or fixed home-directory
convention, and the single-file form inspects no neighboring file.

Scan mode reads and sends each candidate through one import operation in sorted
path order; it has no protocol request or server-side batching of its own.

The source path is local presentation only and is never transmitted or
persisted.

Resolving the `latest` position sentinel is client work, not daemon work. Why:
an imported conversation is immutable, so the resolution is stable and an exact
replay names the same boundary.

The browser descriptor route returns no raw blob, normalized record, host path,
or source repository location.

A failed import may leave unregistered store orphans but never an unreachable
catalog row. Why: raw blobs are published before the aggregate transaction, so
the database never references bytes the store lacks.

The display title is presentation evidence, not identity: it never participates
in the source digest, the conversation identity, or the unique source-identity
constraint.

Committed tests and fixtures are entirely synthetic.

## Boundary contracts

Errors, logs, and diagnostic evidence contain classes, counts, and canonical
identifiers. They never contain source bytes, host or credential paths, raw or
unsanitized provider payloads, SQL, or user content; a tool failure may name a
bounded workspace-relative path. Retained source content, such as an imported
transcript entry, is not diagnostic evidence.

An imported conversation is a durable record, never execution. Ingestion
performs no session, scheduler, slot, turn, attempt, model-call, tool,
durable-command, or outbox transition, and it neither creates nor mutates a
session.

Every nonempty physical JSONL record is preserved verbatim before normalization.
A non-message record produces a typed source event rather than being dropped or
recast as conversation text.

Imported text retains the exact decoded scalar sequence, including empty text,
whitespace, line endings, normalization distinctions, and U+0000. An absent
field is typed absence, never a placeholder string, empty object, guessed tool
name, or summary. Imported entries keep their attested source timestamps;
nothing is restamped to import time. Sidechain and metadata flags are
provenance, not exclusion: they remove no content and make no frontier
unseedable.

Duplicate source identifiers, missing or nonlinear parents, sidechains, and
metadata records do not change the physical prefix or prevent selecting any
frontier.

A converter invokes the entry-identity callback only after complete parsing and
normalization, once per emitted entry; it neither preallocates identities nor
invokes the callback for an entry it does not emit. A malformed content shape
fails the complete conversion rather than being dropped or guessed. A behavior
change that alters raw-record boundaries, entries, attestations, content, order,
hashes, or frontiers requires a new converter version; an existing version is
never reinterpreted.

Scan traverses the complete tree rooted at the named directory without following
symbolic links, opening the root and each descendant through no-follow
descriptors. A traversal failure aborts before any request rather than hiding an
unread subtree. A skipped outcome carries the exact client error and means the
client received no definitive successful receipt, not that the request was
uncommitted.

The chunked assembly and the import permit are per-connection state, released by
commit, abort, a terminal size or conversion rejection, and disconnect. An
already-in-progress refusal leaves the existing assembly available for append,
commit, or explicit abort. Commit supplies the whole assembled source to the
same converter and `ImportConversationService` call as the single-shot path. A
database failure is reported as an ambiguous commit, so the operator may retry
the exact format and source bytes.

The inspection read exposes no imported content a transcript snapshot does not
already carry, and adds nothing for events, tools, results, thinking, media,
absence, or raw records. It creates nothing, seeds no session, performs no
durable write, and decodes stored positions, identities, content, and metadata
fail-closed before presentation. The client resolves `latest` against this
read's entry count before constructing the durable command, prints the resolved
ordinal, and sends a concrete position.

One transaction resolves or inserts a complete aggregate. Ingestion publishes
and verifies every raw blob before that transaction, then registers the blob and
replica rows in the same transaction that first references them. One admitted
import awaits at most one raw blob publication or verification at a time while
holding the process-wide bulk-ingest permit; it never fans out concurrently.
Writers acquire shared raw hashes and globally unique entry identities in their
respective sorted key order and store physical positions explicitly.

Once a header exists, any hash mismatch, missing member, gap, duplicate entry
identity, unknown version, invalid value, or lineage mismatch is typed
corruption. The same raw record at two positions is valid when its bytes agree;
equal hashes over differing bytes are corruption. Complete storage records pass
through the domain-owned reconstitution seam; adapters never default or drop a
malformed value. For each converter version, that seam independently re-derives
every expected entry and requires exact agreement in count, order, content,
speaker, and source metadata. Encoded collection counts bound parsing but never
drive capacity allocation directly: collections grow fallibly after each decoded
element.

An ignored opt-in integration test consumes caller-provided local files only
when both an explicit enable variable and a source-directory variable are set.
It reports only aggregate counts and typed failure classes, never paths,
identifiers, raw bytes, text, tool data, thinking, media, or parser excerpts.

The bulk-ingest permit, the frame bound, and every socket request and refusal
shape are owned by [process-protocol](process-protocol.md). Blob identity and
the catalog the raw records converge onto are owned by
[blob-storage](blob-storage.md). The rule that no database transaction spans
store I/O is owned by [persistence-protocol](persistence-protocol.md).

## Planned

- Usage evidence on imported entries, with cost derived at read time from the
  price window covering the attested timestamp:
  [conversation-import design](../design/conversation-import.md).
- Blob-backed source conversion that streams from the blob substrate without
  materializing the whole source:
  [conversation-import design](../design/conversation-import.md).

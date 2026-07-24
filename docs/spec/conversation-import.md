# Conversation import

This page specifies immutable imported conversation snapshots, raw source-record
preservation, source-neutral normalization, addressable imported frontiers, the
format-versioned converter seam, the first Claude Code JSONL converter, and the
append-only Postgres import store. Later session creation from one imported
frontier is owned by [sessions-and-transcript](sessions-and-transcript.md);
native turn activation and model-call rendering are owned by
[turn-lifecycle-and-scheduling](turn-lifecycle-and-scheduling.md) and
[model-call-execution](model-call-execution.md).

## Record and ingestion boundary

An imported conversation is durable record, never execution. It has one
hub-minted `ImportedConversationId`, one closed-source format and converter
version, one source-content digest, an immutable nonempty sequence of raw source
record occurrences, and an immutable nonempty sequence of normalized
`ImportedTranscriptEntry` values (INV-001, INV-038). Every raw record produces
at least one normalized entry.

Imported entries never carry an `AcceptedInputId`, `TurnId`, `TurnAttemptId`,
`ModelCallId`, native tool identity, or native terminal evidence. They record
what an external source contained; they do not establish that Signalbox accepted
input, authorized or attempted a call, ran a tool, or observed an outcome.
Ingestion performs no session, scheduler, slot, turn, attempt, model-call, tool,
durable-command, or outbox transition.

Why: treating external history as native execution would fabricate the evidence
chain required by the native lifecycle invariants.

Ingestion is idempotent and future-use-neutral. The source-content digest is
SHA-256 over a domain-separated, length-framed sequence containing the format
version and each raw record's exact content hash in physical order. Reingesting
the same format and exact raw record sequence returns the existing imported
conversation identity; caller-supplied candidate identities from that attempt
are discarded. A changed raw sequence is a new immutable snapshot with a new
identity. Common raw records are still deduplicated by content hash.

The digest is not a source session identifier or filename key. No source path,
wall-clock import time, adoption choice, target session, or future-use policy
participates in it. The imported aggregate is separate from `Session`, and an
import neither creates nor mutates a session.

Why: retrying or copying the same source must not duplicate history, while an
append or edit cannot mutate the snapshot that an existing session already
names.

## Raw source records

Every nonempty physical JSONL record is preserved before normalization. A raw
record blob stores the exact bytes between line delimiters and their SHA-256
content hash; a conversation occurrence stores the blob hash and a positive
contiguous physical-record position. Line delimiters and source paths are not
part of a record. Duplicate content in one or many conversations creates
distinct ordered occurrences referencing one content-addressed blob.

The Postgres representation uses `bytea`, not `jsonb`, as raw authority. JSON
key order, whitespace, escapes, number spelling, empty strings, and U+0000
therefore remain recoverable even when normalization has a different typed
representation. A hash collision whose stored bytes differ is typed corruption
and fails closed; equality is never inferred from the hash alone at a checked
boundary.

Each occurrence also carries the complete source JSON object normalized into the
source-neutral structured-value algebra. Non-message records produce a typed
`SourceEvent` entry rather than being dropped or recast as conversation text. A
source-defined message block without a more specific normalized variant produces
a typed `SourceMessageBlock`, so its boundary and type remain explicit while the
complete normalized owning record retains every block field. The normalized
sequence and every entry's raw-record reference make each conversion decision
traceable back to exact source bytes.

## Source attestations and normalized content

Every normalized entry has its own `ImportedTranscriptEntryId`, owning
conversation, positive contiguous imported position, source-speaker attestation,
raw-record occurrence, position within that record, and source metadata:

- source record identifier;
- source parent record identifier;
- source session identifier;
- source timestamp;
- sidechain flag; and
- metadata-record flag.

Each source field is independently `Attested(value)`, `AttestedAbsent`, or
`NotAttested`. JSON `null` maps to `AttestedAbsent`; an omitted field maps to
`NotAttested`. The converter never derives a missing value from a filename,
neighboring record, wall clock, or another field. Sidechain and metadata flags
are provenance, not exclusion: they do not remove content or make an imported
frontier unseedable.

Imported text retains the exact decoded Unicode scalar sequence, including an
empty sequence, whitespace, line endings, normalization distinctions, and
U+0000. Imported structured values use a source-neutral JSON algebra rather than
`serde_json` or provider wire types. They retain decoded scalar values, array
order, and every object member; raw records remain the authority for lexical
JSON details and member spelling/order.

The closed normalized content vocabulary is:

- `SourceEvent`, retaining the source record-type attestation and complete
  normalized record for a non-message record;
- `SourceMessageBlock`, retaining the source block-type attestation and complete
  normalized owning record for a source-defined message block;
- `Text`, retaining attested exact user or assistant text or the field's precise
  typed absence;
- `ToolCall`, retaining independently attested source call identity, tool name,
  structured input, and caller metadata;
- `ToolResult`, retaining independently attested source call identity, error
  flag, and either exact text or an ordered sequence of typed text, image, and
  tool-reference result blocks whose own fields also retain attestations;
- `Thinking`, retaining independently attested exact thinking and signature;
- `RedactedThinking`, retaining the source's independently attested redacted
  data;
- `Document`, retaining independently attested media source kind, media type,
  and exact data; and
- `MessageContentAbsent`, distinguishing an omitted or explicit-null message,
  omitted or explicit-null message content, and an attested empty content-block
  array.

An absent field is typed absence, never a placeholder string, empty object,
guessed tool name, or summary. Exact raw bytes back every normalized variant,
but raw preservation is not permission to drop a supported semantic field.

Why: maximum-fidelity normalization makes later rendering choices reversible,
while a source-neutral algebra keeps provider JSON outside the domain.

## Imported frontier points

Every normalized entry boundary is one immutable, addressable
`ImportedTranscriptFrontier`. A frontier names its conversation and inclusive
final imported entry; its resolved sequence is exactly positions `1..=N` in
physical record/entry order. An aggregate therefore exposes one frontier point
per normalized entry, including source-event, message-content-absence, tool,
result, thinking, redacted-thinking, and document boundaries.

The converter retains `parentUuid` as source attestation but does not follow,
repair, or use it to reorder version-1 frontiers. Duplicate source identifiers,
missing parents, nonlinear parents, sidechains, and metadata records do not
change the physical prefix. They also do not prevent a client from later
selecting any imported frontier.

Why: a stable prefix boundary is available for every observed entry even when
source ancestry is incomplete or ambiguous; adjacency is not recast as proof of
external causality.

## Converter seam

`ImportedConversationConverter` is the application-facing edge seam. A converter
consumes source bytes plus caller-supplied hub identity candidates and returns
one completely checked domain aggregate or a typed conversion error. The
application calls the append-only store once only after complete conversion; a
conversion failure performs no durable write.

Every converter declares a closed `ImportedConversationFormat` containing both
the source family and Signalbox converter version. Converter versions describe
Signalbox's interpretation, not a source application's release. A behavior
change that could alter raw-record boundaries, normalized entries, attestations,
content, order, hashes, or frontier points requires a new converter version; an
existing version is never reinterpreted.

The converter does not read files or choose paths. Its caller supplies bytes, so
later formats implement the same seam without adding filesystem types to the
domain or application crates.

## Claude Code session JSONL version 1

`ClaudeCodeJsonlConverter` implements
`ClaudeCodeSessionJsonl { converter_version: 1 }`. It parses one JSON object per
nonempty line, raw-preserves every record, and processes records in physical
file order. Version 1 scans for LF bytes. An LF ends and is excluded from a
record; an immediately preceding CR is also excluded as the other half of a CRLF
delimiter. A CR anywhere else remains record content. Nonempty bytes after the
final delimiter form a final unterminated record, while a terminal LF or CRLF
does not create another record. An empty delimited record rejects the complete
conversion.

The parser retains object-member order and duplicate names in the complete
normalized source object. At every object level, repeating a member name that
version 1 consults to produce a normalized entry or attestation rejects the
complete conversion. Duplicate names inside otherwise unmodeled structured
values remain preserved and do not acquire fabricated selection semantics.

Records then normalize as follows:

1. A top-level record whose `type` is neither `user` nor `assistant` produces
   one `SourceEvent` containing its type attestation and complete normalized
   object.
2. For a user or assistant record, the top-level type supplies its attested
   speaker. The `message` envelope and its `role` are retained independently as
   attested, explicitly absent, or unattested; a present role must agree with
   the top-level type. A non-null envelope or role of the wrong JSON type fails
   conversion.
3. String message content produces one `Text` entry. Array content produces one
   entry per block, preserving block order within its source record. An omitted
   or null message, omitted or null content, or empty content array produces one
   precisely distinguished `MessageContentAbsent` entry.
4. `text`, `tool_use`, `tool_result`, `thinking`, `redacted_thinking`, and
   `document` blocks map to their corresponding normalized variants. A
   `fallback` block maps to `SourceMessageBlock`; its `from` and `to` objects
   remain in the complete normalized owning record. Tool-result arrays admit
   ordered `text`, `image`, and `tool_reference` blocks. All modeled fields
   retain their exact decoded or structured values and typed attestations; a
   `text` or image-result block whose value is omitted or null remains that
   block with precise typed absence.
5. An unknown content shape, content-block type, or tool-result block type fails
   the complete conversion rather than being silently dropped or guessed.

Version 1 accepts both user/final-response-only records and records containing
structured tool traffic, signed thinking, image results, tool references,
document blocks, model-fallback notices, attachments, and administrative source
events.

Malformed JSON, a blank line, invalid UTF-8, unsupported content, an identity
collision inside the candidate set, a position overflow, JSON deeper than 128
nested array or object containers, or a source with no JSON records rejects the
complete conversion. The depth bound applies to every complete source record and
modeled nested value. U+0000, empty strings, and a source containing only
non-message records do not: raw and normalized storage retain them.

## Persistence and reconstitution

The Postgres representation uses append-only `imported_raw_source_record` blobs,
`imported_conversation` headers, `imported_conversation_raw_record` occurrences,
and `imported_transcript_entry` members. Imported text and opaque media data use
UTF-8 `bytea`; complete structured records and nested values use a checked
adapter encoding of the domain algebra, never provider JSON as a domain type.

One transaction resolves or inserts a complete aggregate:

- an existing format/source-content digest must reconstitute completely and
  byte-match every raw occurrence before returning `AlreadyImported`;
- a new digest inserts or verifies every content-addressed raw blob, then
  atomically inserts one header, every raw occurrence, and every normalized
  entry; a concurrent header-insert loser re-inspects and completely
  reconstitutes the winner, returning `AlreadyImported` only after the same
  byte-for-byte match, and raw-blob insert conflicts likewise reload and verify
  the winning bytes before reuse; and
- deferred constraints require exact declared counts, contiguous positions,
  globally distinct imported-entry identities, valid raw-record references, and
  agreement between every member's owner and header.

No partial aggregate can commit (INV-038).
`ImportedConversationRepository::load` returns `None` only when the requested
header does not exist. Once a header exists, a hash mismatch, missing blob or
member, gap, duplicate, unknown discriminator/version, contradictory variant
columns, invalid source value, or domain correlation failure is typed
corruption. Complete storage records pass through the domain-owned
reconstitution seam; adapters never default or drop a malformed value (INV-002).

## Test data and local validation

Committed tests and fixtures are entirely synthetic. An ignored opt-in
integration test may consume caller-provided local files only when both an
explicit enable variable and a source-directory variable are set. It reports
only aggregate counts and typed failure classes: it never prints paths, source
identifiers, raw bytes, text, tool arguments/results, thinking, media data, or
JSON parser excerpts. Its checks include complete conversion, raw hash
round-trip, addressable frontier count, Postgres reconstitution, and
second-import idempotency.

## Open edges

- Codex sessions and older backup formats have no converter yet. Adding one
  requires a new format variant, converter implementation, synthetic fixtures,
  and persistence round-trip coverage; it does not reinterpret Claude Code
  converter version 1.
- Import discovery, directory traversal, file watching, bulk-import policy,
  source-size admission, client presentation, and raw-record access surfaces are
  not implemented.
- Rich model rendering of imported source events, content absence, tools,
  results, thinking, and media is not implemented; the conservative version-1
  projection is owned by [model-call-execution](model-call-execution.md).
- Imported-conversation archive, retention, and destructive deletion policy
  remain part of the open
  [archive lifecycle](../open-questions.md#archival-and-retention).

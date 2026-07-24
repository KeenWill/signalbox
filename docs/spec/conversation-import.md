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
at least one normalized entry. Application orchestration rejects a converted
aggregate carrying any conversation or entry identity that the hub did not
supply to that conversion invocation.

Imported entries never carry an `AcceptedInputId`, `TurnId`, `TurnAttemptId`,
`ModelCallId`, native tool identity, or native terminal evidence. They record
what an external source contained; they do not establish that Signalbox accepted
input, authorized or attempted a call, ran a tool, or observed an outcome.
Ingestion performs no session, scheduler, slot, turn, attempt, model-call, tool,
durable-command, or outbox transition.

Why: treating external history as native execution would fabricate the evidence
chain required by the native lifecycle invariants.

Ingestion is idempotent and future-use-neutral. The source-content digest is
SHA-256 over this exact preimage:

1. the ASCII domain tag `signalbox.imported-conversation.source-digest.v1`,
   prefixed by its unsigned 64-bit big-endian byte length;
2. the ASCII format tag `claude-code-session-jsonl-v1`, prefixed by the same
   length encoding;
3. the raw-record count as an unsigned 64-bit big-endian integer; and
4. for each raw record in physical order, its 32-byte SHA-256 content hash
   prefixed by the unsigned 64-bit big-endian value 32.

The one-record synthetic vector whose exact raw bytes are hexadecimal `7b7d` has
raw hash `44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a` and
source-content digest
`b836a3fb00465c2c7ec01cf2c4b2c98845cbc9cdaf28892b910ce225d2079a5c`.

Reingesting the same format and exact raw record sequence returns the existing
imported conversation identity; caller-supplied candidate identities from that
attempt are discarded. A changed raw sequence is a new immutable snapshot with a
new identity. Common raw records are still deduplicated by content hash.

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

Each occurrence additionally stores an `ImportedRawRecordConversionDigest` that
authenticates its exact raw hash and complete normalized structured value
without moving JSON parsing out of the edge converter. Reconstitution derives
the digest again and fails typed corruption before trusting a mismatched
normalized record or its entry projection.

The conversion digest is SHA-256 over a preimage beginning with the
length-framed ASCII domain tag
`signalbox.imported-conversation.raw-record-conversion.v1`, then the
length-framed 32-byte raw hash, then one recursively encoded structured value.
Lengths and collection counts are unsigned 64-bit big-endian integers. The value
tags are `00` null, `01` false, `02` true, `03` number, `04` string, `05` array,
and `06` object. Number spellings and string UTF-8 bytes follow their tag with a
byte length. Arrays follow their tag with an element count and the encoded
elements. Objects follow their tag with a member count and, in exact order for
every member including duplicates, the name's length-framed UTF-8 bytes and
encoded value. For the source-content vector above normalized as an empty
object, the conversion digest is
`3d06f834c1c2fddbbf454716da309af393d15530870d969f4e73b4960ae90793`.

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

Global imported positions are one-based and contiguous across the conversation.
Positions within one raw-record occurrence are likewise the one-based contiguous
sequence `1..=K` for that record's `K` emitted entries and restart at `1` for
the next raw occurrence. A source event, text message, or content-absence record
that emits one entry therefore uses within-record position `1`; an array block
at zero-based source index `i` uses within-record position `i + 1`.

Each source field is independently `Attested(value)`, `AttestedAbsent`, or
`NotAttested`. JSON `null` maps to `AttestedAbsent`; an omitted field maps to
`NotAttested`. The converter never derives a missing value from a filename,
neighboring record, wall clock, or another field. Sidechain and metadata flags
are provenance, not exclusion: they do not remove content or make an imported
frontier unseedable.

Claude Code version 1 maps the four text-valued provenance fields from the exact
top-level members `uuid` (source record identifier), `parentUuid` (source parent
record identifier), `sessionId` (source session identifier), and `timestamp`
(source timestamp). For each, omission maps to `NotAttested`, JSON `null` maps
to `AttestedAbsent`, and a JSON string maps to `Attested(exact text)`; every
other JSON type rejects the complete conversion. It maps the sidechain and
metadata-record flags from the exact top-level members `isSidechain` and
`isMeta`, with the same omission/null behavior, an attested JSON Boolean value,
and rejection for every other type. Repeating any of these six consulted members
rejects the complete conversion.

Imported text retains the exact decoded Unicode scalar sequence, including an
empty sequence, whitespace, line endings, normalization distinctions, and
U+0000. Imported structured values use a source-neutral JSON algebra rather than
`serde_json` or provider wire types. They retain decoded scalar values, array
order, and every object member. A JSON number is an arbitrary-length string that
must match the complete RFC 8259 number grammar; normalization retains its exact
source token and never converts through an integer or binary floating-point
type. Thus `9007199254740993`, `1e400`, and distinct valid spellings such as `1`
and `1.0` remain exact and distinct. Raw records remain the authority for
whitespace, string-escape spellings, delimiters, and other lexical details.
Paired JSON UTF-16 surrogate escapes decode to their one Unicode supplementary
scalar. A lone high surrogate, lone low surrogate, or mismatched pair has no
decoded Unicode scalar sequence and rejects the complete conversion as invalid
JSON; it is never replaced or retained as a pseudo-character.

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
consumes source bytes, one caller-supplied conversation identity, and a total
lazy callback that supplies hub-minted imported-entry identities. After it has
completely parsed and normalized the source, the converter invokes that callback
exactly once immediately before emitting each normalized entry, in global
physical entry order; it neither preallocates identities nor invokes the
callback for an entry it does not emit. The callback's return type is an
identity, not an option or result, so exhaustion is deliberately unrepresentable
at this seam: a caller must provide one identity for every invocation. A
duplicate identity or any later aggregate failure rejects the complete
conversion without retrying or reusing consumed candidates.

The converter returns one completely checked domain aggregate or a typed
conversion error. The application calls the append-only store once only after
complete conversion; a conversion failure performs no durable write.

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
conversion. Version 1 never strips a UTF-8 byte-order mark: the bytes `EF BB BF`
at the beginning of any physical record are not JSON whitespace and reject that
record as invalid JSON.

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
   `document` blocks map to their corresponding normalized variants using these
   exact consulted members:

   - `text.text` supplies the text attestation;
   - `tool_use.id`, `.name`, `.input`, and `.caller` supply call identity, tool
     name, structured input, and caller metadata;
   - `tool_result.tool_use_id`, `.is_error`, and `.content` supply call
     identity, error status, and result content. Omitted content is
     `NotAttested`, null content is `AttestedAbsent`, string content is exact
     text, and array content is an ordered result-block sequence;
   - `thinking.thinking` and `.signature` supply exact thinking and signature;
   - `redacted_thinking.data` supplies exact redacted data;
   - `document.source` supplies the media source; and
   - the exact block discriminator `"fallback"` maps to `SourceMessageBlock`;
     its consulted `type` member supplies the source-block type attestation,
     while `from`, `to`, and every other member remain in the complete
     normalized owning record. Every other unrecognized discriminator rejects.

   A tool-result `text.text` supplies its text attestation, `image.source`
   supplies its media source, and `tool_reference.tool_name` supplies its
   tool-name attestation. Every media source consults exactly `type`,
   `media_type`, and `data`.

5. An unknown content shape, content-block type, or tool-result block type fails
   the complete conversion rather than being silently dropped or guessed.

For every consulted text member, omitted, null, and string map respectively to
`NotAttested`, `AttestedAbsent`, and `Attested(exact text)`; any other JSON type
fails conversion. Consulted booleans follow the same rule with a Boolean value.
`tool_use.input` and `tool_use.caller` admit any non-null source-neutral JSON
value. `tool_result.content` instead admits only the exact string or array
shapes specified above; Boolean, number, and object values reject the complete
conversion. Consulted media sources admit omitted, null, or an object whose
three consulted members follow the text rule; every other shape fails. Each
content or result block must be an object with exactly one consulted `type`
member containing a recognized string. As above, repeating any consulted member
at its object level fails the complete conversion. These rules apply
independently, so a missing or null `tool_use.id` remains typed absence while a
non-string value is invalid.

Version 1 accepts both user/final-response-only records and records containing
structured tool traffic, signed thinking, image results, tool references,
document blocks, model-fallback notices, attachments, and administrative source
events.

Malformed JSON, a blank line, invalid UTF-8, unsupported content, an identity
collision inside the candidate set, a position overflow, JSON deeper than 128
nested array or object containers, or a source with no JSON records rejects the
complete conversion. Container depth is the count of arrays and objects on one
root-to-value path: the required top-level record object has depth `1`, entering
each child array or object adds `1`, and scalars add nothing. Depth `128` is
admitted and attempting to enter a container at depth `129` rejects the whole
source. The same count applies to every complete source record and modeled
nested value. U+0000, empty strings, and a source containing only non-message
records do not: raw and normalized storage retain them.

## Persistence and reconstitution

The Postgres representation uses append-only `imported_raw_source_record` blobs,
`imported_conversation` headers, `imported_conversation_raw_record` occurrences,
and `imported_transcript_entry` members. Imported text and opaque media data use
UTF-8 `bytea`; complete structured records and nested values use a checked
adapter encoding of the domain algebra, never provider JSON as a domain type.
Every encoded top-level value carries a fixed format version and payload-kind
discriminator; a decoder rejects a value from another column kind rather than
reinterpreting it. Encoded collection counts bound parsing but never directly
drive capacity allocation: collections grow fallibly after each decoded element.

One transaction resolves or inserts a complete aggregate:

- an existing format/source-content digest must reconstitute completely and
  match the candidate conversion before returning `AlreadyImported`. Equality
  includes every exact raw record, normalized structured record, conversion
  digest, entry position, raw/within-record position, speaker attestation,
  content, and source metadata; only the candidate conversation and entry
  identities are excluded. A semantic mismatch is typed
  `ExistingSnapshotMismatch`, never accepted as replay;
- a new digest inserts or verifies every content-addressed raw blob, then
  atomically inserts one header, every raw occurrence, and every normalized
  entry; a concurrent header-insert loser re-inspects and completely
  reconstitutes the winner, returning `AlreadyImported` only after the same
  conversion-equivalence check, and raw-blob insert conflicts likewise reload
  and verify the winning bytes before reuse; and
- every raw occurrence stores and rechecks its conversion digest before its
  normalized value is accepted; and
- deferred constraints require exact declared counts, contiguous positions,
  globally distinct imported-entry identities, valid raw-record references, and
  agreement between every member's owner and header.

No partial aggregate can commit (INV-038).
`ImportedConversationRepository::load` returns `None` only when the requested
header does not exist. Once a header exists, a hash mismatch, missing blob or
member, gap, duplicate, unknown discriminator/version, contradictory variant
columns, invalid source value, or domain correlation failure is typed
corruption. Complete storage records pass through the domain-owned
reconstitution seam; adapters never default or drop a malformed value. For
Claude Code version 1, that seam independently re-derives every expected entry
from each complete normalized record and requires exact agreement in entry
count, order, content, speaker, and source metadata. It also reapplies the
128-container bound to complete records and entry-carried structured values
(INV-002).

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

- Exact mappings for additional source formats and the unimplemented import
  operational surfaces remain in the
  [conversation-import questions](../open-questions.md#conversation-import).
- Rich model rendering of imported source events, content absence, tools,
  results, thinking, and media remains in the
  [model-input projection questions](../open-questions.md#model-input-projection);
  the conservative version-1 projection is owned by
  [model-call-execution](model-call-execution.md).
- Imported-conversation archive, retention, and destructive deletion policy
  remain part of the open
  [archive lifecycle](../open-questions.md#archival-and-retention).

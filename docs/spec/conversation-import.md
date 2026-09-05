# Conversation import

This page specifies immutable imported conversation snapshots, raw source-record
preservation, source-neutral normalization, addressable imported frontiers, the
format-versioned converter seam, Claude Code session and Codex rollout JSONL
converters, the append-only Postgres import store, evidence-derived display
titles, the user-operated one-file and directory-scan import surfaces, and the
imported-conversation inspection read. Session creation from one imported
frontier is owned by [sessions-and-transcript](sessions-and-transcript.md);
native turn activation and model-call rendering are owned by
[turn-lifecycle-and-scheduling](turn-lifecycle-and-scheduling.md) and
[model-call-execution](model-call-execution.md).

## Record and ingestion boundary

An imported conversation is durable record, never execution. It has one
daemon-minted `ImportedConversationId`, one closed-source format and converter
version, one source-content digest, an immutable nonempty sequence of raw source
record occurrences, and an immutable nonempty sequence of normalized
`ImportedTranscriptEntry` values (INV-001, INV-038). Every raw record produces
at least one normalized entry. Application orchestration rejects a converted
aggregate carrying any conversation or entry identity that the daemon did not
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
2. the converter version's ASCII format tag (`claude-code-session-jsonl-v1`,
   `claude-code-session-jsonl-v2`, or `codex-rollout-jsonl-v1`), prefixed by the
   same length encoding;
3. the raw-record count as an unsigned 64-bit big-endian integer; and
4. for each raw record in physical order, its 32-byte SHA-256 content hash
   prefixed by the unsigned 64-bit big-endian value 32.

The one-record synthetic vector whose exact raw bytes are hexadecimal `7b7d` has
raw hash `44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a`. Its
version-1 source-content digest is
`b836a3fb00465c2c7ec01cf2c4b2c98845cbc9cdaf28892b910ce225d2079a5c`; its
version-2 source-content digest is
`117ac9599571f7ff2839069ae5252236d79ea148fe518baa1f914d629fba00df`. The same
vector's Codex rollout version-1 source-content digest is
`67666ac67ac3b0215f3b5e5e74968c8e2f2ee7574718f4779173696cecf624df`.

Reingesting the same format and exact raw record sequence returns the existing
imported conversation identity; caller-supplied candidate identities from that
attempt are discarded. A changed raw sequence is a new immutable snapshot with a
new identity. Common raw records are still deduplicated by content hash.

The digest is not a source session identifier or filename key. No source path,
wall-clock import time, adoption choice, target session, or future-use policy
participates in it. The imported aggregate is separate from `Session`, and an
import neither creates nor mutates a session.

A newly inserted `imported_conversation` header also carries nullable
`source_session_id` lineage evidence. The value is the exact UTF-8 bytes of the
converter-extracted source-session identifier when at least one record attests
one and every attested source-session identifier in the snapshot is equal.
Omitted and explicit-null fields do not supply evidence; conflicting attested
identifiers make the header value `NULL`. The nullable byte value has a
non-unique equality index so callers can group every exact snapshot carrying the
same evidence. Checked loading rejects a non-null header value that disagrees
with the source-session evidence reconstituted from its entries; `NULL` remains
unknown. It never participates in the source digest, the `imported_conversation`
identity, or the unique source-identity constraint. The importer never derives
this evidence or any identity from a filename, source path, neighboring record,
or import-time context.

Why: retrying or copying the same source must not duplicate history, while an
append or edit cannot mutate the snapshot that an existing session already
names.

## Raw source records

Every nonempty physical JSONL record is preserved before normalization. A raw
record blob stores the exact bytes between line delimiters under their SHA-256
identity; a conversation occurrence stores the blob digest and a positive
contiguous physical-record position. Line delimiters and source paths are not
part of a record. Duplicate content in one or many conversations creates
distinct ordered occurrences referencing one content-addressed blob.

One source admits at most 65,536 physical records. Conversion counts records in
physical order before any per-record blob publication and rejects record 65,537
with the typed `raw_record_count_exceeded` conversion class and that one-based
ordinal. The fixed count bounds per-object publication, catalog work, relational
members, and the time one import can retain the process-wide bulk-ingest permit
independently of its source-byte ceiling.

The verified blob is the raw-byte authority; PostgreSQL stores only its ordinary
blob digest and occurrence relationships, never a second `bytea` copy.
Exact-byte loading through that reference preserves JSON key order, whitespace,
escapes, number spelling, empty strings, and U+0000 even when normalization has
a different typed representation. A referenced blob whose bytes disagree with
its digest is typed corruption and fails closed; equality is never inferred from
an unverified hash at a checked boundary.

Each occurrence also carries the complete source JSON object normalized into the
source-neutral structured-value algebra. Non-message records produce a typed
`SourceEvent` entry rather than being dropped or recast as conversation text.
Under Claude Code version 2, a source-defined message block without a more
specific normalized variant produces a typed `SourceMessageBlock`, so its
boundary and type remain explicit while the complete normalized owning record
retains every block field. Codex rollout version 1 applies the same generic
variant to source-defined message and reasoning blocks. The normalized sequence
and every entry's raw-record reference make each conversion decision traceable
back to exact source bytes.

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

Imported entries keep their attested source timestamps; nothing is restamped to
import time.

**Committed unimplemented functionality.** No imported entry carries usage
evidence today. Where one does, dollar cost derives at read time from the window
covering that attested timestamp under the same
[configuration and credentials](configuration-and-credentials.md#the-static-model-alias-and-web-fetch-catalog)
contract native calls use, and no dollar amount is persisted.

Claude Code versions 1 and 2 map the four text-valued provenance fields from the
exact top-level members `uuid` (source record identifier), `parentUuid` (source
parent record identifier), `sessionId` (source session identifier), and
`timestamp` (source timestamp). For each, omission maps to `NotAttested`, JSON
`null` maps to `AttestedAbsent`, and a JSON string maps to
`Attested(exact text)`; every other JSON type rejects the complete conversion.
They map the sidechain and metadata-record flags from the exact top-level
members `isSidechain` and `isMeta`, with the same omission/null behavior, an
attested JSON Boolean value, and rejection for every other type. Repeating any
of these six consulted members rejects the complete conversion.

Codex rollout version 1 maps source record identity from exact `payload.id`,
source session identity from exact `payload.session_id`, and source timestamp
from exact top-level `timestamp`. The other three metadata fields are
`NotAttested`; the converter does not infer them from session metadata,
filenames, adjacent records, item call identities, or item kinds. Omission,
null, string, wrong-type, and repeated-member handling follows the same
attestation rules above. A recognized `response_item` message also maps its
exact `user` or `assistant` role into both speaker and message-role
attestations. Every other role remains a `SourceEvent` with no speaker
assertion, while the complete normalized record retains the exact source role.

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
  normalized record for anything not normalized as a user or assistant message;
- `SourceMessageBlock`, retaining the source block-type attestation and complete
  normalized owning record for a source-defined message block;
- `Text`, retaining attested exact user or assistant text or the field's precise
  typed absence;
- `ToolCall`, retaining independently attested source call identity, tool name,
  structured input, and caller metadata;
- `ToolResult`, retaining independently attested source call identity, error
  flag, and either exact text or an ordered sequence of typed text, image, and
  tool-reference result blocks whose own fields also retain attestations; a
  source-defined result block without a more specific normalized variant retains
  its exact block-type attestation as `SourceResultBlock`, while the complete
  normalized owning record retains every field;
- `Thinking`, retaining independently attested exact thinking and signature;
- `RedactedThinking`, retaining the source's independently attested redacted
  data;
- `Document`, retaining independently attested media source kind, media type,
  and exact data; and
- `MessageContentAbsent`, distinguishing an omitted or explicit-null message,
  omitted or explicit-null message content, and an attested empty content-block
  array.

An absent field is typed absence, never a placeholder string, empty object,
guessed tool name, or summary. Exact raw bytes back every normalized variant;
normalization retains every supported semantic field regardless.

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
repair, or use it to reorder either version's frontiers. Duplicate source
identifiers, missing parents, nonlinear parents, sidechains, and metadata
records do not change the physical prefix. They also do not prevent a client
from later selecting any imported frontier.

Why: a stable prefix boundary is available for every observed entry even when
source ancestry is incomplete or ambiguous; adjacency is not recast as proof of
external causality.

## Converter seam

`ImportedConversationConverter` is the application-facing edge seam. A converter
consumes source bytes, one caller-supplied conversation identity, and a total
lazy callback that supplies daemon-minted imported-entry identities. After it
has completely parsed and normalized the source, the converter invokes that
callback exactly once immediately before emitting each normalized entry, in
global physical entry order; it neither preallocates identities nor invokes the
callback for an entry it does not emit. The callback's return type is an
identity, not an option or result, so exhaustion is unrepresentable at this
seam: a caller must provide one identity for every invocation. A duplicate
identity or any later aggregate failure rejects the complete conversion without
retrying or reusing consumed candidates.

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

Blob-bearing import conversion is committed unimplemented functionality: no
present surface supplies a blob-backed source directly to a converter. The
compatibility constraint is that such a path streams from the blob substrate
through conversion without materializing the whole blob. The text-file and
chunked socket paths perform the bounded whole-source conversion described
below; this streaming seam does not reinterpret an existing converter version.

## Operational surface

The user terminal provides the single-file form,
`signalbox import --format <claude-code|codex> <file>`, and the directory form,
`signalbox import --format <claude-code|codex> --scan <dir>`. Exactly one file
or scan directory is required. Format selection is explicit in both forms:
`claude-code` selects `ClaudeCodeSessionJsonlV2`, and `codex` selects
`CodexRolloutJsonlV1`. No source path is inferred from an environment or fixed
home-directory convention.

The single-file form reads exactly the named file and performs no neighboring
file inspection. Scan mode first traverses the complete tree rooted at the named
directory without following symbolic links. Traversal opens the root and each
descendant directory through no-follow descriptors; later candidate reads reopen
every relative path component from the retained root descriptor with no-follow
semantics, so replacing a queued path with a symbolic link cannot redirect the
read outside that tree. Final candidate opens are nonblocking and the opened
descriptor must still name a regular file, so a special-file replacement is
skipped instead of blocking the scan. Traversal selects only regular files whose
extension is exactly lowercase `.jsonl`, sorts their full paths, and imposes no
candidate-count cap. A traversal failure aborts before any request rather than
hiding an unread subtree. Each candidate is then read and sent through one
import operation in that sorted order; scan mode has no protocol request or
server-side batching of its own. An operation uses `import_conversation` when
the exact single-shot frame fits and the chunked request sequence otherwise.

For every candidate, the terminal prints an escaped, quoted local path and one
`imported`, `already_imported`, or `skipped` outcome. Successful outcomes name
the imported conversation identity. A skipped outcome carries the exact client
error; it means the client did not receive a definitive successful receipt, not
that an ambiguity reason proves the request uncommitted. Processing continues
after a skip. A final summary prints the uncapped imported, already-imported,
and skipped counts; any skip makes the invocation fail after the remaining
candidates are processed, while an empty matching set succeeds with zero counts.
The source path is local presentation only and is never transmitted or
persisted.

The wire encodes exact source bytes as canonical padded base64 under the 8 MiB
per-frame bound. For a file whose descriptor metadata size could fit one frame,
the terminal reads the complete bytes, constructs the exact worst-case
single-shot request envelope, and uses the
`import_conversation { format, source }` request when that frame fits. For a
larger file it opens one connection, declares the descriptor metadata size in
`begin_conversation_import { format, declared_size_bytes }`, streams the file
through nonempty `append_conversation_import { chunk }` requests carrying at
most 4 MiB of decoded bytes apiece, and finishes with
`commit_conversation_import {}`. Acknowledgements echo the declared size at
begin and cumulative assembled size at each append.
`abort_conversation_import {}` explicitly discards a partial assembly;
disconnect has the same effect. Invalid base64 remains a malformed frame.

The daemon configuration's optional `conversation_import.max_source_bytes` is
the maximum assembled source size and defaults to 268,435,456 bytes (256 MiB).
Begin rejects a larger declared size before retaining source bytes. Commit
rechecks the bound and requires actual appended bytes to equal the declaration,
so a file that changes size after metadata observation is rejected with both
exact counts. The assembly and import permit are per-connection state. Commit,
abort, a terminal size or conversion rejection, and disconnect release them; an
`already_in_progress` refusal leaves the existing assembly available for append,
commit, or explicit abort. Commit then supplies the whole assembled source to
the same converter and `ImportConversationService` call as the single-shot path.
The service executes away from asynchronous runtime workers against the
append-only Postgres repository and admits one in-progress or single-shot import
at a time.

Every import refusal is `invalid_request` with typed, content-silent evidence.
State refusals name `conversation_import_already_in_progress` or
`conversation_import_not_in_progress`. Size refusals name the configured limit,
declared size, and actual size when append or commit knows it; a
declaration/assembly mismatch names both counts. Converter refusals use the
closed class and ordinal inventory in the
[process protocol's conversation-import refusal mapping](process-protocol.md#server-messages).
Errors and logs contain classes and ordinals only, never source content,
source-derived identifiers, paths, or parser excerpts. Database failure is
`commit_ambiguous`, so the operator may retry the exact format and source bytes.
Assembly allocation exhaustion or blob-store unavailability is `unavailable`;
blob integrity failure is `internal`.

A new exact snapshot returns
`conversation_import_inserted { imported_conversation_id }`; exact reingestion
through either transport returns
`conversation_import_already_imported { imported_conversation_id }` naming the
existing identity. The terminal prints these as distinct `inserted` and
`already_imported` outcomes with that identity. Neither outcome creates or seeds
a session, and changed raw-record content or order creates a new exact snapshot
under the identity model above.

## Imported-conversation inspection

An import prints only the imported conversation's identity, while
[later session creation](sessions-and-transcript.md) selects one inclusive
imported position. `signalbox imported <imported-conversation-uuid>` is the read
that makes those positions observable: it sends one `read_imported_conversation`
request and prints one line per normalized entry plus a final `entry_count`.
Each line names the entry's one-based imported position, its imported entry
identity, its exact source-speaker attestation, its normalized content kind, and
— for an entry whose content is attested `Text` — that text's exact leading
scalars bounded to 256 UTF-8 bytes with an explicit truncation marker. The
complete message sequence and its bounds are owned by the
[process protocol](process-protocol.md#server-messages).

The read exposes no imported content a transcript snapshot does not already
carry: it bounds exactly the attested text that snapshot carries in full and
adds nothing for source events, tools, results, thinking, media, absence detail,
or raw records. It reads the normalized relational projection and never fetches
verbatim raw-source blobs. The read creates nothing, seeds no session, and
performs no durable write; stored entry positions, identities, typed content,
speaker attestations, and source metadata are decoded fail-closed before
presentation.

`signalbox continue` consumes those positions. Its `--through-position` is
required, and it accepts either a positive decimal or the exact sentinel
`latest`. The client resolves `latest` against this read's entry count before
constructing the durable command, prints the resolved ordinal, and sends that
concrete position. Because an imported conversation is immutable, the resolution
is stable and an exact replay names the same boundary. An out-of-range position
on an existing imported conversation is a rejection naming the selectable range,
not an absent-identity `not_found`; both classifications are owned by the
[process protocol](process-protocol.md#server-messages).

### Bounded browser discovery and continuation

The browser HTTP contract exposes the same immutable imports as a selective read
model rather than adapting the complete inspection read above.
`GET /api/imports/` returns at most 100 summaries in ascending
`ImportedConversationId` order. An optional `after` identity is an exclusive
keyset cursor; the response includes a next cursor only when a bounded lookahead
finds another row. Exact format/converter filters compose with that cursor.
Exact attested source-session filters use the bounded raw `text/plain` body of
`POST /api/imports/searches`. The body is the exact UTF-8 identifier, preserving
empty text and edge whitespace, while avoiding URL expansion. A client-selected
correlation UUID and SHA-256 of the complete exact value are echoed so truncated
evidence remains unambiguous. Catalog and descriptor responses project at most
512 UTF-8 bytes of source-session evidence with explicit complete/truncated
classification. The response has no total count and never reconstructs a
complete imported aggregate.

`GET /api/imports/{imported-conversation-id}` returns the immutable identity,
evidence-derived display title, raw-record and normalized-entry counts, exact
source format and converter version, source digest, optional consistent source
session evidence, and first and latest continuation frontiers. Its three size
facts are sums of raw source-record occurrence bytes, normalized source-record
encoding bytes, and normalized entry plus source-metadata encoding bytes. They
are descriptors only: this route returns no raw blob, normalized record, host
path, or source repository location.

`GET /api/imports/{imported-conversation-id}/entries` selects a window around
`first`, `latest`, or one exact positive `position`. The requested neighbors
plus the anchor may total at most 101 entries. PostgreSQL reads and checked
decoding cover only that contiguous immutable range; neither this route nor its
browser scenario calls the complete aggregate loader. Every returned entry names
its imported entry identity, global position, raw-record and within-record
positions, source-speaker attestation, normalized content kind, and continuation
frontier. Attested text includes at most 512 UTF-8 bytes at a scalar boundary
plus an explicit complete/truncated classification; other normalized content
remains a typed descriptor for the blob and rendering surfaces owned elsewhere.
First/latest bounds and every entry in a selected window are the available
continuation positions. A supplied position outside an existing immutable
timeline is a typed bad request rather than storage corruption.

The browser labels all these rows as imported source evidence. A source role,
tool-shaped record, result, or other normalized kind does not become native
Signalbox acceptance, turn, call, tool, or result evidence through projection.

`POST /api/imports/{imported-conversation-id}/continuations` creates a native
session from one selected frontier, whose durable semantics are owned by
[sessions-and-transcript](sessions-and-transcript.md), with `resume` or `fork`,
one exact direct model-selection or model-alias identity, and provider defaults.
The client mints and retains the durable command identity before I/O. Exact
replay returns the recorded session, conflicting reuse is rejected, and an
ambiguous commit instructs the client to retry the same command and payload. The
server verifies that the immutable entry identity and position still agree
before applying the imported-frontier session-creation command. The response
returns the new session identity and selected frontier; session timeline
navigation is the separate browser timeline address contract.

## Claude Code session JSONL versions 1 and 2

`ClaudeCodeJsonlConverter` implements
`ClaudeCodeSessionJsonl { converter_version: 2 }`. Version 1 is the
interpretation for stored version-1 snapshots. Both versions parse one JSON
object per nonempty line, raw-preserve every record, and process records in
physical file order. They scan for LF bytes. An LF ends and is excluded from a
record; an immediately preceding CR is also excluded as the other half of a CRLF
delimiter. A CR anywhere else remains record content. Nonempty bytes after the
final delimiter form a final unterminated record, while a terminal LF or CRLF
does not create another record. An empty delimited record rejects the complete
conversion. Neither version strips a UTF-8 byte-order mark: the bytes `EF BB BF`
at the beginning of any physical record are not JSON whitespace and reject that
record as invalid JSON.

The parser retains object-member order and duplicate names in the complete
normalized source object. At every object level, repeating a member name that
the selected version consults to produce a normalized entry or attestation
rejects the complete conversion. Duplicate names inside otherwise unmodeled
structured values remain preserved and do not acquire fabricated selection
semantics.

Records then normalize as follows:

1. A top-level record whose `type` is neither `user` nor `assistant` produces
   one `SourceEvent` containing its type attestation and complete normalized
   object. Its source-speaker attestation is `NotAttested`: an omitted, null, or
   other source `type` is retained only as the independent record-type
   attestation and never reinterpreted as a speaker assertion.

2. For a user or assistant record, the top-level type supplies its attested
   speaker. The `message` envelope and its `role` are retained independently as
   attested, explicitly absent, or unattested; a present role must agree with
   the top-level type. A non-null envelope or role of the wrong JSON type fails
   conversion.

3. String message content produces one `Text` entry. Array content produces one
   entry per block, preserving block order within its source record. An omitted
   or null message, omitted or null content, or empty content array produces one
   precisely distinguished `MessageContentAbsent` entry.

4. In both versions, `text`, `tool_use`, `tool_result`, `thinking`,
   `redacted_thinking`, and `document` blocks map to their corresponding
   normalized variants using these exact consulted members:

   - `text.text` supplies the text attestation;
   - `tool_use.id`, `.name`, `.input`, and `.caller` supply call identity, tool
     name, structured input, and caller metadata;
   - `tool_result.tool_use_id`, `.is_error`, and `.content` supply call
     identity, error status, and result content. Omitted content is
     `NotAttested`, null content is `AttestedAbsent`, string content is exact
     text, and array content is an ordered result-block sequence;
   - `thinking.thinking` and `.signature` supply exact thinking and signature;
   - `redacted_thinking.data` supplies exact redacted data; and
   - `document.source` supplies the media source.

   A tool-result `text.text` supplies its text attestation, `image.source`
   supplies its media source, and `tool_reference.tool_name` supplies its
   tool-name attestation. Every media source consults exactly `type`,
   `media_type`, and `data`.

5. Version 1 maps the exact message-block discriminator `fallback` to
   `SourceMessageBlock`. Any other message-block discriminator without a
   specific variant, and every omitted or null message-block discriminator,
   makes the version-1 projection invalid. Its tool-result block vocabulary is
   likewise limited to `text`, `image`, and `tool_reference`; any other,
   omitted, or null result-block discriminator makes the projection invalid.

6. Version 2 maps any message-block discriminator without a specific variant,
   including an omitted or null discriminator, to `SourceMessageBlock`. It maps
   any result-block discriminator without a specific variant, including an
   omitted or null discriminator, to `SourceResultBlock`. Each generic variant
   retains the exact, explicitly absent, or unattested `type`, while the
   complete normalized owning record and verbatim raw record retain every other
   source field.

7. A malformed content shape still fails the complete conversion rather than
   being silently dropped or guessed. Version 2 does not reject a structurally
   valid source-defined block merely because its discriminator has no more
   specific normalized variant.

For every consulted text member, omitted, null, and string map respectively to
`NotAttested`, `AttestedAbsent`, and `Attested(exact text)`; any other JSON type
fails conversion. Consulted booleans follow the same rule with a Boolean value.
`tool_use.input` and `tool_use.caller` admit any non-null source-neutral JSON
value. `tool_result.content` instead admits only the exact string or array
shapes specified above; Boolean, number, and object values reject the complete
conversion. Consulted media sources admit omitted, null, or an object whose
three consulted members follow the text rule; every other shape fails. Each
content or result block must be an object with at most one consulted `type`
member; an exact string selects a specific or version-dependent generic variant,
omission and null follow the selected version's rules above, and any other value
shape fails. As above, repeating any consulted member at its object level fails
the complete conversion. These rules apply independently, so a missing or null
`tool_use.id` remains typed absence while a non-string value is invalid.

Both versions accept user/final-response-only records and records containing
their recognized structured tool traffic, signed thinking, image results, tool
references, document blocks, model-fallback notices, and administrative source
events. Version 2 additionally retains structurally valid source-defined message
and result blocks instead of rejecting their complete conversion.

Malformed JSON, a blank line, invalid UTF-8, malformed modeled content, an
identity collision inside the candidate set, a position overflow, JSON deeper
than 128 nested array or object containers, or a source with no JSON records
rejects the complete conversion. Container depth is the count of arrays and
objects on one root-to-value path: the required top-level record object has
depth `1`, entering each child array or object adds `1`, and scalars add
nothing. Depth `128` is admitted and attempting to enter a container at depth
`129` rejects the whole source. The same count applies to every complete source
record and modeled nested value. U+0000, empty strings, and a source containing
only non-message records do not. In version 2, unknown source block
discriminators likewise do not: raw and normalized storage retain them.

## Codex rollout JSONL version 1

`CodexRolloutJsonlConverter` implements
`CodexRolloutJsonl { converter_version: 1 }`. It uses the same exact physical
JSONL record, UTF-8, JSON structure, number, Unicode, 128-container depth,
duplicate-consulted-member, and blank-line rules specified for Claude Code
above.

The converter treats `response_item` as Codex's semantic conversation stream.
All other top-level item kinds, including `session_meta`, `turn_context`,
`event_msg`, `world_state`, `compacted`, and source-defined future kinds,
produce one `SourceEvent`. This retains administrative and presentation events
without reclassifying their mirrored text or tool progress as duplicate
conversation entries. A source event keeps its exact top-level `type`
attestation and complete normalized record.

A `response_item` must have an object-valued `payload`. Recognized payloads map
as follows:

1. A `message` with exact role `user` or `assistant` emits one entry per
   `content` block. `input_text` and `output_text` map exact `text` to `Text`;
   every other object-valued block maps to `SourceMessageBlock` with its exact
   type attestation. String-valued legacy content maps to one `Text`. Omitted,
   null, or empty-array content maps to the corresponding
   `MessageContentAbsent`. Other roles produce one `SourceEvent` and do not
   assert a user or assistant speaker.

2. A `reasoning` item emits one `Thinking` entry for every `summary_text`,
   `reasoning_text`, or `text` block in exact `summary` then `content` order.
   Its exact text is the thinking attestation and signature is `NotAttested`.
   Other object-valued blocks become `SourceMessageBlock`. An omitted
   `encrypted_content` emits no redacted entry, null emits
   `RedactedThinking(AttestedAbsent)`, and a string emits
   `RedactedThinking(Attested(exact data))`. If the item emits no specific
   entry, it remains one `SourceEvent`.

3. `function_call` and `custom_tool_call` map exact `call_id` and `name` to
   `ToolCall`. Their exact `arguments` or `input` JSON string remains a
   source-neutral structured string rather than being reparsed as nested JSON;
   raw and complete normalized records therefore retain its lexical form.
   `tool_search_call` and `local_shell_call` map exact structured `arguments` or
   `action` respectively, with no fabricated tool name. `web_search_call` maps
   exact item `id` as its call identity and structured `action` as input, also
   with no fabricated name. Codex does not attest caller metadata for these
   records.

4. `function_call_output` and `custom_tool_call_output` map exact `call_id` and
   `output` to `ToolResult`. Omitted, null, and string output become
   `NotAttested`, `AttestedAbsent`, and exact text. Array output preserves
   order: `input_text` becomes a text result block; `input_image` becomes an
   image result block whose kind and exact `image_url` are attested and whose
   media type is `NotAttested`; every other object-valued block becomes
   `SourceResultBlock`. Codex does not attest an error Boolean in these records.

5. `tool_search_output` maps exact `call_id` to `ToolResult`. Its omitted or
   null `tools` value is typed absence; an array emits one ordered
   `SourceResultBlock` per source element, retaining an object element's exact
   type attestation when present. The complete normalized record retains every
   tool field.

Every other structurally valid `response_item` payload type remains one
`SourceEvent`, including response-item variants introduced after this converter
version. A malformed modeled envelope, role, content, reasoning, tool call, tool
result, or consulted member rejects the complete conversion. No non-message
response item acquires a fabricated assistant speaker merely because Codex
produced it.

## Persistence and reconstitution

The Postgres representation uses append-only `imported_raw_source_record` blob
references, `imported_conversation` headers, `imported_conversation_raw_record`
occurrences, and `imported_transcript_entry` members. Imported text and opaque
media data use UTF-8 `bytea`; complete structured records and nested values use
a checked adapter encoding of the domain algebra, never provider JSON as a
domain type. Every encoded top-level value carries a fixed format version and
payload-kind discriminator; a decoder rejects a value from another column kind
rather than reinterpreting it. Encoded collection counts bound parsing but never
directly drive capacity allocation: collections grow fallibly after each decoded
element. Structured-value and source-metadata encodings are at version `1`.
Content without `SourceResultBlock`, including `SourceMessageBlock`, is at
version `1`. A content value containing `SourceResultBlock` uses version `2`;
content decoding retains the version-1 message-block tag and rejects the
result-block tag beneath a version-1 header.

Raw source bytes live in the blob store under their SHA-256 content hash; the
relational record stores that ordinary blob digest and never a second copy.
Ingestion publishes and verifies every raw blob before the aggregate
transaction, then registers all blob and replica rows in the same transaction
that first references them. One admitted import starts and awaits at most one
raw blob publication or verification operation at a time; the process-wide
bulk-ingest permit remains held across that sequential traversal, so one import
cannot fan out a record inventory into concurrent store operations. A failed
import can therefore leave deterministic unregistered store orphans but no
unreachable catalog rows. Loading first reads the complete append-only
relational projection and releases its transaction, then reads and verifies the
referenced blobs, so no database transaction spans store I/O. Conversion and
reconstitution are bounded whole-source operations; streaming conversion is the
committed unimplemented seam above. Each checked aggregate load — including
ordinary read, replay comparison, and imported-frontier reconstitution — has one
non-resetting 24-hour monotonic deadline shared across every referenced digest
and replica candidate. It performs at most one referenced-blob store operation
at a time; digest or candidate changes never restart the deadline. Every checked
load acquires the blob contract's shared 16-slot read-traversal admission
without waiting; when no slot is immediately available, it returns the ordinary
unavailable outcome and retains no queued connection task.

One transaction resolves or inserts a complete aggregate:

- an existing format/source-content digest must reconstitute completely and
  match the candidate conversion before returning `AlreadyImported`. Equality
  includes every exact raw record, normalized structured record, conversion
  digest, entry position, raw/within-record position, speaker attestation,
  content, and source metadata; only the candidate conversation and entry
  identities are excluded. A semantic mismatch is typed
  `ExistingSnapshotMismatch`, never accepted as replay;
- a new digest enters this transaction only after every content-addressed raw
  blob has been published and verified without an open database transaction,
  then atomically registers their blob and replica rows and inserts one header,
  every raw occurrence, and every normalized entry; a concurrent header-insert
  loser re-inspects and completely reconstitutes the winner, returning
  `AlreadyImported` only after the same conversion-equivalence check; writers
  acquire both shared raw hashes and globally unique imported-entry identities
  in their respective sorted key order while storing physical positions
  explicitly;
- every raw occurrence stores and rechecks its conversion digest before its
  normalized value is accepted; and
- deferred constraints require exact declared counts, contiguous positions,
  globally distinct imported-entry identities, valid raw-record references, and
  agreement between every member's owner and header. Checked loading also
  compares every raw occurrence's declared entry count with its reconstructed
  normalized members.

No partial aggregate can commit (INV-038).
`ImportedConversationRepository::load` returns `None` only when the requested
header does not exist. A resolved display-title column that disagrees with pure
re-derivation from the reconstituted records is likewise typed corruption
([derived display titles](#derived-display-titles) below). Once a header exists,
a hash mismatch, missing blob or member, gap, duplicate, unknown
discriminator/version, contradictory variant columns, invalid source value,
non-null source-session lineage mismatch, or domain correlation failure is typed
corruption. Complete storage records pass through the domain-owned
reconstitution seam; adapters never default or drop a malformed value. For each
Claude Code and Codex converter version, that seam independently re-derives
every expected entry using that version's fixed interpretation and requires
exact agreement in entry count, order, content, speaker, and source metadata. It
also reapplies the 128-container bound to complete records and entry-carried
structured values (INV-002).

The raw-source schema is blob-reference-only. The runtime repository accepts
only that shape, and imports write only blob references. The owning
[blob-storage configuration contract](blob-storage.md) defines when omitted
configuration is valid.

## Derived display titles

Every imported conversation carries one optional bounded display title on its
header row (`imported_conversation.display_title` plus the closed
`display_title_state` discriminator, migration
`202607290201_imported_conversation_display_title.sql`), derived once from the
preserved source records so the unified conversation listing owned by
[process-protocol](process-protocol.md) can present imported rows by name. The
title is presentation evidence, not identity: it never participates in the
source digest, the imported-conversation identity, or the unique source-identity
constraint. This section owns the derivation placement contract.

`ImportedConversationDisplayTitle::derive` reads only the immutable aggregate —
never a filename, wall clock, or import-time context — so re-deriving from the
same aggregate always returns the same value. Candidate strings are tried in a
fixed per-format order and the first candidate that shapes to a nonempty title
wins:

- Claude Code versions 1 and 2: for every raw record in physical order whose
  normalized value is an object whose first `type` member is the string
  `summary`, the string value of its first `summary` member; then every
  attested-text entry with an attested `user` speaker, in imported order.
- Codex rollout version 1: for every raw record in physical order whose
  normalized value is an object whose first `type` member is the string
  `session_meta` and whose first `payload` member is an object, the string value
  of the payload's first `title` member; then the string value of each such
  payload's first `instructions` member; then every attested-text entry with an
  attested `user` speaker, in imported order.

Each path step selects the first member with that exact name; an absent member
or a value of another shape exhausts that record without failing the derivation.
The shape of a candidate is its prefix up to the first line feed, carriage
return, or U+0000, with leading and trailing ASCII space and tab removed,
truncated to the first 256 Unicode scalars, and stripped of any
truncation-exposed trailing ASCII space or tab; an empty shape exhausts the
candidate. A conversation with no shapeable candidate has no display title,
recorded as the `underivable` state, never a fabricated placeholder. Storage
CHECK constraints enforce the derived shape — nonempty single-line text of at
most 256 scalars without edge ASCII whitespace, present exactly in the `derived`
state.

Insertion always resolves the title. The closed state discriminator admits only
`derived` and `underivable`; runtime reads and writes only that shape. Checked
complete loads re-derive and reject a resolved title that disagrees with the
records; exact reingestion resolves through the digest and
conversion-equivalence check.

## Test data and local validation

Committed tests and fixtures are entirely synthetic. An ignored opt-in
integration test may consume caller-provided local files only when both an
explicit enable variable and a source-directory variable are set. It reports
only aggregate counts and typed failure classes: it never prints paths, source
identifiers, raw bytes, text, tool arguments/results, thinking, media data, or
JSON parser excerpts. Its checks include complete conversion, raw hash
round-trip, addressable frontier count, Postgres reconstitution, and
second-import idempotency. Claude Code and Codex use separate opt-in variables,
so neither private corpus is selected implicitly.

## Open edges

- Exact mappings for further source formats and the unimplemented file-watching
  and raw-access surfaces remain in the
  [conversation-import questions](../open-questions.md#conversation-import).
- Rich model rendering of imported source events, content absence, tools,
  results, thinking, and media remains in the
  [model-input projection questions](../open-questions.md#model-input-projection);
  the conservative version-1 projection is owned by
  [model-call-execution](model-call-execution.md).
- Imported-conversation archive, retention, and destructive deletion policy
  remain part of the open
  [archive lifecycle](../open-questions.md#session-organization-visibility-and-retention).

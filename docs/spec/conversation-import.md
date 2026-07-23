# Conversation import

This page specifies the conversation-import stack rooted at the foundation
proposal `agent/conversation-import-spec`. It owns immutable imported
conversation records, source attestations, the format-versioned converter seam,
the first Claude Code JSONL converter, and the append-only Postgres import
store. Session ancestry, seed-session creation, and the semantic seed frontier
are owned by [sessions-and-transcript](sessions-and-transcript.md); native turn
activation and model-call rendering are owned by
[turn-lifecycle-and-scheduling](turn-lifecycle-and-scheduling.md) and
[model-call-execution](model-call-execution.md).

## Record boundary

An imported conversation is durable record, never execution. It has a hub-minted
`ImportedConversationId`, one closed source format and converter version, and an
immutable nonempty sequence of `ImportedTranscriptEntry` values. Every entry has
its own `ImportedTranscriptEntryId`, its owning conversation, and a positive
contiguous source-order position (INV-001, INV-038).

Imported entries never carry an `AcceptedInputId`, `TurnId`, `TurnAttemptId`, or
`ModelCallId`. They cannot establish that Signalbox accepted input, authorized
or attempted a call, ran a tool, or reached a native terminal disposition.
Importing performs no scheduling, slot, attempt, model-call, tool, or outbox
transition (INV-038).

Why: treating an external transcript as native execution would fabricate the
evidence chain that Signalbox's lifecycle invariants require.

The aggregate is separate from `Session`. Repeatedly importing the same source
creates distinct imported-conversation identities; source metadata is
provenance, not a deduplication key. A session may later name one imported
conversation as immutable ancestry, but neither aggregate embeds or owns the
other.

## Source attestations and content

Each entry retains exact source metadata independently:

- the source record identifier;
- the source parent record identifier;
- the source session identifier;
- the source timestamp;
- whether the record was marked as a sidechain; and
- whether the record was marked as metadata.

Every field is represented as `Attested(value)`, `AttestedAbsent`, or
`NotAttested`. JSON `null` maps to `AttestedAbsent`; an omitted field maps to
`NotAttested`. The converter never derives a missing value from the file name,
neighboring records, wall-clock time, or another entry.

Why: explicit absence and unavailable evidence have different meanings from a
source assertion, and filling gaps would turn an import heuristic into false
provenance.

An entry's speaker is the source-attested `User` or `Assistant`. Its content is
one of:

- `Text`, retaining the exact nonempty Unicode scalar sequence; or
- `Unavailable`, with a closed reason naming the source content class that the
  converter deliberately does not represent.

Whitespace-only text is text. Text containing U+0000 is rejected because the
accepted Postgres representation cannot retain it exactly. An empty text block
becomes typed `EmptyText` absence rather than fabricated whitespace.

Each entry also has a seed disposition. Ordinary user and assistant text is
`Included`; source records explicitly marked as sidechain or metadata are
retained but `Excluded` with that reason. Unavailable content is never
model-visible. Exclusion does not remove or rewrite the imported entry.

## Converter seam

`ImportedConversationConverter` is the application-facing edge seam. A converter
consumes source bytes plus caller-supplied hub identity candidates and returns
one completely checked domain aggregate or a typed conversion error. The
application persists only a complete successful conversion and calls the store
once; a conversion failure performs no durable write.

Every converter declares a closed `ImportedConversationFormat` containing both
the source family and the Signalbox converter version. Converter versions
describe Signalbox's interpretation, not a source application's release version.
A behavior change that could alter accepted records, entry boundaries,
attestations, content, ordering, or seed dispositions requires a new converter
version; an existing version is never reinterpreted.

Why: format quirks belong at the edge, while the domain and store consume one
stable source-neutral record shape.

The converter does not read files or choose paths. Its caller supplies bytes, so
a later Codex-session or backup converter implements the same seam without
adding filesystem types to the domain or application crates.

## Claude Code session JSONL version 1

`ClaudeCodeJsonlConverter` implements
`ClaudeCodeSessionJsonl { converter_version: 1 }`. It parses one JSON object per
nonempty line and processes records in physical file order:

1. Top-level records whose `type` is neither `user` nor `assistant` are
   non-transcript records and do not produce imported entries.
2. For a user or assistant record, `message.role` must exist and agree with the
   top-level type. Missing, unknown, or contradictory roles fail the complete
   conversion.
3. String message content produces one entry. Array content produces one entry
   per block, preserving block order within the source record.
4. A `text` block retains exact text. `tool_use`, `tool_result`, `thinking`, and
   `redacted_thinking` blocks produce typed unavailable entries and retain their
   source role and metadata; version 1 does not summarize their payload.
5. An unknown content shape or block type fails the complete conversion rather
   than being silently dropped or guessed.

The converter does not follow or repair `parentUuid` chains. It retains each
record's parent attestation and uses physical file/block order as the imported
order, including duplicate source identifiers. It does not infer that adjacency
proves causality.

Why: Claude Code session files contain non-message records, sidechains, metadata
messages, and tool traffic, while parent chains can be incomplete or nonlinear;
the converter must preserve what it observed without claiming more.

Malformed JSON, a blank line, invalid UTF-8, unsupported content, U+0000 in a
retained source string, identity collisions inside the candidate set, a position
overflow, or a source containing no user/assistant content blocks rejects the
complete conversion.

## Persistence and reconstitution

The Postgres representation uses an `imported_conversation` header and
`imported_transcript_entry` members. The header records format and converter
version plus the exact member count. Entry rows record contiguous position,
speaker, content or typed absence, seed disposition, and every source
attestation. Both tables are append-only.

One transaction inserts the header and every entry. Deferred constraints require
exactly the declared number of members, positions `1..=member_count`, globally
distinct entry identities, and agreement between every member's stored owner and
the header. No partial imported conversation can commit (INV-038).

`ImportedConversationRepository::load` returns `None` only when the requested
header does not exist. Once a header exists, missing members, gaps, duplicates,
unknown discriminators or versions, contradictory variant columns, invalid
source values, or domain correlation failures are typed corruption. Complete
storage records pass through the domain-owned reconstitution seam; the adapter
never defaults or drops a malformed entry (INV-002).

## Open edges

- Codex sessions and older backup formats have no converter yet. Adding one
  requires a new format variant, converter implementation, fixtures, and
  persistence round-trip coverage; it does not change version 1 of the Claude
  Code converter.
- Import discovery, directory traversal, file watching, bulk-import policy,
  user-facing duplicate detection, and client presentation are not implemented.
- Rich preservation or semantic summarization of tool, thinking, image, and
  other non-text blocks is unavailable. A later converter version may add a
  typed representation; version 1's unavailable entries remain unchanged.
- Imported-conversation archive, retention, and destructive deletion policy
  remain part of the open
  [archive lifecycle](../open-questions.md#archival-and-retention).

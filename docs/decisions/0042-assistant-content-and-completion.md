# ADR-0042: Assistant content and completed-turn semantic entries

- Date: 2026-07-20
- Supersedes: none
- Superseded by: none
- Depends on: [ADR-0001](0001-domain-terminology-and-identity.md),
  [ADR-0004](0004-turn-and-attempt-lifecycle.md),
  [ADR-0005](0005-model-call-retry-semantics.md),
  [ADR-0027](0027-input-delivery-lifecycle.md),
  [ADR-0030](0030-context-frontier-snapshots.md),
  [ADR-0036](0036-initial-semantic-transcript-entries.md), and
  [ADR-0037](0037-baseline-user-content.md)
- Refines: ADR-0036's closed semantic-entry payload set and ADR-0005's
  assistant-outcome commit boundary
- Resolves: assistant text, assistant tool-use references, and completed-turn
  semantic entries for the first model-call slice; later content and outcome
  variants remain open
- Decision questions: assistant-content variants and provenance; tool-use
  reference boundary; completed-turn marker; final-response commit and recovery
  atomicity; closed-set extension boundary

## Context

ADR-0036 deliberately admits only origin-accepted-input and failed-turn semantic
entries. ADR-0005 nevertheless requires authoritative assistant content to
remain correlated with the model call that produced it, and ADR-0027 requires a
completed predecessor's frontier to retain committed assistant content plus an
explicit completion marker. Those records do not define the payloads or the
transaction that makes a final provider response semantic history.

That gap blocks the first scripted-provider model-call slice. Treating final
text as a generic role message would lose its producing-call provenance.
Embedding provider tool-call blocks would make provider wire syntax or future
tool execution shapes part of semantic history before the reserved tool
decisions define them. Treating `ModelCallDisposition::Completed` as turn
completion would also collapse two different facts: a completed call may request
tools or otherwise leave its turn active.

The first slice needs a closed assistant-content algebra, an explicit completed
turn marker, and a commit boundary that preserves final output without turning
streaming drafts, provider wire values, or tool operations into canonical
semantic entries.

## Decision

### Closed payload extension

The complete semantic-entry payload set after this extension is:

```text
SemanticTranscriptEntryPayload =
    OriginAcceptedInput {
        accepted_input: AcceptedInputId
    }
  | TurnFailed {
        turn: TurnId
    }
  | AssistantText {
        producing_call: ModelCallId,
        value: AssistantText
    }
  | AssistantToolUse {
        producing_call: ModelCallId,
        request: ToolRequestId
    }
  | TurnCompleted {
        turn: TurnId
    }
```

This is semantic pseudocode, not a Rust enum, storage record, provider block,
wire message, or presentation model. ADR-0036 continues to own the first two
variants. The three variants introduced here are the only additional payload
variants admitted by this record; construction authority for `AssistantToolUse`
remains gated by the reserved tool decisions. There is no generic role, metadata
map, serialized provider response, content block, or “other” variant.

Every new entry retains ADR-0036's distinct `SemanticTranscriptEntryId` and
`source_session`. A referenced model call, tool request, or turn never becomes
the entry identity. Equal text, repeated tool references in separately
authorized occurrences, or equal turn dispositions do not canonicalize entries.

### Exact assistant text with producing-call provenance

`AssistantText` is a decoded, nonempty Unicode scalar-value sequence with no
U+0000. Its constructibility and exact preservation mirror ADR-0037's baseline
text value: whitespace-only text is valid, while trimming, newline rewriting,
Unicode normalization, case folding, markup interpretation, and semantic
equivalence are prohibited. Equality compares the exact ordered scalar-value
sequence. This value is assistant-owned content; it is not `UserContent` and
does not copy or acquire an accepted-input identity.

Each `AssistantText` entry names the exact outcome-authoritative `ModelCallId`
whose definitive final response supplied that text. Construction validates that
the call belongs to the entry's source session and owning turn, and that the
same serialized outcome transition makes the response material authoritative.
That can be the transition that physically completes a nonterminal call or a
later resolving-evidence transition that preserves an already-terminal
`Ambiguous` physical disposition while establishing its definitive response for
turn-level use. A call that is known failed, refused, cancelled, fatally
invalidated, or no longer outcome-authoritative cannot produce an
`AssistantText` entry. Refusal text is not reclassified as ordinary assistant
text; its semantic variant remains open.

The domain value has no maximum length. Admission limits, quotas, and provider
or protocol frame limits remain separate resource policy. They may reject before
canonical construction but cannot truncate, summarize, normalize, or replace
committed text.

### Tool use is a logical-request reference

`AssistantToolUse` records that the producing call's final semantic output
refers to one logical `ToolRequestId`. It also names that producing call
explicitly; provenance is not inferred from adjacency, provider syntax, or a
later tool attempt.

The entry carries no tool name, arguments, provider tool-call identifier, policy
result, approval state, placement, risk class, execution envelope,
`ToolAttemptId`, result, or error. Those facts belong to the authoritative
logical-request and execution records whose identity and lifecycle boundaries
are reserved to ADR-0011 through ADR-0014. Provider adapters may translate a
final provider block into a proposed logical request, but provider wire shapes
never enter this payload.

When the reserved decisions make tool-request creation available, the outcome
transaction must validate that the referenced request belongs to the same turn,
was derived from the named call's definitive final response, and is created or
already proven canonical under that same atomic boundary. A dangling,
cross-turn, cross-session, or differently producing request cannot construct the
entry. This reference boundary does not decide normalized argument shape,
policy, approval, dispatch, retry, result content, or whether any tool is
executable.

### Completed turn is an explicit aggregate fact

`TurnCompleted` references the exact turn that reaches
`TurnDisposition::Completed`. It is appended exactly once, by the transaction
that terminalizes that turn, after every semantic content or effect fact
supporting completion. Construction validates that the turn belongs to
`source_session`, every ADR-0004 terminal guard holds, and the same transaction
terminalizes the turn as completed. That transaction ends a current attempt when
one exists; when resolving evidence completes a turn already awaiting a recovery
decision, it instead validates the already-ended issuing attempt and atomically
closes that exact wait.

A physically completed model call does not imply a completed turn. A call that
produces one or more tool-use entries ends physically `Completed`, while the
turn remains active under its existing lifecycle until every logical dependency
and physical operation is closed and later orchestration establishes a
conversational outcome. No `TurnCompleted` marker exists on that path until the
turn itself completes.

The marker carries no model-call identity because it records the aggregate
outcome, not another provider response. The preceding assistant entries retain
their own producing calls. Failed, refused, cancelled, and
reconciliation-required turns use their distinct semantic outcome boundaries;
none may be represented as `TurnCompleted`.

### Final-response commit boundary

Provider interaction occurs outside a database transaction. Transient deltas and
provisional provider blocks remain replaceable drafts. Only one complete,
definitive provider result presented to the serialized owning-turn transition
may become semantic history.

For an outcome-authoritative successful response, one commit transaction:

1. validates the call, exact owning turn and attempt, source session, current
   lifecycle phase, outcome authority, and all provider-target evidence whose
   precedence can make response material non-authoritative;
2. either terminally classifies a nonterminal model call as `Completed`, or
   records resolving evidence while preserving an already-terminal `Ambiguous`
   call unchanged;
3. appends the complete ordered sequence of final `AssistantText` and
   `AssistantToolUse` entries, preserving the semantic order supplied by the
   checked adapter projection and creating or validating any referenced logical
   requests under the boundary above; and
4. either preserves the supported nonterminal turn lifecycle for unfinished
   work, or, when every ADR-0004 completion guard holds, ends the current
   attempt if one exists or closes the exact recovery wait after validating its
   already-ended attempt, terminalizes the turn as `Completed`, and appends
   `TurnCompleted` last.

No transaction may commit a prefix of the final assistant sequence, a tool-use
entry without its canonical logical request, a completed turn without its
marker, or the marker before its supporting content. A response can contain zero
or more assistant-content entries; accepting a content-free successful response
is adapter-contract scope, but any turn completion still requires the explicit
marker.

A crash before commit exposes none of the new call classification or resolving
evidence, final assistant entries, tool-request references, attempt outcome,
turn disposition, or completion marker from that transition. A restart or retry
after an uncertain commit reads the canonical result and exact committed entry
sequence; it cannot append a second sequence or marker. Startup evidence that
contains a definitive successful response uses the same validation, precedence,
and all-or-nothing boundary while ending the abandoned attempt under ADR-0004's
`Lost` rules. If the physical call was already `Ambiguous`, resolving evidence
never reopens or rewrites it. Evidence that cannot establish a definitive
response classifies the physical call without promoting a partial draft.

If authority transferred before the result commits, all later material from the
prior call remains audit or reconciliation evidence and creates no entries from
this record. If trusted mismatch evidence arrives after valid completion while
the turn remains active, ADR-0005 preserves already committed history and adds
its distinct invalidation semantics; it does not rewrite or delete these
entries. The semantic invalidation marker remains outside this record.

### Ordering, frontiers, and mappings

Assistant entries appear in final-response semantic order. A completed-turn
marker follows every entry produced or otherwise committed by that turn. Later
context frontiers retain those source-qualified identities and their order under
ADR-0030 and ADR-0027; renderers may group them as one visual message but cannot
merge identities, reorder them, or erase producing-call references.

Persistence uses closed, checked representations for the new payload kinds,
native typed references for `ModelCallId`, `ToolRequestId`, and `TurnId`, and
exact text preservation. Reading rejects an unknown kind, an invalid reference,
forbidden text, a missing provenance field, an impossible order, or a completion
marker unsupported by the reconstructed aggregate. A generic JSON provider block
or presentation message is not a semantic-entry mapping.

## Invariants

- INV-001: semantic-entry, model-call, tool-request, turn, and session
  identities remain distinct even when an entry references another identity.
- INV-004: a completed physical call and a logical tool request remain separate
  from assistant-content occurrences and turn completion.
- INV-005: final semantic content, operational call/request records, audit
  evidence, transient drafts, provider values, and client presentation remain
  distinct correlated representations.
- INV-006: only the guarded aggregate transition appends `TurnCompleted`; a
  terminal turn or call never reopens, including when resolving evidence makes
  an `Ambiguous` call's response usable at turn level.
- INV-014: producing-call provenance and outcome authority are checked before
  provider response material becomes authoritative, while later evidence never
  rewrites committed content.
- INV-015: later calls consume an exact frontier containing the ordered
  committed entries rather than an inferred visual message.
- INV-032: reconnect replaces drafts with the atomic durable sequence and never
  treats lost deltas as final content.

## Strongest alternative

Persist one extensible assistant message containing a role, provider-native
content-block array, optional related identities, and arbitrary metadata. It
would resemble common provider APIs and could carry text, tool calls, images,
files, refusals, and future content without schema evolution.

It is rejected because the optional relationships would make producing-call
provenance and tool-request identity conventions rather than domain rules.
Provider blocks would also import wire and SDK evolution into semantic history,
while a generic extension map would bypass the foundation decisions needed for
resource-bearing content.

## Rejected alternatives

- **Omit the producing call from text or tool use.** Adjacency and turn
  ownership cannot identify which physical interaction produced content when a
  turn owns several calls.
- **Embed tool name, arguments, policy, attempts, or results.** It would
  duplicate authoritative tool records and preempt ADR-0011 through ADR-0014.
- **Reference a provider tool-call identifier instead of `ToolRequestId`.**
  Provider syntax is neither Signalbox logical identity nor stable authority
  across adapters.
- **Treat call completion as turn completion.** A completed call can create tool
  work or precede another continuation, so the inference would append a false
  terminal marker.
- **Store one response blob instead of identified entries.** It hides content
  order and typed references inside serialization, prevents source-qualified
  frontier membership, and couples semantic history to one renderer.
- **Promote streaming chunks as entries.** Deltas can be duplicated, revised, or
  lost; making them canonical would violate reconnect replacement and atomic
  finality.
- **Use `UserContent` for assistant text.** The exact scalar rules align, but
  ownership, provenance, creation authority, and replay semantics are different.
- **Add a generic future-content case now.** It would make images, files, or
  other resource-bearing values constructible without decided identity,
  durability, limits, or capability semantics.

## Consequences

The scripted-provider slice can commit final assistant text with exact physical
provenance and can terminalize a completed turn with an explicit semantic
boundary. Storage and domain mappings need three additional closed payload kinds
plus idempotent complete-sequence validation.

Repeated equal text remains separate semantic occurrences. Tool use has a stable
future-facing reference without freezing execution shape, but no tool can run
merely because the reference variant exists. Provider and client renderers must
derive their messages from the ordered entries and related records.

## Scenario walkthroughs

- **S02:** Final provider output is projected into an ordered sequence of
  assistant text and logical tool-use references. One transaction completes the
  call and appends the whole sequence. If the response completes the turn, the
  same transaction ends the attempt and turn and appends `TurnCompleted` last; a
  response that creates tool work appends no completion marker.
- **S04:** Restart never promotes a partial draft. Definitive recovered success
  uses the same all-or-nothing sequence commit and outcome precedence while the
  abandoned attempt ends in the matching `Lost` branch. Later definitive
  resolving evidence may commit content from an outcome-authoritative call
  already classified `Ambiguous`, but that physical disposition remains
  unchanged; unresolved acceptance or response stays ambiguous.
- **S21:** Each committed content entry names the exact pinned,
  outcome-authoritative call. A provider-target mismatch prevents content
  authority when observed before commit and follows ADR-0005's non-rewriting
  invalidation rule when learned later.

## Extension implications

A future rich-content decision may add typed assistant variants for images,
files, audio, or other values. It must define value or identity semantics,
producer provenance, equality, durable ownership, retention, resource limits,
protocol capability, provider projection, client rendering, and migration.
Adding a variant is additive; it cannot reinterpret baseline text or hide a
resource inside text, metadata, a URI convention, or provider JSON.

A future tool decision may authorize a concrete transition that constructs
`AssistantToolUse` with its logical request. It must preserve the reference and
atomicity fixed here while defining request payload, policy, approval,
execution, retry, and outcome semantics in their owning records.

## Open questions

- Refusal, cancellation, reconciliation, mismatch invalidation, accepted-risk,
  steering, tool-result, approval, and delegation semantic variants and their
  commit boundaries remain open.
- Tool-request payload, lifecycle, policy, approval, execution, retry, and
  result semantics remain reserved to ADR-0011 through ADR-0014.
- Rich assistant content, attachments, resource governance, provider/client
  rendering, prompt projection, and public or wire representation remain open.
- Streaming checkpoint policy remains separate from final semantic content.

## Explicit non-decisions

This record adds no Rust type, table, migration, provider adapter, SDK, protocol
schema, renderer, prompt template, tool registry, policy, executor, or client
UI. It does not define refusal text, tool-result content, provider identifier
normalization, fallback, retry, token budgets, rich-content limits, or the
reserved tool lifecycles. Names above describe semantic relationships, not final
APIs.

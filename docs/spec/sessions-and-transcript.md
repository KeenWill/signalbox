# Sessions and the transcript

This page specifies the implemented behavior of session creation and ancestry,
creation from an imported frontier, session-level configuration defaults and
their replacement, replaceable organizational metadata and listing, the
long-lived session aggregate, semantic transcript entries, accepted-input user
content, and actor attribution. It was verified against the implementing stack
through PR #265 (`agent/tool-batch-tier0`); the defaults-epoch and
model-identity boundary were additionally verified through PR #272
(`agent/mid-session-model`); the imported-frontier process surface was verified
through PR #294 (`agent/continue-imported-conversation`); the session system
prompt was verified through PR #286 (`agent/session-system-prompt`); and the
version-thirteen input-delivery surface and its user-reachable steering boundary
were verified through PR #302 (`agent/mid-turn-steering`). The append-only
context-compaction record and projection were verified against
`agent/context-compaction-core`. The imported-conversation record and converter
are owned by [conversation-import](conversation-import.md). Where a law is cited
as `INV-NNN`, [invariants.md](../invariants.md) is the catalog of record; where
mechanics owned by another decision are summarized, the owning sibling page is
linked inline.

## Session identity and creation provenance

A session is one durable, independently browsable conversation with its own
`SessionId`, distinct from every other identity kind (INV-001). Every session
records two required, independent, immutable creation facts, paired as
`SessionCreationProvenance` (INV-003):

- **Creation cause** — why the session exists. The only constructible variant is
  `OwnerInitiated`. Reserved causes (application, schedule, delegation) are not
  represented as placeholder variants.
- **Transcript ancestry** — where initial semantic context came from: `None`
  (explicitly no prior transcript), `SingleSource` naming one source `SessionId`
  and one opaque `TranscriptFrontier`, or `ImportedConversation` naming one
  `ImportedConversationId`, one inclusive imported entry boundary, and either a
  `Resume` or `Fork` relationship to that point. `SingleSource` remains
  unconstructible; imported-frontier session creation is the sole trusted
  producer of imported ancestry.

Why: deriving one fact from the other would make ordinary forks look delegated
and force delegated children to inherit transcripts.

Neither fact can be rewritten after creation, and later source-session activity
cannot change a descendant's recorded ancestry (INV-030). The `session` table
stores cause and ancestry as independently constrained columns and is
append-only. Imported conversations are immutable, so later imports or native
session activity likewise cannot change an imported ancestry boundary (INV-038,
INV-039).

Imported ancestry deliberately contains no local `ContextFrontierId`: ancestry
records the selected external source, while a materialized context frontier is a
Signalbox-owned session artifact. Every imported-seeded session therefore owns
exactly one separate immutable `ImportedSessionSeed`, pairing its `SessionId`
with the exact generated seed `ContextFrontierId`. Sessions with `None` or
`SingleSource` ancestry cannot own this record. Checked construction and
reconstitution require the record's session to carry matching imported ancestry
and require its frontier to contain exactly that imported prefix in order.
Equal-content frontier reminting never satisfies the identity link (INV-015,
INV-039).

## Session creation

`CreateSession` carries the durable command identity, the provenance pair, and
one complete unversioned initial defaults value. Structural equality excludes
only the command identifier (INV-012). Three topics are owned by
[identity-and-commands](identity-and-commands.md): durable-command storage, the
structural-equality doctrine, and identity generation, supply, and encoding.

Application orchestration (`crates/application/src/create_session.rs`):

- rejects nil/max sentinel command identities before canonical construction;
- fixes cause `OwnerInitiated` and ancestry `None` — the request type has no
  cause or ancestry inputs;
- mints one fresh UUIDv7 `SessionId` candidate per invocation (the UUID
  timestamp confers no domain order or authority); and
- calls one atomic transaction port exactly once, with no retry.

Domain preparation admits only the owner-initiated, no-ancestry pair. A
`SingleSource` command is a valid canonical value but fails preparation with
`TranscriptAncestryUnavailable` — a nonterminal error that claims no command
identifier. Forks are therefore typed but not yet creatable. Import-seeded
creation uses the separate command path below; it does not widen
`CreateSession`.

The committing transaction atomically inserts the session row, the scheduler
registration (`session_scheduler`), defaults version one, the current-defaults
pointer, the typed command record, and the owner-global registry claim.
Completeness at every commit boundary is enforced by deferred reverse foreign
keys (`session_current_defaults_fk`, `session_scheduler_row_fk`) plus deferred
constraint triggers `session_requires_creation_command` and
`durable_command_requires_typed_record`. The family-aware session trigger
replaced `session_create_command_fk` when imported-frontier creation added its
separate command family (migration `202607240002_imported_session_seed.sql`);
migration `202607180002_replace_session_defaults.sql` installed in place of the
dropped durable-command reverse foreign key (migrations
`202607180001_create_session.sql`, `202607180004_turn_lifecycle_storage.sql`;
INV-008, INV-012). Every table in this set is append-only except
`session_current_defaults`: its one row per session is the deliberately mutable
pointer that defaults replacement later moves in place. The same transaction
appends a `session_created` update event to the outbox
([persistence-protocol](persistence-protocol.md)).

Command claim, fail-closed replay reconstitution, and conflicting-reuse
resolution follow the shared durable-command contract owned by
[identity-and-commands](identity-and-commands.md), implemented for this kind in
`crates/persistence/src/create_session.rs`. The session-specific consequence:
equal replay returns the recorded receipt, which may name a different session
than the freshly minted candidate; the unused candidate is simply discarded.

Why (append-only, one exception): provenance, defaults versions, command
receipts, and scheduler registration are historical facts; in-place mutation
would rewrite recorded intent and the context that later work consumed. The
current-defaults pointer alone is mutable because "current" is a present choice,
not a historical fact.

### Create from an imported frontier

`CreateSessionFromImportedFrontier` is a distinct durable command family
carrying command identity, one addressable `ImportedTranscriptFrontier`, one
`ImportedSessionRelationship` (`Resume` or `Fork`), and complete unversioned
initial defaults. The frontier itself names its `ImportedConversationId` and
inclusive entry boundary; the command accepts no second independently supplied
conversation identity. Its structural replay equality excludes only command
identity. Separating the family preserves its imported-ancestry contract and
keeps its replay record distinct from the no-ancestry `CreateSession` family;
the shared defaults-bearing storage versions are owned by
[persistence-protocol](persistence-protocol.md).

The relationship records the client's creation-time intent: `Resume` declares a
new Signalbox continuation from the selected imported point; `Fork` declares a
new Signalbox branch from it. Both create independent session identities, use
the same exact imported prefix, and leave the imported conversation unchanged.
Neither mode resumes a provider process, mutates a source file, or grants
external execution authority.

Import never chooses this relationship or a frontier. At any later time, and
more than once, a client may invoke this session-creation command against any
entry boundary of any imported conversation.

Protocol version ten exposes that command as
`create_session_from_imported_frontier`. Its wire address is the imported
conversation identity plus a positive inclusive imported position; the daemon
resolves the immutable aggregate to the canonical sealed frontier before
application construction. The terminal `continue` verb requires that position,
`Resume` or `Fork`, and the initial model selection explicitly, prints its
recovery command identity before socket I/O, and returns the created live
session identity.

The application uses `UuidV7CreateSessionFromImportedFrontierIdGenerator` for
the session, imported-provenance semantic entries, and seed context frontier. It
supplies the fixed session and frontier candidates and an application-owned
semantic-entry generator closure to one atomic transaction port. Imported
aggregates are immutable, so the repository takes no explicit imported-record
row lock; after resolving the complete selected prefix, it invokes the closure
exactly once per prefix member in order. The transaction first follows the
owner-global claim protocol in
[identity-and-commands](identity-and-commands.md). A claimed identifier resolves
to its recorded equal replay or conflicting reuse before any imported-target
lookup. Only for an unclaimed identifier does the transaction load the complete
imported conversation named by `frontier.conversation()`, resolve exactly
positions `1..=N` through that frontier's inclusive boundary, and either:

- returns `ImportedConversationNotFound` or `ImportedFrontierNotFound` without
  claiming the command identity; or
- atomically claims the command and creates the complete session seed, with a
  lost claim race re-inspected against the winner by the shared protocol.

An equal replay returns the recorded created session and ignores unused fresh
identity candidates. Changed frontier, relationship, or defaults under an
already claimed command identity is conflicting reuse; selecting another
conversation necessarily changes the frontier. Cross-kind reuse follows the
owner-global durable-command contract in
[identity-and-commands](identity-and-commands.md).

The committing transaction atomically inserts:

- the owner-initiated session whose immutable ancestry names the imported
  conversation and boundary derived from the selected frontier, plus the
  relationship;
- defaults version one, its current pointer, scheduler registration, typed
  command record, registry claim, and the ordinary `session_created` outbox
  event;
- one imported-provenance semantic entry for every normalized imported entry in
  the exact prefix, including non-text content; and
- one immutable seed context frontier containing exactly those semantic entries
  in imported position order, plus the one-to-one `ImportedSessionSeed` linking
  the session to that exact frontier identity.

Unique conflicts for generated session, semantic-entry, and seed-frontier
candidates are returned as typed identity collisions by identity kind. The
failed transaction rolls back its registry claim.

No imported tool, call, attempt, or turn lifecycle event is emitted. The
imported aggregate remains the content authority: each semantic seed entry
records its exact imported-entry reference, source-speaker attestation, and
normalized content rather than fabricating an accepted input, producing call, or
native tool identity (INV-038).

Why (one transaction): a visible seeded session must never name a missing
imported aggregate, nonmember boundary, partial semantic projection, or
incomplete initial frontier.

Creation replay and every purpose-specific read that resolves imported semantic
context require imported ancestry and its `ImportedSessionSeed` together,
validate that the linked frontier is owned by the same session, and compare its
complete ordered members with the selected imported prefix. A missing,
duplicate, cross-session, mismatched-boundary, or
equal-content-but-different-identity seed is typed corruption. First-turn
scheduling and transcript projection use this checked loader and the stored
identity; neither reconstructs authority by minting another frontier.

## Session defaults and replacement

Session configuration defaults contain the model-selection request, the
dangerously named tool blanket
`DangerousToolAutoApproval::{Disabled, ApproveAll}`, and one optional session
system prompt. The selection algebra and model configuration are owned by
[configuration-and-credentials](configuration-and-credentials.md); blanket
semantics and per-turn freeze are owned by [tool-loop](tool-loop.md). Defaults
are immutable epochs identified by a positive `u64` ordinal:

- session creation establishes version one;
- each replacement installs the checked successor ordinal as a new immutable row
  and moves the session's single current pointer; and
- an exhausted ordinal (`u64::MAX`) is a typed recorded rejection
  (`VersionExhausted`), not a panic or wraparound.

Origin acceptance is the logical start of a turn for defaults binding. It
freezes the epoch current in that acceptance transaction; replacing defaults
later never rebinds that origin, whether the turn is still queued, active, or
terminal. An installed epoch therefore affects only origin input accepted
afterward and never rewrites creation provenance or earlier work (INV-008,
INV-046). Configuration-free steering inherits from its source turn rather than
reading defaults.

The model selection in a replacement may name a configured direct selection or
alias whose target belongs to another provider; the domain and replacement
command impose no same-provider restriction. The successor turn resolves and
pins its target and non-secret credential reference at its own model-call
boundary. The predecessor's prepared or in-flight call retains its existing
pins, so credential affinity and provider prompt-cache prefixes do not move
mid-call (INV-046).

### Session system prompt

A present session system prompt (`SessionSystemPrompt`) is nonempty exact
Unicode text that rejects U+0000 and carries at most
`SessionSystemPrompt::MAX_UTF8_BYTES` = 1,048,576 UTF-8 bytes, mirroring the
accepted-input content bound below; construction rejects excess without
truncating or rewriting, and equality is the exact ordered scalar sequence.
Absence is typed `None`, never empty text. `CreateSession` and
`CreateSessionFromImportedFrontier` carry the optional prompt inside their
complete unversioned initial defaults, and `ReplaceSessionDefaults` replaces it
only as part of the complete successor epoch — there is no prompt-only mutation,
template, or named profile; the
[bound-and-placement decision](../decisions.md#2026-07-26--bound-the-session-system-prompt-as-a-defaults-epoch-value)
records the capacity and epoch-placement choice. Matching
`octet_length(convert_to(system_prompt, 'UTF8'))` CHECK constraints protect the
durable epoch and command columns (migration
`202607280303_session_system_prompt.sql`), and command/defaults schema agreement
extends through a generated exact-encoding SHA-256 digest column because
megabyte text cannot join a btree key.

The immutable epoch row is the prompt's single content authority. Origin
acceptance keeps freezing only the epoch; per-turn origin rows copy no prompt
text, and model-call preparation reads the prompt through the calling turn's
frozen defaults version — including a reclassified-steering origin's inherited
version — so every call the turn prepares sets `ModelOperation.system` to
exactly that epoch's prompt, or none. A replacement that changes only the system
prompt appends no semantic transcript entry: the new instructions reach the
provider whole and out of band on the successor turn's calls, and the turn's
frozen epoch already records durably which prompt governed it, as recorded by
the
[no-transcript-boundary decision](../decisions.md#2026-07-26--deliver-system-prompt-changes-without-a-transcript-boundary).
The `ModelIdentityChanged` boundary below remains keyed to the frozen direct
model selection alone.

`ReplaceSessionDefaults` carries exactly command identity, target session,
expected current version, and the complete replacement; equality excludes only
the command identifier. The handling transaction loads the authoritative session
and compare-and-sets the expected version:

- expected differs from current → recorded `CurrentVersionMismatch`;
- absent session → recorded `SessionNotFound`;
- no representable successor → recorded `VersionExhausted`;
- otherwise the applied result carries the complete installed version.

The expected-version check is enforced twice inside the one transaction
(`crates/persistence/src/replace_session_defaults.rs`). Domain preparation runs
against a load of the authoritative session; when it yields the applied result,
the adapter moves the pointer with a SQL compare-and-set conditioned on the
expected version. Zero affected rows re-derives the result against current state
in the same transaction and records the typed rejection; a re-derivation that
still reports applied — a CAS loss without a version change — fails closed as
corruption, as does an update affecting more than one row. Equal replay and
cross-kind identifier reuse resolve through the same fail-closed
reconstitute-and-compare path as `CreateSession` (INV-012).

Why (compare-and-set): the caller names the version its intent was formed
against, so a racing replacement surfaces as a typed rejection instead of a
silent lost update.

A supplied session that does not match the command target is a nonterminal
preparation error, not a recorded rejection. Application orchestration
constructs the canonical command once and calls its atomic port exactly once,
with no preload and no retry.

When a started turn's frozen direct selection differs from its immediate
predecessor's, eligibility appends one `ModelIdentityChanged` semantic entry
immediately before that turn's origin entry. The entry names the turn, its
frozen defaults epoch, and its exact direct selection. It is absent for the
first turn and for equal-selection successors. Thus the frontier records the
model identity actually crossed by executed conversation history rather than
unused or redundant replacement epochs (INV-046). The exact provider-message
projection is recorded by the
[model-identity injection decision](../decisions.md#2026-07-25--render-model-identity-boundaries-as-injected-user-role-events).
Started frontiers committed before this boundary existed retain their exact
historical membership: an immutable per-turn compatibility fact grandfathers
only those already-active or terminal starts. Turns still queued at migration
and every newly accepted turn require the boundary normally, as recorded by the
[legacy-frontier decision](../decisions.md#2026-07-25--grandfather-pre-boundary-started-frontiers).

## Session metadata and list projection

Session metadata is a purpose-specific satellite snapshot, not part of the
long-lived `Session` aggregate (INV-005). It contains exactly:

- an optional title. A present title is nonempty exact Unicode text;
- a flat set of nonempty exact Unicode tags;
- a map from nonempty exact Unicode keys to exact Unicode values, including the
  empty value; and
- one archive boolean.

Every string rejects U+0000, which PostgreSQL text cannot store. No string is
trimmed, normalized, or case-folded. Set and map equality is independent of
caller order; duplicate tags or attribute keys fail construction rather than
silently selecting a winner. Tags are human-facing organization and attributes
are machine-facing provenance; neither shape substitutes for the other.

A snapshot carries at most 256 tags, at most 256 attributes, and at most 262,144
total UTF-8 bytes across its present title, tags, attribute keys, and attribute
values. Each tag and attribute key carries at most 1,024 UTF-8 bytes so its
composite PostgreSQL index entry remains representable. Construction rejects any
excess before command handling; the exact provisional capacity choice is
recorded in the
[metadata-bound decision](../decisions.md#2026-07-25--bound-session-metadata-for-storage-and-process-frames).

The root `session_metadata` row and normalized
`session_metadata_tag`/`session_metadata_attribute` rows (migration
`202607260101_session_metadata.sql`) exist only after the first metadata write.
Their absence is the canonical initial projection: no title, no tags or
attributes, not archived, and no last-writer stamp. Creation therefore does not
fabricate an actor that its command does not carry.

A single-session metadata read collects the root, tags, and attributes in one
read-only repeatable-read transaction that selects the owning session row.
Missing session identity returns the typed absent outcome used by the process
boundary's `not_found` response; only an existing session without a metadata
root returns the complete initial projection. A successful read therefore
returns either that initial projection or one complete committed replacement,
never a combination of separately committed snapshots.

`ReplaceSessionMetadata` carries durable command identity, target session,
actor, and one complete replacement snapshot. Its replay equality covers every
field except command identity (INV-012). The process-facing application request
fixes `Actor::Owner`. A separate purpose-specific application constructor
accepts only `Actor::Tool { request }` for the exact executing tool request;
model, recovery, and arbitrary caller-selected actors remain unconstructible.
First handling locks the target session, then either records `SessionNotFound`
without an effect or atomically replaces the complete root, tag, and attribute
snapshot. After acquiring that lock, a separate statement samples PostgreSQL
statement time at microsecond precision; the applied result records it together
with the command actor as the one last-writer stamp. An equal replay returns
that exact recorded result and timestamp. The database timestamp is result
evidence, not caller intent or a global ordering token, so it does not
participate in command equality.

There is deliberately no expected or installed metadata version and no versioned
metadata-history API. Two distinct writes are last-writer-wins after
serialization on the session row; a full replacement can overwrite an earlier
writer's unrelated field. Callers that need to preserve fields read the current
snapshot before forming the replacement. The owner-global durable-command
contract retains each command's payload and typed result for replay, so those
records preserve prior replacement values. Persistence also retains append-only
internal evidence that each applied receipt became current exactly once; it
rejects reinstalling an earlier receipt after a later replacement. Neither form
of evidence is an optimistic-concurrency mechanism, aggregate version, or
metadata-history projection.

Archive is organizational visibility state only. Archiving never cancels,
pauses, rejects, or rewrites accepted, queued, active, or terminal work and
never cascades to descendants or otherwise related sessions. Restore is the same
replacement operation with `archived = false`; it has no lifecycle target to
reconstruct. Destructive retention remains a separate open question.

The paginated list projection joins current defaults with metadata but does not
reconstitute the `Session` aggregate. Each result carries session identity, the
current defaults version, model selection, and dangerous-tool auto-approval
flag, title, tags, archive state, and optional last-writer stamp; attributes
remain available through the single-session metadata read, and the current
epoch's optional system prompt is deliberately absent from list rows — the
process boundary's single-session defaults read
([process-protocol](process-protocol.md)) returns it exactly. A query has an
exact tag set of at most 256 members, optional exact case-sensitive title
substring, `include_archived`, a page size from 1 through 100, and an exclusive
`after_session_id` cursor. Required tags use the metadata tag rules, a present
title substring is nonempty and rejects U+0000, and all filter strings together
carry at most 262,144 UTF-8 bytes:

- every requested tag must exist (AND-match); an empty set matches all;
- a title query matches only a present title containing that exact scalar
  sequence;
- archived sessions are excluded when `include_archived` is false, which is the
  default view, and included when it is true; and
- matching rows are ordered by `SessionId` UUID value.

One read-only repeatable-read transaction selects each page and fetches at most
one extra identity to determine whether another page exists. When it does, the
next cursor is the last emitted session identity. A later page is a new
snapshot: pagination guarantees deterministic keyset traversal, not a cross-page
snapshot under concurrent creation or replacement.

The unified conversation listing owned by
[process-protocol](process-protocol.md) reads the same session, current
defaults, and metadata facts for its native rows — title, archive state, and
current defaults version — alongside imported-conversation headers, in one
bounded keyset page of its own. It adds no session state and changes none of the
rules above.

Because `OwnerInitiated` is the only constructible creation cause and every
current session-creation boundary lacks actor attribution, the implemented
default view is exactly all non-archived sessions. No visibility taxonomy,
creation-time override, or inference from missing attribution is stored. The
dependency for future creation-derived visibility is recorded in
[open-questions.md](../open-questions.md#session-organization-visibility-and-retention).

## The session aggregate

The long-lived domain `Session` (`crates/domain/src/session.rs`) contains
exactly three facts: `SessionId`, the immutable creation provenance, and the
complete current defaults version selected by the durable pointer. It embeds
nothing else — no transcript entries, accepted inputs, turns, queue facts,
command history, evidence, or presentation state (INV-005). Those remain
independently stored facts correlated by typed identity.

Why (small aggregate): embedding session-associated collections would turn an
ordinary session read into an unbounded reconstruction crossing several
lifecycle and transaction boundaries, and possessing `Session` alone must never
imply authority to perform a transition.

A `Session` is an owned snapshot, not a live cache: any transition that depends
on current defaults revalidates them inside its own transaction. The pre-commit
candidate (`InitialSession`), the command receipt
(`CreateSessionAppliedResult`), and the loaded `Session` are distinct types;
loading never returns a receipt and command replay never returns a `Session`.

### Loading and reconstitution

`load_session(SessionId)` performs one statement-consistent read joining the
session row, its one current-defaults pointer, and exactly the version that
pointer names (`crates/persistence/src/session.rs`). For imported ancestry, the
same bounded read joins the one-to-one seed record and its frontier header as a
constant-size proof: seed and frontier ownership and identity must agree with
the session, and the stored member count must equal the selected imported
boundary position. It does not materialize the imported conversation, frontier
members, or semantic entries. Full prefix comparison belongs to creation replay
and purpose-specific semantic-context resolution. The pointer is authoritative;
a load never infers current defaults from version one, the greatest stored
version, a caller-supplied version, or a cache.

Why (pointer authority): append-only version existence does not mean
installation; only the pointer records the accepted current choice.

`None` is returned only when no session row exists in the read snapshot. Once
the row exists, a missing pointer, missing selected version, ownership mismatch,
pointer/record version disagreement, unknown discriminator, invalid ordinal, or
an absent or inconsistent bounded imported-seed proof fails closed as typed
corruption: the adapter's decode checks feed the domain-owned
`SessionReconstitutionInput::reconstitute` seam, which accepts only complete
agreeing domain values (INV-002, INV-039). Reconstitution never yields `None`, a
default, or a partial session.

Why (fail closed): a fabricated or partial session would mask corruption and
launder invalid durable state into valid-looking domain values.

## Semantic transcript entries

A semantic transcript entry is one immutable identified semantic-history fact:
its own `SemanticTranscriptEntryId`, a source session, and a closed payload
(`crates/domain/src/semantic_entry.rs`). The implemented payload set is complete
and closed:

- `OriginAcceptedInput { accepted_input }` — the exact accepted input whose
  origin turn became eligible;
- `SteeringAcceptedInput { accepted_input, source_turn }` — accepted
  next-safe-point input consumed by its exact source turn;
- `ModelIdentityChanged { turn, defaults_version, selected }` — the exact
  successor-turn boundary at which execution first observes a different frozen
  direct model identity;
- `ContextSummary { producing_call, summarized, value }` — exact model-produced
  summary text retaining its dedicated physical call and the first and through
  source-qualified entries of the inclusive range it represents;
- `TurnFailed { turn }` — an explicit marker that the turn terminalized as
  failed;
- `AssistantText { producing_call, value }` — exact assistant text with
  outcome-authoritative producing-call provenance;
- `AssistantToolUse { producing_call, request }` — one logical request from the
  completed producing call, with name and arguments resolved through the request
  record;
- `ToolExecutionResult { attempt }` — executed success or error evidence owned
  by the referenced attempt;
- `ToolDenied { request }` — a denial owned by the referenced request and
  decision;
- `ToolClosed { request }` — a request closed by turn end before it completed
  ordinary execution, including undecided and approved-but-unattempted requests.
  A crash-lost attempt is terminal `KnownFailed` evidence and uses
  `ToolExecutionResult`;
- `Imported { imported_entry, source_speaker, content }` — one exact normalized
  imported content value and its speaker attestation, including source event,
  source-defined message block, message-content absence, text, tool, result,
  thinking, redacted thinking, or document data, carrying imported rather than
  native execution provenance;
- `TurnCompleted { turn }` — the explicit final marker for a completed turn; and
- `TurnCancelled { turn }` — the explicit final marker for a turn ended by its
  applied interrupt.

There is no generic text, role, metadata, or "other" payload. Entry identity is
distinct from accepted-input, imported-entry, and turn identity (INV-001); equal
content in two inputs or imports yields distinct entries. Entry construction is
sealed inside the domain crate — checked constructors are `pub(crate)`.
`turn_eligibility.rs` produces eligibility and recovery history;
`model_execution.rs` produces assistant and turn-terminal history;
imported-frontier session creation is the only producer of `Imported`; and
sealed tool transitions produce tool-use/result references only through the
atomic boundaries owned by [tool-loop](tool-loop.md).

`OriginAcceptedInput` and `SteeringAcceptedInput` reference the accepted input's
identity; neither copies content. Steering additionally names the exact active
turn from its immutable delivery binding. Why: two authoritative content copies
could diverge and would need an unnecessary precedence rule, while the
source-turn correlation prevents a valid input from steering different work.

Storage (`semantic_transcript_entry`, migration
`202607180004_turn_lifecycle_storage.sql`) enforces globally unique entry
identity, at most one origin entry per accepted input, at most one failed marker
per turn, same-session references, and append-only rows (INV-005). Migration
`202607240002_imported_session_seed.sql` adds imported-entry provenance,
restricts it to imported-ancestry sessions and the exact selected source prefix,
and keeps imported entries outside every native subject-identity constraint.
Migration `202607220001` adds the unique completion marker, `202607220004` adds
the unique steering entry, and `202607220005` adds the unique cancellation
marker. Migration `202607280201_mid_session_model_selection.sql` adds the unique
per-turn model-identity boundary and checks it against the origin's frozen epoch
and selection. The tool-loop migration adds request/result references while
widening the corresponding closed payload shapes. The origin-disposition guard
arrived later: migration `202607180005_occupied_slot_submit_input.sql` — the
migration that first admits the `pending_steering` disposition — replaces the
entry/turn-state trigger so an origin entry additionally requires its input's
`origin_of` disposition (constraint
`semantic_transcript_entry_origin_disposition`); pending steering can never
appear as a semantic origin.

### Context compaction

Context compaction changes model visibility, never durable history. A completed
compaction has five correlated immutable facts: its identity and optional
same-session predecessor, the complete source frontier, a dedicated physical
model call, the exact inclusive source-qualified range summarized, and a result
frontier equal to the source frontier plus one new `ContextSummary`. The summary
entry names the producing call and repeats the exact range. Storage and domain
reconstitution reject a missing endpoint, reversed range, mismatched summary,
non-completing call, different source frontier, or result that is not exactly
that one-entry append (INV-005, INV-015).

The transcript therefore remains complete and addressable after compaction. No
entry or frontier is deleted, replaced, reordered, or rewritten. The
compaction-call record separately retains the session's current direct model
selection, resolved provider target, source frontier, physical lifecycle and
disposition, non-secret credential reference, and each independently optional
provider-reported usage field. Summary production is its own model call; it is
not assistant output attributed to an accepted-input turn.

Compactions in one session form a forward-only chain. A successor's source must
retain its predecessor's complete result frontier as a semantic prefix. A later
ordinary turn cannot opt back into an uncompacted projection. The existing
continue-from-boundary operation remains the escape hatch: choosing a position
before the summary creates a different session whose ancestry frontier does not
contain that compaction.

For model input only, a complete frontier containing summaries is projected from
its latest summary: render that summary first and then every complete-frontier
entry physically after the summary's through-boundary, excluding the selected
summary from its later physical position. With no summary, projection is the
complete frontier order. This rule deliberately separates the frontier a call
durably records from the ordered subset the selected model sees.

Explicit compaction chooses an optional through position, defaulting to the
latest safe boundary. The daemon also compacts before an ordinary model send
when that call's rendered input would exceed the current selection's declared
context window. Both paths use the required deployment-configured compaction
prompt and the session's current direct selection. Trigger and configuration
mechanics are owned by [model-call-execution](model-call-execution.md).

### When entries come to exist

An accepted input is durable at acceptance (INV-007) but becomes transcript
history only at eligibility: the activation transaction commits the one origin
entry together with the starting context snapshot, lineage, and activation facts
(INV-009, INV-015). Before that commit no semantic entry exists for the queued
turn — and none can exist: the schema rejects any entry for a queued turn, so
the accepted-input record alone carries no semantic commitment.

Why (entry at eligibility, not acceptance): queue acceptance has not fixed
lineage or the snapshot that consumes the entry; eligibility fixes both
atomically.

Imported semantic entries have a different commit boundary. Imported-frontier
session creation appends the complete selected prefix before any native turn
exists, together with imported ancestry and the exact seed frontier. They never
require or create accepted-input, turn, attempt, call, or native tool records.
The first native turn's eligibility transaction creates a new successor frontier
whose predecessor is that immutable seed frontier and whose appended member is
the ordinary `OriginAcceptedInput`. Every later native frontier retains its
predecessor terminal prefix, then appends the model-identity boundary when the
frozen direct selection changed, and finally appends its ordinary origin
(INV-039, INV-046).

Pending steering has a separate safe-point boundary (INV-036). A
version-thirteen `steer` submit accepted while its exact source turn is active
returns the accepted-input identity, immutable acceptance position, and source
turn immediately; it creates no origin turn or semantic entry at acceptance.
Immediately before an initial or continuation call is prepared, the transaction
appends one `SteeringAcceptedInput` per pending input in ascending acceptance
position, derives one frontier extending the starting frontier for the admitted
initial call, changes every input to `ConsumedAsSteering { call }`, and inserts
that exact `Prepared` call against the extended frontier. All four effects
commit or roll back together. The entry therefore becomes semantic history only
with the call that first observes it; the immutable accepted-input row remains
the content authority.

Tool-use entries become history with the producing call's completed observation;
tool-result entries become history only at the all-resolved continuation or
terminal-stop boundary. Request, attempt, and decision records remain the single
content authorities. Exact ordering and closure rules are owned by
[tool-loop](tool-loop.md).

Entry/turn-state agreement is a durable schema invariant, not only transactional
practice. Deferred constraint triggers around
`assert_turn_lifecycle_final_state` (migration `202607180004`) check every
commit bidirectionally: a queued turn carries zero origin or failure entries; a
started turn carries exactly one correlated origin entry, and its starting
frontier ends with exactly that entry. A non-first turn carries exactly one
immediately preceding model-identity entry iff its frozen direct selection
differs from its predecessor's, subject to the immutable pre-boundary
compatibility fact described above; a failed turn's terminal frontier extends
its latest call frontier (or starting frontier) by its exact terminal
tool-result suffix when one exists and exactly its failure marker last; a
completed turn's terminal frontier extends its producing call's frontier by the
call's assistant entries plus exactly its completion marker last; a cancelled
turn's terminal frontier extends the latest call frontier (or starting frontier)
by its exact terminal tool-result suffix when one exists and exactly its
cancellation marker last; and a refused turn's terminal frontier is a distinct
equal-content copy of its latest call frontier. A reconciliation-required turn
over a model call likewise carries a distinct equal-content terminal frontier;
one over a tool attempt extends the producing call's frontier by its exact
terminal tool-result suffix. Both retain exactly one ambiguous operation plus
the correlated ended turn attempt and applied interrupt proof. Migration
`202607220001` first defined the model-call assertion; migrations `202607220004`
and `202607220005` widen it for steering and stop requests. A writer that
diverges from the transactional practice above is rejected at the commit
boundary.

`TurnFailed` now has two producers — the model-call known-failure closure and
startup recovery — each appending the marker after every earlier committed entry
and emitting a `turn_failed` update event atomically. A later successor's
starting frontier retains the failed predecessor's exact terminal prefix,
including that marker. Turn and attempt lifecycle doctrine is
[turn-lifecycle-and-scheduling](turn-lifecycle-and-scheduling.md), the entry
commit boundaries are this page's own material, and update-event delivery is
[persistence-protocol](persistence-protocol.md).

## User content

Accepted-input content is the closed one-variant algebra `UserContent`; its only
variant is `Text { value: NonEmptyUnicodeText }`
(`crates/domain/src/user_content.rs`). Construction rejects empty text and any
text containing U+0000 (which PostgreSQL text cannot store); whitespace-only
text is content. The domain applies no trimming, Unicode normalization, case
folding, or any other rewriting, and equality is the exact ordered scalar
sequence — normalization-distinct spellings are unequal. That exact value
participates in `SubmitInput` replay equality (INV-012).

Why (exact, unnormalized): replay equality must not depend on a normalization
policy; search or display projections may normalize without changing accepted
intent.

The process boundary exposes the existing delivery algebra without changing
content ownership. An accepted omitted or explicit `start_when_idle` submit and
a `queue` submit create an accepted origin turn with frozen configuration;
`queue` binds its acceptance to the exact active turn it follows. A `steer`
submit instead creates configuration-free pending steering bound to that exact
source turn. The closed version-thirteen wire spelling and typed receipts are
owned by [process-protocol](process-protocol.md#client-requests).

The accepted input owns the one immutable authoritative content value; the
`accepted_input` row admits exactly two guarded updates from pending steering:
consumption to `consumed_as_steering`, changing only disposition plus the exact
consuming call, or reclassification to `reclassified_as_turn_origin`, changing
only disposition plus the fresh origin turn. Neither changes content, and
semantic history references that content rather than copying it (INV-005,
INV-007, INV-036).

### Bounds

The domain value is unbounded. Admission is bounded at the application boundary:
`SubmitInputRequest::try_new` rejects text whose UTF-8 encoding exceeds
`MAX_CONTENT_UTF8_BYTES` = 1,048,576 bytes before typed command construction, so
no command identifier is claimed. The `OversizedContent` failure retains only
the byte length, never the rejected content. Matching
`octet_length(convert_to(content_text, 'UTF8'))` CHECK constraints protect both
durable content columns (migration `202607200001_bounded_user_content.sql`).

Why (bytes, at admission): byte measurement matches wire and storage cost and
keeps the domain value exactly as accepted; rejecting before construction can
never truncate or rewrite content.

This is a provisional owner-decided floor (decision log, 2026-07-20), not the
resource-governance policy.

## Actor attribution

The actor algebra (`Owner`, `Model { turn }`, `Recovery`, `Tool { request }`),
its participation in structural replay equality, and its closed-discriminator
storage convention are owned by
[identity-and-commands](identity-and-commands.md). Attribution is provenance
only — not authentication, authorization, or approval — and model agency can
never compare equal to owner agency (INV-020).

The session-command consequences: `SubmitInput` and `ReplaceSessionMetadata` are
the command payloads carrying an actor inside the conversational command
surface. `SubmitInput` fixes `Actor::Owner`. Metadata replacement admits the
owner-facing constructor plus the purpose-specific `Actor::Tool { request }`
constructor described above; neither accepts arbitrary agency. Domain
reconstitution compares the stored actor against the canonical command and fails
closed on mismatch (`StoredActorMismatch` for input and `CommandActorMismatch`
for metadata). The metadata actor also becomes its organizational last-writer
stamp.

Why (seeded before expansion): carrying owner attribution from the first
metadata write preserved a truthful backfill, and the existing closed actor
columns now admit tool attribution without a semantic migration.

`CreateSession` carries no actor; amending it remains an explicit owner choice
that has not been taken. `Recovery` and `Model` remain representable without an
implemented command-producing boundary.

## Implemented transcript projections

The terminal client renders the authoritative semantic-entry projection for
`transcript` and snapshot-first `follow`, including user and assistant text plus
completed- and failed-turn markers. The version-one wire mapping, update
synchronization, and presentation rules are owned by
[process-protocol](process-protocol.md). The provider-prompt projection is also
implemented: `PreparedModelOperation::render` maps frontier entries to
provider-neutral messages and binds the frozen epoch's optional session system
prompt; multi-source system-prompt composition remains deferred under the open
edges of [model-call-execution](model-call-execution.md).

## Open edges

- Native fork creation remains typed but unimplemented: `SingleSource` ancestry
  fails preparation (`TranscriptAncestryUnavailable`) until a trusted native
  `TranscriptFrontier` producer exists. Imported boundaries are independently
  selectable at every entry and do not select or authorize a native-session
  fork. Selectable native fork boundaries remain open
  ([open-questions.md](../open-questions.md), selectable native
  transcript-frontier boundaries).
- Multi-source ancestry and transcript merge remain future decision scope, and
  retention when an ancestry source is destructively deleted is undecided; both
  are recorded in [open-questions.md](../open-questions.md).
- Creation-attributed default visibility, richer metadata filters, and
  destructive retention remain cataloged under
  [Session organization, visibility, and retention](../open-questions.md#session-organization-visibility-and-retention).
- The static eligible-failure path (terminalize at eligibility without an
  attempt, committing origin plus failed marker in one transaction) has no
  implemented producer; startup recovery and the model-call known-failure
  closure are the committed `TurnFailed` sources today.
- Assistant text, tool-use/result references, completed-turn, steering, and
  cancelled-turn semantic entries are implemented; refusal, reconciliation,
  approval-event, and delegation entry variants remain open.
- `ReplaceSessionDefaults` carries no `actor` field although the accepted
  actor-attribution design slated it for first-accepted-version adoption; its
  record family has since committed storage versions 1 and 2 without one, so
  later adoption needs another kind-scoped storage version; the truthful `Owner`
  backfill that design relies on still exists.
- `CreateSession` actor attribution remains implicit pending an explicit owner
  amendment choice.
- `Recovery` and `Model` actor variants have no constructing boundary;
  per-transition attribution adoption schedules remain open.
- The 1 MiB content bound is a provisional owner floor; the resource-governance
  limit question stays open, and non-text content kinds remain unconstructible
  pending their owning decisions.
- The session system prompt is one optional bounded string per session.
  Composition from base, per-use-case, and instruction-file sources, templates,
  and named profiles remain the open
  [configuration-category capability](../open-questions.md#configuration-categories).

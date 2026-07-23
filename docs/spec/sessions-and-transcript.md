# Sessions and the transcript

The baseline session and transcript behavior was verified through PR #175
(`agent/stop-requests`); the import additions specify the implementing stack
rooted at `agent/conversation-import-spec`. This page covers session creation
and ancestry, import-seeded session creation, session-level configuration
defaults and their replacement, the long-lived session aggregate, semantic
transcript entries, accepted-input user content, and actor attribution. The
imported-conversation record and converter are owned by
[conversation-import](conversation-import.md). Where a law is cited as
`INV-NNN`, [invariants.md](../invariants.md) is the catalog of record; where
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
  `ImportedConversationId` and the exact immutable seed frontier projected from
  it. `SingleSource` remains unconstructible; seed-from-import is the sole
  trusted producer of an imported frontier.

Why: deriving one fact from the other would make ordinary forks look delegated
and force delegated children to inherit transcripts.

Neither fact can be rewritten after creation, and later source-session activity
cannot change a descendant's recorded ancestry (INV-030). The `session` table
stores cause and ancestry as independently constrained columns and is
append-only. Imported conversations are immutable, so later imports or native
session activity likewise cannot change an imported ancestry boundary (INV-038,
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
keys (`session_current_defaults_fk`, `session_create_command_fk`,
`session_scheduler_row_fk`) plus one deferred constraint trigger,
`durable_command_requires_typed_record`, which migration
`202607180002_replace_session_defaults.sql` installed in place of the dropped
reverse foreign key (migrations `202607180001_create_session.sql`,
`202607180004_turn_lifecycle_storage.sql`; INV-008, INV-012). Every table in
this set is append-only except `session_current_defaults`: its one row per
session is the deliberately mutable pointer that defaults replacement later
moves in place. The same transaction appends a `session_created` update event to
the outbox ([persistence-protocol](persistence-protocol.md)).

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

### Seed from an imported conversation

`SeedSessionFromImport` is a distinct durable command family carrying command
identity, one `ImportedConversationId`, and complete unversioned initial
defaults. Its structural replay equality excludes only command identity.
Separating the family preserves storage version 1 and the no-ancestry contract
of `CreateSession`; it does not make imported record look like a native fork.

The application supplies fresh candidates for the session, seed semantic
entries, and seed frontier, then calls one atomic transaction port. The
transaction loads the complete imported conversation, prepares its exact
seedable projection, and either:

- returns `ImportedConversationNotFound` or `NoSeedableTranscriptEntries`
  without claiming the command identity; or
- handles command claim/replay and creates the complete session seed.

An equal replay returns the recorded created session and ignores unused fresh
identity candidates. Changed imported conversation or defaults under an already
claimed command identity is conflicting reuse. Cross-kind reuse follows the
owner-global durable-command contract in
[identity-and-commands](identity-and-commands.md).

The committing transaction atomically inserts:

- the owner-initiated session whose immutable ancestry names the imported
  conversation and seed frontier;
- defaults version one, its current pointer, scheduler registration, typed
  command record, registry claim, and the ordinary `session_created` outbox
  event;
- one imported-provenance semantic entry for each seed-included text entry, in
  exact imported position order; and
- one immutable seed context frontier containing exactly those semantic entries.

No import, tool, call, attempt, or turn lifecycle event is emitted. The imported
aggregate remains the content authority: the semantic seed entry records its
exact imported-entry reference, speaker, and checked content projection rather
than fabricating an accepted input or producing call (INV-038).

Why (one transaction): a visible seeded session must never name a missing
imported aggregate, partial semantic projection, or incomplete initial frontier.

## Session defaults and replacement

Session configuration defaults are model-selection-only in the baseline; the
selection algebra, configuration freeze at acceptance, and per-turn effective
configuration are owned by
[configuration-and-credentials](configuration-and-credentials.md) and
[turn-lifecycle-and-scheduling](turn-lifecycle-and-scheduling.md). Defaults are
immutable versions with a positive `u64` ordinal:

- session creation establishes version one;
- each replacement installs the checked successor ordinal as a new immutable row
  and moves the session's single current pointer; and
- an exhausted ordinal (`u64::MAX`) is a typed recorded rejection
  (`VersionExhausted`), not a panic or wraparound.

An installed version affects only origin input accepted afterward; it never
rewrites creation provenance or queued, active, or completed work (INV-008).
Configuration-free steering inherits from its source turn rather than reading
defaults.

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
pointer names (`crates/persistence/src/session.rs`). The pointer is
authoritative; a load never infers current defaults from version one, the
greatest stored version, a caller-supplied version, or a cache.

Why (pointer authority): append-only version existence does not mean
installation; only the pointer records the accepted current choice.

`None` is returned only when no session row exists in the read snapshot. Once
the row exists, a missing pointer, missing selected version, ownership mismatch,
pointer/record version disagreement, unknown discriminator, or invalid ordinal
fails closed as typed corruption: the adapter's decode checks feed the
domain-owned `SessionReconstitutionInput::reconstitute` seam, which accepts only
complete agreeing domain values (INV-002). Reconstitution never yields `None`, a
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
- `TurnFailed { turn }` — an explicit marker that the turn terminalized as
  failed;
- `AssistantText { producing_call, value }` — exact assistant text with
  outcome-authoritative producing-call provenance;
- `AssistantToolUse { producing_call, request }` — typed, but storage rejects it
  (`semantic_transcript_entry_tool_use_unavailable`) until the reserved tool
  decisions land; and
- `ImportedText { imported_entry, speaker, value }` — exact text projected from
  one seed-included imported entry, carrying imported rather than native
  execution provenance;
- `TurnCompleted { turn }` — the explicit final marker for a completed turn; and
- `TurnCancelled { turn }` — the explicit final marker for a turn ended by its
  applied interrupt.

There is no generic text, role, metadata, or "other" payload. Entry identity is
distinct from accepted-input, imported-entry, and turn identity (INV-001); equal
content in two inputs or imports yields distinct entries. Entry construction is
sealed inside the domain crate. Native producers remain eligibility and model
execution; seed-session preparation is the only producer of `ImportedText`.

`OriginAcceptedInput` and `SteeringAcceptedInput` reference the accepted input's
identity; neither copies content. Steering additionally names the exact active
turn from its immutable delivery binding. Why: two authoritative content copies
could diverge and would need an unnecessary precedence rule, while the
source-turn correlation prevents a valid input from steering different work.

Storage (`semantic_transcript_entry`, migration
`202607180004_turn_lifecycle_storage.sql`) enforces globally unique entry
identity, at most one origin entry per accepted input, at most one failed marker
per turn, same-session references, and append-only rows (INV-005). Migration
`202607220001` adds the unique completion marker, `202607220004` adds the unique
steering entry, and `202607220005` adds the unique cancellation marker while
widening the corresponding closed payload shapes. The origin-disposition guard
arrived later: migration `202607180005_occupied_slot_submit_input.sql` — the
migration that first admits the `pending_steering` disposition — replaces the
entry/turn-state trigger so an origin entry additionally requires its input's
`origin_of` disposition (constraint
`semantic_transcript_entry_origin_disposition`); pending steering can never
appear as a semantic origin.

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

Imported semantic entries have a different commit boundary. Seed-session
creation appends them before any native turn exists, together with the imported
ancestry and exact seed frontier. They never require or create accepted-input,
turn, attempt, or call records. The first native turn's eligibility transaction
extends that immutable seed frontier with its ordinary `OriginAcceptedInput`;
every later native frontier follows the existing predecessor-prefix rules
(INV-039).

Pending steering has a separate safe-point boundary (INV-036). Immediately
before a later call is prepared, the transaction appends one
`SteeringAcceptedInput` per pending input in ascending acceptance position,
derives one frontier extending the starting frontier for the admitted initial
call, changes every input to `ConsumedAsSteering { call }`, and inserts that
exact `Prepared` call against the extended frontier. All four effects commit or
roll back together. The entry therefore becomes semantic history only with the
call that first observes it; the immutable accepted-input row remains the
content authority.

Entry/turn-state agreement is a durable schema invariant, not only transactional
practice. Deferred constraint triggers around
`assert_turn_lifecycle_final_state` (migration `202607180004`) check every
commit bidirectionally: a queued turn carries zero origin or failure entries; a
started turn carries exactly one correlated origin entry, and its starting
frontier ends with exactly that entry; a failed turn's terminal frontier extends
its latest call frontier (or starting frontier) by exactly its failure marker; a
completed turn's terminal frontier extends its producing call's frontier by the
call's assistant entries plus exactly its completion marker last; a cancelled
turn's terminal frontier extends the latest call frontier (or starting frontier)
with exactly its cancellation marker; and a refused turn's terminal frontier is
a distinct equal-content copy of its latest call frontier. A
reconciliation-required turn likewise carries a distinct equal-content terminal
frontier over its exact ambiguous call, correlated ended attempt, and applied
interrupt proof. Migration `202607220001` first defined the model-call
assertion; migrations `202607220004` and `202607220005` widen it for steering
and stop requests. A writer that diverges from the transactional practice above
is rejected at the commit boundary.

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

The session-command consequences: `SubmitInput` is the only command payload
carrying an actor, and its constructor fixes `Actor::Owner`, so no non-owner
agency can claim a command through this boundary; domain reconstitution compares
the stored actor against the canonical command and fails closed on mismatch
(`StoredActorMismatch`).

Why (seeded while constant): `Owner` is currently the only truthful issuer, so
seeding the field now preserves a truthful backfill; retrofitting after several
agencies exist would force a semantic migration.

`CreateSession` carries no actor; amending it remains an explicit owner choice
that has not been taken. `Recovery`, `Model`, and `Tool` are representable but
no implemented boundary constructs them.

## Open edges

- Native fork creation remains typed but unimplemented: `SingleSource` ancestry
  fails preparation (`TranscriptAncestryUnavailable`) until a trusted native
  `TranscriptFrontier` producer exists; imported ancestry does not select or
  authorize a native fork. Selectable fork boundaries remain open
  ([open-questions.md](../open-questions.md), selectable transcript-frontier
  boundaries).
- Multi-source ancestry and transcript merge remain future decision scope, and
  retention when an ancestry source is destructively deleted is undecided; both
  are recorded in [open-questions.md](../open-questions.md).
- The static eligible-failure path (terminalize at eligibility without an
  attempt, committing origin plus failed marker in one transaction) has no
  implemented producer; startup recovery and the model-call known-failure
  closure are the committed `TurnFailed` sources today.
- Assistant-text, completed-turn, steering, and cancelled-turn semantic entries
  are implemented; the tool-use variant is typed but storage-blocked; refusal,
  reconciliation, approval, and delegation entry variants remain open.
- The client transcript rendering projection over semantic entries is not
  implemented. The provider-prompt message projection is:
  `PreparedModelOperation::render` maps frontier entries to provider-neutral
  messages ([model-call-execution](model-call-execution.md)); only system-prompt
  composition remains deferred, as that page's open edge.
- `ReplaceSessionDefaults` carries no `actor` field although the accepted
  actor-attribution design slated it for first-accepted-version adoption; its
  record family has since committed at storage version 1 without one, so under
  the first-acceptance storage freeze later adoption needs a kind-scoped storage
  version, while the truthful `Owner` backfill that design relies on still
  exists.
- `CreateSession` actor attribution remains implicit pending an explicit owner
  amendment choice.
- `Recovery`, `Model`, and `Tool` actor variants have no constructing boundary;
  per-transition attribution adoption schedules remain open.
- The 1 MiB content bound is a provisional owner floor; the resource-governance
  limit question stays open, and non-text content kinds remain unconstructible
  pending their owning decisions.
- Session archive and retention lifecycle are absent from the aggregate and
  remain open ([open-questions.md](../open-questions.md), archival and
  retention).

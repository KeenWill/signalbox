# Sessions and the transcript

The bounded browser session descriptor and historical timeline foundation are
verified against the parent PR (`agent/web-session-timeline`). Typed detail
bodies and progressive body continuation are verified against the detail stack
through `agent/web-timeline-detail-bodies`.

The user-vocabulary surface on this page was re-verified through PR #378
(`agent/user-vocabulary`).

The multipart user-content aggregate below is the foundation proposal from PR
`#553` (`agent/blob-storage-foundation`) and becomes verified with its
implementing child stack.

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
input-delivery surface and its user-reachable steering boundary were verified
through PR #302 (`agent/mid-turn-steering`). The copy-on-create session-template
provenance and creation mode were verified through PR #311
(`agent/session-templates-spec`). Delegated creation provenance and its durable
mapping are the foundation proposal at the bottom of the delegation stack and
become verified only with its implementing child pull requests. The append-only
context-compaction record and projection were verified through PR #312
(`agent/context-compaction-core`); the command path and canonical visible-range
selection were verified through PR #314 (`agent/context-compaction-protocol`).
The runner placement-entry paragraphs are the foundation proposal at the bottom
of their implementing stack and become verified only with those child pull
requests. The imported-conversation record and converter are owned by
[conversation-import](conversation-import.md). Where a law is cited as
`INV-NNN`, the generated [invariant test index](../invariants.md) resolves it;
where mechanics owned by another contract are summarized, the owning sibling
page is linked inline.

The path-scoped session-placement domain and persistence paragraphs were
verified through PR #423 (`agent/scoped-visibility-placement`); fail-closed
current-head authentication is additionally verified against the parent slice
(`agent/scoped-visibility`). The read-scope enforcement and process surface are
verified against this PR (`agent/scoped-visibility-wiring`).
Defaults-replacement settings admission and its locked expected-epoch handoff
are verified against this PR (`agent/model-settings-execution`). The
automatic-reconciliation child outcome — the failed result carrying the
`ChildResultUnavailable` reason and the exact reconciled child turn that the
daemon's durable attempt seals for a parent whose delegated call the provider
can never settle — is verified against this PR
(`agent/turn-lifecycle-hardening`).

## Session identity and creation provenance

A session is one durable, independently browsable conversation with its own
`SessionId`, distinct from every other identity kind (INV-001). Every session
records two required, independent, immutable creation facts, paired as
`SessionCreationProvenance` (INV-003):

- **Creation cause** — why the session exists. The constructible variants are
  `UserInitiated` and `Delegated { spawning_request }`. The delegated variant is
  produced only by the spawning-request path and fixes ancestry to `None`;
  application and schedule causes are not represented as placeholders.
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

`CreateSession` carries the durable command identity, the provenance pair, one
complete unversioned initial defaults value, one path-scoped placement decision,
and one optional complete session runner placement, plus its explicit or
template-derived creation mode. Structural equality excludes the command
identifier. Explicit mode compares provenance, the complete defaults, and the
placement; template-derived mode compares provenance, the placement, and the
caller-supplied template name while excluding the copied defaults and content
digest. The two modes are never equal (INV-012, INV-047). Three topics are owned
by [identity-and-commands](identity-and-commands.md): durable-command storage,
the structural-equality doctrine, and identity generation, supply, and encoding.

The placement is absent for a daemon-only session. When present it is the
complete immutable request — runner selector, working-directory selection,
credential-profile selection, workspace requirement, sandbox profile, and tool
permission overrides — with every axis stated explicitly and none inferred from
another; the axes and their independence are owned by
[runner protocol and placement](runner-protocol.md#session-composition). Because
placement is a caller-supplied semantic field, it participates in replay
equality in both creation modes: replaying one command identity under a
different placement, including under a placement where the first handling had
none, is conflicting reuse rather than a corrected request. Template-derived
creation carries the same placement field as explicit creation; a resolved
template supplies defaults and never a placement, so the two choices compose
instead of excluding each other and no selected placement can be silently
discarded.

Application orchestration (`crates/application/src/create_session.rs`):

- rejects nil/max sentinel command identities before canonical construction;
- fixes cause `UserInitiated` and ancestry `None` — the request type has no
  cause or ancestry inputs;
- mints one fresh UUIDv7 `SessionId` candidate per invocation (the UUID
  timestamp confers no domain order or authority); and
- calls one atomic transaction port exactly once, with no retry.

Domain preparation admits only the user-initiated, no-ancestry pair. A
`SingleSource` command is a valid canonical value but fails preparation with
`TranscriptAncestryUnavailable` — a nonterminal error that claims no command
identifier. Forks are therefore typed but not yet creatable. Import-seeded
creation uses the separate command path below; it does not widen
`CreateSession`.

The committing transaction atomically inserts the session row, the scheduler
registration (`session_scheduler`), defaults version one, the current-defaults
pointer, the typed command record, the user-global registry claim, and — when
the request carried a placement — that session's initial `Unpinned`
`SessionRunnerPlacement` record at revision one with its complete request. A
visible session therefore never names a placement its creation command did not
carry, and a carried placement is never dropped between the claim and the
session. Completeness at every commit boundary is enforced by deferred reverse
foreign keys (`session_current_defaults_fk`, `session_scheduler_row_fk`) plus
deferred constraint triggers `session_requires_creation_command` and
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

### Path-scoped session placement

Path placement is opt-in. `Pathless` is the compatibility value and preserves
today's cross-session read behavior exactly. A placed session carries one
validated dotted path whose nonempty ASCII label segments admit letters, digits,
hyphen, and underscore; each segment is at most 64 bytes and a path is at most
64 segments. Process-protocol requests and responses admit that same complete
structural range, preserving exact replay of placement commands already recorded
under the migration. The initial value is pinned by creation. Only the explicit
`UpdateSessionPlacement` durable command changes it, appending a versioned
`Updated` event that names its predecessor and command identity; creation itself
appends version-one `Created`, so no update rewrites history. Every
current-placement load authenticates the contiguous history from version one
through the selected head against each event's typed receipt and durable-command
registry claim and rejects a head when immutable history contains a later event.
Equal native and imported-frontier creation replay likewise rejects a missing or
lagging current head while reconstituting its immutable creation receipt. A
placement-update replay authenticates the current head event and rejects either
a head that selects no authenticated event or a head that lags later history
before reconstituting applied or stateful-rejection evidence. A missing or
lagging head, cross-wired history, or invalid command fact fails closed as typed
storage corruption.

A placed requester's readable scope is its parent directory's subtree. The
decision computes the requesting path's parent prefix once and performs one
prefix comparison against the target placement: siblings and descendants are
allowed; ancestors, pathless targets, and disjoint subtrees are refused. A
refusal is typed evidence containing that requesting directory and the closed
reason `OutsideRequestingDirectorySubtree`, never an empty successful result.
The conversation tool renders ordinary refusals with the full reason name. If a
maximum-width directory would exceed the unchanged durable error-detail bound,
it uses the closed compact spelling `o<requesting-directory>`; `o` means that
same outside-requesting-directory-subtree reason and the directory remains
byte-exact. The conversation-introspection adapter enforces this decision when
it opens a selected native transcript. It loads requester and target placement
and applies the prefix decision in the same repeatable-read transaction that
opens the transcript cursor. Conversation-list inventory is discovery rather
than a selected transcript read, and imported conversations are not sessions;
neither surface is filtered by this rule.

A one-segment placement sits in the root directory and therefore has global
conversation read, including pathless sessions. It is legal only through the
loud `SessionPlacement::root_global_read` constructor, which requires
`RootPlacementGlobalReadIntent::Acknowledged`. The creation command, typed
record, and version-one event all preserve both its path and the explicit
global-read-intent bit. Ordinary scoped construction rejects a root path.

Why (append-only, two pointer exceptions): provenance, defaults versions,
placement events, command receipts, and scheduler registration are historical
facts; in-place mutation would rewrite recorded intent and the context that
later work consumed. The current-defaults pointer and
`session_current_placement` head are mutable because each selects a present
choice without rewriting history.

### Create from an imported frontier

`CreateSessionFromImportedFrontier` is a distinct durable command family
carrying command identity, one addressable `ImportedTranscriptFrontier`, one
`ImportedSessionRelationship` (`Resume` or `Fork`), complete unversioned initial
defaults, and the same optional complete session runner placement as ordinary
creation, on the same replay-equality terms. The frontier itself names its
`ImportedConversationId` and inclusive entry boundary; the command accepts no
second independently supplied conversation identity. Its structural replay
equality excludes only command identity. Separating the family preserves its
imported-ancestry contract and keeps its replay record distinct from the
no-ancestry `CreateSession` family; the shared defaults-bearing storage versions
are owned by [persistence-protocol](persistence-protocol.md).

The relationship records the client's creation-time intent: `Resume` declares a
new Signalbox continuation from the selected imported point; `Fork` declares a
new Signalbox branch from it. Both create independent session identities, use
the same exact imported prefix, and leave the imported conversation unchanged.
Neither mode resumes a provider process, mutates a source file, or grants
external execution authority.

Import never chooses this relationship or a frontier. At any later time, and
more than once, a client may invoke this session-creation command against any
entry boundary of any imported conversation.

The process protocol exposes that command as
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
user-global claim protocol in [identity-and-commands](identity-and-commands.md).
A claimed identifier resolves to its recorded equal replay or conflicting reuse
before any imported-target lookup. Only for an unclaimed identifier does the
transaction load the complete imported conversation named by
`frontier.conversation()`, resolve exactly positions `1..=N` through that
frontier's inclusive boundary, and either:

- returns `ImportedConversationNotFound` or `ImportedFrontierNotFound` without
  claiming the command identity; or
- atomically claims the command and creates the complete session seed, with a
  lost claim race re-inspected against the winner by the shared protocol.

An equal replay returns the recorded created session and ignores unused fresh
identity candidates. Changed frontier, relationship, or defaults under an
already claimed command identity is conflicting reuse; selecting another
conversation necessarily changes the frontier. Cross-kind reuse follows the
user-global durable-command contract in
[identity-and-commands](identity-and-commands.md).

The committing transaction atomically inserts:

- the user-initiated session whose immutable ancestry names the imported
  conversation and boundary derived from the selected frontier, plus the
  relationship;
- defaults version one, its current pointer, scheduler registration, typed
  command record, registry claim, the initial `Unpinned` placement record when
  the request carried a placement, and the ordinary `session_created` outbox
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
only as part of the complete successor epoch — there is no prompt-only mutation.
This section owns the capacity and epoch-placement contract. Matching
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
frozen epoch already records durably which prompt governed it. The
`ModelIdentityChanged` boundary below remains keyed to the frozen direct model
selection alone.

### Session-template provenance

A user-initiated session may carry one optional immutable
`SessionTemplateProvenance`, distinct from its creation cause and transcript
ancestry. Presence pairs a validated `SessionTemplateName` with an exact 32-byte
`SessionTemplateContentDigest`; absence denotes explicit creation. Template
provenance never joins defaults replacement, origin freezing, model call
preparation, imported continuation, or transcript content.

For template creation, the daemon supplies a resolved bundle containing the
model-selection request, system prompt, and dangerous-tool blanket. Domain
creation establishes the ordinary defaults version one from that complete copy
and seals the name/digest alongside the session. The stored session has no
template lookup operation: every later consumer reads its durable defaults and
provenance only (INV-047).

A template supplies no placement. Template-derived creation therefore carries
the caller's optional placement exactly as explicit creation does, and its typed
command record stores that placement in full. Why: silently discarding a typed
flag a caller supplied is the false-confidence pattern — the session would run
daemon-only while the caller believed it had a runner — so the two choices
compose and neither excludes the other.

Durable-command equality distinguishes the caller's two creation modes. An
explicit command compares its complete caller-supplied defaults exactly. A
template command compares the caller-supplied template name and ignores the
daemon-resolved candidate bundle for equal replay, so the same command and name
returns its first recorded session after a template edit. A different template
name, or switching between explicit and template creation under one command
identity, is conflicting reuse. First handling still stores and cross-checks the
complete resolved defaults and digest; this replay rule cannot rewrite them.

Migration `202607300101_session_template_provenance.sql` adds nullable
name/digest pairs to `session` and `create_session_command`, with `MATCH FULL`
shape, name validation, a 32-byte digest bound, append-only protection, and
command/session agreement. Existing and explicit sessions backfill as absent; no
applied migration is modified.

`ReplaceSessionDefaults` carries exactly command identity, target session,
expected current version, and the complete replacement; equality excludes only
the command identifier. The handling transaction loads the authoritative session
and compare-and-sets the expected version:

- expected differs from current → recorded `CurrentVersionMismatch`;
- absent session → recorded `SessionNotFound`;
- no representable successor → recorded `VersionExhausted`;
- otherwise the applied result carries the complete installed version.

The expected-version check is enforced twice inside the one transaction
(`crates/persistence/src/replace_session_defaults.rs`); the lock mechanics are
owned by the [persistence lock protocol](persistence-protocol.md#lock-protocol).
For an unseen command, the adapter locks the current-defaults pointer before
domain preparation loads the authoritative session. When preparation yields the
applied result, the adapter moves that locked pointer with a SQL compare-and-set
conditioned on the expected version. Zero affected rows re-derives the result
against current state in the same transaction and records the typed rejection; a
re-derivation that still reports applied — a CAS loss without a version change —
fails closed as corruption, as does an update affecting more than one row. A
boundary that must defer settings validation may ask this same transaction to
admit rejection only: a mismatch is recorded, while an expected version that is
current under the lock rolls back the command claim and applies nothing. Equal
replay and cross-kind identifier reuse resolve through the same fail-closed
reconstitute-and-compare path as `CreateSession` (INV-012). Before returning a
settings-validation error after an unlocked read, the adapter repeats this
rejection-only admission: a concurrent pointer advance records its authoritative
mismatch, while a current expected version under the lock linearizes the caller
error before any later advance.

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
unused or redundant replacement epochs (INV-046). The provider-message
projection is owned by [model-call execution](model-call-execution.md). Started
frontiers committed before this boundary existed retain their exact historical
membership: an immutable per-turn compatibility fact grandfathers only those
already-active or terminal starts. Turns still queued at migration and every
newly accepted turn require the boundary normally.

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
excess before command handling; this section owns the capacity contract.

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
fixes `Actor::User`. A separate purpose-specific application constructor accepts
only `Actor::Tool { request }` for the exact executing tool request; model,
recovery, and arbitrary caller-selected actors remain unconstructible. First
handling locks the target session, then either records `SessionNotFound` without
an effect or atomically replaces the complete root, tag, and attribute snapshot.
After acquiring that lock, a separate statement samples PostgreSQL statement
time at microsecond precision; the applied result records it together with the
command actor as the one last-writer stamp. An equal replay returns that exact
recorded result and timestamp. The database timestamp is result evidence, not
caller intent or a global ordering token, so it does not participate in command
equality.

There is deliberately no expected or installed metadata version and no versioned
metadata-history API. Two distinct writes are last-writer-wins after
serialization on the session row; a full replacement can overwrite an earlier
writer's unrelated field. Callers that need to preserve fields read the current
snapshot before forming the replacement. The user-global durable-command
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

Because `UserInitiated` is the only constructible creation cause and every
current session-creation boundary lacks actor attribution, the implemented
default view is exactly all non-archived sessions. No visibility taxonomy,
creation-time override, or inference from missing attribution is stored. The
dependency for future creation-derived visibility is recorded in
[open-questions.md](../open-questions.md#session-organization-visibility-and-retention).

**Committed unimplemented functionality.** No present surface constructs a
program creation cause. The closed creation-cause vocabulary gains `workflow`
and `eval` variants for sessions created by registered programs under the
[program substrate](program-substrate.md): each names the creating program run
(and, for `eval`, the trial identity the [evaluation system](eval-system.md)
defines), is constructible only by the substrate's host-side session capability,
and joins the stored closed-discriminator convention beside `user_initiated` and
`delegated`. This constrains present change: creation-cause readers must not
assume the two-variant vocabulary is final, and the stored discriminator's
decode surface must stay extensible without reinterpreting existing spellings.

## The session aggregate

The long-lived domain `Session` (`crates/domain/src/session.rs`) contains
exactly four facts: `SessionId`, the immutable creation provenance, optional
immutable template provenance, and the complete current defaults version
selected by the durable pointer. It embeds nothing else — no transcript entries,
accepted inputs, turns, queue facts, command history, evidence, or presentation
state (INV-005). Those remain independently stored facts correlated by typed
identity.

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
session row — including its creation provenance and optional template provenance
— its one current-defaults pointer, and exactly the version that pointer names
(`crates/persistence/src/session.rs`). For imported ancestry, the same bounded
read joins the one-to-one seed record and its frontier header as a constant-size
proof: seed and frontier ownership and identity must agree with the session, and
the stored member count must equal the selected imported boundary position. It
does not materialize the imported conversation, frontier members, or semantic
entries. Full prefix comparison belongs to creation replay and purpose-specific
semantic-context resolution. The pointer is authoritative; a load never infers
current defaults from version one, the greatest stored version, a
caller-supplied version, or a cache.

Why (pointer authority): append-only version existence does not mean
installation; only the pointer records the accepted current choice.

`None` is returned only when no session row exists in the read snapshot. Once
the row exists, a missing pointer, missing selected version, ownership mismatch,
pointer/record version disagreement, unknown discriminator, invalid ordinal, or
an absent or inconsistent bounded imported-seed proof fails closed as typed
corruption: the adapter's decode checks feed the domain-owned
`SessionReconstitutionInput::reconstitute` seam. Its complete input retains the
requested and stored session identities, creation provenance, optional template
provenance, the current-pointer identity and version, and the selected defaults
record's identity, version, and value; it accepts only complete agreeing domain
values (INV-002, INV-039). Reconstitution never yields `None`, a default, or a
partial session.

Why (fail closed): a fabricated or partial session would mask corruption and
launder invalid durable state into valid-looking domain values.

## Bounded browser session timeline

The browser historical plane addresses every durable session event by the pair
`(session_id, event_sequence)`. On the wire the session UUID is carried by the
endpoint or result and `WebTimelineAddress.event_sequence` is the positive
decimal string of the global durable outbox sequence. The sequence is allocated
once across ordinary and delegation outbox events. It is append-only, totally
ordered, independent of table offsets and query plans, and never renumbered.
Another session's events may create gaps. A search or other navigator therefore
carries both fields and can open an unloaded region with an `around` read;
arithmetic adjacency is never required.

`GET /api/sessions/{session_id}` returns the exact first and latest addresses,
durable event count, current global observation cursor, active and queued turn
counts, and explicit projected-size facts. Projected text bytes are the UTF-8
bytes currently stored inline for accepted input, assistant text, and context
summaries. Projected structured bytes sum a fixed 64-byte header envelope plus
the UTF-8 event-kind spelling for every durable event. These values are loading
policy estimates, not encoded-response promises. Referenced blob count and byte
length are separate facts and are zero until a durable timeline-to-blob relation
exists; a future nonzero byte length will still describe a reference, not
materialized bytes.

`GET /api/sessions/{session_id}/timeline` accepts `first`, `latest`, `before`,
`after`, and `around` anchors. Addressed anchors require one positive decimal
`address`. Every request supplies `max_items` from 1 through 256 and `max_bytes`
from 256 through 65,536. Persistence reads at repeatable-read isolation, fetches
at most 257 lightweight headers, enforces both requested limits, and returns
items in address order. `continuation_before` is the first returned address only
when earlier items exist; `continuation_after` is the last returned address only
when later items exist. Thus truncation is explicit, and continuing repeats only
the boundary address in the request, never an item in the next strict keyset
window. The `around` query forms bounded candidates independently from the
indexed prefix and suffix before choosing the nearest headers; it does not sort
the session's lifetime event set.

Each header retains one of the closed ordinary, goal, model, tool, runner, or
delegation event categories. An unknown durable category is corruption rather
than generic prose. This foundation intentionally exposes header facts rather
than storage records or process frames. Browser DTOs are generated from the Rust
web-contract schema; application values, persistence rows, browser DTOs, and
presentation items remain distinct.

The browser session-history adapter validates the generated DTOs, treats every
64-bit value as a decimal string and `bigint`, clamps each request to the server
ceilings, and retains at most 768 event headers. Every newly inspected window is
retained preferentially, so moving among the first, latest, and an arbitrary
million-event address never makes lifetime history a client-memory precondition.
Transcript `full`, `condensed`, and `results` remain local presentation choices
and do not alter any server query.

### Bounded typed timeline detail

Three historical reads share one application-owned detail page and one generated
browser DTO: `GET /api/sessions/{session_id}/timeline/{address}/detail` selects
one item, `GET /api/sessions/{session_id}/turns/{turn_id}/timeline-detail`
selects the events associated with one exact turn, and
`GET /api/sessions/{session_id}/timeline-detail` selects an inclusive region
from its required `first` address through its required `through` address. Every
request supplies `max_items` from 1 through 128 and `max_bytes` from 256 through
65,536. Turn and region reads select at most 129 addresses and return at most
128 detail records; item reads retain the same explicit item ceiling for typed
bodies with repeated members. All selected rows are decoded through the same
fail-closed typed outbox projection as durable dispatch, under one
repeatable-read transaction.

The response reports `projected_body_bytes` and never silently truncates. A
continuation is either `more_at`, naming the exact stable address at which the
next item read starts, or `more_body`, naming the same stable address, one
closed body field, a repeated-member index, and the next UTF-8 byte offset.
Requests resume with `cursor_address` and, for body continuation,
`cursor_field`, `cursor_member`, and `cursor_offset`. Offsets must be UTF-8
boundaries. The byte accounting charges a fixed 128-byte body envelope and the
exact UTF-8 excerpt bytes, so a response cannot exceed the selected
projected-body budget. An oversized text is a typed bounded excerpt carrying its
total byte length and exact continuation; it is never flattened into a summary
that appears complete.

Detail bodies project accepted input with reference-only attachment facts;
model-call request context count, selected model identity, response text,
reported token usage, terminal disposition, and provider failure cause code; and
activated or terminalized turn lifecycle with a cause code. The remaining closed
variants carry session creation and imported-frontier evidence, settings
changes, tool requests and attempts, explicit approval provenance and judge
escalation, closed goal transitions and blocked reasons, context compaction,
reconciliation and operator-required parking, runner sandbox posture, and
delegation updates and wakes. Child-spawn detail preserves the selected
background or bound policy, including both bound parent-terminal actions. Tool
arguments, results, failures, approval rationale, goal text, compaction
summaries, and delegation content use their own continuation fields; repeated
tool members advance by explicit member index. Tool-attempt state, terminal
disposition, failure cause, sandbox posture, and result and failure payloads are
snapshotted into the transition's immutable detail members when the transition
event commits, so a historical transition address never changes as the live
attempt row advances. An unknown durable event or state is corruption, never a
generic body or guessed prose.

Browser DTOs remain distinct from the application projection, persistence rows,
and process messages. Text already masked before durable storage remains masked:
the read path neither consults credentials nor reconstructs provider-native
material. Blob facts are references with identity, length, and optional media
type only; detail reads do not fetch blob bytes.

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
- `RunnerPlacementChanged { placement_revision }` — a reference to the complete
  checked successor placement record at a user-explicit relocation boundary. One
  entry kind covers every session-relocation fact: a move to a different runner
  and a working-directory move on the same runner both require it, and the
  referenced record is the authority for which of them occurred. Splitting the
  kind so that a working-directory-only move carries its own payload variant
  changes no other contract on this page, since every consumer resolves the
  record rather than reading the payload;
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
imported-frontier session creation is the only producer of `Imported`; sealed
tool transitions produce tool-use/result references only through the atomic
boundaries owned by [tool-loop](tool-loop.md); and the checked owner placement
transactions are the only producers of `RunnerPlacementChanged`.

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

Each compaction range starts at the current model-visible frontier start: the
complete frontier's first entry for a root compaction, or the predecessor
summary entry for a successor. Its through endpoint selects how much of that
visible frontier the new summary replaces; a compaction cannot hide an
unsummarized visible prefix. The through boundary is safe only when every
assistant tool proposal inside the summarized range has its execution result,
denial, or turn-end closure inside that same range; a boundary cannot leave a
provider-visible tool result in the suffix after hiding its proposal.

For model input only, summaries are applied in physical append order to the
current model-visible sequence. Each summary replaces the visible prefix through
its exact boundary with itself; entries after that boundary in the already
projected sequence remain in order, even when a retained suffix physically
precedes an earlier summary. The final sequence is therefore the latest summary
plus its visible suffix. With no summary, projection is the complete frontier
order. This rule deliberately separates the frontier a call durably records from
the ordered subset the selected model sees.

Explicit compaction chooses an optional through position, defaulting to the
latest safe boundary. The daemon also compacts before an ordinary model send
when that call's rendered input plus its full configured output-token
reservation would exceed the current selection's declared context window. Both
paths use the required deployment-configured compaction prompt and the session's
current direct selection. Trigger and configuration mechanics are owned by
[model-call-execution](model-call-execution.md).

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

Session relocation has a session-level frontier boundary. Every transaction that
installs a successor placement — loss replacement today, and the committed
user-directed move of a healthy session or of its working directory later
([runner protocol and placement](runner-protocol.md#committed-functionality-beyond-version-one))
— appends one `RunnerPlacementChanged` entry after the latest authoritative
semantic frontier, or establishes a one-entry root when no frontier exists, and
advances the session placement-frontier pointer with the placement revision.
Active continuation and the next eligible origin both extend that exact boundary
before any execution on the successor placement. A same-revision,
missing-record, non-prefix, cross-session, or second placement boundary fails
closed. When the installing command runs while an authorized model call is still
in flight, the boundary is appended only after that call's observation commits,
so the call's own entries precede it and the prefix-only law holds
([turn-lifecycle-and-scheduling](turn-lifecycle-and-scheduling.md#runner-loss-session-recovery)).
The entry copies no runner advertisement, workspace path, credential fact, or
tool output; the placement record remains its content authority. The provider
projection resolves that record to the exact injected placement event owned by
[model-call execution](model-call-execution.md#frontier-rendering) (INV-015,
INV-044).

Pending steering has a separate safe-point boundary (INV-036). A `steer` submit
accepted while its exact source turn is active returns the accepted-input
identity, immutable acceptance position, and source turn immediately; it creates
no origin turn or semantic entry at acceptance. Immediately before an initial or
continuation call is prepared, the transaction appends one
`SteeringAcceptedInput` per pending input in ascending acceptance position,
derives one frontier extending the starting frontier for the admitted initial
call, changes every input to `ConsumedAsSteering { call }`, and inserts that
exact `Prepared` call against the extended frontier. All four effects commit or
roll back together. The entry therefore becomes semantic history only with the
call that first observes it; the immutable accepted-input row remains the
content authority.

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

**Committed unimplemented functionality — pre-call pool exhaustion.** The
credential-pool implementing child adds a third `TurnFailed` producer for the
`pre-call fail` and `wait-transition fail (no call)` endings of
[the credential-availability machine](credential-availability.md#the-credential-availability-machine):
an active turn exhausts its frozen pool before any model call is prepared and
that exhaustion selects no wait. Its single transaction ends the current attempt
`KnownFailure`, appends the marker after the attempt's starting frontier,
terminalizes the turn `Failed`, and emits both the ordinary `turn_failed` update
and the typed `turn_credential_pool_exhausted` event. The sealed failure and
complete member evidence are owned by
[model-call execution](model-call-execution.md#availability-successor-calls). No
present transcript writer can produce this shape.

That third producer serves two endings, which share its commit shape exactly.
This page owns the transcript-producer column of
[the credential-availability machine](credential-availability.md#the-credential-availability-machine),
and that column is total over all nine endings: `pre-call fail` and
`wait-transition fail (no call)` use this new producer; `post-failure fail` and
`terminal` use the existing model-call known-failure closure, because their turn
did issue a call and that closure is already the writer which commits one; and
`selected`, `contended-wait`, `exhausted-wait`, and `successor` have no producer
and append no entry, because none of them terminalizes a turn.

The remaining ending, `wait-transition fail (after call)`, needs a **fourth**
producer, because it is a transition rather than an initial admission and the
model-call closure cannot serve it: that closure committed earlier in the turn
without terminalizing and is not available to a transition happening now. Its
producer is a wait-transition failure producer, which commits the same shape as
the pre-call producer and additionally names the predecessor model call whose
cause it carries. This inventory is closed at four.

## User content

Accepted-input content is `UserContent`, one ordered nonempty sequence of closed
text or attachment parts under the cross-crate contract owned by
[blob storage](blob-storage.md#multipart-user-content). A text part carries
`NonEmptyUnicodeText`; construction rejects empty text and any text containing
U+0000 (which PostgreSQL text cannot store), while whitespace-only text remains
content. The domain applies no trimming, Unicode normalization, case folding, or
other rewriting. Equality is exact part order plus each part's complete value
and metadata, so normalization-distinct text spellings are unequal and any
attachment difference changes replay equality. That exact value participates in
`SubmitInput` replay equality (INV-012).

Why (exact, unnormalized): replay equality must not depend on a normalization
policy; search or display projections may normalize without changing accepted
intent.

The process boundary exposes the existing delivery algebra without changing
content ownership. An accepted omitted or explicit `start_when_idle` submit and
a `queue` submit create an accepted origin turn with frozen configuration;
`queue` binds its acceptance to the exact active turn it follows. A `steer`
submit instead creates configuration-free pending steering bound to that exact
source turn. The closed wire spelling and typed receipts are owned by
[process-protocol](process-protocol.md#client-requests).

The accepted input owns the one immutable authoritative content value; the
`accepted_input` row admits exactly two guarded updates from pending steering:
consumption to `consumed_as_steering`, changing only disposition plus the exact
consuming call, or reclassification to `reclassified_as_turn_origin`, changing
only disposition plus the fresh origin turn. Neither changes content, and
semantic history references that content rather than copying it (INV-005,
INV-007, INV-036).

### Bounds

The multipart value and application admission apply the exact structural,
text-byte, and attachment-metadata bounds owned by
[blob storage](blob-storage.md#multipart-user-content) before typed command
construction, so no command identifier is claimed for a structurally invalid
value. Typed construction and the registry claim precede the current-state
catalog-existence, aggregate-attachment, and prospective-complete-frontier
checks. Failure of one of those post-claim checks records its closed terminal
rejection and no accepted-input effect. Resource failures retain counts and
configured maxima, never rejected text or attachment metadata. The final schema
stores one complete ordinally guarded part sequence in each mirrored command and
accepted-input satellite, with no `content_text` authority; its one-time
migration and exact storage version are owned by that same cross-crate contract.

Why (bytes and parts, at admission): measurement matches wire, storage, and
verification cost and keeps the domain value exactly as accepted. Stable shape
failures precede construction, while checks whose answer depends on current
catalog or session state occur under durable command authority; neither path can
truncate, reorder, or rewrite content.

This is a provisional maintainer-approved floor, not the resource-governance
policy.

## Actor attribution

The actor algebra (`User`, `Model { turn }`, `Recovery`, `Tool { request }`),
its participation in structural replay equality, and its closed-discriminator
storage convention are owned by
[identity-and-commands](identity-and-commands.md). Attribution is provenance
only — not authentication, authorization, or approval — and model agency can
never compare equal to user agency (INV-020).

The session-command consequences: `SubmitInput` and `ReplaceSessionMetadata` are
the command payloads carrying an actor inside the conversational command
surface. `SubmitInput` fixes `Actor::User`. Metadata replacement admits the
user-facing constructor plus the purpose-specific `Actor::Tool { request }`
constructor described above; neither accepts arbitrary agency. Domain
reconstitution compares the stored actor against the canonical command and fails
closed on mismatch (`StoredActorMismatch` for input and `CommandActorMismatch`
for metadata). The metadata actor also becomes its organizational last-writer
stamp.

Why (seeded before expansion): carrying user attribution from the first metadata
write preserved a truthful backfill, and the existing closed actor columns now
admit tool attribution without a semantic migration.

`CreateSession` carries no actor; amending it remains an explicit maintainer
choice that has not been taken. `Recovery` and `Model` remain representable
without an implemented command-producing boundary.

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

## Session delegation

This section is the foundation proposal at the bottom of the session-delegation
stack and becomes verified only with that stack's scheduling and tool/client
pull requests. A delegated child is a distinct, independently browsable session.
Its `SessionCreationCause::Delegated` names the exact spawning `ToolRequestId`;
its `TranscriptAncestry` is independently `None`. Delegation does not copy,
reference, merge, or expose the parent transcript, and it does not widen the
none-or-one ancestry baseline.

The child copies the complete `SessionConfigurationDefaults` value from the
immutable defaults epoch frozen to the parent turn that owns the spawning
request. The spawn transaction resolves that stored epoch through the parent
turn's frozen defaults version and establishes the exact copy as the child's
defaults version one. It never reads the parent's current-defaults pointer for
this choice, so replacement after parent-turn acceptance, including replacement
while the spawn request awaits approval or execution, cannot change the child.
Tool arguments supply no defaults field.

The delegated-task turn nevertheless retains its parent's exact requested and
frozen model configuration as turn-origin provenance. A direct override remains
an override, and an alias retains both its frozen definition and selected direct
model; reconstitution does not replace either form with the copied child default
merely because both resolve to the same effective model.

The checked spawn task becomes one `DelegatedTask` semantic entry in the child,
referencing the exact spawning request and its parent session and turn. It is
model/tool-authored delegation work, not accepted input and not `Actor::User`;
the child's first turn has a distinct delegation-task origin and starts from
that entry. Reconstitution resolves the request and requires its checked task
bytes, parent, turn, child relationship, and entry to agree before the task
becomes model-visible.

Each spawning request creates at most one immutable parent/child relationship.
The public domain surface accepts neither a caller-supplied relationship count
nor an unsealed relationship slice as evidence of that uniqueness. Aggregate
construction remains sealed in the foundation slice; the persistence slice in
this stack admits a spawn only from the complete parent relationship inventory
held under the spawn transaction's lock, together with the child-session
uniqueness check. The relationship records the exact parent session and turn,
child session and delegated-task turn, and one parent-chosen policy:

- `Background` never derives a child stop or cancellation from a parent state;
- `Bound` states separate `on_parent_stopped` and `on_parent_cancelled` actions,
  each exactly `KeepRunning`, `Stop`, or `Cancel`.

The `SessionDelegation` aggregate records an admitted sealed
`DelegatedSpawnRequest`'s parent, bounded task, policy, child, and spawn
provenance as the first event in one contiguous history. Typed await and message
requests may act only on their exact relationship and only under sealed
in-flight dispatch authority carrying that complete immutable request; matching
identities cannot substitute a different producing call, ordinal, tool name,
arguments, or approval posture. Consuming transition failures return the
unchanged aggregate and attempted input. Message delivery remains available
after a terminal outcome. Outcome authority is checked against the relationship
before recording, including an exact match to this spawn's delegated-task turn:
an equal authority-and-outcome replay is idempotent, `ContinueRunning` preserves
the active lifecycle, and every other outcome terminalizes it.

A user termination command also carries `ParentAlone` or `ParentAndDescendants`.
`ParentAlone` does not evaluate descendants. The descendant form walks the
durable relationship tree: background edges and bound `KeepRunning` edges
produce explicit continue-running dispositions, while bound stop/cancel actions
produce the corresponding typed outcome. If the child already has its unique
terminal result, the edge instead records `AlreadyTerminal` with the new parent
command provenance and an exact check of that prior result; it creates no second
terminal result. Traversal still visits that child's outgoing relationships.
Every evaluated relationship therefore records an outcome with the parent event,
exact spawn request, and user command provenance. No path deletes the child or
its history, and neither a continued child nor a terminated child can become a
silent orphan or silent kill.

Delegation messages are immutable, bounded, nonempty content records with a
distinct `DelegationMessageId`, the spawning relationship, exact sender and
recipient, per-relationship ordinal, and sending `ToolRequestId`. Parent and
child may each send to the other before or after either session stops, cancels,
or completes. `DelegationMessage` semantic entries refer to those records; they
do not reclassify model-authored content as input from the user. Undelivered
messages and background results share one positive, gap-free `delivery_sequence`
allocated under the recipient session lock. An active recipient consumes pending
items at the next model-call safe point in that recipient-wide order. An idle
recipient gets one delegation-origin queued turn, and further items coalesce
into its starting frontier in the same order until activation. Per-relationship
message ordinals remain provenance and do not serve as a cross-relationship
tie-break. Message admission preserves the final positive relationship ordinal
for a future terminal child outcome. Exhaustion therefore rejects the
nonterminal message with typed transition evidence instead of allowing later
child terminalization to fail without a result event.

A child result is delivered content, never transcript access. Its immutable
record targets the exact spawning request and carries either the returned
`DelegationContent` or a typed failed, stopped, or cancelled outcome together
with exact provenance. Returned content, failure, and a child's own cancellation
carry the exact terminal child turn. Returned content is derived only from the
proof-bearing completed call; independently supplied text cannot authorize a
result. A completed turn with empty or oversized returned text records the
distinct `ChildResultUnavailable` reason. Reconciliation-required work is not
terminal delegation evidence on its own and produces no outcome while its
ambiguity stands. Automatic reconciliation is the exception: the daemon's
durable attempt seals the child as a failed result carrying that same
`ChildResultUnavailable` reason and the exact reconciled child turn, in the
transaction that commits the terminal transition, so a parent waiting on a call
whose provider outcome can never be established is woken by evidence rather than
left waiting on a turn that has already ended. **Committed unimplemented
functionality.** Durable terminal-result reconstitution is not exposed by this
foundation slice; the persistence slice must consume a sealed reconstituted
ended-call/turn projection rather than accepting parallel raw identities or
semantic entries. A parent-policy stop or cancellation instead carries opaque
authority from the exact applied parent termination result. Every authority
exposes its parent session, durable user command, command kind, and descendant
scope; a turn interrupt additionally names its exact turn, while a goal stop
names the exact goal generation and carries no turn. Raw identities cannot
construct that authority, `parent_alone` authority cannot produce a child
disposition, and the recorded outcome reason must match its command kind and
scope. Parent-policy stop and cancellation both terminalize the exact delegated
child turn through its existing cancelled-turn lifecycle state and exact
cancellation marker; the relationship outcome preserves whether the chosen
policy action was `ChildStopped` or `ChildCancelled`. `ChildStopped` is produced
only by a parent-policy stop; the existing proof-bearing failed, refused, and
cancelled model-call turn candidates can name any turn origin, including the
delegated-task origin, but do not fabricate a distinct stopped outcome from that
evidence. Delivery appends a `DelegationResult` semantic entry only to the
target parent, names the exact awaiting request that receives the result, and is
idempotent by that awaiting request. A foreground delivery correlates that entry
as the logical result of its still-open `await_session` request. A background
delivery retains the awaiting request only as delegation provenance, without a
tool-result correlation, because that request already completed with its
registration receipt; the result instead arrives as wake content. The immutable
child result remains keyed by the spawning request. A detached child may return
after the parent has stopped or cancelled; the result remains durable and
independently inspectable even when no parent turn can consume it.

**Committed unimplemented functionality.** A spawned child defaults into its
parent's directory. No present delegation or placement surface implements or
derives this default; its implementation is deferred to the session-placement
surface. This compatibility constraint does not copy the parent's complete
placement and this stack implements no placement logic.

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
- Assistant text, tool-use/result references, completed-turn, steering,
  cancelled-turn, delegated-task, delegation-message, and delegation-result
  semantic entries are implemented. Refusal, reconciliation, mismatch,
  accepted-risk, and approval-event variants remain open.
- `ReplaceSessionDefaults` carries no `actor` field although the accepted
  actor-attribution design slated it for first-accepted-version adoption; its
  record family has since committed storage versions 1 and 2 without one, so
  later adoption needs another kind-scoped storage version; the truthful `User`
  backfill that design relies on still exists.
- `CreateSession` actor attribution remains implicit pending an explicit
  maintainer amendment choice.
- `Recovery` and `Model` actor variants have no constructing boundary;
  per-transition attribution adoption schedules remain open.
- The multipart part-count, text-byte, metadata, and attachment-work bounds are
  provisional maintainer floors; the resource-governance limit question stays
  open.
- The session system prompt remains one optional bounded string per defaults
  epoch. Composition from base, per-use-case, and instruction-file sources and
  richer named profiles remain the open
  [configuration-category capability](../open-questions.md#configuration-categories).

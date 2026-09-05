# Sessions and the transcript

This subsystem creates sessions, holds each session's configuration defaults and
metadata, and records the semantic transcript that every model call reads.

## Map

A session is one durable, independently browsable conversation with its own
`SessionId`. This page covers session creation and ancestry, creation from an
imported frontier, configuration defaults and their replacement, replaceable
metadata and listing, the loaded session aggregate, semantic transcript entries
and context compaction, accepted-input content, actor attribution on session
commands, delegation between sessions, and the bounded browser reads over
sessions. The imported conversation record belongs to
[conversation-import](conversation-import.md), turn and attempt lifecycle to
[turn-lifecycle-and-scheduling](turn-lifecycle-and-scheduling.md), and
update-event delivery to [persistence-protocol](persistence-protocol.md); this
page owns the boundaries at which transcript entries commit. Every command on
this page claims and replays under the command contract of
[identity-and-commands](identity-and-commands.md), and the actor a command
carries is provenance only under the same page.

Every session records two independent immutable creation facts, paired as
`SessionCreationProvenance`. The cause says why the session exists: a user
created it, a module dispatched it, or a parent session's tool request delegated
it. The ancestry says where its initial context came from: no prior transcript,
one source session at one frontier, or one imported conversation at one
inclusive boundary. An imported ancestry also records whether the client resumed
or forked from that point. Resume declares a continuation and fork declares a
branch, and the relationship records creation-time intent only.

Two durable command families create sessions from outside a turn, and a
delegation spawn creates a child session from inside one. `CreateSession`
creates a session with no ancestry, from explicit defaults or from a named
session template. `CreateSessionFromImportedFrontier` creates a session seeded
with the semantic entries of an imported prefix and links it to that seed
frontier through an `ImportedSessionSeed`. Either family may carry a path-scoped
placement, which bounds which other sessions' transcripts the session may read,
and a runner placement, which [runner-protocol](runner-protocol.md) owns.

Session configuration defaults are immutable numbered epochs holding the model
selection, the dangerous-tool blanket, and an optional system prompt. A session
points at its current epoch, and a turn freezes the epoch current when its
origin input is accepted. Session metadata is a separate replaceable snapshot of
title, tags, attributes, and an archive flag. The loaded `Session` aggregate
holds identity, creation provenance, optional template provenance, and the
current defaults epoch, and nothing else.

The transcript is a sequence of semantic entries grouped into frontiers. A
`SemanticTranscriptEntry` is one immutable identified semantic-history fact with
its own identity, a source session, and a closed payload. Payloads reference
rather than copy: an origin or steering entry names the accepted input, an
assistant text or tool-use entry names the completed producing call, and a
tool-result entry names the attempt that owns the evidence. A context summary
carries the summary text its dedicated call produced and the inclusive range it
stands for. An imported entry carries one normalized imported content value with
its speaker attestation, covering text, tool, result, thinking, redacted
thinking, document, and content-absence forms. A model-identity entry marks
where executed history crossed to a different frozen model selection. Delegation
entries record a delegated task, a delegation message, and a delivered result.
Each turn ends with exactly one terminal marker: completed, cancelled, or
failed.

A `ContextCompaction` has five correlated immutable facts: its identity and
optional predecessor, the source frontier, a dedicated model call, the
summarized inclusive range, and the result frontier. The compaction-call record
separately retains the session's current direct selection, the resolved provider
target, the source frontier, the call's lifecycle and disposition, its
credential reference, and each optional usage field. Explicit compaction chooses
an optional through position, defaulting to the latest safe boundary; automatic
compaction is triggered as [model-call-execution](model-call-execution.md)
describes.

Accepted-input content is `UserContent`, one ordered nonempty sequence of closed
text or attachment parts. The multipart bounds belong to
[blob-storage](blob-storage.md).

A delegated child is a distinct independently browsable session whose cause
names the exact spawning tool request and whose ancestry is none. The
`SessionDelegation` aggregate records the parent and child, the task, the
parent-chosen policy, messages in both directions, and the child's one result.

The browser read plane serves a session catalog with attention states, a live
projection and follow stream for one session, a timeline of durable events with
typed detail, and lexical search. Its request and response shapes live in
`crates/web-contract`.

## Decisions

Cause and ancestry are recorded as independent facts, because deriving one from
the other would make ordinary forks look delegated and force delegated children
to inherit transcripts. The cause vocabulary carries no placeholder variants for
causes no surface produces. Imported ancestry names the external source and no
local frontier identity, because a materialized frontier is a Signalbox-owned
artifact; the seed frontier is linked through a separate record instead. A
source-session ancestry is typed but unconstructible, and imported-frontier
creation is the sole producer of imported ancestry.

Import-seeded creation is a separate command family rather than a widening of
`CreateSession`, so its imported-ancestry contract and replay record stay
distinct from the no-ancestry family. It takes no lock on the imported record,
because imported aggregates are immutable. It commits in one transaction, so a
visible seeded session never names a missing imported aggregate, a nonmember
boundary, a partial semantic projection, or an incomplete initial frontier.
Neither resume nor fork resumes a provider process, mutates a source file, or
grants external execution authority. Seeding emits no tool, call, attempt, or
turn lifecycle event, and imported entries never require or create
accepted-input, turn, attempt, call, or native tool records.

Creation facts, defaults versions, placement events, command receipts, and
scheduler registration are append-only, because in-place mutation would rewrite
intent that later work consumed. The current-defaults pointer is the one mutable
row in that set, because the existence of a version does not mean it was
installed; only the pointer records the accepted current choice.

Template-derived creation carries the caller's placement exactly as explicit
creation does, because a resolved template supplies defaults and never a
placement, and discarding a placement silently would run the session daemon-only
while the caller believed it had a runner. Template provenance never joins
defaults replacement, origin freezing, model-call preparation, imported
continuation, or transcript content; the stored session has no template lookup,
and every later consumer reads its durable defaults and provenance only.

The placement read rule filters selected transcript reads only;
conversation-list inventory is discovery and imported conversations are not
sessions, so neither is filtered.

Defaults replacement is a compare-and-set on the version the caller names, so a
racing replacement surfaces as a typed rejection instead of a silent lost
update. A replacement may name a selection whose target belongs to another
provider; neither the domain nor the command imposes a same-provider
restriction. A replacement that changes only the system prompt appends no
transcript entry, because the new instructions reach the provider whole on the
successor turn's calls and the frozen epoch already records which prompt
governed each turn. The model-identity boundary records the model identity that
executed history actually crossed, not every replacement epoch, so an epoch no
started turn used leaves no entry.

Tags are human-facing organization and attributes are machine-facing provenance;
neither substitutes for the other. Creation writes no metadata row and
fabricates no actor its command does not carry. The metadata last-writer
timestamp is result evidence, not caller intent or an ordering token, so it does
not participate in command equality. Metadata has no expected or installed
version and no history API; the retained command payloads and installation
evidence are neither an optimistic-concurrency mechanism nor a history
projection. Archive is organizational visibility only: it never cancels, pauses,
rejects, or rewrites work and never cascades to descendants or related sessions.
Because no creation boundary carries actor attribution, the default list view is
exactly all non-archived sessions, and no visibility taxonomy, creation-time
override, or inference from missing attribution is stored.

`Session` embeds no transcript entries, accepted inputs, turns, queue facts,
command history, evidence, or presentation state, because embedding them would
turn an ordinary read into an unbounded reconstruction, and holding a `Session`
must never imply authority to perform a transition. A session load does not
materialize the imported conversation, frontier members, or semantic entries;
full prefix comparison belongs to creation replay and semantic-context
resolution.

The browser catalog extends the fleet attention projection rather than
maintaining a second session-state classifier, and sort and filter state are
client-local inputs, not durable session state. Projected-size values on the
timeline are loading-policy estimates, not encoded-response promises. Text
masked before durable storage stays masked: detail reads consult no credentials,
reconstruct no provider-native material, and return blob facts as references
without fetching bytes. Browser search accepts only the lexical strategy and
passes text to PostgreSQL full-text search, so query operators are not product
semantics and a future strategy cannot turn the request into a database query
language. Attachment filenames, attachment media metadata, and derived text
artifacts are content classes the schema admits and a read returns, and no
producer publishes them; the projection performs no implicit attachment reading,
OCR, text extraction, or model pass. No browser read materializes or scans a
session transcript.

There is no generic text, role, metadata, or other payload; every entry kind is
a closed semantic fact. Entries reference accepted input and never copy its
content, because two authoritative copies could diverge and would need a
precedence rule. An accepted input becomes transcript history at eligibility,
not at acceptance, because acceptance has not fixed lineage or the snapshot that
consumes the entry and eligibility fixes both atomically.

A later ordinary turn cannot opt back into an uncompacted projection; continuing
from a boundary before the summary creates a different session whose ancestry
frontier does not contain that compaction. The projection rule separates the
frontier a call durably records from the ordered subset the selected model sees.

User content is stored exactly as accepted, with no trimming, Unicode
normalization, or case folding, because replay equality must not depend on a
normalization policy; search and display projections may normalize without
changing accepted intent. Multipart bounds are measured in parts and bytes at
admission, because that matches wire, storage, and verification cost and keeps
the value exactly as accepted. Stable shape checks precede typed construction
and the registry claim, while checks that depend on current catalog or session
state run under durable command authority. The multipart bounds are a
provisional floor, not the resource-governance policy.

`CreateSession` carries no actor, and the recovery and model actors are
representable without an implemented command-producing boundary.

Delegation does not copy, reference, merge, or expose the parent transcript, and
it does not widen the none-or-one ancestry baseline. The spawning tool's
arguments supply no defaults field for the child. No path deletes a child or its
history, and neither a continued nor a terminated child can become a silent
orphan or a silent kill. Per-relationship message ordinals are provenance and do
not serve as a cross-relationship tie-break. A child result is delivered
content, never transcript access.

## Contracts

A session that is waiting on a human is in the parked state and no other. When a
module parks a thing that is or contains a session, the module also moves that
session to parked. One classifier derives the attention states shown to
operators from durable facts. A read that encounters a state it does not
recognize returns an error rather than a guess.

The only way to derive a new transcript snapshot is to append to the old one, so
every earlier entry stays in order. Two frontiers are equal only if they are the
same frontier; comparing content is a separate explicit operation. Compaction
changes which entries are visible to the model, never what is stored. A summary
cannot hide an unsummarized prefix, and its end boundary must close every tool
exchange it covers.

A turn binds its configuration when its origin is accepted. Replacing defaults
later never rebinds it, whether the turn is queued, running, or finished. A
delegated child copies the parent turn's frozen configuration, never the
parent's current defaults pointer.

The application's `CreateSession` request has no cause or ancestry input and
fixes the interactive cause with no ancestry; the imported-frontier family
records the interactive cause as well. A command naming a source-session
ancestry is well formed but fails preparation with a nonterminal error that
claims no command identifier.

Replay equality for explicit creation compares provenance, the complete
defaults, and the placement. Template-derived creation compares provenance,
placement, and the caller-supplied template name and ignores the daemon-resolved
bundle, so the same command and name return the first session after a template
edit. The two modes are never equal under one command identity. First handling
still stores and cross-checks the complete resolved defaults and digest,
establishes defaults version one from that copy, and seals the template name and
digest alongside the session.

Placement participates in replay equality in both modes; replaying one command
identity under a different placement, including where the first handling had
none, is conflicting reuse. Equal replay returns the recorded receipt, which may
name a different session than the freshly minted candidate, and the unused
candidate is discarded.

The committing transaction of either family inserts the session row, its
scheduler registration, defaults version one, the current-defaults pointer, the
typed command record, the registry claim, any initial placement record, and the
session-created outbox event together. A visible session never names a placement
its creation command did not carry, and a carried placement is never dropped
between the claim and the session. Every table in this set is append-only except
the current-defaults pointer.

The daemon resolves the addressed imported aggregate to its canonical sealed
frontier before constructing the command; the frontier names its own imported
conversation and inclusive boundary, and the command accepts no second
conversation identity. Import never chooses the relationship or a frontier, and
a client may create a session against any boundary of any imported conversation,
at any later time and more than once. Resume and fork both create independent
session identities, use the same exact imported prefix, and leave the imported
conversation unchanged.

A missing imported conversation or frontier is returned without claiming the
command identity. A changed frontier, relationship, or defaults under an already
claimed identity is conflicting reuse; selecting another conversation
necessarily changes the frontier. Unique conflicts for the generated session,
semantic-entry, and seed-frontier candidates are typed identity collisions by
kind, and the failed transaction rolls back its registry claim.

Every imported-seeded session owns exactly one immutable `ImportedSessionSeed`
pairing its session identity with the generated seed frontier identity;
reminting an equal-content frontier never satisfies that link. Creation replay
and every read that resolves imported semantic context require the imported
ancestry and its seed together and validate that the linked frontier belongs to
the same session. First-turn scheduling and transcript projection use that
checked loader and the stored identity; neither mints another frontier.

The seeding transaction inserts one imported-provenance entry for every
normalized imported entry in the exact prefix, including non-text content. Each
records its exact imported-entry reference, source-speaker attestation, and
normalized content, and fabricates no accepted input, producing call, or native
tool identity. Imported-frontier creation is the only producer of imported
entries; imported provenance is restricted to imported-ancestry sessions and the
exact selected prefix, and stays outside every native subject-identity
constraint.

The initial path placement is pinned by creation, and only the
`UpdateSessionPlacement` command changes it, appending a versioned event that
names its predecessor and command identity; creation appends version one, so no
update rewrites history. Every current-placement load authenticates the
contiguous history from version one through the selected head against each
event's typed receipt and registry claim, rejects a head when history contains a
later event, and fails closed as typed corruption on a missing or lagging head,
cross-wired history, or invalid command fact.

A placed requester's readable scope is its parent directory's subtree, and a
refusal is typed evidence carrying the requesting directory and a closed reason,
never an empty successful result. A one-segment placement sits in the root
directory and has global conversation read, including pathless sessions. The
conversation-introspection adapter applies the decision in the same
repeatable-read transaction that opens the transcript cursor.

Defaults are immutable epochs identified by a positive 64-bit ordinal. Each
replacement installs the checked successor ordinal as a new immutable row and
moves the session's single current pointer. The dangerous-tool blanket is
disabled for explicit creation, copied from the resolved template for
template-derived creation, and changes only when a replacement installs a
complete later epoch.

Configuration-free steering inherits its configuration from its source turn and
reads no defaults. A predecessor turn's prepared or in-flight call keeps its
existing pins, so credential affinity and provider prompt-cache prefixes do not
move mid-call.

Both creation families carry the optional system prompt inside their complete
initial defaults, and a replacement changes it only as part of the complete
successor epoch. The domain imposes no byte policy on the prompt; daemon
configuration applies its byte limit at each ingress. The epoch row is the
prompt's single content authority: per-turn origin rows copy no prompt text, and
model-call preparation reads the prompt through the calling turn's frozen
version, including the inherited version of a reclassified steering origin.

When a started turn's frozen direct selection differs from its immediate
predecessor's, eligibility appends one model-identity entry immediately before
that turn's origin entry, keyed to the frozen direct selection alone. Frontiers
started before the boundary existed keep their exact historical membership
through an immutable per-turn compatibility fact.

A replacement whose compare-and-set affects zero rows re-derives the result
against current state in the same transaction; a re-derivation that still
reports applied, or an update affecting more than one row, fails closed as
corruption. A caller that validates settings outside the lock may ask the same
transaction to record a version mismatch only; a current expected version under
the lock rolls back the claim and applies nothing. A supplied session that does
not match the command target is a nonterminal preparation error, not a recorded
rejection.

No metadata string is trimmed, normalized, or case-folded. The daemon applies
deployment-owned tag and attribute count policies before command handling;
domain reconstitution has no count policy. Absence of metadata rows is the
canonical initial projection: no title, tags, or attributes, not archived, and
no last-writer stamp. A missing session returns the typed absent outcome behind
the process boundary's not-found response; only an existing session without a
metadata root returns the initial projection.

`SubmitInput` and `ReplaceSessionMetadata` are the conversational command
payloads that carry an actor. `SubmitInput` fixes the user actor; the
process-facing metadata request fixes the user actor, and a separate constructor
accepts only the tool actor for the exact executing tool request.

First handling of a metadata replacement locks the target session, then either
records session-not-found without an effect or atomically replaces the complete
root, tag, and attribute snapshot, recording the database statement time sampled
after the lock and the command actor as the one last-writer stamp. Two distinct
writes are last-writer-wins after serialization on the session row, so a full
replacement can overwrite an earlier writer's unrelated field. Persistence
retains append-only evidence that each applied receipt became current exactly
once and rejects reinstalling an earlier receipt after a later replacement.

The paginated list joins current defaults with metadata and does not
reconstitute the aggregate. Every requested tag must match, an empty set matches
all sessions, archived sessions appear only when requested, and rows are ordered
by session identity. A later page is a new snapshot: pagination guarantees
deterministic keyset traversal, not a cross-page snapshot under concurrent
creation or replacement.

A `Session` is an owned snapshot, not a live cache; any transition that depends
on current defaults revalidates them inside its own transaction. The pre-commit
candidate, the creation receipt, and the loaded `Session` are distinct types:
loading never returns a receipt, and replay never returns a `Session`.

A session load is one statement-consistent read joining the session row, its one
current-defaults pointer, and exactly the version that pointer names; for
imported ancestry it also joins the seed record and frontier header as a
constant-size proof. The pointer is authoritative, and a load never infers
current defaults from version one, the greatest stored version, a
caller-supplied version, or a cache. The load returns none only when no session
row exists in the read snapshot; a row that exists but does not decode follows
the corruption rule in [persistence-protocol](persistence-protocol.md).

The attention journal is authoritative for activity kind and timestamp. The
per-session last-activity timestamp maintained from it is only the indexed
keyset substrate, and missing substrate fails the catalog read closed rather
than hiding a session. Session metadata changes publish a session fact through
the same journal and invalidate a hot follow snapshot. Catalog order is total,
by descending last activity with ascending session identity as tie-breaker, or
by ascending session identity; catalog search is an exact case-sensitive
substring of the title or the canonical session UUID.

The follow stream subscribes to the daemon's browser monitor fanout before
reading the snapshot, then emits the snapshot as its first item. Deltas queued
when the snapshot completes are discarded, and lag confined to records the
snapshot cursor covers is absorbed silently. Falling behind past covered
records, or saturating the monitor while retained fragment text is draining,
emits one positive-cursor resync item and ends the response; the client then
replaces all transient presentation with a fresh live snapshot and resumes
durable history above its cursor without reloading the historical transcript.

The timeline sequence is allocated once across ordinary and delegation outbox
events; it is append-only, totally ordered, independent of table offsets and
query plans, and never renumbered. Another session's events may create gaps, so
a navigator carries session and sequence and opens an unloaded region with an
around read; arithmetic adjacency is never required. Continuation repeats only
the boundary address, never an item in the next keyset window.

Browser DTOs are generated from the Rust web-contract schema, and application
values, persistence rows, browser DTOs, and presentation items remain distinct.
Every selected row is decoded through the same fail-closed typed outbox
projection as durable dispatch, under one repeatable-read transaction. A detail
response reports its projected body bytes and never silently truncates: an
oversized text is a typed bounded excerpt carrying its total length and exact
continuation, never a summary that appears complete. A known category without a
richer typed body is a closed event fact; an unknown durable event or state is
corruption, never a generic body or guessed prose.

A search result's address is directly usable with the timeline around read even
when the matching region is not loaded, and each returned source is correlated
with its canonical record and with the exact durable event that supplies its
reveal address. An unknown stored source or content class, malformed identity,
invalid address, mismatched reveal event, or contradictory source shape fails
closed, including for the unreturned lookahead item. Results are in strict
newest-address-first keyset order.

Entry construction is sealed inside the domain crate. Origin and steering
entries name the accepted input's identity, and a steering entry also names the
exact active turn from its immutable delivery binding. A tool-closed entry
covers a request closed by turn end before ordinary execution, including
undecided and approved-but-unattempted requests; a crash-lost attempt is
terminal known-failed evidence and gets a result entry instead.

The activation transaction commits the one origin entry together with the
starting context snapshot, lineage, and activation facts. A steer accepted while
its source turn is active returns the accepted-input identity, its acceptance
position, and the source turn immediately, and creates no turn or entry.
Immediately before an initial or continuation call is prepared, one transaction
appends one steering entry per pending input in acceptance order, derives one
frontier extending the starting frontier, marks every input consumed, and
inserts the prepared call against the extended frontier; the four effects commit
or roll back together.

Tool-use entries become history with the producing call's completed observation;
tool-result entries become history only at the all-resolved continuation or
terminal-stop boundary, as [tool-loop](tool-loop.md) describes.

Entry and turn-state agreement is a durable schema invariant checked in both
directions at every commit by the deferred constraint triggers around
`assert_turn_lifecycle_final_state`. A failed, completed, or cancelled turn's
terminal frontier extends its latest call or starting frontier by the exact
terminal tool-result suffix when one exists and then by exactly its own marker.
A refused turn's terminal frontier is a distinct equal-content copy of its
latest call frontier; a reconciliation-required turn over a model call carries
the same distinct copy, and one over a tool attempt extends the producing call's
frontier by its terminal tool-result suffix.

The failed-turn marker has three producers: the model-call known-failure
closure, startup recovery, and pre-call credential-pool exhaustion, which
[credential-availability](credential-availability.md) owns. Each producer emits
the turn-failed update event atomically with the marker.

Summary production is its own model call and is not assistant output attributed
to an accepted-input turn. A compact-session command record begins pending with
its dedicated prepared call, then changes exactly once to applied or failed; its
request fields never change. Both compaction paths use the deployment-configured
compaction prompt and the session's current direct selection, and automatic
compaction selects a bounded safe prefix so its own summary request does not
repeat the complete oversized input.

Compactions in one session form a forward-only chain, and a successor's source
retains its predecessor's complete result frontier as a semantic prefix. For
model input only, summaries apply in physical append order to the current
model-visible sequence: each summary replaces the visible prefix through its
exact boundary with itself, and entries after that boundary in the already
projected sequence stay in order.

User-content equality is exact part order plus each part's complete value and
metadata, so normalization-distinct spellings are unequal and any attachment
difference changes replay equality. The multipart value applies the structural,
text-byte, and attachment-metadata bounds owned by
[blob-storage](blob-storage.md) before typed command construction. Failure of a
post-claim check records its closed terminal rejection and no accepted-input
effect, and resource failures retain counts and configured maxima, never the
rejected text or attachment metadata.

A steer submit creates configuration-free pending steering bound to its exact
source turn. The accepted input owns the one immutable authoritative content
value, and its row admits exactly three guarded updates from pending steering:
consumption by a call, reclassification as a turn origin, and closure under a
committed session closure.

The provider-prompt projection maps frontier entries to provider-neutral
messages and binds the frozen epoch's optional system prompt.

The delegated task is model- or tool-authored delegation work, not accepted
input and not user-attributed; the child's first turn has a distinct
delegation-task origin and starts from that entry. Each spawning request creates
at most one immutable parent-child relationship, and persistence admits a spawn
only from the complete parent relationship inventory held under the spawn
transaction's lock together with the child-session uniqueness check.

Typed await and message requests act only on their exact relationship and only
under sealed in-flight dispatch authority carrying the complete immutable
request. Outcome authority is checked against the relationship before recording,
including an exact match to this spawn's delegated-task turn; an equal
authority-and-outcome replay is idempotent, continue-running preserves the
active lifecycle, and every other outcome terminalizes it. Message delivery
remains available after a terminal outcome.

A user termination command carries a parent-alone or parent-and-descendants
scope, and parent-alone does not evaluate descendants. If a child already has
its unique terminal result, the edge records already-terminal with the new
parent command provenance and an exact check of that prior result, creating no
second result; traversal still visits that child's outgoing relationships.

Delegation-message entries refer to message records and do not reclassify
model-authored content as input from the user. Undelivered messages and
background results share one positive gap-free delivery sequence allocated under
the recipient session lock. An active recipient consumes pending items at the
next model-call safe point in recipient-wide order; an idle recipient gets one
delegation-origin queued turn, and further items coalesce into its starting
frontier. Message admission preserves the final relationship ordinal for a
future terminal child outcome and rejects a nonterminal message on exhaustion
with typed transition evidence.

Returned content derives only from the proof-bearing completed call;
independently supplied text cannot authorize a result. Reconciliation-required
work is not terminal delegation evidence and produces no outcome while its
ambiguity stands; automatic reconciliation seals the child as a failed result
carrying child-result-unavailable and the exact reconciled child turn, in the
transaction that commits the terminal transition.

A parent-policy stop or cancellation carries opaque authority from the exact
applied parent termination result, exposing the parent session, durable user
command, command kind, and descendant scope; raw identities cannot construct it,
parent-alone authority cannot produce a child disposition, and the recorded
outcome reason must match its command kind and scope. Both actions terminalize
the exact delegated child turn through its cancelled-turn lifecycle state and
cancellation marker; the relationship outcome preserves whether the action was a
stop or a cancel, and child-stopped is produced only by a parent-policy stop.

Delivery appends a delegation-result entry only to the target parent, names the
exact awaiting request, and is idempotent by that request; a foreground delivery
correlates the entry as the logical result of its still-open await request. The
immutable child result stays keyed by the spawning request, and a detached child
may return after the parent has stopped or cancelled.

## Not built

- Instruction-aware defaults replacement, rejecting a model selection whose
  targets lack instruction transport or capacity for the session's admitted set
  ([design](../design/sessions-and-transcript.md)).
- Workflow and eval creation causes for sessions created by registered programs
  ([design](../design/sessions-and-transcript.md)).
- Browser follow route used only by the open workspace
  ([design](../design/sessions-and-transcript.md)).
- Durable timeline-to-blob relation behind the referenced blob count and byte
  length ([design](../design/sessions-and-transcript.md)).
- Search publication through the typed projection-writer port
  ([design](../design/sessions-and-transcript.md)).
- Session relocation boundary entry referencing the successor placement record
  ([design](../design/sessions-and-transcript.md)).
- Durable terminal-result reconstitution consumed by delegation result sealing
  ([design](../design/sessions-and-transcript.md)).
- Spawned child defaulting into its parent's directory
  ([design](../design/sessions-and-transcript.md)).
- Static eligible-failure producer terminalizing a turn at eligibility without
  an attempt ([design](../design/sessions-and-transcript.md)).
- Semantic entry variants for refusal, reconciliation, mismatch, accepted risk,
  and approval events ([design](../design/sessions-and-transcript.md)).

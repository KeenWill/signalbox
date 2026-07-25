# Review workflows

This page specifies the implemented review-workflow bounded context as verified
against the implementing stack rooted at PR #221 (`agent/review-workflow-spec`).
It owns review targets, workflow runs, session-backed passes, findings, external
links, and their relational store. Session execution remains owned by
[sessions and transcript](sessions-and-transcript.md), turn evidence by
[turn lifecycle and scheduling](turn-lifecycle-and-scheduling.md), tool
execution by [tool loop](tool-loop.md), and relational mechanics shared with the
rest of the hub by [persistence protocol](persistence-protocol.md). Invariant
tags cite [the invariant catalog](../invariants.md).

## Bounded-context boundary

The review-workflow context sits above sessions. It records why review work
exists, which exact repository revision it concerns, which session input carried
out each pass, and how findings and external objects relate to that evidence. It
does not place workflow state inside `Session`, infer workflow progress from
free-form transcript text, or treat an external code-host object as a domain
aggregate.

`ReviewTargetId`, `ReviewRunId`, `ReviewPassId`, `ReviewFindingId`, and
`ReviewExternalLinkId` are distinct UUID-backed domain identities. A
`ReviewRunRef` binds a run to its target; a `ReviewPassRef` binds a pass to that
run and target; and a `ReviewFindingRef` binds a finding to its exact producing
pass and therefore its run and target. Child records carry those complete
references. Why: complete typed ownership facts make a cross-wired
target/run/pass/finding combination unconstructible in normal domain use and
rejectable during reconstitution (INV-001, INV-040).

Key-like and narrative values preserve their exact UTF-8 content without
trimming or normalization. Construction rejects empty values, U+0000, and values
beyond the key or narrative byte limits fixed by the
[review-text admission decision](../decisions.md#2026-07-25--bound-exact-review-workflow-text-by-utf-8-bytes).
The provisional admission budgets are enforced in both domain construction and
relational checks.

## Targets and frozen policy

A `ReviewTarget` is one immutable snapshot:

- an opaque code-host provider key and repository key;
- either a positive change-request number with an exact base revision, or a
  commit subject;
- an exact head revision and optional exact base revision; and
- an optional parent target naming the immediately preceding stack node.

Refreshing a moving change request creates another target snapshot. It never
rewrites the revision under an existing run. A parent is a topology fact for
that snapshot, not permission to rewrite either branch. It must be a distinct
target whose canonical provider and repository equal the child snapshot's; its
canonical head revision must equal the child's exact base revision. Construction
and reconstitution reject self-parent, base-less parented targets,
cross-repository edges, revision-disconnected edges, and any repeated target in
the complete canonical parent chain. Review-target parentage is therefore
acyclic and always terminates at a root.

Every `ReviewRun` names one target, one closed workflow kind, and one complete
`ReviewPolicy`. The implemented workflow kinds are external-context import,
read-only review, judgment, deduplication, external publication, finding repair,
and stack propagation. Policy is immutable run input rather than process
configuration: it carries an ordinal version plus minimum judge and publication
confidence values. Confidence is an exact integer count of basis points from
zero through 10,000. Version one's exact thresholds and ordering are fixed by
the
[basis-point policy decision](../decisions.md#2026-07-25--store-review-confidence-as-versioned-basis-point-policy);
construction and reconstitution admit only version one and enforce its exact
thresholds and ordering. An unknown version fails closed until a later recorded
decision adds its exact tuple; support for that later version changes only later
runs. Why: stored exact policy data makes the reason for unattended judgment and
publication reconstructible without depending on the executing binary's
defaults.

Runs use the closed state machine
`Queued → Running → {Succeeded, Failed, Blocked, Cancelled}`, with
`Queued → Cancelled` as the only pre-start terminal edge. `Running`,
`Succeeded`, `Failed`, and `Blocked` name the exact active or concluding
`ReviewPassRef`; cancellation records an optional last pass. A run admits at
most one pass. The pass belongs to the run, its kind is the one-to-one pass-kind
counterpart of the run workflow, and its canonical state matches the projected
run state. Queued cancellation names no pass only when no pass was recorded; if
a queued pass exists, the run retains that canonically pre-start-cancelled pass.
Running cancellation retains its canonically cancelled pass. Terminal states do
not return to running.

## Passes use session evidence

One `ReviewPass` names its exact run, pass kind, session, and accepted input.
The closed pass kinds are external-context import, read-only review, judgment,
deduplication, external publication, finding repair, and stack propagation. The
session and accepted input are mandatory even while the pass is queued. A pass
is therefore recorded only after its orchestration input has been durably
accepted; an optional session identifier is not a substitute for execution
evidence. The accepted input must belong to the pass session; construction,
persistence, and reconstitution reject a cross-wired pair even when no turn has
started. Its canonical scheduling projection must already classify it as the
origin of its own queued turn. Pending or consumed steering cannot back a pass;
a next-safe-point input becomes eligible only after canonical reclassification
creates its successor origin turn. The turn later named by the pass must be that
exact origin turn. One accepted input is owned by at most one review pass. A
pass may enter `Running` or a post-start terminal state only in the same
relational transaction that projects its run through the corresponding state. A
queued pass may be cancelled before start. Executable orchestration is not
implemented; see [Open edges](#open-edges).

Pass state is:

- `Queued`;
- `Running { turn }`;
- `Succeeded { turn, output_frontier, result? }`;
- `Failed { turn }`;
- `Blocked { turn, result? }`; or
- `Cancelled { turn? }`.

`Queued` is initial. It may become `Running` or cancel without a turn. `Running`
may become succeeded, failed, blocked, or cancelled while retaining its exact
turn. No terminal state transitions again, and no other edge is permitted.

Every named turn belongs to the pass's accepted input and session. Pass terminal
state is the durable workflow-operation outcome; turn outcome authenticates the
execution boundary but does not decide that operation outcome by itself. A
successful frontier is exactly the pass turn's canonical terminal frontier. A
running pass names an active turn or monotonically lags that turn's canonical
terminal outcome until reconciliation updates the pass. Succeeded names a
completed turn. Failed may name a completed, failed, or refused turn, allowing
validated malformed workflow output or a definitive operation rejection to fail
even though session execution completed. Blocked names a turn requiring
reconciliation, and cancellation with a turn names a cancelled turn. Persistence
loads those canonical outcomes in addition to enforcing ownership with composite
foreign keys; domain reconstitution rejects an invalid transition, regressive or
mismatched terminal outcome, terminal frontier, or cross-wired reference. Passes
never copy model output, tool results, or transcript content into workflow
state. The session transcript is the execution evidence of record; the pass
state is the operation outcome of record.

The optional `result` is one closed `ReviewPassResult`:

- `ProducedFindings` carries a canonical identity-ordered inventory of zero
  through 32 exact finding references. Each names this read-only-review pass as
  producer; its immutable canonical finding row supplies the content, so the
  inventory commits the exact identities and content without copying a second
  content authority.
- `FindingEvent` names one exact finding, event ordinal, and projected event
  payload. The payload commits the discriminator and every meaning-bearing
  value: rejection or blocking reason, referenced finding identity plus its
  authenticated admission status for duplicate or superseded. `Posted` is the
  one finding event committed by the attachment result instead, because its pass
  is also the attachment producer.
- `ExternalLinkAttachment` names the exact reservation and canonical external
  object key. When attachment also posts a finding, it additionally commits that
  exact finding, event ordinal, `Posted` discriminator, and reservation in the
  same result.
- `ExternalLinkObservation` names the exact reservation, observation ordinal,
  and observed state.

The result variant must match the pass kind, terminal outcome, and admitted
effect. `ProducedFindings` belongs only to succeeded read-only review;
`FindingEvent` follows the finding machine's pass-kind and outcome table except
for `Posted`; `ExternalLinkAttachment` belongs only to succeeded external
publication or external-context import and carries `Posted` when that attachment
posts a finding; and `ExternalLinkObservation` belongs only to succeeded
external-context import. An effect-producing terminal pass may bind an absent
result exactly once in the same transaction that admits the complete finding
inventory, appends the event, attaches the external object, atomically attaches
and posts, or appends the observation. That monotonic binding does not change
the pass lifecycle state; a bound result is immutable. Equal replay observes the
existing effect; no distinct later effect may cite that pass. A terminal pass
that produced no typed effect may retain an absent result; a read-only-review
pass that completed its output admission binds `ProducedFindings`, including an
empty inventory when it produced none.

## Finding machine

A `ReviewFinding` is immutable proposed content owned by one canonically
succeeded read-only-review pass. It stores an exact file path, an optional
closed positive line range and diff side, title, body, severity, confidence,
category, and optional recommended fix. A diff side is admitted only when the
finding's target snapshot carries an exact base revision. Its closed vocabulary
is `Left`, identifying the base or removed side, and `Right`, identifying the
head or added side. A file-relative line range needs no base. Its current status
is derived from an append-only ordered event history rather than a freely
writable status field. Severity is the closed vocabulary `Info`, `Low`,
`Medium`, `High`, or `Critical`.

The producing pass's immutable `ProducedFindings` result must contain the
finding's exact reference, and no other result inventory may contain it.
Reconstitution validates the complete inventory against all canonical findings
owned by that pass, so a pass cannot acquire another finding after its result is
bound.

The initial state is `Open`. The nine-state machine is:

1. `Open`
2. `Accepted`
3. `Rejected`
4. `Duplicate`
5. `Superseded`
6. `Stale`
7. `Posted`
8. `Fixed`
9. `BlockedWithReason`

An open finding may be judged accepted, rejected, duplicate, superseded, or
stale. `Accepted` requires the finding's confidence to meet the frozen policy's
minimum judgment confidence. Each judgment event names the pass that made it;
duplicate and superseded additionally name the canonical or successor finding in
the same run. Accepted findings may be posted, fixed, blocked with a nonempty
reason, deduplicated, superseded, or made stale. Every `Posted` transition
requires the finding's confidence to meet the frozen policy's minimum
publication confidence; this foundation defines no override. Posted findings may
be fixed, blocked, superseded, or made stale. Blocked findings may later be
fixed, superseded, or made stale; a finding blocked by publication may also
become posted after reconciliation supplies an attached external link not
consumed by any earlier posted event for that finding. Each posted event
consumes its link as publication evidence. Rejected, duplicate, superseded,
stale, and fixed are terminal. Every event carries its owning finding reference,
a contiguous one-based ordinal, and a same-target pass reference. The event
pass's canonical run supplies its workflow and the exact `ReviewPolicy` frozen
by the finding's producing run, so judgment, deduplication, and every later
classification remain under one policy even though their one-pass workflows use
separate run identities. The pass's terminal result commits to the event's exact
finding, ordinal, and projected payload; a posted event is committed inside that
pass's attachment result. Event and pass kinds are compatible only as follows:
accepted, rejected, and stale events name a judgment pass; duplicate and
superseded events name a deduplication pass; posted names an
external-publication or external-context-import pass; fixed names a
finding-repair pass; and blocked-with-reason names either an
external-publication or finding-repair pass. Every event except
blocked-with-reason names a canonically succeeded pass; blocked-with-reason
names a canonically blocked pass.

Duplicate and superseded events freeze the referenced finding's canonically
authenticated current status at admission. That status must be `Open` or
`Accepted`. The append-only event stores that authenticated status as a durable
admission fact. The store locks both finding roots in identity order, verifies
the referenced finding's current history under those locks, and appends the
fact; later reconstitution validates the frozen fact rather than comparing it
with a status that may since have advanced. A finding becomes terminal when it
acquires either reference, so no later reference may point back to it; direct
and transitive reference cycles therefore fail closed. Reconstitution validates
the complete history and fails closed on a foreign owner, run-workflow or policy
mismatch, gaps, illegal edges, incompatible or contradictory pass evidence, an
event not exactly named by its pass result, self-reference, foreign-run or
ineligible finding references, reuse of a link consumed by an earlier posted
event, or a publication event whose external link is not an attached link
associated with that finding or whose external object kind is not review,
review-thread, inline-review-comment, or general change-request-comment. A
posted event's pass is the attachment's exact producing pass (INV-040).

## External links and posting reservations

`ReviewExternalLink` correlates a target, run, or finding with one external
object kind at one opaque provider. The closed object kinds are change request,
commit, review, review thread, inline review comment, and general change-request
comment. Its immutable reservation row is the aggregate root. The
caller-selected link identity is also the idempotency key: equal replay returns
the same reservation, while reusing it for a different association, provider, or
object kind conflicts. The reservation provider must equal the canonical
provider of the target carried by its target, run, or finding association.

External publication uses two durable steps. The reservation commits before the
external API call. A successful or reconciled call then appends one immutable
attachment containing the owning reservation identity, the exact external object
identifier, and the producing pass. The attachment's reservation must equal the
aggregate root. Its producing pass and canonical run evidence must agree and
prove either succeeded external publication or, for the no-write read-only case,
succeeded external-context import; the pass belongs to the target carried by the
reservation's target, run, or finding association. If attachment publishes a
finding, the same result and transaction commit its exact posted event; the
reservation association names that finding and the object kind carries review
content. Construction and reconstitution reject another same-target reservation.
The identifier is an opaque canonical provider-wide key. An adapter qualifies a
repository-scoped host identifier with the canonical repository key before
constructing it. The store uniquely admits one attachment per reservation and
one attached provider/kind/object identity per exact target snapshot. The first
attachment also establishes that object's logical target identity. Another
snapshot may attach the same canonical object only when both snapshots are
change requests with the same canonical provider, repository, and positive
change-request number; their exact revisions may differ. A commit or an
unrelated change request cannot reassociate the object. Every refreshed snapshot
uses its own reservation and succeeded import or publication pass. A reservation
without an attachment is explicitly pending; it is never interpreted as proof
that the external effect did not occur and is not automatically retried
(INV-025, INV-026). Read-only import may reserve and attach in one local
transaction because it issues no external write (INV-041).

After attachment, append-only observations record `Current`, `Outdated`, or
`Resolved` with the owning reservation identity, a same-target pass, and a
contiguous ordinal. The observing pass and its canonical run agree on a
succeeded external-context-import operation. Reconstitution rejects another
reservation even when both share a target and rejects contradictory evidence
reused under one pass identity across the attachment or observations.
Observations describe the external object's reported state; they do not rewrite
finding status.

## Store and reconstitution

The PostgreSQL store uses append-only target, run, pass, finding,
pass-result-inventory, finding-event, external-link reservation, attachment, and
observation records. The pass projection carries a nullable closed result
discriminator and the exact scalar result payload; produced-finding result
members are normalized references to the immutable canonical finding rows.
Deferred relational validation compares their complete canonical
identity-ordered inventory with every finding owned by that producing pass. The
only mutable workflow columns are the current run and pass state projections;
their evidence-bearing fields, including the one-time absent-to-bound result,
change atomically under row lock, and database checks close every nullable
shape. Immutable content and history tables reject update and delete.

Store loaders read a complete aggregate projection. The adapter decodes closed
discriminators and assembles domain reconstitution inputs; the domain validates
ownership, state shape, event order, and transitions. A missing referenced
record, unknown discriminator, incomplete history, or failed domain check is
reported as corruption rather than normalized into a plausible aggregate
(INV-040, INV-041).

The first store surface creates and loads complete aggregates, idempotently
reserves external links, attaches external identifiers, and appends external
observations. Executable orchestration is not implemented.

## Open edges

- [Review-workflow orchestration](../open-questions.md#destination-features-target-model).
- [Artifacts](../open-questions.md#general-purpose-artifacts) remain a separate
  future aggregate; review workflow rows contain references and evidence, not
  copied general-purpose artifacts.

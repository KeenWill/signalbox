# Review workflows

This page specifies the implemented review-workflow bounded context. It owns
review targets, workflow runs, session-backed passes, findings, external links,
their relational store, application orchestration, the closed concern library,
the relational orchestration attempt and command-receipt store, the client-fed
daemon adapter, and the process and terminal surfaces. Session execution remains
owned by [sessions and transcript](sessions-and-transcript.md), turn evidence by
[turn lifecycle and scheduling](turn-lifecycle-and-scheduling.md), tool
execution by [tool loop](tool-loop.md), and relational mechanics shared with the
rest of the daemon by [persistence protocol](persistence-protocol.md). Invariant
tags cite [the invariant test index](../invariants.md).

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
beyond the key or narrative byte limits fixed by this contract. The admission
budgets are enforced in both domain construction and relational checks.

## Targets and frozen policy

A `ReviewTarget` is one immutable snapshot:

- an opaque code-host provider key and repository key;
- either a positive change-request number with an exact base revision, or a
  commit subject;
- an exact head revision and optional exact base revision; and
- an optional parent target naming the immediately preceding stack node.

Refreshing a moving change request creates another target snapshot. It never
rewrites the revision under an existing run.

A parent records stack topology for that snapshot; it does not authorize
rewriting either branch. It must be a distinct target whose canonical provider
and repository equal the child snapshot's; its canonical head revision must
equal the child's exact base revision. Construction and reconstitution reject
self-parent, base-less parented targets, cross-repository edges,
revision-disconnected edges, and any repeated target in the complete canonical
parent chain. Two snapshots of the same change request, identified by equal
canonical provider, repository, and positive change-request number, also cannot
be parent and child or otherwise both appear in one chain: refresh history is
not stack topology. Review-target parentage is therefore acyclic and always
terminates at a root.

Every `ReviewRun` names one target, one closed workflow kind, and one complete
`ReviewPolicy`. The implemented workflow kinds are external-context import,
read-only review, judgment, deduplication, external publication, finding repair,
and stack propagation.

Policy is immutable run input rather than process configuration: it carries an
ordinal version plus minimum judge and publication confidence values. Confidence
is an exact integer count of basis points from zero through 10,000, and both
thresholds apply only to a finding's confidence that the issue is real. Version
one's exact thresholds and ordering are fixed by this contract; construction and
reconstitution admit only version one and enforce them. An unknown version fails
closed until a later contract revision adds its exact tuple; support for that
later version changes only later runs. Why: stored exact policy data makes the
reason for unattended judgment and publication reconstructible without depending
on the executing binary's defaults.

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
The closed pass kinds are the one-to-one counterparts of the run workflow kinds
above. The session and accepted input are mandatory even while the pass is
queued. A pass is therefore recorded only after its orchestration input has been
durably accepted; an optional session identifier is not a substitute for
execution evidence.

The accepted input must belong to the pass session; construction, persistence,
and reconstitution reject a cross-wired pair even when no turn has started. Its
canonical scheduling projection must already classify it as the origin of its
own queued turn. Pending or consumed steering cannot back a pass; a
next-safe-point input becomes eligible only after canonical reclassification
creates its successor origin turn. The turn later named by the pass must be that
exact origin turn. One accepted input is owned by at most one review pass.

A pass may enter `Running` or a post-start terminal state only in the same
relational transaction that projects its run through the corresponding state. A
queued pass may be cancelled before start. Executable orchestration is not
inferred or scheduled automatically: the implemented caller-driven process
surface admits a pass only after its accepted input and canonical origin turn
exist, then binds activation to that exact turn.

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
reconciliation, and cancellation with a turn names a cancelled turn.

Persistence loads those canonical outcomes in addition to enforcing ownership
with composite foreign keys; domain reconstitution rejects an invalid
transition, regressive or mismatched terminal outcome, terminal frontier, or
cross-wired reference. Passes never copy model output, tool results, or
transcript content into workflow state. The session transcript is the execution
evidence of record; the pass state is the operation outcome of record.

The optional `result` is one closed `ReviewPassResult`:

- `ProducedFindings` carries a canonical identity-ordered inventory of zero
  through 32 exact finding references. Each names this read-only-review pass as
  producer; its immutable canonical finding row supplies the content, so the
  inventory commits the exact identities and content without copying a second
  content authority.
- `FindingEvent` names one exact finding, event ordinal, and projected event
  payload. The payload commits the discriminator and every meaning-bearing
  value: rejection or blocking reason, referenced finding identity plus its
  authenticated admission status for duplicate or superseded, and the exact
  pending reservation attempted by a blocked external-publication pass.
  Finding-repair blocking names no reservation. `Posted` is the one finding
  event committed by the attachment result instead, because its pass is also the
  attachment producer.
- `ExternalLinkAttachment` names the exact reservation and canonical external
  object key. When attachment also posts a finding, it additionally commits that
  exact finding, event ordinal, `Posted` discriminator, and reservation in the
  same result.
- `ExternalLinkObservation` names the exact reservation, observation ordinal,
  and observed state.
- `ExternalLinkNoChange` names the exact reservation, consumed observation
  ordinal, and unchanged reported state. It consumes a succeeded
  external-context-import pass without appending a meaning-bearing observation.
- `ExternalLinkPublicationBlocked` names the exact pending reservation and
  nonempty reason for a blocked publication that does not use the
  reservation-bearing finding event.

The result variant must match the pass kind, terminal outcome, and admitted
effect:

- `ProducedFindings` belongs only to succeeded read-only review;
- `FindingEvent` follows the finding machine's pass-kind and outcome table
  except for `Posted`;
- `ExternalLinkAttachment` belongs only to succeeded external publication or
  external-context import, and carries `Posted` when that attachment posts a
  finding;
- `ExternalLinkObservation` and `ExternalLinkNoChange` belong only to succeeded
  external-context import; and
- `ExternalLinkPublicationBlocked` belongs only to blocked external publication.

An effect-producing terminal pass may bind an absent result exactly once, in the
same transaction that admits the complete finding inventory, appends the event,
attaches the external object, atomically attaches and posts, appends the
observation, or proves the locked state comparison unchanged. That monotonic
binding does not change the pass lifecycle state, and a bound result is
immutable. Equal replay observes the existing effect; no distinct later effect
may cite that pass. A terminal pass that produced no typed effect may retain an
absent result; a read-only-review pass that completed its output admission binds
`ProducedFindings`, including an empty inventory when it produced none.

Every blocked external-publication pass binds its exact pending reservation. A
finding-associated operation that also blocks the finding uses the
reservation-bearing `FindingEvent` result; every other blocked publication uses
`ExternalLinkPublicationBlocked`. Reconciliation may attach only that same
reservation.

## Finding machine

A `ReviewFinding` is immutable proposed content owned by one canonically
succeeded read-only-review pass. It stores an exact file path, an optional
closed positive line range and diff side, title, body, severity, two confidence
axes, category, and optional recommended fix. Severity is the closed vocabulary
`Info`, `Low`, `Medium`, `High`, or `Critical`. The confidence axes are
independent exact basis-point values using the same zero-through-10,000
representation: `is_real_confidence` states whether the issue exists and merits
attention, while `severity_label_confidence` states whether its severity
classification is correct. A diff side is admitted only when the finding's
target snapshot carries an exact base revision; its closed vocabulary is `Left`,
identifying the base or removed side, and `Right`, identifying the head or added
side. A file-relative line range needs no base. The finding's current status is
derived from an append-only ordered event history rather than a freely writable
status field.

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
stale. `Accepted` requires the finding's is-real confidence to meet the frozen
policy's minimum judgment confidence, comparing `is_real_confidence` only:
`severity_label_confidence` is input to judgment and never a filter, so
uncertainty about whether a real issue is `High` or `Medium` cannot suppress the
issue. Each judgment event names the pass that made it; duplicate and superseded
additionally name the canonical or successor finding in the same run.

Accepted findings may be posted, fixed, blocked with a nonempty reason,
deduplicated, superseded, or made stale. Every `Posted` transition requires the
finding's `is_real_confidence` to meet the frozen policy's minimum publication
confidence; severity-label confidence cannot suppress publication, and no
override exists. Posted findings may be fixed, blocked, superseded, or made
stale. Blocked findings may later be fixed, superseded, or made stale; a finding
blocked by publication may also become posted after reconciliation attaches the
exact reservation named by that blocking event, provided no earlier posted event
for the finding consumed it. Each posted event consumes its link as publication
evidence. Rejected, duplicate, superseded, stale, and fixed are terminal.

Every event carries its owning finding reference, a contiguous one-based
ordinal, and a same-target pass reference. The event pass's canonical run
supplies its workflow and the exact `ReviewPolicy` frozen by the finding's
producing run, so judgment, deduplication, and every later classification remain
under one policy even though their one-pass workflows use separate run
identities. The pass's terminal result commits to the event's exact finding,
ordinal, and projected payload; a posted event is committed inside that pass's
attachment result.

Event and pass kinds are compatible only as follows:

- accepted, rejected, and stale events name a judgment pass;
- duplicate and superseded events name a deduplication pass;
- posted names an external-publication or external-context-import pass;
- fixed names a finding-repair pass; and
- blocked-with-reason names either an external-publication or finding-repair
  pass.

Every event except blocked-with-reason names a canonically succeeded pass;
blocked-with-reason names a canonically blocked pass. An external-publication
block names one exact pending reservation associated with its finding; a
finding-repair block names none.

Duplicate and superseded events freeze the referenced finding's canonically
authenticated current status at admission. That status must be `Open` or
`Accepted`. The append-only event stores that authenticated status as a durable
admission fact.

For ordinary event admission, the store locks the complete target finding
inventory in identity order, then verifies the referenced finding's current
history under those locks and appends the fact. A waiter loads the graph from
read-committed snapshots taken after the winning event commits; the held
inventory stabilizes that graph across the loader statements.

Relational admission additionally owns one mutable current-event head per
finding. Every event insert locks the subject and referenced heads in identity
order and authenticates the ordinal, subject transition, and referenced status
from those post-wait values before insertion. Only after the append-only event
exists does its trigger advance the subject head; a direct head advance must
name that exact next durable event. Because terminalization advances the locked
head, a waiter receives its post-wait version even when the outer insert began
with an older event-table snapshot. No event-history snapshot may independently
decide the ordinal, referenced status, or subject transition. The head and its
trigger functions remain in the persistent schema selected by the migration
connection, with temporary objects ordered after that schema for trigger-time
lookup. Deferred constraints bind each head to the exact latest append-only
event and prove that its ordinal equals the contiguous history length, while
reconstitution rejects a missing or mismatched head.

Read-committed external-link transitions lock the reservation and then any
associated finding before loading its multi-statement projection, so a
concurrent finding event cannot split that projection across snapshots.

Later reconstitution validates the frozen fact rather than comparing it with a
status that may since have advanced. A finding becomes terminal when it acquires
either reference, so no later reference may point back to it; direct and
transitive reference cycles therefore fail closed. Reconstitution validates the
complete history and fails closed on:

- a foreign owner, run-workflow or policy mismatch;
- gaps or illegal edges;
- incompatible or contradictory pass evidence, or an event not exactly named by
  its pass result;
- self-reference, or foreign-run or ineligible finding references;
- reuse of a link consumed by an earlier posted event; and
- a publication event whose external link is not an attached link associated
  with that finding, or whose external object kind is not review, review-thread,
  inline-review-comment, or general change-request-comment.

A posted event's pass is the attachment's exact producing pass (INV-040).

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
aggregate root.

A blocked call consumes its pass with the exact pending reservation and reason,
whether that reservation is associated with a target, run, or finding;
reconciliation may attach only that same reservation. Its producing pass and
canonical run evidence must agree and prove either succeeded external
publication or, for the no-write read-only case, succeeded external-context
import; the pass belongs to the target carried by the reservation's target, run,
or finding association. If attachment publishes a finding, the same result and
transaction commit its exact posted event; the reservation association names
that finding and the object kind carries review content. Construction and
reconstitution reject an attachment attributed to another same-target
reservation: an attachment is valid only through its own reservation.

The identifier is an opaque canonical provider-wide key. An adapter qualifies a
repository-scoped host identifier with the canonical repository key before
constructing it. The store uniquely admits one attachment per reservation and
one attached provider/kind/object identity per exact target snapshot. The first
attachment also establishes that object's logical target identity in an
immutable provider/kind/object registry, and attachment admission serializes on
that canonical object identity before reading or establishing its logical
target.

Another snapshot may attach the same canonical object only when both snapshots
are change requests with the same canonical provider, repository, and positive
change-request number; their exact revisions may differ. A commit or an
unrelated change request cannot reassociate the object. Every refreshed snapshot
uses its own reservation and succeeded import or publication pass.

A reservation without an attachment is explicitly pending; it is never
interpreted as proof that the external effect did not occur and is not
automatically retried (INV-025, INV-026). Read-only import may reserve and
attach in one local transaction because it issues no external write (INV-041).

After attachment, append-only observations record `Current`, `Outdated`, or
`Resolved` with the owning reservation identity, a same-target pass, and a
contiguous ordinal. The observing pass and its canonical run agree on a
succeeded external-context-import operation. A report equal to the latest
recorded state is a semantic no-op: it appends no observation but binds the
observing pass's exact reservation, latest durable observation ordinal, and
reported state as `ExternalLinkNoChange`, making that pass ineligible for any
later effect. A changed report appends the next ordinal and binds that exact
effect. Reconstitution validates a no-change claim against its consumed
observation ordinal, rejects another reservation even when both share a target,
and rejects contradictory evidence reused under one pass identity across the
attachment or observations. Observations describe the external object's reported
state; they do not rewrite finding status.

## Review orchestration

The implemented application orchestration boundary composes the one-pass run and
finding primitives above; it does not replace their lifecycle, finding-state,
publication-reservation, or frozen-policy contracts.

### Attempt identity, configuration, and adapter seams

One orchestration attempt names one immutable target, one complete frozen
policy, one ordered concern-set version, and the exact template content digests
used by its passes. Every stage retains that attempt identity. Refreshing the
change request, changing policy, editing the concern set, or changing any
resolved prompt template requires a new attempt; an orchestrator never mixes
those inputs while resuming an old attempt.

The application accepts one user-supplied immutable attempt with an exact
target, policy, concern-set version, non-concern stage digests, and ordered
concern specifications. It rejects an empty inventory or a repeated concern key.
Recording the attempt before any pass starts makes an equal retry resume those
values and makes a distinct reuse conflict.

The daemon's optional closed version-one review library resolves at startup. It
stores one shared header, one body for each of the four non-concern stages, and
one body for each concern in the exact ordered initial set: correctness,
interface and type design, test quality, security, and documentation-versus-code
drift. It generates nine reserved ordinary session templates. Every generated
system prompt is the exact header bytes, two LF bytes, and its stage or concern
body bytes, without trimming or interpolation, and the complete assembled bundle
follows ordinary copy-on-create session provenance.

A review start selection names its concern-set version, all four labeled stage
template names, and the ordered concern-key/template-name pairs. The daemon
constructs the application attempt only when that complete selection exactly
matches the resolved library; an absent library, changed version, changed stage
name, extra or missing concern, reordered concern, or changed concern template
fails closed rather than being replaced by daemon defaults.

Each generated session template retains the ordinary content digest over source
version, model selection, approval blanket, and complete assembled prompt. The
orchestration attempt separately retains a domain-separated digest committing
the stage or concern key, source version, model selection, approval blanket, and
separate SHA-256 digests of the exact shared-header and body bytes.

For every client-fed pass or finding effect, the daemon loads the named pass,
its canonical run, and the pass session. The session's copied template name and
ordinary content digest must equal the currently resolved reserved template
provenance for that stage or concern; missing or mismatched provenance rejects
the submission. The application then authenticates the separately frozen
orchestration digest carried by the attempt. Session execution evidence and
attempt configuration therefore agree through two independently checked bindings
rather than a caller-supplied claim.

The application exposes ports for immutable-target context import,
session-backed passes, repair, and reserved publication. Adapter success returns
typed evidence naming the exact target, policy, run, pass, session, and template
inputs; a mismatch blocks the attempt rather than being repaired by
substitution. Failed, blocked, and post-admission cancelled imports likewise
carry their canonical terminal pass and run plus the exact import template.

A successful import pass may be result-free or carry one domain-compatible
attachment, observation, or no-change result. A result-bearing success also
carries the canonical external-link aggregate; its association must own the
attempt target and its exact attachment, observation, or durable no-change claim
must authenticate the pass result. Every success carries the imported-context
digest as evidence bound to that exact import pass; a context value naming
another pass fails before it can be sealed or fanned out. Only cancellation
before pass admission may omit both pass and run; every other passless or
partially populated terminal outcome fails closed.

Concrete provider, model, and workspace adapters are committed but
unimplemented; no present daemon surface supplies them. Future workspace
adapters must prepare either a read-only checkout for review and judgment or an
explicitly writable checkout for repair, always at the exact target head and
comparison revision.

### Fan-out and complete-set barrier

After a validated external-context-import result is durable, the service calls
its session-backed pass port concurrently once for every configured concern.
Each work item carries the immutable attempt, the digest from the authenticated
pass-bound imported-context evidence, and one exact concern specification.
Successful members must report the same target, equal frozen policy, and exact
resolved concern-template digest from their own runner evidence.
`ReviewConcernWork` carries no repair or publication handle and no other
member's uncommitted output.

The attempt durably records the complete expected concern inventory before any
member starts. Judgment is eligible only after every expected member has
succeeded and bound its complete `ProducedFindings` inventory, including an
explicit empty inventory. If one member fails, blocks, or is cancelled while
others succeed, the successful findings remain valid evidence but the fan-out
set is incomplete and no judgment, repair, or publication work is eligible.

The orchestrator may retry only the failed member against the same target,
policy, concern-set version, and template digests. Before scheduling that retry
it rejects any extra or repeated current claim, then authenticates the current
failed claim's target and template; mismatched durable evidence blocks the
attempt and cannot be overwritten. A changed input starts a new attempt. The
eventual barrier includes exactly one successful member for every expected
concern and rejects missing, extra, repeated, or superseded member claims, so it
cannot silently present a partial review as complete.

### Structured finding return

The application pass port returns typed findings plus canonical producer pass
and run evidence. Its complete-set barrier accepts them only when the exact
canonical `ProducedFindings` inventory agrees, including an explicit empty
inventory.

The model-runtime realization of the `submit_review_findings` structured-output
contract is committed but unimplemented. No present adapter forces its one tool
call, disables parallel tool use, or performs provider-independent decoding. A
future realization must observe exactly one contract value. Free-form assistant
text is transcript evidence only and never becomes a finding.

The exact logical JSON Schema is below. The post-decode domain validator
enforces `line_end >= line_start` and the UTF-8 byte rules that JSON Schema
cannot state portably.

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "type": "object",
  "additionalProperties": false,
  "required": ["findings"],
  "properties": {
    "findings": {
      "type": "array",
      "maxItems": 32,
      "items": {
        "type": "object",
        "additionalProperties": false,
        "required": [
          "file_path",
          "line_start",
          "line_end",
          "diff_side",
          "title",
          "body",
          "severity",
          "is_real_confidence",
          "severity_label_confidence",
          "category",
          "recommended_fix"
        ],
        "properties": {
          "file_path": { "type": "string" },
          "line_start": {},
          "line_end": {},
          "diff_side": { "enum": [null, "left", "right"] },
          "title": { "type": "string" },
          "body": { "type": "string" },
          "severity": {
            "enum": ["info", "low", "medium", "high", "critical"]
          },
          "is_real_confidence": {
            "type": "integer",
            "minimum": 0,
            "maximum": 10000
          },
          "severity_label_confidence": {
            "type": "integer",
            "minimum": 0,
            "maximum": 10000
          },
          "category": { "type": "string" },
          "recommended_fix": {
            "type": ["string", "null"]
          }
        },
        "oneOf": [
          {
            "properties": {
              "line_start": { "type": "null" },
              "line_end": { "type": "null" }
            }
          },
          {
            "properties": {
              "line_start": {
                "type": "integer",
                "minimum": 1,
                "maximum": 4294967295
              },
              "line_end": {
                "type": "integer",
                "minimum": 1,
                "maximum": 4294967295
              }
            }
          }
        ]
      }
    }
  }
}
```

A future adapter must fail the pass on no structured value, several values,
malformed JSON, schema mismatch, a domain-invalid item, more than 32 items, or
failure to admit the complete inventory. After decode, ordinary finding
construction enforces byte bounds, nonempty and U+0000 rules, target comparison
evidence, and all typed vocabularies. The adapter must assign stable finding
identities and admit the entire canonical identity-ordered inventory atomically;
none of its proposals may survive as untyped text or a partial inventory.

### Judgment and cross-run deduplication

One judgment analysis pass consumes the sealed finding inventories from all
fan-out members, not a concern subset. The canonical analysis pass must be a
result-free succeeded `Judge` pass; a result-bearing per-finding effect pass
cannot authenticate or be reused as the complete-set analysis. Its structured
result contains exactly one disposition for every input finding identity and no
unknown or repeated identity. A disposition is `accepted`,
`rejected { reason }`, `duplicate { canonical_finding }`,
`superseded { successor_finding }`, or `stale`. Referenced findings carry their
complete original finding references. The result is invalid unless every
accepted finding meets `minimum_judge_confidence` on `is_real_confidence`;
severity-label confidence is available to the judge but never filters a finding.

The orchestrator seals this complete plan before admitting its per-finding
judgment and deduplication events through the single-effect pass primitives in
canonical finding order. An `Applied` result carries the canonical finding
event, its independently loaded pass and run evidence, and the exact
judgment-template digest. The service validates the exact planned disposition
and committed pass result before recording the durable effect receipt. On
resume, loaded receipts must equal the first `k` members of the plan in that
canonical order. A gap, reordering, duplicate, foreign attempt, or unknown
finding fails before runner invocation; only the remaining suffix may execute.
Until every planned event is durably admitted, repair and publication remain
ineligible; a crash resumes the plan rather than asking a model for a different
partial judgment.

Duplicate and superseded events may reference a finding produced by another run
only within one immutable target and one equal complete frozen policy. Each
finding retains its original `ReviewFindingRef`; findings are not copied,
reparented, or promoted into the judgment run. The event and pass result carry
the referenced finding's independent target, run, producing pass, and finding
identities. Why: a cross-run reference keeps one pass per run and the original
evidence chain without duplicating immutable finding content in the judgment
run.

Admission authenticates that complete ancestry against a canonically succeeded
read-only-review producer whose sealed `ProducedFindings` inventory contains the
finding. The subject producer, referenced producer, and event pass must all name
the same exact target and policy. Persistence locks the complete target finding
inventory in identity order and, after any lock wait, admits the reference only
while the referenced finding is `Open` or `Accepted`. A finding already in a
terminal status cannot be newly named as the referenced root.

Self-reference, repeated reference, and direct or transitive cycles fail closed.
Complete-graph domain reconstitution checks every referenced root and edge,
including the edge's frozen producer policy against the supplied root's actual
producer policy, while relational composite references and event guards
authenticate each independent identity. Missing or mismatched
target/run/pass/finding evidence, an unsealed or nonmember producer inventory, a
policy mismatch, or a graph cycle is corruption rather than a best-effort match.

### Repair, publication, import, and escalation

Finding-repair work contains the exact accepted finding inventory only after
every sealed judgment effect is durably applied. A `Fixed` member carries the
canonical fixed event, independently loaded Fix pass and run evidence, and exact
repair-template digest. Only evidence that commits the exact fixed event may
remove a finding from the publication set; a failed or cancelled repair leaves
its finding surviving. The service records the exact terminal outcome inventory
before advancing. A blocked repair returns a typed `RepairIncomplete` attempt
outcome and prevents all publication for that attempt.

Resuming a blocked repair after reconciliation is committed but unimplemented;
no present application-store operation replaces that sealed outcome.

External-publication work contains the exact canonical surviving inventory and
uses the reservation-then-attachment pass boundary. A `Published` member carries
the canonical finding-associated attached link, independently loaded
publication-run evidence, and exact resolved publication-template digest. Before
sealing or completing the attempt, the service authenticates their target and
policy, the succeeded `Publish` pass and concluding `PublishReview` run, and the
attachment result with its exact posted finding event. The result must cover the
inventory exactly; any failed, blocked, or cancelled member returns a typed
`PublicationIncomplete` outcome rather than `Complete`. The publication
admission check uses only `is_real_confidence` and the immutable target head; a
moved change request is another target and does not authorize posting results
produced against the earlier head.

Post-publication external-context import is committed but unimplemented. A
future continuation must use the import pass and no-change evidence rather than
inferring external state.

An incomplete concern barrier, invalid import or judgment evidence, durable seal
conflict, invalid downstream inventory, or incomplete judgment, repair, or
publication outcome stops the service through a closed application result or
error. Adapter-specific workspace, revision, structured-return, and reservation
failures remain typed by the corresponding port implementation. The service does
not drop a concern, silently retarget, or publish a partial successful subset as
complete.

The daemon implements a client-fed adapter over the application service. Its
closed operations start an attempt, record import evidence, record one concern
outcome, seal the complete judgment plan, record one planned judgment effect,
seal repair outcomes, seal publication outcomes, and read durable progress. Each
mutation supplies exactly one stage result to the service; every other runner
port reports that it is awaiting client input. The service may therefore derive
and seal all newly eligible durable stages, but it cannot silently substitute
model output or advance through a stage the client did not supply. Stage
prevalidation rejects an operation that is early, late, or incompatible with the
current durable attempt.

## Store and reconstitution

The PostgreSQL store uses append-only target, run, pass, finding,
pass-result-inventory, finding-event, external-link reservation,
external-object-identity, attachment, and observation records. The pass
projection carries a nullable closed result discriminator and the exact scalar
result payload; produced-finding result members are normalized references to the
immutable canonical finding rows. Each finding row stores independent
`is_real_confidence` and `severity_label_confidence` columns under the shared
zero-through-10,000 bound.

Deferred relational validation compares the complete canonical identity-ordered
inventory with every finding owned by that producing pass. Cross-run references
store the referenced finding's independent target, run, pass, and finding
identities. Composite foreign keys authenticate the canonical finding ancestry
and membership in the succeeded producer's sealed inventory; identity-ordered
complete-target root locking protects eligible status and cycle checks across
the multi-statement graph load.

The only mutable workflow columns are the current run and pass state
projections; their evidence-bearing fields, including the one-time
absent-to-bound result, change atomically under row lock, and database checks
close every nullable shape. Immutable content and history tables reject update
and delete. Every workflow table rejects `TRUNCATE`.

The coherent snapshot lock inventory follows writer acquisition order: every
orchestration parent or seal precedes the member or outcome rows it owns.

Store loaders read a complete aggregate projection. The adapter decodes closed
discriminators and assembles domain reconstitution inputs; the domain validates
ownership, state shape, event order, and transitions. A missing referenced
record, unknown discriminator, incomplete history, or failed domain check is
reported as corruption rather than normalized into a plausible aggregate
(INV-040, INV-041).

The PostgreSQL orchestration adapter implements the application's complete
durable attempt-store port. It records the immutable attempt and ordered concern
slots, imported-context evidence, concern claims and their exact findings, the
complete fan-out seal, judgment plan and applied effects, repair and publication
inventory seals, and their terminal outcome seals. Its loaders reconstruct the
current stage only from those durable records; missing ancestry, an unknown
closed value, a noncanonical count, or contradictory evidence is corruption
rather than an inferred partial result.

Each orchestration mutation also uses a user-global durable command receipt that
binds command identity to the semantic request digest, closed operation kind,
and attempt. After the aggregate effect commits, the adapter appends a recovery
result containing the operation-derived stage and progress before it attempts
the user-global receipt. Serial review-mutation admission prevents a later stage
from beginning before that recovery result exists. The receipt and recovery
record have database constraints relating operation kind, stage, and constituent
progress; a contradictory record is refused.

Equal replay returns the recorded result, distinct reuse conflicts, and a retry
whose receipt was lost materializes that receipt from the recovery result rather
than deriving an answer from later aggregate state. A recovery-only
interrupted-judgment result participates in current-stage and coherent-snapshot
reconstruction, and reserves its identity against every user-global command
family while awaiting receipt materialization.

The review-workflow store creates and loads complete aggregates, idempotently
reserves external links, attaches external identifiers, appends external
observations, lists a run's findings in identity order, and implements
crash-recoverable durable command receipts for the caller-driven process
surface.

## Process and terminal surface

The process protocol exposes closed requests to create a target, start a run and
its session-backed pass, activate that pass, record its complete finding
inventory or one finding event, reserve and attach external links, and read
targets, runs with their pass, individual findings, or an identity-ordered run
finding list. It additionally exposes the closed orchestration start, import,
concern, judgment-plan, judgment-effect, repair, publication, and read
operations consumed by the client-fed adapter. Exact wire shapes, bounds, and
compatibility are owned by [the process protocol](process-protocol.md).

Starting a run requires an existing target plus an accepted input whose
canonical session and origin turn agree with the requested pass. Fresh run and
pass admission commits both valid roots in one transaction. An exact retry may
also complete a compatible run-only intermediate; every such intermediate
remains loadable. Activation requires that origin turn to be the session's
canonical active turn and atomically projects the run and pass to running.
Completion requests authenticate the named turn's canonical terminal outcome and
output frontier before entering the aggregate transaction.

Read-only success is admission-atomic. The public command that records findings
transitions the run and pass to succeeded, binds `ProducedFindings`, and inserts
the complete canonical inventory in one transaction. It cannot commit succeeded
read-only state with an absent result. Generic pass completion therefore refuses
read-only success; the complete-findings command is the sole success path. The
empty inventory follows that same path. Finding events and external-link
attachments likewise bind their exact pass result in the transaction that
appends or attaches the effect. Thus every committed intermediate point remains
a domain-reconstitutable aggregate; a process crash cannot expose a state that
the store's own loaders classify as corruption.

Every review mutation carries a user-global command identity. Before its
aggregate effect, the adapter commits a typed intent binding that identity to
the validated semantic request. The primary aggregate effect commits atomically
with an append-only marker of the exact command. A concern marker also binds the
immutable claim sequence it created, so later replacement of a failed claim
cannot redirect exact replay to the successor. The operation answer is then
derived from the submitted outcome and completed barrier facts, then stored in
an append-only recovery record before the intent is atomically replaced by the
typed receipt.

Exclusive admission prevents overlap while the process is running; the durable
intent covers a stop after the effect and before recovery. An exact retry
authenticates the equal durable effect independently of later aggregate stage,
reconstructs the original operation-stage answer without later facts, and
completes recovery. A fresh stale command remains rejected. A lost receipt is
materialized from its recovery record. Recorded receipts are inspected before
mutable aggregate-state validation; distinct command-identity reuse fails
closed.

The terminal client exposes target creation, run admission and activation,
single-finding read-only completion, finding listing, target, run, and finding
reads, both external-publication reservation and attachment, and every
orchestration operation above. It accepts complete concern, judgment-plan,
repair-outcome, and publication-outcome inventories through strict local JSON
files and renders the durable orchestration stage, ordered concern statuses,
template digests, and progress counts.

A run read reconstructs the run and its optional recorded pass from one
repeatable-read snapshot, so the response cannot combine lifecycle projections
from different commits. Mutation commands print their generated command identity
before socket I/O so an ambiguous attempt can be retried exactly.
Process-derived text uses the terminal-safe rendering contract owned by the
process protocol.

## Open edges

- Concrete model, provider, and workspace adapters, the structured model-output
  runtime, repair reconciliation, post-publication import, and merge-based stack
  propagation remain under
  [review-workflow orchestration](../open-questions.md#destination-features-target-model).
- [Artifacts](../open-questions.md#general-purpose-artifacts) remain a separate
  future aggregate; review workflow rows contain references and evidence, not
  copied general-purpose artifacts.

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
rejectable during reconstitution (INV-001, INV-002, INV-040).

Key-like values preserve their exact UTF-8 content without trimming or
normalization. Construction rejects empty values, U+0000, and values longer than
1,024 UTF-8 bytes for provider, repository, revision, file-path, category, and
external-object keys. Narrative finding titles, bodies, reasons, and recommended
fixes have the same exact-content rules with a 65,536-byte limit. These
provisional admission budgets are enforced in both domain construction and
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
that snapshot, not permission to rewrite either branch.

Every `ReviewRun` names one target, one closed workflow kind, and one complete
`ReviewPolicy`. The implemented workflow kinds are external-context import,
read-only review, judgment, deduplication, external publication, finding repair,
and stack propagation. Policy is immutable run input rather than process
configuration: it carries an ordinal version plus minimum judge and publication
confidence values. Confidence is an exact integer count of basis points from
zero through 10,000. Version one's exact thresholds and ordering are fixed by
the
[basis-point policy decision](../decisions.md#2026-07-25--store-review-confidence-as-versioned-basis-point-policy);
construction enforces that ordering. A later policy version changes only later
runs. Why: stored exact policy data makes the reason for unattended judgment and
publication reconstructible without depending on the executing binary's
defaults.

Runs use the closed state machine
`Queued → Running → {Succeeded, Failed, Blocked, Cancelled}`. `Running`,
`Succeeded`, `Failed`, and `Blocked` name the exact active or concluding
`ReviewPassRef`; cancellation records an optional last pass. A referenced pass
must belong to the run, and its canonical pass state must match the projected
run state. Queued cancellation names no pass; running cancellation names its
canonically cancelled pass. Terminal states do not return to running.

## Passes use session evidence

One `ReviewPass` names its exact run, pass kind, session, and accepted input.
The closed pass kinds are external-context import, read-only review, judgment,
deduplication, external publication, finding repair, and stack propagation. The
session and accepted input are mandatory even while the pass is queued. A pass
is therefore recorded only after its orchestration input has been durably
accepted; an optional session identifier is not a substitute for execution
evidence. The accepted input must belong to the pass session; construction,
persistence, and reconstitution reject a cross-wired pair even when no turn has
started (INV-040).

Pass state is:

- `Queued`;
- `Running { turn }`;
- `Succeeded { turn, output_frontier }`;
- `Failed { turn }`;
- `Blocked { turn }`; or
- `Cancelled { turn? }`.

`Queued` is initial. It may become `Running` or cancel without a turn. `Running`
may become succeeded, failed, blocked, or cancelled while retaining its exact
turn. No terminal state transitions again, and no other edge is permitted.

Every named turn belongs to the pass's accepted input and session. A successful
frontier is exactly the pass turn's canonical terminal frontier. A running pass
names an active turn; succeeded names a completed turn; failed names a failed or
refused turn; blocked names a turn requiring reconciliation; and cancellation
with a turn names a cancelled turn. Persistence loads those canonical outcomes
in addition to enforcing ownership with composite foreign keys; domain
reconstitution rejects an invalid transition, mismatched outcome, terminal
frontier, or cross-wired reference. Passes never copy model output, tool
results, or transcript content into workflow state. The session transcript is
the evidence of record.

## Finding machine

A `ReviewFinding` is immutable proposed content owned by one producing pass. It
stores an exact file path, an optional closed positive line range and diff side,
title, body, severity, confidence, category, and optional recommended fix. A
diff side is admitted only when the finding's target snapshot carries an exact
base revision; a file-relative line range needs no base. Its current status is
derived from an append-only ordered event history rather than a freely writable
status field. Severity is the closed vocabulary `Info`, `Low`, `Medium`, `High`,
or `Critical`.

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
stale. Each judgment event names the pass that made it; duplicate and superseded
additionally name the canonical or successor finding in the same run. Accepted
findings may be posted, fixed, blocked with a nonempty reason, deduplicated,
superseded, or made stale. Posted findings may be fixed, blocked, superseded, or
made stale. Blocked findings may later be fixed, superseded, or made stale.
Rejected, duplicate, superseded, stale, and fixed are terminal. Every event
carries its owning finding reference, a contiguous one-based ordinal, and a
same-target pass reference. Event and pass kinds are compatible only as follows:
accepted, rejected, and stale events name a judgment pass; duplicate and
superseded events name a deduplication pass; posted names an
external-publication pass; fixed names a finding-repair pass; and
blocked-with-reason names either an external-publication or finding-repair pass.
Reconstitution validates the complete history and fails closed on a foreign
owner, gaps, illegal edges, incompatible pass kind, self-reference, foreign-run
finding references, or a publication event whose external link is not an
attached link associated with that finding (INV-040).

## External links and posting reservations

`ReviewExternalLink` correlates a target, run, or finding with one external
object kind at one opaque provider. The closed object kinds are change request,
commit, review, review thread, inline review comment, and general change-request
comment. Its immutable reservation row is the aggregate root. The
caller-selected link identity is also the idempotency key: equal replay returns
the same reservation, while reusing it for a different association, provider, or
object kind conflicts.

External publication uses two durable steps. The reservation commits before the
external API call. A successful or reconciled call then appends one immutable
attachment containing the owning reservation identity, the exact external object
identifier, and the producing pass. The attachment's reservation must equal the
aggregate root, and the producing pass must belong to the target carried by the
reservation's target, run, or finding association. Construction and
reconstitution reject another same-target reservation. The identifier is an
opaque canonical provider-wide key. An adapter qualifies a repository-scoped
host identifier with the canonical repository key before constructing it. The
store uniquely admits one attached provider/kind/object identity and one
attachment per reservation. A reservation without an attachment is explicitly
pending; it is never interpreted as proof that the external effect did not occur
and is not automatically retried (INV-025, INV-026, INV-041). Read-only import
may reserve and attach in one local transaction because it issues no external
write.

After attachment, append-only observations record `Current`, `Outdated`, or
`Resolved` with the owning reservation identity, a same-target pass, and a
contiguous ordinal. Reconstitution rejects another reservation even when both
share a target. Observations describe the external object's reported state; they
do not rewrite finding status.

## Store and reconstitution

The PostgreSQL store uses append-only target, run, pass, finding, finding-event,
external-link reservation, attachment, and observation records. The only mutable
workflow columns are the current run and pass state projections; their
evidence-bearing fields change atomically under row lock, and database checks
close every nullable shape. Immutable content and history tables reject update
and delete.

Store loaders read a complete aggregate projection. The adapter decodes closed
discriminators and assembles domain reconstitution inputs; the domain validates
ownership, state shape, event order, and transitions. A missing referenced
record, unknown discriminator, incomplete history, or failed domain check is
reported as corruption rather than normalized into a plausible aggregate
(INV-002, INV-040, INV-041).

The first store surface creates and loads complete aggregates, idempotently
reserves external links, attaches external identifiers, and appends external
observations. Workflow-facing process protocol, code-host and workspace
adapters, pass scheduling, model prompts, automated publication, repair, and
merge-based propagation remain blocked on the
[review-workflow orchestration design](../open-questions.md#destination-features-target-model).

## Open edges

- [Review-workflow orchestration](../open-questions.md#destination-features-target-model)
  owns application commands, scheduling, adapter seams, workflow-facing
  protocol, prompt contracts, conflict escalation, and stack propagation.
- [Artifacts](../open-questions.md#general-purpose-artifacts) remain a separate
  future aggregate; review workflow rows contain references and evidence, not
  copied general-purpose artifacts.

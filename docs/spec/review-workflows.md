# Review workflows

The review-workflow context records why review work exists, which repository
revision it concerns, which session carried each pass, and how findings and
external code-host objects relate to that evidence.

## Overview

The context sits above sessions. Session execution belongs to
[sessions and the transcript](sessions-and-transcript.md), turn evidence to
[turn lifecycle and scheduling](turn-lifecycle-and-scheduling.md), tool
execution to [tool loop](tool-loop.md), and the shared relational mechanics to
[persistence protocol](persistence-protocol.md). The domain types live in
`crates/domain/src/review_workflow.rs`.

Its records are targets, runs, passes, findings, and external links. A target is
one immutable snapshot of a reviewed revision; a refreshed change request is
another target, and an optional parent records the preceding node of a stack. A
run names one target, one workflow kind, and one frozen policy, and admits at
most one pass. A pass names its run, its session, and the accepted input that
carried it; its terminal state is the outcome of the workflow operation, and an
optional bound result records the one typed effect it produced. A finding is
immutable proposed content owned by one succeeded read-only-review pass, and its
status is the tail of an append-only event history. A finding carries two
independent confidences: whether the issue exists and merits attention, and
whether its severity label is correct. An external link correlates a target,
run, or finding with one object at one code host through a reservation, at most
one attachment, and append-only observations of the object's reported state.

References nest. A run reference binds a run to its target, a pass reference
binds a pass to its run and target, and a finding reference (`ReviewFindingRef`)
binds a finding to its producing pass and so to its run and target.

Above these primitives, the orchestration service in
`crates/application/src/review_orchestration.rs` runs one attempt: import
external context, fan out one read-only-review pass per concern, judge the
complete finding set, repair the accepted findings, and publish the survivors.
It sequences the primitives and does not replace their lifecycle, finding-state,
publication-reservation, or frozen-policy rules. The daemon drives that service
through a client-fed adapter whose closed operations each supply one stage
result. A closed review library of prompt templates, resolved at startup under
the catalog rules
[configuration and credentials](configuration-and-credentials.md) states,
supplies the session templates each stage uses. The
[process protocol](process-protocol.md) and the terminal client expose the
primitive and orchestration operations.

The PostgreSQL store in `crates/persistence/src/review_workflow.rs` and
`crates/persistence/src/review_orchestration.rs` keeps append-only content and
history records beside mutable run and pass state projections. Loaders read a
complete aggregate projection; the adapter decodes closed discriminators and the
domain validates ownership, state shape, event order, and transitions. A record
that fails either step is corruption under the rule
[persistence protocol](persistence-protocol.md) states.

## Design decisions

Every child record carries its complete ownership reference, so a cross-wired
target, run, pass, and finding combination cannot be constructed in normal use
and is rejected during reconstitution.

Workflow state does not live inside a session, workflow progress is never
inferred from free-form transcript text, free-form assistant text never becomes
a finding, and an external code-host object is never a domain aggregate.

A parent target records stack topology for that snapshot and authorizes
rewriting neither branch. Two snapshots of the same change request cannot be
parent and child or appear together in one parent chain, because refresh history
is not stack topology.

Policy is immutable run input rather than process configuration: it carries an
ordinal version and minimum judge and publication confidences. Why: the reason
for an unattended judgment or publication is then reconstructible without the
executing binary's defaults. An unknown policy version fails closed until a
later contract revision adds its exact tuple, and supporting that version
changes only later runs.

A pass is recorded only after its orchestration input has been durably accepted
and its origin turn exists, and activation binds to that exact turn; an optional
session identifier is not a substitute for execution evidence. Executable
orchestration is caller-driven, never inferred or scheduled automatically.

Pass terminal state is the workflow-operation outcome; turn outcome
authenticates the execution boundary and does not decide that outcome by itself,
so a failed pass may name a completed turn when workflow output was malformed or
the operation was definitively rejected. Passes never copy model output, tool
results, or transcript content into workflow state: the transcript is the
execution evidence of record and the pass state is the operation outcome of
record.

A produced-findings result commits finding identities and content through the
immutable canonical finding rows, so no second content authority exists.

Finding status is derived from an append-only ordered event history rather than
a writable status field.

The posted event is the one finding event committed by an attachment result
rather than a finding-event result, because its pass is also the attachment
producer.

Accepted and posted transitions compare only is-real confidence against the
frozen policy minimums; severity-label confidence is never a filter and no
override exists. Why: uncertainty about whether a real issue is high or medium
must not suppress the issue. Publication admission is also bound to the
immutable target head: a moved change request is another target and does not
authorize posting results produced against the earlier head.

The event pass's run supplies the exact policy frozen by the finding's producing
run, so judgment, deduplication, and every later classification stay under one
policy across separate runs.

Reconstitution validates the admission status a duplicate or superseded
reference froze, rather than comparing it with a status that may since have
advanced.

A reservation without an attachment is pending: it is never proof that the
external effect did not occur, and it is not retried automatically. Read-only
import may reserve and attach in one local transaction because it issues no
external write.

A report equal to the latest recorded external state appends no observation but
binds the pass's reservation, latest ordinal, and state as a no-change result,
so that pass is spent as durable evidence. Observations describe the external
object's reported state and never rewrite finding status.

The attempt is recorded before any pass starts, so an equal retry resumes the
recorded values and a distinct reuse is a conflict.

The daemon constructs an attempt only when the start selection exactly matches
the resolved review library; an absent library, changed version, changed stage
or concern name, or reordered concern fails closed rather than falling back to
daemon defaults. The concern inventory a library may carry belongs to
[configuration and credentials](configuration-and-credentials.md).

Session execution evidence and attempt configuration agree through two
independently checked bindings, the session's copied template provenance and the
attempt's own digest, rather than through a caller-supplied claim.

Concern work carries no repair or publication handle and no other member's
uncommitted output. Why: a concurrent member holds no more authority than it
needs.

When one fan-out member fails, blocks, or is cancelled, the successful members'
findings remain valid evidence but no judgment, repair, or publication work is
eligible. The service never drops a concern, silently retargets, or publishes
the successful subset as complete.

Until every planned judgment event is durably admitted, repair and publication
stay ineligible; a crash resumes the sealed plan rather than asking a model for
a different partial judgment.

A cross-run duplicate or superseded reference keeps one pass per run and the
original evidence chain: each finding retains its original reference and is
never copied, reparented, or promoted into the judgment run.

Each client-fed mutation supplies exactly one stage result, and every other
runner port reports that it awaits client input. The service may derive and seal
every newly eligible durable stage but cannot substitute model output or advance
through a stage the client did not supply.

Finding events and external-link attachments bind their exact pass result in the
transaction that appends or attaches the effect, so every committed point is an
aggregate the loaders can reconstitute.

How the terminal prints a review mutation's command identity for exact retry
belongs to [process protocol](process-protocol.md).

## Boundary contracts

Refreshing a moving change request creates another target snapshot; it never
rewrites the revision under an existing run.

An effect-producing terminal pass binds its result once. Equal replay observes
the existing effect, and no distinct later effect may cite that pass. A terminal
pass that produced no typed effect may keep an absent result, but a
read-only-review pass that completed output admission binds a produced-findings
result, the empty inventory included. The complete-findings command is the sole
success path for read-only review, and generic pass completion refuses read-only
success.

The caller-selected external-link identity is the idempotency key: equal replay
returns the same reservation, and reusing it for a different association,
provider, or object kind conflicts.

External publication uses two durable steps: the reservation commits before the
external API call, and a successful or reconciled call appends one immutable
attachment.

Ordinary finding-event admission locks the complete target finding inventory in
identity order, then verifies the referenced finding's history under those locks
before appending. The locked heads are the authority for the ordinal, the
referenced status, and the subject transition; no event-history snapshot decides
them independently. An external-link transition locks the reservation and then
any associated finding before loading its multi-statement projection. The lock
protocol these orders extend belongs to
[persistence protocol](persistence-protocol.md).

One orchestration attempt names one immutable target, one frozen policy, one
ordered concern-set version, and the exact template digests its passes use.
Refreshing the change request, changing policy, editing the concern set, or
changing a resolved prompt template requires a new attempt; an orchestrator
never mixes those inputs while resuming. Adapter success returns typed evidence
naming the exact target, policy, run, pass, session, and template inputs; a
mismatch blocks the attempt and is never repaired by substitution.

The attempt durably records the complete expected concern inventory before any
member starts. Judgment is eligible only after every expected member has
succeeded and bound its complete produced-findings inventory, an explicit empty
inventory included. The orchestrator may retry only the failed member, against
the same target, policy, concern-set version, and template digests. One judgment
analysis pass consumes the sealed inventories of all fan-out members, never a
concern subset.

The orchestrator seals the complete judgment plan before admitting its
per-finding judgment and deduplication events through the single-effect pass
primitives in canonical finding order. On resume, the loaded receipts must equal
the first members of the plan in that order; a gap, reordering, duplicate,
foreign attempt, or unknown finding fails before any runner is invoked. Repair
work contains the exact accepted finding inventory only after every sealed
judgment effect is durably applied. Only evidence committing the exact fixed
event removes a finding from the publication set; a failed or cancelled repair
leaves its finding surviving. A blocked repair member yields an incomplete
repair outcome and stops the attempt before the publication inventory is sealed.
The publication result must cover the surviving inventory exactly; any failed,
blocked, or cancelled member yields an incomplete publication outcome, never a
complete one.

Every review mutation carries a user-global command identity under the claim
protocol
[identity, commands, and telemetry correlation](identity-and-commands.md)
states. Before the aggregate effect the adapter commits a typed intent binding
that identity to the validated semantic request. After the effect commits, the
adapter appends a recovery result before it attempts the receipt, and serial
review-mutation admission prevents a later stage from starting before that
recovery result exists. A concern marker binds the immutable claim sequence it
created, so a later replacement of a failed claim cannot redirect exact replay
to the successor. A recovery-only interrupted-judgment result takes part in
stage and snapshot reconstruction and reserves its identity against every
user-global command family until its receipt is materialized.

The orchestration loaders derive the current stage only from durable records;
missing ancestry, an unknown closed value, a noncanonical count, or
contradictory evidence is corruption and never an inferred result.

## Planned

- Concrete provider, model, and workspace adapters for the orchestration runner
  ports, with checkout preparation and external-identifier qualification:
  [review workflows design](../design/review-workflows.md).
- The model-runtime realization of the `submit_review_findings` structured
  finding return: [review workflows design](../design/review-workflows.md).
- Resuming a blocked repair after reconciliation:
  [review workflows design](../design/review-workflows.md).
- Post-publication external-context import:
  [review workflows design](../design/review-workflows.md).

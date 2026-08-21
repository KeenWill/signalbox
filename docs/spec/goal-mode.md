# Goal mode

**Implemented behavior.** This page owns the cross-crate contract for one
commissioned goal attached to a session: its immutable statements, event-sourced
state, user commands, model declarations, scheduler continuation, process wire,
and terminal-client verbs. The domain and persistence surface was verified
through PR #384 (`agent/goal-mode-runtime`). The scheduling, model-tool,
process, and terminal surfaces were verified through PR #384
(`agent/goal-mode-runtime`). Dispatch-composed commissions and the generation a
turn's authority resolves to were verified through PR #562
(`agent/dispatch-session-goals`). The binding of an already-accepted turn to a
generation was verified through PR #578 (`agent/commission-binding`). Resolving
that authority again when a consumer commits is verified against this PR
(`agent/judge-completion-recheck`). Repository-watch-composed stops are verified
against this PR (`agent/daemon-ops-overnight`). This bottom specification diff
owns both stack slices. Bounded automatic resumption of execution-failure blocks
is verified against this PR (`agent/goal-blocked-autoresume`), and its one
exemption — the block an unattended repository-watch approval escalation appends
— against this PR (`agent/headless-approval-escalation`). Ordinary bounded
reconciliation of an operator-commissioned escalation is verified against this
PR (`agent/daemon-live-commissioned-escalation-resume`). Restart reconciliation
of pending automatic resumptions is verified against this PR
(`agent/daemon-live-goal-resume-rearm`). Restart-caused failure accounting is
verified against this PR (`agent/daemon-live-restart-recovery-accounting`).
Identity and durable-command mechanics remain owned by
[identity and commands](identity-and-commands.md), turn execution by
[turn lifecycle and scheduling](turn-lifecycle-and-scheduling.md), tool dispatch
by [tool loop](tool-loop.md), and framing by
[process protocol](process-protocol.md). INV-048 is the lifecycle enforcement
family indexed by [the invariant test index](../invariants.md).

## Statement lineage and state

**Implemented behavior.** A goal statement is exact, nonempty UTF-8 bounded to 1
MiB and immutable after admission; no statement-edit operation exists. Attach
commissions generation one when no lineage exists, and may commission the next
generation after an achieved or user-stopped generation. A pursuing or blocked
generation is active and rejects attach: changing its scope requires supersede,
and mid-goal guidance that does not change scope uses steer while pursuing or
resume while blocked.

**Implemented behavior.** Supersede is one atomic event: it marks the active
generation `superseded { by_generation }`, commissions its immutable successor
as `pursuing`, and retains the replacement statement and user command
provenance. All earlier generations and events remain readable, and exactly one
generation can be active at a time.

**Implemented behavior.** The current state is derived only by replaying the
session's append-only goal event stream; no mutable goal-state column is
authoritative. The state algebra is:

- `pursuing`;
- `blocked { reason, need }` — scheduler-terminal, but admits explicit resume or
  supersede;
- `achieved { report_ref }` and `user_stopped` — each ends that generation, and
  a later explicit attach may start another; and
- `superseded { by_generation }` — terminal for the replaced generation, while
  its same event starts the successor.

**Implemented behavior.** The closed event vocabulary is `commissioned`,
`blocked`, `resumed`, `achieved`, `user_stopped`, and `superseded`. Positive
event ordinals are contiguous within one session, and positive statement
generations are contiguous across commission and supersession. Domain replay
rejects a missing first commission, a noncontiguous event, or a transition that
is invalid from the preceding derived state (INV-048).

## Transition authority and provenance

**Implemented behavior.** User transitions carry their user-global durable
command identity. The user commands are attach, resume with optional guidance,
stop, and supersede with a replacement statement. Their immutable receipts
record either the appended event ordinal or a closed rejection, including
`unknown_model_alias` when the session's selected alias is absent at turn
acceptance and `acceptance_position_exhausted` when the session's positive
accepted-input ordinal cannot advance. Equal replay returns the recorded result;
structurally different reuse is a conflict.

**Implemented behavior.** A commission need not originate from a user request.
Repository-watch dispatch composes an attach for the session it is creating and
commits it in that creation's own transaction, with its own durable command
identity and the same receipt and event shapes any attach records; the goal
statement is synthesized from the dispatch rather than supplied as text. Such a
statement is system-authored in shape but not in every byte, because the
identifiers it renders come from the watched repository, so a consumer placing
it in a model prompt owes it exactly the quoting it owes any session text.

**Implemented behavior.** A synthesized statement delimits every
repository-supplied identifier it renders, because those identifiers are
ordinary text and the sentence around them is not: an identifier left bare could
close the field it sits in and continue as though it were the statement. Each is
rendered between double quotes, with the quote and the backslash escaped so the
closing delimiter cannot be forged, and with every line terminator escaped so a
value cannot leave its line. The encoding is injective, so two distinct
identifiers never render alike and the statement always says which one it named.
Delimiting bounds where the repository's bytes begin and end; it does not make
quoted data harmless, and a consumer still owes the whole statement the quoting
above.

**Implemented behavior.** Every goal turn records the generation it belongs to,
and a consumer reading the authority a turn ran under reads that generation and
not the session's current one, so a supersession while the turn is parked cannot
broaden what that consumer sees. A turn with no such record resolves to no
statement, leaving the consumer to treat the authority as unsettled. A goal
session runs such turns — an ordinary input submitted into a session that
already has a goal — and no generation states anything about them, so inferring
one from the lineage's shape would let a goal attached after the turn already
existed supply authority it never covered.

**Implemented behavior.** The delegated tool-approval judge is the one consumer
that binds its read to its commit. It resolves the statement again when it
commits, under the lock the commit takes, and compares it against the one it
read. Equal statements commit the decision. A statement that resolved before and
resolves to nothing now belongs to a generation that closed, whether it was
stopped, achieved, or replaced by a supersession — a replacement closes the
generation the decision was formed under rather than restating it, so it too
resolves to nothing. That escalates rather than committing a decision formed
under authority no longer in force. Escalating means the attended park, except
for a turn the unattended terminal path claims — one judged under dispatch
authority recorded by [repository watch](repo-watch.md) or by an
operator-commissioned dispatch (also specified there), unsteered, and either the
dispatched work itself or work whose authority has since ended — and which fails
the turn without blocking the generation that has already closed. Work an
operator resumed after an earlier escalation is the other case, and it parks
while its authority stands, because the exemption stated below means only a
person could have resumed it. A judge that read no statement decided without
one, so a generation attached since withdraws nothing and leaves that decision
alone: the comparison pins withdrawal, not novelty.

**Implemented behavior.** The commit-time resolution is not the reading
resolution. Reading binds a recorded generation exactly, so a supersession while
a turn is parked cannot broaden what the consumer is shown. Committing asks
whether the authority the decision was formed under is still in force, so a
recorded generation supplies its statement only while it remains open. A
resolution that bound the generation exactly at commit time would compare a
statement against itself and find withdrawn authority intact.

**Committed unimplemented functionality.** No consumer other than the approval
judge resolves the authority it read a second time when it commits. Such a
consumer commits under the statement as it stood at its read. A future consumer
binding its own read to its own commit follows the escalation rule above rather
than choosing again.

**Implemented behavior.** A model may declare only `blocked` or `achieved`
through the session-scoped goal declaration tool. The declaration has no
caller-supplied session identity: trusted tool-dispatch correlation supplies the
invoking session, turn, and tool-request identity, and persistence requires that
exact triple to name the request. The request must name `goal_declare`, carry
canonical transition-and-reason JSON, be immediately preceded by one
assistant-text part in the same model response, and be that response's final
part. That text is the exact need or report and must match the event the request
causes. A request failing any of those requirements cannot commit. Only the
current goal turn may declare; an otherwise valid request from an older turn
returns `NotCurrentGoalTurn` without appending an event. A tool-request identity
can cause at most one goal declaration event. An achieved event stores the exact
final report and derives its transcript reference from that same invocation.

**Implemented behavior.** Model-selectable blocked reasons are
`user_input_required`, `external_change_required`, and `authorization_required`.
Every blocked event carries exact nonempty need text. `execution_failure` is the
fourth stored reason and is scheduler-only: its provenance shape requires the
source turn and cannot be constructed from a model declaration.

**Implemented behavior.** Stop and supersede are explicit user authority. Stop
yields `user_stopped`, distinct from model-declared achievement and blocking;
supersede is admitted only while the current generation is pursuing or blocked.
Repository watch may compose that same durable parent-only stop solely to
withdraw a generation-one commission it created when the target pull request
closes or merges. It cannot stop descendants or a later user-authored
generation. Resume is admitted only while blocked, and its optional guidance
becomes the next turn's input. Existing steer behavior is unchanged and remains
the only mid-pursuit guidance path.

## Scheduler continuation

**Implemented behavior.** While the current goal state is pursuing, successful
turn terminalization causes the daemon scheduler to create and start the next
turn without user input, and repeats after each successful turn while replayed
state remains pursuing. Goal state is the only continuation stopping condition:
there is no goal turn count, elapsed-time budget, verdict counter, or silent
model fallback. If current defaults still name the predecessor's alias, restart
reconciliation may reuse that turn's frozen definition when the catalog entry
has disappeared. A changed current alias with no definition returns a typed
`UnknownModelAlias` continuation outcome and never falls back or becomes durable
corruption. If the session's positive accepted-input ordinal is exhausted,
continuation returns typed `AcceptancePositionExhausted` without appending a
successor. If scheduler failure blocking cannot append because the positive
goal-event ordinal is exhausted, it returns typed `EventOrdinalExhausted`
instead of classifying valid durable state as corrupt.

**Implemented behavior.** A failed goal turn is not retried; in the same
scheduler disposition path the daemon appends `blocked` with reason
`execution_failure`, need text stating either the scheduled automatic resumption
or the operator repair required, and the exact failed-turn provenance. That
scheduler-turn provenance is single-use and durably requires the current goal
turn to have an unsuccessful terminal disposition. A delayed replay of an
already-recorded failure returns that blocked transition without appending a
second event, including after resume. An unrecorded failure from an older turn
returns `NotCurrentGoalTurn` once resume has made a successor turn current, so
it cannot block the resumed pursuit. Continuation stops on blocked, achieved,
user-stopped, and a superseded generation; supersession's successor is pursuing
and therefore independently eligible to continue.

**Implemented behavior.** An execution-failure block owes its own bounded
automatic resumption. The daemon derives from the goal event history how many
consecutive automatic resumptions the current run has already spent: the run is
the trailing alternation of execution-failure blocks and the resumptions that
answered them, and every other event ends it. Below a budget of five consecutive
attempts, the appended need text states that automatic resumption is scheduled
and names the operator repair for a goal still blocked once resumption ends, and
exactly one resume follows after a backoff of two minutes doubled per attempt
already spent, to a thirty-minute maximum. At the budget the goal stays blocked,
and its need text states that automatic resumption is exhausted and states the
operator repair. All three bounds are fixed in source and no configuration reads
them: an automatic resumption spends provider budget on a session no operator
asked about, so its cadence and its end are product decisions rather than
deployment ones. Every need text an execution-failure block carries names the
operator repair, because an armed attempt can also fail to resume by being
durably rejected, by losing its process, or by never reaching the database, and
in each case that text is what an operator reads. Resumption does not bypass
execution-failure blocking or make a failure a silent retry — the block is
appended first, and every attempt is an ordinary recorded `resumed` event.

A resumed turn the daemon itself loses across restart does not spend that
five-attempt goal budget. The lineage still records its ordinary resumed event
and execution-failure block, while budget derivation associates that resumption
with the turn it started and discounts it only when the turn's terminal attempt
has an append-only startup-recovery origin and the exact model-call or
tool-attempt automatic reconciliation is durably `reconciled`. The startup scan
writes that origin in the transaction that creates the ambiguous wait; the live
slot-held watchdog does not. Runtime boundary loss therefore remains chargeable.
Typed records rather than a restart log line remain authority, and startup
rearming can continue through repeated deploys without turning deployment count
into goal-attempt exhaustion.

**Implemented behavior.** An automatic resumption's durable command identity is
derived from the session and the exact blocked event it answers rather than
minted. A repeated attempt is therefore an exact command replay rather than a
second resume, and the recorded `resumed` event is self-identifying: a resume
carrying any other identity is an operator's, ends the run, and restarts the
budget. Each attempt carries the blocked event it answers into the command, and
that expectation is checked against the lineage under the same session lock the
resume would append within: an automatic resume applies to exactly that blocked
event or to nothing, and an unmet expectation appends nothing and leaves the
derived identity unspent. A goal since resumed, stopped, superseded, or blocked
for another reason is therefore left alone even when it moved between the
attempt's read and its lock. The model-selectable reasons are never
automatically resumed: each names a condition no retry can clear, and only
execution-failure blocking arms an attempt.

**Implemented behavior.** An attempt that reaches no durable answer is owed
another, because nothing else re-reads a blocked goal: an attempt whose database
call fails retries up to three times at the base backoff, reusing its derived
identity so a retry that follows a lost acknowledgement replays rather than
resumes twice. Blocking whose own commit acknowledgement is lost is reconciled
the same way — the daemon reads the lineage back and arms the execution-failure
block it finds, since the need text it was appending expects resumption whether
or not the acknowledgement arrived. That read is retried under the same bound,
because the event ordinal it recovers is the one thing the lost acknowledgement
did not report and the derived identity is a function of it, and it runs off the
scheduler pass, which returns its ambiguity without waiting. Arming a block
another pass already armed is harmless for the same reason a retry is: both
derive one identity, and the second attempt replays it.

**Implemented behavior.** Startup inventories current execution-failure blocks
whose exact need promises automatic resumption and treats their lost timers as
immediately due. The inventory excludes exhausted attempts and blocks whose need
requires an operator, including unattended approval escalations. Inventory
failure receives three retries at a one-second cadence and then remains a
visible durable block; individual resume attempts use the ordinary bounded
reconciliation and derived command identity, so concurrent or repeated startup
attempts cannot append two resumptions for one block.

**Implemented behavior.** One execution-failure block is exempt from automatic
resumption: the block an unattended repository-watch approval escalation appends
in the transaction that fails its turn, described by
[repository watch](repo-watch.md). It arms no attempt, and its need text states
that and names the operator repair directly instead. The work that block ended
is already owed a different retry — repository watch redispatches it under a
fresh dispatch while its rule and target remain eligible — so resuming this goal
would re-run an escalating turn against a request no user is attending, beside
that redispatch, until the budget ran out. Where that redispatch is withheld,
because the rule was deactivated or the pull request closed or merged, the work
is not wanted at all, and an automatic resumption would be the only thing still
pursuing it. An operator-commissioned dispatch has no independent redispatch
path, so its unattended escalation leaves the terminal goal turn for ordinary
reconciliation. The resulting execution-failure block receives the bounded
automatic resumption above and visibly requires an operator only after that
budget is exhausted. Every other execution-failure block likewise owes the
bounded resumption, including one appended for a session repository watch
created or an operator commissioned.

**Implemented behavior.** A periodic durable sweep includes a pursuing goal
whose current goal turn is terminal and still owed continuation or blocking. The
initial post-startup sweep therefore recovers a process loss between turn
terminalization and goal disposition; reconciliation is idempotent and removes
that terminal turn from this candidate shape by scheduling its successor or
appending the blocking event.

**Implemented behavior.** Attaching or superseding commissions a pursuing
generation and schedules its first turn; resuming schedules exactly one next
turn and supplies guidance, when present, as that turn's accepted input.

**Implemented behavior.** A queued turn whose goal generation becomes blocked,
achieved, user-stopped, or superseded remains immutable history but is
ineligible for activation and is excluded from queue predecessor selection and
periodic reconciliation hints. When such a retired origin falls inside an active
turn's accepted-input tail, the runtime projection retains its immutable
acceptance position with an explicit retired-goal-origin marker while omitting
it from the process transcript's turn inventory; tail completeness and the
session acceptance high-water mark therefore remain exact. A stop or supersede
that retires queued goal work appends a durable `goal_turn_retired` update
before any replacement input acceptance. A live follower clears only that exact
queued identity, so obsolete work cannot mask a replacement activation. Durable
event and input correlation makes retrying command delivery idempotent rather
than duplicating continuation work.

## Persistence and process surfaces

**Implemented behavior.** Migration `202608020013` owns `goal_command` and
`goal_event`. Both are append-only and reject truncation, relational checks
close every discriminator and payload shape, and loads replay complete rows
through the domain aggregate rather than reading a mutable current-state
projection (INV-048). Durable rules bind append, correlation, and provenance:

- a session-row lock serializes event append, and a trigger enforces ordinal and
  generation continuity;
- an applied receipt can reference only the event carrying its own command
  identity, operation, and statement or guidance payload, and every user event
  reverse-correlates to that exact applied receipt;
- rejected reasons are closed over the operations that can produce them,
  including durable acceptance-position exhaustion for pursuit-starting
  commands;
- every pursuit-starting user event reverse-correlates to exactly one queued
  goal turn, whose requested and frozen configuration derives from its exact
  defaults epoch;
- a continuation successor must name the acceptance-latest successfully
  completed goal turn in its generation, so an older turn cannot branch after
  resume;
- model-declaration requests and scheduler-failure turns are single-use, and
  composite foreign keys enforce user-command, model-invocation, and
  scheduler-turn provenance; and
- deferred constraints bind each model event to the current goal turn, the exact
  `goal_declare` name and canonical arguments of its request, the immediately
  preceding assistant-text part, and its final position in the model response,
  and bind every scheduler failure event to the current unsuccessfully terminal
  goal turn.

**Implemented behavior.** Migration `202608110013` supersedes the two rule
functions `202608020013` installed for a goal turn's accepted input. A
generation's turn is either scheduled by the goal machinery or bound to a turn a
command already accepted. The machinery mints an accepted input with no
accepting command and writes the statement, or the resume guidance, into it; the
rule that a goal turn's input restate its immutable source verbatim applies to
exactly that case, because it is what proves the machinery invented no text. A
bound turn's text was authored by whoever issued its command — for
repository-watch dispatch, the tagged context of the event dispatched on — so it
carries that command instead. A goal turn therefore either restates its
statement or names an accepting command, and never neither, and an accepted
input with no command still requires exactly one goal source. Every other proof
listed above is unchanged, and both relaxations only widen: no shape admitted
before this migration stops being admitted, so it changes no stored row.

**Implemented behavior.** The process protocol exposes attach, show, resume,
stop, and supersede requests. Show returns the current generation and complete
ordered event history, and the terminal client provides exactly the
corresponding verbs. Attach and supersede statements, and optional resume
guidance, accept either inline text or one bounded UTF-8 file so the 1 MiB
goal-text contract is not constrained by operating-system argument limits.
Session creation may compose an explicit attach immediately after creation,
while the two durable commands retain separate replay identities.

## Compatibility constraints

**Committed unimplemented functionality.** No present goal-mode surface
delegates work or creates child sessions. Future child sessions will compose
with goal mode rather than becoming a goal transition, so version-one goal
events and commands reserve no delegation variant.

**Committed unimplemented functionality.** No present goal-mode surface starts
or governs a review workflow. Future review composition must refer to goal and
session evidence without adding review states to the goal state algebra.

**Committed unimplemented functionality.** No present goal-mode surface chooses
runner placement. Future runner placement of goal sessions must leave goal
statements, transitions, and stopping authority independent of placement.

**Committed unimplemented functionality.** No present goal-mode surface
automatically falls back between models. Future fallback work must not turn a
failed goal turn into a silent retry or bypass execution-failure blocking.

**Committed unimplemented functionality.** No present goal-mode surface has a
goal priority or more than one concurrent goal per session. Future extension
must preserve immutable statements, full lineage, and the version-one rule that
at most one generation is pursuing or blocked.

**Committed unimplemented functionality.** No present judge record carries both
what the provider recommended and what the repository committed. Escalating a
completion whose authority was withdrawn overwrites the provider's answer with
the escalation, so the record retains only the second. A replay can therefore
prove that a substitution was legitimate — the authority is still withdrawn, and
a closed generation cannot reopen — but not which recommendation was
substituted, so a retry offering a different recommendation from the one first
offered is admitted as an exact replay. The structural answer is for the record
to carry both, after which a replay compares the offered value against the
stored offered value and needs no such proof. The same loss admits the mirror
case: a provider's own escalation, committed while the authority was open and
followed by a withdrawal, is indistinguishable from a substituted one, so a
retry offering an approval or a denial is admitted there too. Until then the
exposure is latent rather than live: no caller retries a completion with a
recommendation other than the one it first offered, because the only
uncertain-commit path fails an in-flight judge as ambiguous instead of
re-entering completion.

## Open edges

**Deferred or undecided work.** Separating consecutive execution failures from
ones distant in the same pursuit is recorded under
[goal mode](../open-questions.md#goal-mode). No other goal-mode open question is
recorded by this version-one contract.

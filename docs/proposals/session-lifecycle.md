# Session lifecycle

**Proposed for owner decision; design only.** Nothing on this page is
implemented. Normative paragraphs are marked **Proposed behavior** and describe
what the daemon must do once this specification is ratified; unmarked prose is
context. The proposal is grounded in the owner's 2026-09-01 live-database census
and failure taxonomy; provenance is held in the owner's records. Owner rulings
restated here are binding and are not re-argued.

This specification changes when capabilities land, not which capabilities exist:
reliability work lands first, and no wanted feature is removed. Goal mode,
custom compaction, and the API adapters all stay: base behavior first,
complexity iterated back on afterward behind evals.

## Why

The dogfood database holds 5,851 sessions, 21,851 turns, and 116,429 model calls
over 2026-08-10 to 2026-08-26; 99.7% of the sessions are template-dispatched
repo-watch work. Of those sessions, 105 (1.8%) ever reached `achieved`; 3,598
(61.5%) end on a blocked goal never resumed; 38.7% of all turns end `failed`.
281 sessions (4.8%) never reached any terminal state, every one already idle
more than 72 hours while the daemon was still running. Half of the failed turns
— 4,251 of 8,462 — carry no classified cause. There is no session state column
and no timestamp on any lifecycle row, so none of this was visible without
hand-written SQL. The owner's target: less than 10% of dispatched sessions
failing to reach their finish point, then 2–5%.

Two rules apply to every section below.

**Proposed behavior.** Lifecycle state, deadlines, budgets, and recovery live in
daemon core. No module implements or re-implements any of them. If a module
needs lifecycle behavior that core does not provide, it requests a core change.

**Proposed behavior.** Every numeric bound in this specification — deadlines,
watchdog bounds, retry backoffs — is defined in config, never hardcoded. Values
named in this text are example defaults. Config may set any such bound to none,
meaning unbounded. The only hardcoded limits permitted are guards against
algorithmic explosion — unbounded loops, unbounded recursion, unbounded queue
growth. A new lifecycle limit is a product decision; it is surfaced to the owner
before it ships.

## 1. Session state machine

**Proposed behavior.** Every session is in exactly one of eight states, stored
as a durable core-owned column. Physically the lifecycle columns — state,
`ended_at`, the ownership bit, the payload measurements — land in a mutable
core-owned lifecycle satellite of the session row, the pattern every mutable
per-session value follows today; the committed append-only guard on `session`
itself is untouched, and prose here that says "the session row" means that
satellite. The satellite takes a declared place in the committed lock order — in
the session-then-scheduler prefix, never acquired after the scheduler row — and
the implementing change amends the lock inventory. The states: `created`,
`dispatched`, `active`, `waiting`, `recovering`, `blocked`, `parked`,
`terminal`. Today no durable session state exists; the nearest thing is the web
queue's derived `AttentionState` classifier. Once the column lands, the
classifier becomes a projection of these states plus turn phase — never an
independent machine.

```text
CREATED ─┬─→ DISPATCHED ─→ ACTIVE
         └─→ TERMINAL{retired}                     (admission expiry, §10)
DISPATCHED ─→ TERMINAL{retired}                    (admission expiry, §10)
ACTIVE ─┬─→ WAITING{kind, deadline, waker}   → ACTIVE | PARKED (deadline expiry)
        ├─→ RECOVERING{op, bound}            → ACTIVE | PARKED
        ├─→ BLOCKED{reason, cycle}           → ACTIVE | PARKED
        └─→ TERMINAL{outcome}
PARKED{cause, owner, since} → ACTIVE | WAITING | RECOVERING
                              | CREATED | DISPATCHED
                              (operator/coordinator action; the suspended phase's state, §1)
                            | TERMINAL{abandoned | failed_retryable | failed_structural
                                       | failed_unknown | superseded}
any non-terminal state → PARKED{module_park}       (module target parks, §1)
any non-terminal state → TERMINAL{superseded | stopped}
```

The diagram carries the `retired` expiries §10 defines, the WAITING deadline
escalation §13 requires, and the park closures §2 defines. A module-dispatched
session enters `dispatched`. An interactive session's first accepted input also
moves it to `dispatched`, not `active`: the accepted turn is queued and
activated by the unchanged scheduler contract, and only turn activation makes
the session `active` — so for an owned session the admission deadline covers a
queued interactive turn that never activates. Deadlines are owned-session
obligations everywhere in this specification: the states describe, ownership
governs, and no state arms any deadline on an unmonitored session (§6). A queued
successor turn inside a live session never re-enters `dispatched`; the active
stall deadline covers it.

**Proposed behavior.** `created` and `dispatched` are distinct from `active`
because 229 sessions died between creation and first turn and 52 died with a
queued turn that never activated. A dispatched session that does not reach
`active` within its admission deadline has failed and is retired (§10); it is
never treated as at rest.

**Proposed behavior.** `waiting` carries a typed kind, a deadline, and a
designated waker. The closed kind vocabulary is `approval{decider}`,
`external{gate, recheck}`, `child{session}`, `provider_retry{backoff}`,
`pipeline{backlog}`, `scheduler{fault}`. Deadline expiry is a transition —
escalate to `parked` — never a silent hold. This closes the 110 stuck approvals
and the 10 external-gate stalls: an expired approval escalates and survives turn
boundaries instead of failing the turn (17 headless-escalation kills). Where a
module's redispatch owns the retry — the unattended repo-watch escalation fails
its turn today precisely because a fresh dispatch supersedes it — the redispatch
closes the parked predecessor as `superseded{by}`, so survival never duplicates
pursuit.

**Proposed behavior.** For an owned session, `active` carries a stall deadline;
an unmonitored conversation carries none, per §6. The model-call count per
session is recorded as a measurement the owner reads. Runaways were loud, not
silent: 200+ model calls against an already-merged branch, one four-day session.

**Proposed behavior.** An owned session's deadlines are three — admission (§10),
active stall, and waiting — each config-sourced. A missing deadline is
unbounded, not a violation.

**Proposed behavior.** `parked` is the single state in which an owned session
may wait on a human. It carries a machine-readable cause and an owner. The
operator queue is exactly `SELECT * FROM session WHERE state = 'parked'` — the
query that could not be written for the 281 stuck sessions.

**Proposed behavior.** The turn machine (`queued / active / terminal`, with the
`awaiting_*` phases) persists unchanged beneath the session machine, and the
session state column is authoritative: core updates it in the same transaction
as every turn or goal transition that changes the mapping, so the two machines
never disagree. The mapping: a turn in `running` ⇒ `active`;
`awaiting_tool_approval` ⇒ `waiting{approval}`; `awaiting_child` ⇒
`waiting{child}`; `awaiting_model_call_recovery`, `awaiting_tool_recovery`, or
`awaiting_runner_recovery` ⇒ `recovering{op}`; a `blocked` goal with no live
turn ⇒ `blocked`. `parked` is the one session state that overrides the mapping:
parking suspends a live turn in place — the turn keeps its phase, and no model
call, tool execution, or delivery proceeds while the session is parked — and
terminalizes nothing (§13). The eligibility sweep and the liveness watchdog gain
the parked conjunct in the same change: a parked session's rows are neither
sweep candidates nor watchdog candidates until it leaves `parked`. Leaving
`parked` re-enters the state the mapping gives for the suspended turn's phase.
The mapping therefore governs every non-parked session; a deadline expiry that
parks mid-turn uses this suspension, which is how an expired approval survives
without failing its turn (the waiting rule above).

**Proposed behavior.** Module machinery that parks its own targets today —
`convergence_sweep_target` rows in `parked`, repo-watch external obligations
with `parked_at` — must drive the session itself to core `parked` whenever the
parked thing is or wraps a session. Module-internal parks may remain only for
non-session obligations. No module state may hold a session waiting on a human
outside core `parked` (§13); otherwise the operator-queue query above is
incomplete.

## 2. Terminal outcomes

**Proposed behavior.** The closed terminal-outcome vocabulary is
`achieved_verified`, `achieved_declared`, `failed_retryable{cause}`,
`failed_structural{cause}`, `failed_unknown`, `stopped{actor, sticky}`,
`superseded{by}`, `abandoned`, `retired`. A structural failure, an unknown
failure, or an exhausted retry budget on a live owned session parks the session
(§1) rather than terminalizing it, with the typed cause attached. Every parked
closure — `supersede`, `abandon`, `close_failed`, `stop` alike — first settles a
suspended turn through the committed machinery: an applied interrupt is the only
cancellation authority, an approval wait is denied before interruption as that
machinery requires, and a possibly-executed operation terminalizes
`reconciliation_required`, never `cancelled` — ambiguity evidence is never
erased. The session records terminal only after the turn settles, so no terminal
session leaves a non-terminal turn behind (§1). Session terminalization likewise
settles the current goal generation in the same closure — a terminal goal event
matching the outcome — because goal state is the sole continuation-stopping
condition in the committed goal contract: a pursuing or resumable goal must
never survive beneath a terminal session, scheduling work no one owns. The park
then closes with the outcome that matches its resolution: `superseded{by}` when
a fresh respawn takes the work; `abandoned` on operator write-off;
`failed_retryable{cause}`, `failed_structural{cause}`, or `failed_unknown` when
it closes as failed with the cause standing. Each outcome carries its warranted
recovery as normative behavior:

- `achieved_verified` — recorded when the session's declared finish check
  passes: for repo-watch work, the external gate re-checked on the exact head
  (sessions have been declared achieved on non-converged heads); otherwise the
  finish condition declared at creation or goal-attach. A finish condition is
  optional everywhere; the check's verdict is recorded, and a failing check
  commits no `achieved` event: the goal blocks with the check result as its need
  text and stays resumable. Recovery: none; slots and worktrees are released.
- `achieved_declared` — achievement recorded when no finish condition was
  declared. Recovery: as `achieved_verified`.
- `failed_retryable{cause}` — provider transient, quota, overload,
  infrastructure blip. A retryable failure on a live owned session does not
  terminalize it: the session passes through `recovering` or `blocked` (§1)
  while budgeted retries run, and `failed_retryable` is recorded only when the
  session closes with the retryable cause standing — retry budget exhausted, or
  a park closed as failed. Recovery at the point of failure: budgeted backoff;
  quota causes trigger credential-pool rotation. (Today no quota trigger is
  wired to the pool machinery: 1,552 `quota_exhausted` calls and zero
  credential-pool actions.)
- `failed_structural{cause}` — the same input will fail again: compaction wall,
  broken toolchain, moderation block whose resume re-trips the same flag.
  Recovery: never auto-resume. The session parks with the structural cause
  attached; the preferred closure is a fresh respawn, which closes the park as
  `superseded{by}` (§9).
- `failed_unknown` — no classified cause. Recovery: park. Its count is a §12
  view.
- `stopped{actor, sticky}` — a human or rule stop. Sticky: re-dispatch is
  suppressed until the dispatch source is updated (a stopped goal was
  re-dispatched minutes later because the allowlist was not pruned). Stopping
  retires queued turns legally (§10) and settles a live turn through the
  committed interrupt machinery — no standalone cancellation authority is
  minted, an issued call resolves through its durable cancellation state, and
  live ambiguity terminalizes `reconciliation_required` — with the session
  recording terminal only after the turn settles (§1); nothing is orphaned.
- `superseded{by}` — a newer session owns the work, or the goal itself is no
  longer valid. `by` is optional: it names the successor when one exists and is
  empty when the issuing module finds the work gone with nothing replacing it.
  Recovery: release everything; further escalations and notifications are
  forbidden.
- `abandoned` — an operator writes off a parked session. Recovery: none;
  worktrees, containers, and slots are released.
- `retired` — the session never did the work and never will: admission expiry
  (§10). A goal turn retired during goal replacement (§7) is a turn disposition
  only; it never retires a live session.

## 3. Timestamps

**Proposed behavior.** The outbox header carries
`recorded_at timestamptz NOT NULL`. The session row carries `created_at` and
`ended_at`. Every lifecycle row — `turn_lifecycle`, `turn_attempt`,
`model_call`, `tool_attempt`, `goal_event` — is stamped at write time. Today
none of those five tables carries a timestamp. The only clocks near a session
are command claim times (`durable_command.claimed_at`), a handful of side
journals, and creation instants derivable from UUIDv7 identities. Turn duration,
queue wait, and every rate are therefore unanswerable from the lifecycle rows
themselves.

**Proposed behavior.** The compaction call lifecycle is stamped at each step:
the command's acceptance as a durable `requested_at` —
`durable_command.claimed_at` is non-semantic operational metadata and never
stands in for it — then the call's `prepared`, `in_flight`, and `terminal`
transitions, and the application row written at apply time.

**Proposed behavior.** Watchdog state survives restarts: staleness evidence is
durable, never a process-local ledger. The committed watchdog decides staleness
by repeated observation, deliberately storing no clock — sound under clock
adjustment, but its observation ledger is process-local, and a daemon restart
resets every staleness clock exactly when restarts are the leading creator of
stuck sessions. This proposal keeps the repeated-observation structure and its
clock-skew argument — ordering authority stays with commit-ordered sequences,
and no wall-clock comparison alone ends work — and makes the observations
durable, so a restart costs nothing instead of one more bound per wedge.

## 4. Mandatory cause classification

**Proposed behavior.** Every turn that reaches `terminal` records a non-null
typed cause. The vocabulary is closed and includes `context_headroom_exhausted`
and `context_compaction_wall`. Today 4,251 of 8,462 failed turns (50.2%) carry
no classified cause: 2,850 have only a bare `known_failure` attempt disposition,
1,273 model-call failures have a null provider cause, and 128 are `lost`. 898
attempts ended `lost` with zero runner-loss records to explain them.

**Proposed behavior.** The two guard closures that today exist only as log lines
— `reported_usage_context_compaction_exhausted` and
`reported_usage_context_still_exceeded`, the pre-activation context guard's two
walls — become durable typed causes.

**Proposed behavior.** Cause-completeness is measured as §12 defines it.
Presence of a typed cause is 100% by construction under this section's mandate,
so the measure is usability: the share of terminal turns, and of `known_failed`
model calls — the one disposition that admits a provider cause — carrying a
cause outside the catch-all set. Today 66.4% of `known_failed` model calls carry
no usable cause — 2,366 `unrecognized` plus 1,273 with none at all, of 5,484.

## 5. Lifecycle event vocabulary

**Proposed behavior.** The module-facing lifecycle vocabulary is exactly eight
event kinds, each with a typed payload, carried on the existing transactional
outbox: `session_created`, `session_state_changed`, `session_terminal`,
`turn_terminal`, `goal_changed`, `command_settled`, `injection_settled`,
`session_ownership_changed`. Every event carries `recorded_at` from the header,
and a session reference where one exists.

**Proposed behavior.** Each of the seventeen existing outbox kinds gets an
explicit disposition; the vocabulary is never doubled. `session_created` evolves
in place: a new `storage_version` carries the typed provenance payload.
`turn_terminal` replaces the six per-disposition turn kinds (`turn_completed`,
`turn_failed`, `turn_refused`, `turn_cancelled`, `turn_reconciliation_required`,
and `turn_tool_reconciliation_required`, whose distinct tool-attempt payload the
typed disposition keeps) and subsumes `goal_turn_retired` as
`turn_terminal{disposition: retired}`. The turn-progress frontier keeps its
exclusion by disposition: `turn_terminal{retired}` is not turn progress —
retiring queued work happens to a session while its active turn sits still — and
the frontier's partial index migrates to a disposition-aware predicate in the
same change. The consumers that decode the old kinds — the session-timeline
projection, the operator-attention triggers, the process-protocol decoders —
migrate in the same change. The remaining kinds (`model_call_transition`,
`tool_batch_transition`, `tool_approval_decided`, `input_accepted`,
`turn_activated`, `turn_model_settings_resolved`,
`session_model_settings_changed`, `context_compacted`,
`runner_state_transition`) stay as core-internal events: still on the outbox,
not part of the module-facing vocabulary, unavailable across the module seam —
the process protocol's wire fan-out keeps decoding every one of them unchanged;
clients lose nothing.

**Proposed behavior.** If a module needs an event kind core does not emit, it
requests the kind as a vocabulary change; modules never reconstruct events by
joining core tables.

**Proposed behavior.** `command_settled` is the one kind that can settle without
a session — a command against an unknown session. The outbox header's session
column, today `NOT NULL` with a foreign key, becomes nullable for exactly this
kind.

**Proposed behavior.** The session timeline, which queries the per-kind event
tables directly today, becomes a durable projection maintained from the outbox
write. The singleton delivery row and the outbox's append-only rule are
untouched; the structural fix for outbox growth remains frontier normalization,
the owner's standing 2026-08-23 ruling.

## 6. Provenance and ownership

**Proposed behavior.** Session creation records a typed cause: `interactive`,
`module_dispatched{module, dispatch_ref}`, or `delegated`. Today's closed
vocabulary is `user_initiated | delegated`, and in the measured window every one
of the 5,851 sessions is `user_initiated` — including all machine-dispatched
ones. Only the soft `template_name` string separates 99.7% machine work from 19
interactive sessions. The committed imported-frontier creation family records
`interactive` with its import reference in the payload — the import is a
user-initiated act, and the vocabulary stays closed.

**Proposed behavior.** Every session carries an explicit owned-or-unmonitored
bit, set at creation and flippable both ways as a journaled adopt or release
transition. Owned means the daemon holds a liveness obligation: deadlines and a
driven path to a terminal outcome. Unmonitored means a conversation: no
deadlines, no watchdogs, no auto-resume, no slot held, and no external sweep may
act on it (wrong-agent stops and accidental mass-stops). Unmonitored sessions
are excluded from occupancy accounting. `release` never interrupts a live
operation: a running turn completes to its boundary under the resources already
held, and the slot releases at that boundary; the flip drops the forward-looking
obligations — deadlines, watchdogs, auto-resume — immediately, disarming every
deadline without changing state (§13). `release` on a `parked` session is
rejected: `parked` is an owned-only state (§1), so the park is closed or resumed
first.

**Proposed behavior.** Every command and every state transition records its
actor from the closed vocabulary `core`, `operator`, `module{name}`, `watchdog`.
This classifies the existing domain `Actor` (`User`, `Model{turn}`, `Recovery`,
`Tool{request}`) rather than replacing it: the domain algebra, its total wire
projection, and its replay-equality contract are untouched — every command keeps
its exact domain actor. The lifecycle record derives its classification from
that actor: `User` reads as `operator`, the recovery scan as `watchdog`, model-
and tool-initiated agency as `core` with the exact turn and request identity
kept in the payload, and module-issued commands as `module{name}`. The committed
program-issuance arm is preserved untouched: the vocabulary stays extensible to
the verified program-run actor the identity contract commits — a run-scoped
reference, not `module{name}` — which lands with the program substrate. Today
manual operator workarounds — the crutch layer — are indistinguishable from
daemon actions in the record.

## 7. Command surface

**Proposed behavior.** The core command surface for session lifecycle is:
`create_session{template, provenance, start_gate, ownership, finish_condition, payload}`,
`release_start`, `submit_input`,
`goal{attach | resume_with_guidance | stop{sticky}}`, `adopt`, `release`.
`payload` is the dispatch content whose measurements §15 records at creation:
module dispatch supplies it atomically in the creation command, as the
commissioning path does today, so a `dispatched` session never precedes its
recorded payload; an interactive creation omits it and is measured at first
input (§15). `finish_condition` is optional everywhere (§2); `goal{attach}`
confers ownership. This specification adds five commands to that list so that
every §2 outcome and every §1 transition is reachable. Every stop — goal, turn,
and session level — carries the committed `descendant_scope` member as durable
intent, unchanged, and the session-level closures record the same cascade
provenance the goal and turn stops record today, so a delegated child is never
silently orphaned. The five: `stop{actor, sticky}` at session level, because a
goal-less owned session needs a stop path; `supersede{successor}`, the closure a
respawning client issues against its predecessor, `successor` omitted when the
work is gone with nothing replacing it (§2); `abandon`, the operator write-off
of a parked session; `close_failed`, the operator closure of a parked session as
failed with its standing cause — `failed_retryable`, `failed_structural`, or
`failed_unknown` (§2); and `resume`, the operator or coordinator transition of a
parked session back to the state §1's mapping derives from its suspended turn's
phase — `active`, `waiting`, or `recovering`; `active` when no turn was
suspended — where no blocked goal applies. A parked session with a blocked goal
resumes through `goal{resume_with_guidance}` (§9); one with a pursuing goal may
use `resume`.

**Proposed behavior.** The existing goal-command operation named `supersede` —
new goal generation within the same session — is unrelated to the session
outcome `superseded{by}` and keeps its machinery unchanged. This specification
calls it goal replacement.

**Proposed behavior.** Core mints all lifecycle identities. No module
pre-allocates turn, input, or frontier identities inside its own transaction
(today's dispatch pre-mints four core identities — a turn, an accepted input, a
cancellation entry, and a cancellation frontier). Caller-minted
`DurableCommandId`s are untouched: retransmitting under the same command
identity is the caller's idempotent retry path under the commands contract, and
that path requires the identity to exist before submission.

**Proposed behavior.** Every claimed command settles as a `command_settled`
receipt carrying applied-or-rejected with a closed rejection kind (§5).
Pre-claim admission errors keep their committed synchronous error path and
record nothing.

**Proposed behavior.** `start_gate` is a core concept; the module-owned
dispatch-lease tables — `repo_watch_dispatch_start_lease` with its `_expiration`
and `_quarantine` companions — remain while current dispatch paths read them. A
session created with a held start gate stays in `created` until `release_start`
or, on an owned session, admission expiry, which retires it (§10); releasing its
ownership opens the held gate in the same transaction, and an unmonitored held
gate carries no deadline (§6).

**Proposed behavior.** Ownership is advisory: an owner module observes events
and issues commands like any other client; it never sits between core and the
session.

## 8. Injection contract

**Proposed behavior.** Message injection — operator text, coordinator guidance,
steering — is legal in every non-terminal state, regardless of ownership: the
owner can send a message at any point.

**Proposed behavior.** An injection into any non-terminal state queues durably
and is delivered at the next legal boundary. It is never rejected for state and
never silently lost: every accepted injection settles with a durable
`injection_settled` receipt whose closed outcomes include `delivered`,
`not_delivered`, and `rejected{kind}`. State means the session's lifecycle state
— the committed correlation contracts stand untouched: an injection that names
an exact turn or defaults version keeps its typed mismatch rejection, settling
`rejected` rather than retargeting, and a correlation mismatch is not a state
rejection. The enumerated violations this repairs: injections rejected while
awaiting approval, rejected while awaiting recovery, silently dropped on resume,
and swallowed by the composer.

**Proposed behavior.** Pending injections never block terminalization. A turn
boundary is delivery, not loss: the committed reclassification of pending
steering into the successor origin turn stands and settles the injection
`delivered`. Session terminalization — where no successor turn can exist —
closes pending steering with a `not_delivered` receipt; pending steering never
refuses it. That closure is a third pending-steering disposition added to the
committed two (consumed, reclassified): the pending-steering guard is amended
for it in the implementing change, explicitly — the schema today forbids any
drop.

**Proposed behavior.** Injecting into an unmonitored session creates no
obligations. Injecting into an owned session resets no deadline.
`resume_with_guidance` journals its guidance (§9).

**Proposed behavior.** Approval decisions are injections: durable, surviving
turn boundaries, socket loss, and drains (110 stuck approvals; decision-channel
failures, including drains removing the socket mid-decision). Durability changes
the transport, not the correlation contract: a decision stays bound to the exact
tool request it decides, as the existing `DecideToolRequest` contract already
requires. A decision arriving after its request was decided, or after the turn
moved past that request, settles `not_delivered` — it is never applied to a
different request.

**Proposed behavior.** The web UI exposes send-message on every live session.

## 9. Goal cycle governance

**Proposed behavior.** Every resume journals its guidance and its actor, so
automatic and operator resumes are distinguishable in the durable record. All
5,590 resumes in the measured window look identical. Goal-mode code landed since
gives automatic resumes a domain-separated derived identity and journaled
guidance text. It already carries a config-sourced attempt budget whose
infrastructure retries charge nothing, and, independently, the committed
lifetime attempt ceiling that counts every attempt whatever its fault
attribution — the limit that ends a run whose every failure is exempt. Both
stay: a run ends at whichever limit it reaches first, read by the same operator
projection, exactly as committed.

**Proposed behavior.** Reaching either limit transitions the goal's session from
`blocked` to `parked`, where the owner sees it. Exhaustion is never a silent
stop (budget exhaustion today is an invisible park).

**Proposed behavior.** A fresh respawn is preferred over resuming a structurally
failed session; the respawn closes its predecessor's park as `superseded{by}`
(§2; a fresh session converged in 10 minutes where the resumed one had looped).

**Proposed behavior.** Goal mode is reserved for long-horizon work — hours or
days. Routine dispatch uses plain sessions with vendor compaction. Goal mode is
never stripped. Its compaction contract: a goal session that compacts re-reads
its goal after compaction and carries its working state forward — testable
because the next turn's context contains the goal statement and the carried
state. This extends the base case, a plain session that can compact and finish,
and is improved later behind evals.

## 10. Retired and the admission deadline

**Proposed behavior.** `retired` is a legal terminal disposition for any queued
turn that never activated — goal turns, and the queued admission turns whose
expiry this section defines, an owned interactive first turn included. For goal
turns, `goal_turn_retired` is published today but the turn vocabulary cannot
express it: all 52 non-terminal turns in the database are exactly this shape,
and every published retirement is such a turn — the match holds in both
directions. Adding `retired` closes all 52. A turn terminal with disposition
`retired` contributes no terminal frontier and stays excluded from queue
predecessor selection, exactly as retired queued work is excluded today: the
disposition changes the vocabulary, never the lineage rules.

**Proposed behavior.** Every owned session carries one config-sourced admission
deadline covering `created` and `dispatched` alike — a held start gate, an
awaited first input, or a queued turn that never activates; an unmonitored
session carries none (§1, §6). Expiry retires the session with cause
`admission_deadline_expired`, and any queued turn retires with it in the same
transaction — the machines move together (§1). This closes the 229 zombie
sessions that were created, never ran a turn, and sat idle forever with a
lifespan of 0.0 hours.

## 11. Compaction observability

**Proposed behavior.** Every successor turn created after a compaction records
`preceding_compaction_id`, linking it to the compaction that made room for it.
Today 2,465 compactions exist with no such linkage.

**Proposed behavior.** The compaction funnel is fully queryable from durable
state: requested, prepared, applied, failed, each stamped (§3), with input size
and fit result on the failure path. The funnel stages map onto §3's stamps
one-to-one: `requested` is the accepted compaction command, `prepared` and
`in_flight` are the call lifecycle, `applied` is the application row, and
`failed` is a call reaching `terminal` with no application row — one
classification per compaction, however it is queried. Today the only durable
compaction-failure trace is 23 `goal_execution_failure_recovery` rows, all with
the single cause `context_compaction_input_does_not_fit`.

**Proposed behavior.** Every compaction records whether it was vendor or custom,
and on which adapter. The owner ruling covers all four adapters — codex_cli,
claude_code CLI, Anthropic API, OpenAI API — codex first. This is the eval
scaffold the compaction ruling requires.

**Proposed behavior.** Vendor compaction is the default path. The home-rolled
compactor stays as the eval baseline — never deleted, re-added per adapter once
the eval system can reliably measure it. The re-enable gate itself is a product
decision surfaced to the owner at that point, not shipped silently.

**Proposed behavior.** A compaction-wall event records the session's initial
payload size alongside the wall (§15). Their handling is §2's park-and-respawn
with cause `context_compaction_wall`; their rate is §12's `wall_rate` view.

## 12. Metrics

**Proposed behavior.** Five metrics are defined on durable columns, never on
proxies.

- `session_completion_failure_rate` — the headline. Cohort: sessions reaching
  `terminal` in a calendar week that were owned at any point in their life, per
  the journaled ownership record (§6). Denominator: the cohort minus `stopped`
  and minus the supersessions that closed no failure; a `superseded{by}` that
  closes a park holding a failure cause is failure-driven and stays in both
  denominator and numerator under its standing cause. Numerator:
  `failed_retryable`, `failed_structural`, `failed_unknown`, `abandoned`,
  `retired`, and failure-driven supersessions. Today's equivalent is inverted:
  1.8% of sessions ever achieved and 38.7% of turns failed.
- `overflow_incidence` — fraction of the full weekly terminal cohort, before the
  stopped/superseded trim, recording cause `context_headroom_exhausted` on any
  turn.
- `P(finish | overflow)` — of the sessions counted by `overflow_incidence`, the
  fraction whose terminal outcome is `achieved_verified` or `achieved_declared`.
  The owner's observation this measures: a session that starts small and grows
  into the wall through real work almost always succeeds.
- `wall_rate` — fraction of sessions dispatched in the calendar week recording
  cause `context_compaction_wall`. The rate counts walls of every kind, organic
  growth included; the recorded initial payloads (§15) sit beside it.
- `cause_completeness` — terminal turns whose typed cause is usable — outside
  the catch-all set: `unrecognized`, absent, or a bare unknown bucket — over all
  terminal turns, and, for model calls, usable causes over the calls whose
  disposition admits a cause — `known_failed`, the only disposition the schema
  allows a provider cause on — never over all terminal calls, most of which
  complete (§4).

**Proposed behavior.** Two companion views: `nonterminal_past_deadline` counts
owned sessions whose armed deadline has expired without its transition firing —
a session that never terminalizes never enters any weekly cohort, and the
281-session class this specification opens with is visible here, not in the
headline; `failed_unknown_share` is the share of `failed_unknown` in the weekly
terminal cohort over the headline's denominator (§2, §4).

These metrics are read-only views; targets and the decision to start substrate
work are owner decisions made outside the daemon.

## 13. Watchdog and recovery posture

**Proposed behavior.** Recovery preserves rather than terminalizes whenever the
guarded operation may still be live. The measured record demands it: 100 of 777
incident reports (12.9%) are root-caused safety-backfire. The window's worst
population-wide outages were caused by protection machinery: a 1-second
reconciliation bound terminalizing live 10-minute model calls, and a restart
chain parking all 24 commissioned sessions at once. Turn-level failure
terminalization that arms goal recovery — the liveness pass ending a provably
wedged turn as failed, blocking its goal with `execution_failure` — is not
session terminalization and stays: this section governs session disposition, not
the committed turn watchdog's disposition of dead work.

**Proposed behavior.** Recovery bounds are config-sourced; the reconciliation
bound behind the recorded backfire is raised.

**Proposed behavior.** Every deadline expiry is a transition. For a session past
admission, the escalation target is `parked` — the single enumerable
human-attention state (§1). The admission deadline is the exception: it
terminalizes as `retired` (§10) — before first activity nothing live is guarded
and no human attention is owed. No deadline expiry is a silent hold, and none
terminalizes work whose operation may be live.

**Proposed behavior.** No staleness machinery exists outside core. Modules and
the future substrate subscribe to deadline events; they do not grow their own
watchdogs. The default posture for a tripped guard is wait, ask, or hand off to
`parked` — never terminalize on local staleness evidence alone.

## 14. Schema

**Proposed behavior.** The dogfood DB never fences a good schema change. At
most, an agent shoehorns old rows later.

**Proposed behavior.** Lifecycle DDL lands as layered statements in the clean
per-domain schema files — sessions, goals, outbox — split by purpose, never
chronologically. Forward changes are small separate migrations that the next
collapse folds back in. A collapse is a deliberate chain reset under the owner's
rule-now-reset-later ruling: the forward-only immutability rule governs between
collapses, and each collapse is its own sanctioned one-time rewrite, exactly as
the 2026-09-01 fifteen-file baseline was.

## 15. Dispatch payload

**Proposed behavior.** Every dispatched session records the size of its initial
payload at creation: token count as estimated for the target model, and byte
count, stored durably on the session row. No dispatch path may skip this. An
interactive session has no payload at creation (§1); it records the same
measurements when its first input is accepted — that input is what the session
was handed.

**Proposed behavior.** Per-template payload sizes are a view the owner reads
alongside `wall_rate` (§12). The owner's reframe binds this section: frequent
walls mean the payload was too large to begin with — judge passes and fixup
briefs handed too many comments or too much commit diff history — not organic
context growth. Payload sizing is fixed first; the wall path is the backstop.

**Proposed behavior.** Sessions that start around 10% full and hit the wall
through real work almost always succeed; they count in the rate (§12), and the
recorded initial payloads are what separate dispatch-defect walls from those
rare organic ones.

**Proposed behavior.** When a wall still occurs, the session parks with cause
`context_compaction_wall`. The backstop is respawn-fresh, which closes the park
as `superseded{by}`; a park closed as failed records
`failed_structural{context_compaction_wall}` (§2, §9). Auto-resume into the same
wall is forbidden. The codex goal path already suppresses automatic resumption
on its one wall cause; this rule generalizes that suppression to every
structural cause. The recorded failure shape: 26 block-resume cycles against one
wall with zero progress (21 recorded occurrences).

**Proposed behavior.** The web UI shows what a session was handed: the session
view renders the recorded initial payload — size, source, and content. The
sessions list shows each session's state and payload size at a glance. The
existing dashboard could not show the owner that nearly all dispatched sessions
were dead. The sessions list, session-text reading, and dispatch-payload display
are the requirements the first web slice is defined against.

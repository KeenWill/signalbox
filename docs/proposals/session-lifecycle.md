# Session lifecycle

**Proposed for owner decision.** Normative paragraphs are marked **Proposed
behavior** or **Implemented behavior**; unmarked prose is context. The proposal
is grounded in the owner's 2026-09-01 live-database census and failure taxonomy;
provenance is held in the owner's records. Owner rulings restated here are
binding and are not re-argued.

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

**Proposed behavior.** Every numeric bound in this specification — cycle
budgets, dispatch deadlines, watchdog bounds, payload budgets, retry backoffs —
is defined in config or the database, never hardcoded. Values named in this text
are example defaults. Config may set any such bound to none, meaning unbounded;
§1 defines how an unbounded deadline satisfies the owned-session invariant. The
only hardcoded limits permitted are guards against algorithmic explosion —
unbounded loops, unbounded recursion, unbounded queue growth. A new lifecycle
limit is a product decision; it is surfaced to the owner before it ships.

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
DISPATCHED ─→ TERMINAL{retired}                    (dispatch-deadline expiry, §10)
ACTIVE ─┬─→ WAITING{kind, deadline, waker}   → ACTIVE | PARKED (deadline expiry)
        ├─→ RECOVERING{op, bound}            → ACTIVE | PARKED
        ├─→ BLOCKED{reason, cycle}           → ACTIVE | PARKED
        └─→ TERMINAL{outcome}
PARKED{cause, owner, since} → ACTIVE | WAITING | RECOVERING
                              (operator/coordinator action; the suspended phase's state, §1)
                            | TERMINAL{abandoned | failed_retryable | failed_structural
                                       | failed_unknown | superseded}
any non-terminal state → TERMINAL{superseded | stopped}
```

The diagram carries the `retired` expiries §10 defines, the WAITING deadline
escalation §13 requires, and the park closures §2 defines. A module-dispatched
session enters `dispatched`. An interactive session's first accepted input also
moves it to `dispatched`, not `active`: the accepted turn is queued and
activated by the unchanged scheduler contract, and only turn activation makes
the session `active` — so for an owned session the dispatch deadline covers a
queued interactive turn that never activates. Deadlines are owned-session
obligations everywhere in this specification: the states describe, ownership
governs, and no state arms any deadline on an unmonitored session (§6). A queued
successor turn inside a live session never re-enters `dispatched`; the active
stall deadline covers it.

**Proposed behavior.** `created` and `dispatched` are distinct from `active`
because 229 sessions died between creation and first turn and 52 died with a
queued turn that never activated. A dispatched session that does not reach
`active` within its dispatch deadline has failed and is retired (§10); it is
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

**Proposed behavior.** For an owned session, `active` carries two obligations
beyond a stall deadline; an unmonitored conversation carries neither, per §6.
First, a progress budget of model calls and wall-clock time: the budget
decrements per model call and per elapsed interval, and resets when progress is
recorded — a turn completing, or a goal event advancing the goal. Exhaustion of
either component is a transition to `parked` with cause
`progress_budget_exhausted` — never a silent hold, and never a terminalization
of possibly-live work (§13). Second, a goal-validity recheck at a config-sourced
interval, transitioning to `terminal{superseded}` when the work is gone. The
transition settles any live turn first, through the committed machinery — an
applied interrupt is the only cancellation authority, an issued provider call
resolves through its durable cancellation state, and a possibly-executed
operation terminalizes `reconciliation_required`, never `cancelled`, because
cancellation must not erase ambiguity evidence — and the session records
terminal only once the turn settles, so the turn-to-session mapping never
disagrees. Runaways were loud, not silent: 200+ model calls against an
already-merged branch, one four-day session.

**Proposed behavior.** Invariant for owned sessions: every non-terminal state of
an owned session carries exactly one armed deadline whose expiry is a defined
transition. "No armed deadline on a non-terminal owned session" is a detectable
invariant violation. A deadline whose governing bound is configured none
satisfies the invariant explicitly, not silently: the deadline record exists and
is marked unbounded — a deliberate, journaled configuration choice — and §12's
alarm never counts it. Only a missing deadline record is the violation.

**Proposed behavior.** `parked` is the single state in which an owned session
may wait on a human. It carries a machine-readable cause and an owner. The
operator queue is exactly `SELECT * FROM session WHERE state = 'parked'` — the
query that could not be written for the 281 stuck sessions. A parked session's
armed deadline is a config-sourced re-notification interval: its defined expiry
transition re-raises the operator alert and re-arms — it never moves the session
and never terminalizes it. Because the re-alert is the deadline's defined
transition, a parked session on schedule never trips §12's
`nonterminal_past_deadline`. The re-alert is observable without widening §5's
closed vocabulary: it rides the deadline re-arm — a real row change, so the
outbox's no-event-without-a-write rule holds — emitting `session_state_changed`
with the state unchanged and a typed re-notification payload.

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
The mapping therefore governs every non-parked session; a deadline expiry or
budget exhaustion that parks mid-turn uses this suspension, which is how an
expired approval survives without failing its turn (the waiting rule above).

**Proposed behavior.** Module machinery that parks its own targets today —
`convergence_sweep_target` rows in `parked`, repo-watch external obligations
with `parked_at` — must drive the session itself to core `parked` whenever the
parked thing is or wraps a session. Module-internal parks may remain only for
non-session obligations. No module state may hold a session waiting on a human
outside core `parked` (§13); otherwise the operator-queue query above is
incomplete.

## 2. Terminal outcomes

**Proposed behavior.** The closed terminal-outcome vocabulary is
`achieved_verified`, `failed_retryable{cause}`, `failed_structural{cause}`,
`failed_unknown`, `stopped{actor, sticky}`, `superseded{by}`, `abandoned`,
`retired`. A structural failure, an unknown failure, or an exhausted retry
budget on a live owned session parks the session (§1) rather than terminalizing
it, with the typed cause attached. Every parked closure — `supersede`,
`abandon`, `close_failed`, `stop` alike — first settles a suspended turn through
the committed machinery: an applied interrupt is the only cancellation
authority, an approval wait is denied before interruption as that machinery
requires, and a possibly-executed operation terminalizes
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

- `achieved_verified` — recorded only after the session's declared finish check
  passes: for repo-watch work, the external gate re-checked on the exact head
  (sessions have been declared achieved on non-converged heads); for owned work
  with no external gate, the finish condition declared at creation or
  goal-attach. Every owned session declares one — owned means driven to a
  declared terminal outcome (§6) — so an unverified achieve claim is not
  recordable. The finish check gates the goal event itself: a failing check
  commits no `achieved` event — the scheduler appends
  `blocked{execution_failure}` with the check result as its need text instead —
  so the goal stays resumable under the goal contract, which admits resume only
  from blocked. Recovery: none; slots and worktrees are released.
- `failed_retryable{cause}` — provider transient, quota, overload,
  infrastructure blip. A retryable failure on a live owned session does not
  terminalize it: the session passes through `recovering` or `blocked` (§1)
  while budgeted retries run, and `failed_retryable` is recorded only when the
  session closes with the retryable cause standing — retry budget exhausted, or
  a park closed as failed. Recovery at the point of failure: budgeted backoff;
  quota causes trigger credential-pool rotation. (Today no quota trigger is
  wired to the pool machinery: 1,552 `quota_exhausted` calls and zero
  credential-pool actions.) Retries charge the cycle budget only under §9's
  fault attribution: a provider transient, quota exhaustion, or infrastructure
  blip the session did not cause charges nothing. The configured quota action is
  recorded by the credential pool before a replacement call is prepared.
- `failed_structural{cause}` — the same input will fail again: compaction wall,
  broken toolchain, moderation block whose resume re-trips the same flag.
  Recovery: never auto-resume. The session parks with the structural cause
  attached; the preferred closure is a fresh respawn, which closes the park as
  `superseded{by}` (§9).
- `failed_unknown` — no classified cause. Recovery: park with mandatory
  diagnostic capture. The rate of `failed_unknown` across the owned-session
  population is itself an alarm (§12).
- `stopped{actor, sticky}` — a human or rule stop. Sticky: re-dispatch is
  suppressed until the dispatch source is updated (a stopped goal was
  re-dispatched minutes later because the allowlist was not pruned). Stopping
  retires queued turns legally (§10) and settles a live turn through the
  committed interrupt machinery — no standalone cancellation authority is
  minted, an issued call resolves through its durable cancellation state, and
  live ambiguity terminalizes `reconciliation_required` — with the session
  recording terminal only after the turn settles (§1); nothing is orphaned.
- `superseded{by}` — a newer session owns the work, or the goal itself is no
  longer valid. `by` is optional: it names the successor when one exists — the
  `supersede{successor}` command (§7) always names one — and is empty exactly
  when the validity recheck (§1) finds the work gone with nothing replacing it.
  Recovery: release everything; further escalations and notifications are
  forbidden.
- `abandoned` — an operator writes off a parked session. Recovery: cleanup
  obligations for worktrees, containers, and slots.
- `retired` — the session never did the work and never will: admission expiry
  (held start gate, first-input deadline, or dispatch deadline — §10), or the
  one-time closure of stranded queued-turn sessions (§14). A goal turn retired
  during goal replacement (§7) is a turn disposition only; it never retires a
  live session.

## 3. Timestamps

**Proposed behavior.** The outbox header carries
`recorded_at timestamptz NOT NULL` (on a non-reset database the constraint takes
the form this section's final rule defines). The session row carries
`created_at` and `ended_at`. Every lifecycle row — `turn_lifecycle`,
`turn_attempt`, `model_call`, `tool_attempt`, `goal_event` — is stamped at write
time. On a non-reset database the five columns follow the same migration rule as
the outbox header (this section's final rule): nullable with new-row
enforcement, historical rows backfilled only where a real time is derivable and
null with the backfill marker otherwise — never fabricated. Today none of those
five tables carries a timestamp. The only clocks near a session are command
claim times (`durable_command.claimed_at`), a handful of side journals, and
creation instants derivable from UUIDv7 identities. Turn duration, queue wait,
and every rate are therefore unanswerable from the lifecycle rows themselves.

**Proposed behavior.** The compaction call lifecycle is stamped at each step:
the command's acceptance as a durable `requested_at` —
`durable_command.claimed_at` is non-semantic operational metadata and never
stands in for it — then the call's `prepared`, `in_flight`, and `terminal`
transitions, and the application row written at apply time.

**Implemented behavior.** Watchdog state survives restarts: staleness evidence
is durable, never a process-local ledger. The committed watchdog decides
staleness by repeated observation: ordering authority stays with commit-ordered
sequences, elapsed time derives from persisted scan ordinals and the configured
scan interval, and no wall-clock comparison alone ends work. A restart costs no
staleness bound.

**Proposed behavior.** `web_usage_call_projection.recorded_at` is fixed. The
defect: today every one of the 149,773 backfilled rows carries a migration-day
stamp, not its event time. The fix: backfilled rows receive a real time only
where a terminal-time value is derivable — `recorded_at` is defined as the
terminal statement time (docs/spec/usage-evidence.md), and the UUIDv7 creation
instant is not that time: substituting it would shift usage into earlier ranges
and reorder newest-first pages — and a null with an explicit backfill marker
where no terminal time is derivable. The null requires relaxing the column's
`NOT NULL` and representability CHECK, and — an explicit contract change under
the owner's `recorded_at` ruling — a one-time amendment of the projection's
append-only guard for exactly this correction; the guard closes again behind it.
The same change migrates the usage read surface: the projection reader decodes
`recorded_at` as non-optional and paginates by a `(recorded_at, model_call_id)`
keyset (`crates/persistence/src/usage.rs`), so marked rows get an explicit
ordering and cursor rule — a null key never enters the lexicographic row-value
comparison, the marked rows order deterministically after it, and no marked row
is ever silently dropped by pagination or reaches an unmigrated reader. One rule
is shared with the new outbox `recorded_at` on a database that is not reset
(§14): rows from before a change never receive a fabricated stamp. A `NOT NULL`
column constraint cannot exempt existing rows, so on that path the outbox column
lands nullable with new-row enforcement — a CHECK constraint added `NOT VALID`,
which PostgreSQL enforces on newly inserted and updated rows only; on a reset
database the column is `NOT NULL` outright. A projection does not outlive the
conversations it projects.

## 4. Mandatory cause classification

**Proposed behavior.** Every turn that reaches `terminal` records a non-null
typed cause. The vocabulary is closed and includes `context_headroom_exhausted`
and `context_compaction_wall`. On a non-reset database the constraint binds new
terminalizations only (the §3 pattern); historical causeless turns receive the
backfill-marked `unclassified_historical` cause, which sits in the catch-all set
and never counts toward cause-completeness. Today 4,251 of 8,462 failed turns
(50.2%) carry no classified cause: 2,850 have only a bare `known_failure`
attempt disposition, 1,273 model-call failures have a null provider cause, and
128 are `lost`. 898 attempts ended `lost` with zero runner-loss records to
explain them.

**Proposed behavior.** The two guard closures that today exist only as log lines
— `reported_usage_context_compaction_exhausted` and
`reported_usage_context_still_exceeded`, the pre-activation context guard's two
walls — become durable typed causes.

**Proposed behavior.** Cause-completeness is an acceptance criterion of this
specification itself, measured as §12 defines it. Presence of a typed cause is
100% by construction under this section's mandate, so the criterion is
usability: at least 99% of terminal turns, and more than 90% of `known_failed`
model calls — the one disposition that admits a provider cause — carry a cause
outside the catch-all set. Today 66.4% of `known_failed` model calls carry no
usable cause — 2,366 `unrecognized` plus 1,273 with none at all, of 5,484.

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
clients lose nothing. The second header family, `delegation_outbox_event`,
shares one gap-free global sequence with `outbox_event`, and today's dispatcher
walks both behind the singleton delivery row. Replacing that singleton (below)
therefore cannot leave delegation delivery untouched: the delegation consumer
becomes a named consumer with its own cursor over the shared sequence, its
delivery contract preserved. Its event vocabulary and payload contract stay out
of this specification's scope.

**Proposed behavior.** The vocabulary is closed, core-owned, and versioned per
kind. A new kind lands only as a core specification change plus migration. If a
module needs an event kind core does not emit, it requests the kind as a
vocabulary change; modules never reconstruct events by joining core tables.

**Proposed behavior.** `command_settled` is the one kind that can settle without
a session — a rejected `create_session`, a command against an unknown session.
The outbox header's session column, today `NOT NULL` with a foreign key, becomes
nullable for exactly this kind.

**Proposed behavior.** Delivery uses per-consumer cursors:
`outbox_consumer_cursor (consumer_name, delivered_through, updated_at)` replaces
the singleton delivery row. The consumer set is an authoritative registry, not
whatever rows exist: the migration that introduces a consumer preregisters its
cursor row in the same transaction — as the singleton it replaces was seeded in
its own migration — and the floor is computed over the registry, so a required
consumer that has not yet read cannot be outrun. Cursors partition bookkeeping,
not ordering: each named consumer is one ordered reader of the one shared
sequence, the process-protocol dispatcher remains the single wire fan-out with
its follower contract and monotone cursor presentation untouched, and the
one-active-daemon rule stands. Rows below `min(delivered_through)` and older
than the config-sourced retention window are prunable. Pruning is a real schema
change, not a policy toggle: the outbox's append-only DELETE trigger is amended
to permit deletion below the floor, and the typed per-kind record tables that
reference the header are deleted with it. The TRUNCATE rejection stays:
PostgreSQL truncation is whole-table and cannot respect a floor, so it never
becomes legal on a live outbox table. Consumers that project durable state from
outbox rows hold cursors of their own, advanced only past rows whose derived
state is durably written, so the floor never passes an unprojected row. That is
the derived-state retention exemption. A consumer that reads raw rows on demand
— the session timeline queries the per-kind event tables directly today — cannot
be made prune-safe by a cursor: advancing it permits deleting history later
reads still need, and never advancing it pins the floor. The timeline therefore
becomes a durable projection in the same change that turns on pruning, its
cursor advancing only behind projected rows. Nor is the timeline the only such
reader — delegation reconstitution, child-result admission, and artifact-address
search all join raw headers today — so the rule is generic: before the floor
first advances, every raw reader of either header family holds a durable
projection, a named exemption, or reads bounded to the retention window, and the
implementing change enumerates them. This floor and exemption carry an owner
ruling (2026-09-01); the persistence-protocol page itself is amended by the
owning-page spec diff described below, not by this proposal. The recorded open
question on update-event retention and pruning (docs/open-questions.md) is
decided in substance by that ruling; its formal closure — the owning-page spec
diff — lands at the bottom of the implementing stack, per the foundation rule,
so no page on `main` describes unimplemented pruning as implemented. Retention
repeals only the append-only rule; the structural fix for outbox growth remains
frontier normalization, the owner's standing 2026-08-23 ruling.

## 6. Provenance and ownership

**Proposed behavior.** Session creation records a typed cause: `interactive`,
`module_dispatched{module, dispatch_ref}`, or `delegated`. Today's closed
vocabulary is `user_initiated | delegated`, and in the measured window every one
of the 5,851 sessions is `user_initiated` — including all machine-dispatched
ones. Only the soft `template_name` string separates 99.7% machine work from 19
interactive sessions. The committed imported-frontier creation family records
`interactive` with its import reference in the payload — the import is a
user-initiated act, and the vocabulary stays closed. §14 defines the backfill
mapping onto the new vocabulary.

**Proposed behavior.** Every session carries an explicit owned-or-unmonitored
bit, set at creation and flippable both ways as a journaled adopt or release
transition. Owned means the daemon holds a liveness obligation: one state, one
armed deadline, a driven path to a declared terminal outcome. Unmonitored means
a conversation: no deadlines, no watchdogs, no auto-resume, no slot held, and no
external sweep may act on it (wrong-agent stops and accidental mass-stops).
Unmonitored sessions are excluded from occupancy accounting. `release` never
interrupts a live operation: a running turn completes to its boundary under the
resources already held, and the slot releases at that boundary; the flip drops
the forward-looking obligations — deadlines, watchdogs, auto-resume —
immediately, disarming every deadline without changing state (§13). `release` on
a `parked` session is rejected: `parked` is an owned-only state (§1), so the
park is closed or resumed first.

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
input (§15). `adopt` declares the finish condition an owned session owes (§2)
when the session does not already carry one; an adopt that would leave an owned
session with no finish condition is rejected. `create_session` with owned
ownership obeys the same rule: the finish condition comes from the template or
an explicit command field, and a creation that would commit an owned session
without one is rejected before any row exists. This specification adds five
commands to that list so that every §2 outcome and every §1 transition is
reachable; additions are product decisions, surfaced here rather than shipped
silently. Every stop — goal, turn, and session level — carries the committed
`descendant_scope` member as durable intent, unchanged, and the session-level
closures record the same cascade provenance the goal and turn stops record
today, so a delegated child is never silently orphaned. The five:
`stop{actor, sticky}` at session level, because a goal-less owned session needs
a stop path; `supersede{successor}`, the closure a respawning client issues
against its predecessor; `abandon`, the operator write-off of a parked session;
`close_failed`, the operator closure of a parked session as failed with its
standing cause — `failed_retryable`, `failed_structural`, or `failed_unknown`
(§2); and `resume`, the operator or coordinator transition of a parked session
back to the state §1's mapping derives from its suspended turn's phase —
`active`, `waiting`, or `recovering`; `active` when no turn was suspended —
where no goal applies. A parked goal session resumes through
`goal{resume_with_guidance}` (§9).

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
record nothing. The validations this proposal adds to session creation are
authoritative recorded rejections: the create-session family gains a
recorded-rejection result — an explicit change from its committed
applied-results-only record, made here rather than silently.

**Proposed behavior.** `start_gate` is a core concept; the module-owned
dispatch-lease tables — `repo_watch_dispatch_start_lease` with its `_expiration`
and `_quarantine` companions — die. The scheduler's four-table reach-around dies
with them. A session created with a held start gate stays in `created` until
`release_start` or gate expiry; expiry retires it (§10). A held gate requires
owned ownership: `create_session` combining a held gate with unmonitored
ownership is rejected, because an unmonitored session carries no deadline (§6).

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
obligations. Injecting into an owned session resets no budget and no deadline.
The one exception is `resume_with_guidance`: it journals its guidance and
charges the cycle budget under §9's fault-attribution rule — a resume following
a block the session did not cause charges nothing (§9).

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

**Proposed behavior.** Every blocked goal carries a cycle budget with explicit
charging rules. Only failures the session caused charge the budget; failures
attributed to infrastructure, restarts, or provider transients do not. The
budget replenishes on successful turns. Budget magnitude is config-sourced.
Fault attribution governs every charge, §8's `resume_with_guidance` included.
This closes the defect in both directions at once. In the measured window —
before the landed goal-mode limits — exempt classes charged nothing and cycled
unchecked: one session ran 187 blocked-resumed cycles, and 43 sessions ran more
than 50. Meanwhile 5,296 of 5,566 goal sessions (95%) were never resumed at all.

**Proposed behavior.** Every resume journals its guidance and its actor, so
automatic and operator resumes are distinguishable in the durable record. All
5,590 resumes in the measured window look identical. Goal-mode code landed since
gives automatic resumes a domain-separated derived identity and journaled
guidance text. It already carries a config-sourced attempt budget whose
infrastructure retries charge nothing, and, independently, the committed
lifetime attempt ceiling that counts every attempt whatever its fault
attribution — the limit that ends a run whose every failure is exempt. Both
stay: a run ends at whichever limit it reaches first, read by the same operator
projection, exactly as committed. This section generalizes the landed charging
asymmetry to core lifecycle governance and adds the durable actor record and
success replenishment; it adds no third number and deletes neither limit.

**Proposed behavior.** Budget exhaustion transitions the goal's session from
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

## 10. Retired and dispatch deadlines

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

**Proposed behavior.** Every owned dispatched session carries a dispatch
deadline on the `dispatched` to `active` transition, config-sourced; an
unmonitored session in `dispatched` carries none (§1, §6). Expiry retires the
session with cause `dispatch_deadline_expired`, and any queued turn retires with
it in the same transaction — the machines move together (§1). This closes the
229 zombie sessions that were created, never ran a turn, and sat idle forever
with a lifespan of 0.0 hours.

**Proposed behavior.** A held `start_gate` carries its own deadline; expiry
retires the session with typed cause `start_gate_deadline_expired`.

**Proposed behavior.** An owned session in `created` with no held start gate —
an owned interactive creation — carries the same admission obligation: a
config-sourced first-input deadline armed at creation, whose expiry retires the
session with typed cause `first_input_deadline_expired`. This completes §1's
invariant for the one owned state the other two admission deadlines do not
cover. An unmonitored interactive session carries no deadline (§6).

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
payload size alongside the wall (§15). Walls are expected at §12's `wall_rate`
threshold or rarer. Their handling is §2's park-and-respawn with cause
`context_compaction_wall`; their rate is §12's `wall_rate` alarm.

## 12. Metrics and the gate

**Proposed behavior.** Five metrics are defined on durable columns, never on
proxies. They are the acceptance gate for this specification and the reliability
program behind it.

- `session_completion_failure_rate` — the headline. Cohort: sessions reaching
  `terminal` in a calendar week that were owned at any point in their life —
  membership follows the journaled ownership record (§6), so releasing a
  troubled session never removes it from the gate. Denominator: the cohort minus
  `stopped` and minus only the supersessions that closed no failure — withdrawn
  or moved work is not failure, but a `superseded{by}` that closes a park
  holding a failure cause is failure-driven and stays in both denominator and
  numerator under its standing cause; otherwise every failure recovered by
  respawn-fresh would vanish from the gate. Numerator: `failed_retryable`,
  `failed_structural`, `failed_unknown`, `abandoned`, `retired`, and
  failure-driven supersessions. Target: below 10%, then 2–5%. Today's equivalent
  is inverted: 1.8% of sessions ever achieved and 38.7% of turns failed.
- `overflow_incidence` — fraction of the full weekly terminal cohort, before the
  stopped/superseded trim, recording cause `context_headroom_exhausted` on any
  turn.
- `P(finish | overflow)` — of the sessions counted by `overflow_incidence`, the
  fraction whose terminal outcome is `achieved_verified`. The owner's
  observation this measures: a session that starts small and grows into the wall
  through real work almost always succeeds.
- `wall_rate` — fraction of sessions dispatched in the calendar week recording
  cause `context_compaction_wall`. The rate counts every wall, organic growth
  included: the owner's threshold is for walls of any kind, and the recorded
  initial payloads (§15) attribute a breach when the alarm fires. Alarm
  threshold: 0.1%, config-sourced; the owner's expected steady state is 1 in
  1,000 or rarer, possibly 1 in 10,000. A breached wall rate is a dispatch bug
  (§15), not a lifecycle statistic.
- `cause_completeness` — terminal turns whose typed cause is usable — outside
  the catch-all set: `unrecognized`, absent, or a bare unknown bucket — over all
  terminal turns (target at least 99%; bare presence is 100% by §4's mandate),
  and, for model calls, usable causes over the calls whose disposition admits a
  cause — `known_failed`, the only disposition the schema allows a provider
  cause on — never over all terminal calls, most of which complete (target above
  90%) (§4).

**Proposed behavior.** One companion alarm guards the headline's blind spot: a
session that never terminalizes never enters any weekly cohort.
`nonterminal_past_deadline` counts owned sessions whose armed deadline has
expired without its transition firing, plus owned sessions holding no armed
deadline at all — the §1 invariant violation, wired as an alarm with target
zero. A deadline explicitly configured unbounded (§1) is not counted; a missing
one is. The 281-session class this specification opens with is visible here, not
in the headline. A second companion alarm delivers §2's promised
`failed_unknown` watch: the share of `failed_unknown` in the weekly terminal
cohort, over the headline's denominator, with a config-sourced threshold — a
rising unknown rate is a classification regression even while the headline holds
(§2, §4).

**Proposed behavior.** The substrate-v0 gate requires
`session_completion_failure_rate` below the 10% target, sustained across
consecutive weekly cohorts; the number of weeks is config-sourced. The same
weeks must hold `nonterminal_past_deadline` at zero: the headline is the gate,
and the companion alarm is its integrity condition — a cohort thinned by
sessions stuck outside `terminal` passes nothing. The dispatch re-enable gate
additionally requires cause classification and cycle budgets to have landed.

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

**Implemented behavior.** Recovery bounds are sized per operation class and are
config-sourced. A single bound applied across operation classes is what produced
the 1-second-versus-10-minute backfire. Where a budget is closed today by a
schema CHECK — the five-attempt ceilings on recovery and reconciliation budgets
— the ceiling moves to a config-sourced value by an explicit schema amendment in
the implementing change, under the config-first ruling; it is never silently
exceeded and never silently kept. While a declared slow-substrate condition is
active — a running backup, a restart in progress, a detected lock convoy —
staleness bounds multiply by a config-sourced factor; the condition list and the
factor live in config. A slow substrate therefore does not read as a dead
session (eight healthy turns reaped during a 6.9 GB backup).

**Implemented behavior.** Shutdown stops watchdog decisions; the first scan
after restart applies the declared restart multiplier and startup recovery does
not fail active turns.

**Proposed behavior.** Every deadline expiry is a transition. For a session past
admission, the escalation target is `parked` — the single enumerable
human-attention state (§1). The admission deadlines are the exception: a held
`start_gate`, an owned ungated `created` session's first-input deadline, and the
dispatch deadline on `dispatched` terminalize as `retired` (§10) — before first
activity nothing live is guarded and no human attention is owed. No deadline
expiry is a silent hold, and none terminalizes work whose operation may be live.

**Proposed behavior.** No staleness machinery exists outside core. Modules and
the future substrate subscribe to deadline events; they do not grow their own
watchdogs. A new guard, bound, or fence ships only with a written check against
the recorded safety-caused incidents, surfaced to the owner. The default posture
for a tripped guard is wait, ask, or hand off to `parked` — never terminalize on
local staleness evidence alone.

## 14. Backfill and closure

**Proposed behavior.** If the unconstrained dogfood-DB reset is ratified and
lands first (a leaning, not yet ruled), this section shrinks to the closure
semantics alone and no backfill runs. Otherwise, the 5,851 existing sessions
receive states by derived backfill: state computed from the highest-position
turn joined with the last goal event — the two derivations merged. Where the two
disagree (496 sessions pair a completed last turn with a blocked goal), the goal
event governs, because the goal record is the finish signal. Every derived state
is stamped with a backfill marker. A historical goal ending `achieved` backfills
as `achieved_verified` only under that marker: the migration re-runs no finish
check — sessions have been declared achieved on non-converged heads (§2) — so
the marker means declared, not re-verified, and marked rows never enter §12's
weekly cohorts. No fabricated success reaches the headline.

**Proposed behavior.** Provenance backfills under the same conditional: sessions
with a `commissioned_dispatch` row receive
`module_dispatched{repo_watch, dispatch_ref}` — the dispatch row is the module
proof and supplies the ref. Sessions without one receive `interactive` per their
recorded user-initiated creation path, template-created included: template use
is not module proof, since template creation is itself a user-initiated command.
The window holds zero `delegated` rows. Ownership backfills as unmonitored for
every pre-existing session: the daemon assumes no retroactive liveness
obligation, so no deadline is armed on any backfilled row and §1's invariant
binds only sessions owned after cutover. A module re-adopts any dispatch it
still owns through `adopt` (§7), which arms deadlines by the normal path.

**Proposed behavior.** One-time closure under the new vocabulary: each of the 52
stranded queued-turn sessions closes in one transaction — the queued turn
terminalizes `retired` with a backfill-marked cause, and its session closes
`terminal{retired}` — §10's legal disposition at both levels, leaving no
non-terminal turn behind; the 229 zombies close as `terminal{retired}` with
cause `dispatch_deadline_expired`, backfilled. The two cohorts are disjoint by
construction — the 281 stuck sessions partition exactly into 52 with a queued
turn and 229 with no turn at all — so the closure predicates are mutually
exclusive and each session receives exactly one idempotent terminal update.

**Proposed behavior.** The dogfood DB never fences a good schema change. At
most, an agent shoehorns old rows later.

**Proposed behavior.** Lifecycle DDL lands as layered statements in the clean
per-domain schema files — sessions, goals, outbox — split by purpose, never
chronologically. Forward changes are small separate migrations that the next
collapse folds back in. A collapse is a deliberate chain reset under the owner's
rule-now-reset-later ruling: the forward-only immutability rule governs between
collapses, and each collapse is its own sanctioned one-time rewrite, exactly as
the 2026-09-01 fifteen-file baseline was.

## 15. Dispatch payload budget

**Proposed behavior.** Every dispatched session records the size of its initial
payload at creation: token count as estimated for the target model, and byte
count, stored durably on the session row. No dispatch path may skip this. An
interactive session has no payload at creation (§1); it records the same
measurements when its first input is accepted — that input is what the session
was handed.

**Proposed behavior.** Each dispatch template carries a config-sourced payload
budget. A dispatch whose payload exceeds the budget is recorded as a typed
dispatch defect attributed to the dispatching module — not as a session failure.
The budget is an alarm threshold, not an admission gate: `create_session` is not
rejected for it, and the session proceeds normally under §1 — a rejecting gate
would be new guard machinery owing §13's written safety-backfire check. The
defect record and §12's `wall_rate` carry the enforcement, and the defect record
is never silent even when no wall follows: per-template payload-budget defect
counts are queryable and alarmed alongside `wall_rate`, with a config-sourced
threshold, so an oversized dispatch that happens to finish is still surfaced to
the dispatch layer's owner. The owner's reframe binds this section: frequent
walls mean the payload was too large to begin with — judge passes and fixup
briefs handed too many comments or too much commit diff history — not organic
context growth. Payload sizing is fixed first; the wall path is the backstop.

**Proposed behavior.** `wall_rate` (§12) is wired as a dispatch-bug alarm: a
rate above §12's threshold pages the dispatch layer's owner with the offending
templates and their recorded payload sizes. Sessions that start around 10% full
and hit the wall through real work almost always succeed; they count in the rate
(§12), and the recorded initial payloads are what separate dispatch-defect walls
from those rare organic ones when the alarm fires — the page's subject is the
oversized dispatches.

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

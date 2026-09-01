# Session lifecycle

**Proposed for owner decision; design only.** Nothing on this page is
implemented. Every paragraph is marked **Proposed behavior** and describes what
the daemon must do once this specification is ratified. Citations point at the
written record: the 2026-09-01 live-DB census (census §N), the failure taxonomy
of 750 deduped incidents in 47 classes (taxonomy §N or class letter), the
ownership decision brief (brief §N), the seam study (study B §N), the compaction
study (study C §N), and the owner-intent addendum (intent item N). Owner rulings
cited here are binding and are not re-argued.

This specification changes when capabilities land, not which capabilities exist:
reliability work lands first, and no wanted feature is removed (intent item 1).
Goal mode, custom compaction, and the API adapters all stay: base behavior
first, complexity iterated back on afterward behind evals.

## Why

The dogfood database holds 5,851 sessions, 21,851 turns, and 116,429 model calls
over 2026-08-10 to 2026-08-26; 99.7% of the sessions are template-dispatched
repo-watch work (census §1). Of those sessions, 105 (1.8%) ever reached
`achieved`; 3,598 (61.5%) end on a blocked goal never resumed; 38.7% of all
turns end `failed` (census §2, §3). 281 sessions (4.8%) never reached any
terminal state, every one already idle more than 72 hours while the daemon was
still running (census §2). Half of the failed turns — 4,251 of 8,462 — carry no
classified cause (census §3d). There is no session state column and no timestamp
on any lifecycle row, so none of this was visible without hand-written SQL
(census §6.1, §6.2). The owner's target: less than 10% of dispatched sessions
failing to reach their finish point, then 2–5% (intent item 2).

Three rules apply to every section below.

**Proposed behavior.** Lifecycle state, deadlines, budgets, and recovery live in
daemon core. No module implements or re-implements any of them (intent item 8;
brief §4.1). If a module needs lifecycle behavior that core does not provide, it
requests a core change.

**Proposed behavior.** Every numeric bound in this specification — cycle
budgets, dispatch deadlines, watchdog bounds, payload budgets, retry backoffs —
is defined in config or the database, never hardcoded (intent item 17). Values
named in this text are example defaults. Config may set any such bound to none,
meaning unbounded. The only hardcoded limits permitted are guards against
algorithmic explosion — unbounded loops, unbounded recursion, unbounded queue
growth (intent item 17). A new lifecycle limit is a product decision; it is
surfaced to the owner before it ships (intent item 18).

**Proposed behavior.** This specification never says "fleet." It names the
population it means: the owned-session population, the dispatched sessions of
one module, or all sessions the daemon holds (intent item 25).

## 1. Session state machine

**Proposed behavior.** Every session is in exactly one of eight states, stored
as a durable core-owned column: `created`, `dispatched`, `active`, `waiting`,
`recovering`, `blocked`, `parked`, `terminal` (taxonomy §4.1). Today no durable
session state exists (census §6.1); the nearest thing is the web queue's derived
`AttentionState` classifier. Once the column lands, the classifier becomes a
projection of these states plus turn phase — never an independent machine.

```
CREATED ─┬─→ DISPATCHED ─→ ACTIVE
         └─→ TERMINAL{retired}                     (held start-gate expiry, §10)
DISPATCHED ─→ TERMINAL{retired}                    (dispatch-deadline expiry, §10)
ACTIVE ─┬─→ WAITING{kind, deadline, waker}   → ACTIVE | PARKED (deadline expiry)
        ├─→ RECOVERING{op, bound}            → ACTIVE | PARKED
        ├─→ BLOCKED{reason, cycle}           → ACTIVE | PARKED
        └─→ TERMINAL{outcome}
PARKED{cause, owner, since} → ACTIVE (operator or coordinator action)
                            | TERMINAL{abandoned | failed_structural | failed_unknown | superseded}
any non-terminal state → TERMINAL{superseded | stopped}
```

This diagram extends taxonomy §4.1's with the `retired` expiries §10 defines,
the WAITING deadline escalation §13 requires, and the park closures §2 defines.
A module-dispatched session enters `dispatched`; an interactive session passes
from `created` directly to `active` on its first accepted input.

**Proposed behavior.** `created` and `dispatched` are distinct from `active`
because 229 sessions died between creation and first turn and 52 died with a
queued turn that never activated (census §2). A dispatched session that does not
reach `active` within its dispatch deadline has failed and is retired (§10); it
is never treated as at rest.

**Proposed behavior.** `waiting` carries a typed kind, a deadline, and a
designated waker. The closed kind vocabulary is `approval{decider}`,
`external{gate, recheck}`, `child{session}`, `provider_retry{backoff}`,
`pipeline{backlog}`, `scheduler{fault}` (taxonomy §4.1). Deadline expiry is a
transition — escalate to `parked` — never a silent hold. This closes the 110
stuck approvals (taxonomy A4) and the 10 external-gate stalls (taxonomy D2): an
expired approval escalates and survives turn boundaries instead of failing the
turn (taxonomy A5, 17 headless-escalation kills).

**Proposed behavior.** `active` carries two obligations beyond a stall deadline.
First, a progress budget of model calls and wall-clock time: the budget
decrements per model call and per elapsed interval, and resets when progress is
recorded — a turn completing, or a goal event advancing the goal. Second, a
goal-validity recheck at a config-sourced interval, transitioning to
`terminal{superseded}` when the work is gone. Runaways were loud, not silent:
200+ model calls against an already-merged branch, one four-day session
(taxonomy D4).

**Proposed behavior.** Invariant for owned sessions: every non-terminal state of
an owned session carries exactly one armed deadline whose expiry is a defined
transition (brief §4.1). "No armed deadline on a non-terminal owned session" is
a detectable invariant violation.

**Proposed behavior.** `parked` is the single state in which an owned session
may wait on a human. It carries a machine-readable cause and an owner. The
operator queue is exactly `SELECT * FROM session WHERE state = 'parked'` — the
query that could not be written for the 281 stuck sessions (census §2; taxonomy
§4.1).

**Proposed behavior.** The turn machine (`queued / active / terminal`, with the
`awaiting_*` phases) persists unchanged beneath the session machine, and the
session state column is authoritative: core updates it in the same transaction
as every turn or goal transition that changes the mapping, so the two machines
never disagree. The mapping: a turn in `running` ⇒ `active`;
`awaiting_tool_approval` ⇒ `waiting{approval}`; `awaiting_child` ⇒
`waiting{child}`; `awaiting_model_call_recovery`, `awaiting_tool_recovery`, or
`awaiting_runner_recovery` ⇒ `recovering{op}`; a `blocked` goal with no live
turn ⇒ `blocked`.

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
`retired` (taxonomy §4.2; brief §4.2). A structural or unknown failure detected
on a live owned session parks it (§1) rather than terminalizing it, with the
typed cause attached. The park then closes with the outcome that matches its
resolution: `superseded{by}` when a fresh respawn takes the work; `abandoned` on
operator write-off; `failed_structural{cause}` or `failed_unknown` when it
closes as failed with the cause standing. Each outcome carries its warranted
recovery as normative behavior:

- `achieved_verified` — recorded only after the session's declared finish check
  passes: for repo-watch work, the external gate re-checked on the exact head
  (taxonomy D3: sessions declared achieved on non-converged heads); for owned
  work with no external gate, the finish condition declared at creation or
  goal-attach. Every owned session declares one — owned means driven to a
  declared terminal outcome (§6) — so an unverified achieve claim is not
  recordable. Recovery: none; slots and worktrees are released.
- `failed_retryable{cause}` — provider transient, quota, overload,
  infrastructure blip. Recovery: budgeted backoff; quota causes trigger
  credential-pool rotation. (Today no quota trigger is wired to the pool
  machinery: 1,552 `quota_exhausted` calls and zero credential-pool actions —
  census §3a; taxonomy G1.) Each retry charges the cycle budget (§9).
- `failed_structural{cause}` — the same input will fail again: compaction wall,
  broken toolchain, moderation block whose resume re-trips the same flag
  (taxonomy G8, C4). Recovery: never auto-resume. The session parks with the
  structural cause attached; the preferred closure is a fresh respawn, which
  closes the park as `superseded{by}` (§9).
- `failed_unknown` — no classified cause. Recovery: park with mandatory
  diagnostic capture. The rate of `failed_unknown` across the owned-session
  population is itself an alarm (§12).
- `stopped{actor, sticky}` — a human or rule stop. Sticky: re-dispatch is
  suppressed until the dispatch source is updated (taxonomy D1b: a stopped goal
  was re-dispatched minutes later because the allowlist was not pruned).
  Stopping retires queued turns legally (§10).
- `superseded{by}` — a newer session owns the work, or the goal itself is no
  longer valid. Recovery: release everything; further escalations and
  notifications are forbidden.
- `abandoned` — an operator writes off a parked session. Recovery: cleanup
  obligations for worktrees, containers, and slots (taxonomy G5, G7 debris).
- `retired` — the session never did the work and never will: dispatch deadline
  expiry, held start gate expiry, or goal-turn retirement (§10).

## 3. Timestamps

**Proposed behavior.** The outbox header carries
`recorded_at timestamptz NOT NULL` (study B §2.1). The session row carries
`created_at` and `ended_at`. Every lifecycle row — `turn_lifecycle`,
`turn_attempt`, `model_call`, `tool_attempt`, `goal_event` — is stamped at write
time. Today none of those five tables carries a timestamp. The only clocks near
a session are command claim times (`durable_command.claimed_at`), a handful of
side journals, and creation instants derivable from UUIDv7 identities. Turn
duration, queue wait, and every rate are therefore unanswerable from the
lifecycle rows themselves (census §6.2).

**Proposed behavior.** The compaction call lifecycle is stamped at each step:
the call's `prepared`, `in_flight`, and `terminal` transitions, and the
application row written at apply time (brief §4.3).

**Proposed behavior.** Watchdog clocks read durable timestamps, never
process-local ledgers. A daemon restart today resets every staleness clock
exactly when restarts are the leading creator of stuck sessions (taxonomy §3;
B1).

**Proposed behavior.** `web_usage_call_projection.recorded_at` is fixed: all
149,773 backfilled rows carry migration-day stamps, not event times (census §0;
intent item 22). Backfilled rows receive their real times where derivable — the
UUIDv7 identity carries the creation instant — and a null with an explicit
backfill marker where not. The null requires relaxing the column's `NOT NULL`
and representability CHECK. The same rule governs the new outbox `recorded_at`
on a database that is not reset (§14): rows from before the change never receive
a fabricated stamp — the constraint binds new rows only. A projection does not
outlive the conversations it projects (intent item 22).

## 4. Mandatory cause classification

**Proposed behavior.** Every turn that reaches `terminal` records a non-null
typed cause. The vocabulary is closed and includes `context_headroom_exhausted`
and `context_compaction_wall` (brief §4.4). Today 4,251 of 8,462 failed turns
(50.2%) carry no classified cause: 2,850 have only a bare `known_failure`
attempt disposition, 1,273 model-call failures have a null provider cause, and
128 are `lost` (census §3d). 898 attempts ended `lost` with zero runner-loss
records to explain them (census §6.4).

**Proposed behavior.** The two guard closures that today exist only as log lines
— `reported_usage_context_compaction_exhausted` and
`reported_usage_context_still_exceeded`, the pre-activation context guard's two
walls — become durable typed causes (brief §4.4; study C §3.2).

**Proposed behavior.** Cause-completeness is an acceptance criterion of this
specification itself, measured as §12 defines it. Presence of a typed cause is
100% by construction under this section's mandate, so the criterion is
usability: at least 99% of terminal turns and more than 90% of terminal model
calls carry a cause outside the catch-all set (brief §4.4's ~100%/>90%, made
testable). Today 66.4% of `known_failed` model calls carry no usable cause —
2,366 `unrecognized` plus 1,273 with none at all, of 5,484 (census §3a).

## 5. Lifecycle event vocabulary

**Proposed behavior.** The module-facing lifecycle vocabulary is exactly eight
event kinds, each with a typed payload, carried on the existing transactional
outbox: `session_created`, `session_state_changed`, `session_terminal`,
`turn_terminal`, `goal_changed`, `command_settled`, `injection_settled`,
`session_ownership_changed` (study B §2.2, verbatim; brief §4.5). Every event
carries `recorded_at` from the header, and a session reference where one exists.

**Proposed behavior.** Each of the sixteen existing outbox kinds gets an
explicit disposition; the vocabulary is never doubled. `session_created` evolves
in place: a new `storage_version` carries the typed provenance payload.
`turn_terminal` replaces the five per-disposition turn kinds (`turn_completed`,
`turn_failed`, `turn_refused`, `turn_cancelled`, `turn_reconciliation_required`)
and subsumes `goal_turn_retired` as `turn_terminal{disposition: retired}`. The
consumers that decode the old kinds — the session-timeline projection, the
operator-attention triggers, the process-protocol decoders — migrate in the same
change. The remaining kinds (`model_call_transition`, `tool_batch_transition`,
`tool_approval_decided`, `input_accepted`, `turn_activated`,
`turn_model_settings_resolved`, `session_model_settings_changed`,
`context_compacted`, `runner_state_transition`) stay as core-internal events:
still on the outbox, not part of the module-facing vocabulary, unavailable
across the seam (study B §2.2: deliberately not in v1). The second outbox,
`delegation_outbox_event`, is outside this specification's scope and keeps its
current contract; folding it under consumer cursors is a separate decision.

**Proposed behavior.** The vocabulary is closed, core-owned, and versioned per
kind. A new kind lands only as a core specification change plus migration. If a
module needs an event kind core does not emit, it requests the kind as a
vocabulary change; modules never reconstruct events by joining core tables
(study B §2.2).

**Proposed behavior.** `command_settled` is the one kind that can settle without
a session — a rejected `create_session`, a command against an unknown session.
The outbox header's session column, today `NOT NULL` with a foreign key, becomes
nullable for exactly this kind.

**Proposed behavior.** Delivery uses per-consumer cursors:
`outbox_consumer_cursor (consumer_name, delivered_through, updated_at)` replaces
the singleton delivery row (study B §2.1). Rows below `min(delivered_through)`
and older than the config-sourced retention window are prunable. Pruning is a
real schema change, not a policy toggle: the outbox's append-only DELETE and
TRUNCATE triggers are amended to permit deletion below the floor, and the typed
per-kind record tables that reference the header are deleted with it. Consumers
that project durable state from outbox rows — the session timeline reads
`outbox_event` directly — hold cursors of their own, so the floor never passes
an unprojected row. That is the derived-state retention exemption. This floor
and exemption are the ratified persistence-protocol amendment (brief §3 fork 1).
Retention repeals only the append-only rule; the structural fix for outbox
growth remains frontier normalization, the owner's standing 2026-08-23 ruling
(intent item 19; brief §1).

## 6. Provenance and ownership

**Proposed behavior.** Session creation records a typed cause: `interactive`,
`module_dispatched{module, dispatch_ref}`, or `delegated` (brief §4.6). Today's
closed vocabulary is `user_initiated | delegated`, and in the census window
every one of the 5,851 sessions is `user_initiated` — including all
machine-dispatched ones. Only the soft `template_name` string separates 99.7%
machine work from 19 interactive sessions (census §6.7, §1). §14 defines the
backfill mapping onto the new vocabulary.

**Proposed behavior.** Every session carries an explicit owned-or-unmonitored
bit, set at creation and flippable both ways as a journaled adopt or release
transition (intent item 9; taxonomy §4.3). Owned means the daemon holds a
liveness obligation: one state, one armed deadline, a driven path to a declared
terminal outcome. Unmonitored means a conversation: no deadlines, no watchdogs,
no auto-resume, no slot held, and no external sweep may act on it (taxonomy I2:
wrong-agent stops and accidental mass-stops). Unmonitored sessions are excluded
from occupancy accounting.

**Proposed behavior.** Every command and every state transition records its
actor from the closed vocabulary `core`, `operator`, `module{name}`, `watchdog`
(brief §4.6). This extends the existing domain `Actor` (`User`, `Model{turn}`,
`Recovery`, `Tool{request}`) rather than paralleling it: `module{name}` and
`watchdog` are added, `User` surfaces as `operator`, the recovery scan as
`watchdog`, and model- and tool-initiated agency surfaces as `core` with its
turn- and request-precise attribution kept in the payload. Today manual operator
workarounds — the crutch layer — are indistinguishable from daemon actions in
the record (taxonomy §5).

## 7. Command surface

**Proposed behavior.** The core command surface for session lifecycle is:
`create_session{template, provenance, start_gate, ownership}`, `release_start`,
`submit_input`, `goal{attach | resume_with_guidance | stop{sticky}}`, `adopt`,
`release` (study B §3; brief §4.7). This specification adds three commands to
that list so that every §2 outcome is reachable; additions are product
decisions, surfaced here rather than shipped silently (intent item 18). The
three: `stop{actor, sticky}` at session level, because a goal-less owned session
needs a stop path; `supersede{successor}`, the closure a respawning client
issues against its predecessor; and `abandon`, the operator write-off of a
parked session.

**Proposed behavior.** The existing goal-command operation named `supersede` —
new goal generation within the same session — is unrelated to the session
outcome `superseded{by}` and keeps its machinery unchanged. This specification
calls it goal replacement and never uses `supersede` for it.

**Proposed behavior.** Core mints all identities. No module pre-allocates turn,
input, or frontier identities inside its own transaction (study B §1: today's
dispatch pre-mints four core identities — a turn, an accepted input, a
cancellation entry, and a cancellation frontier).

**Proposed behavior.** Every command settles asynchronously as a
`command_settled` receipt carrying applied-or-rejected with a closed rejection
kind (§5).

**Proposed behavior.** `start_gate` is a core concept; the module-owned
dispatch-lease tables — `repo_watch_dispatch_start_lease` with its `_expiration`
and `_quarantine` companions — die. The scheduler's four-table reach-around dies
with them (brief §3 fork 2; study B §1). A session created with a held start
gate stays in `created` until `release_start` or gate expiry; expiry retires it
(§10).

**Proposed behavior.** Ownership is advisory: an owner module observes events
and issues commands like any other client; it never sits between core and the
session (brief §4.7).

## 8. Injection contract

**Proposed behavior.** Message injection — operator text, coordinator guidance,
steering — is legal in every non-terminal state, regardless of ownership: the
owner can send a message at any point (intent item 10).

**Proposed behavior.** An injection into any non-terminal state queues durably
and is delivered at the next legal boundary. It is never rejected for state and
never lost. The injector receives a durable `injection_settled` receipt
(taxonomy §4.4). The enumerated violations this repairs: injections rejected
while awaiting approval, rejected while awaiting recovery, silently dropped on
resume, and swallowed by the composer (taxonomy §4.4).

**Proposed behavior.** Pending injections never block terminalization.
Terminalization closes pending steering with a `not_delivered` receipt; pending
steering never refuses terminalization (taxonomy §4.4).

**Proposed behavior.** Injecting into an unmonitored session creates no
obligations. Injecting into an owned session resets no budget and no deadline.
The one exception is `resume_with_guidance`: it journals its guidance and
charges the cycle budget under §9's fault-attribution rule — a resume following
a block the session did not cause charges nothing (§9; intent item 18).

**Proposed behavior.** Approval decisions are injections: durable, surviving
turn boundaries, socket loss, and drains (taxonomy A4: 110 stuck approvals; A6:
decision-channel failures, including drains removing the socket mid-decision).

**Proposed behavior.** The web UI exposes send-message on every live session
(intent item 10).

## 9. Goal cycle governance

**Proposed behavior.** Every blocked goal carries a cycle budget with explicit
charging rules. Only failures the session caused charge the budget; failures
attributed to infrastructure, restarts, or provider transients do not (intent
item 18). The budget replenishes on successful turns (intent item 18). Budget
magnitude is config-sourced (intent item 17). Fault attribution governs every
charge, §8's `resume_with_guidance` included. This closes the defect in both
directions at once. Today exempt classes charge nothing and cycle forever: one
session ran 187 blocked-resumed cycles, and 43 sessions ran more than 50.
Meanwhile 5,296 of 5,566 goal sessions (95%) were never resumed at all (census
§3e; taxonomy C3, D1).

**Proposed behavior.** Every resume journals its guidance and its actor, so
automatic and operator resumes are distinguishable in the durable record. All
5,590 census-window resumes look identical (census §6.5). Goal-mode code landed
since gives automatic resumes a domain-separated derived identity and journaled
guidance text. It already carries a config-sourced attempt budget whose
infrastructure retries charge nothing. This section generalizes that landed
asymmetry to core lifecycle governance and adds the durable actor record and
success replenishment (intent item 18).

**Proposed behavior.** Budget exhaustion transitions the goal's session from
`blocked` to `parked`, where the owner sees it. Exhaustion is never a silent
stop (taxonomy C3: budget exhaustion today is an invisible park).

**Proposed behavior.** A fresh respawn is preferred over resuming a structurally
failed session; the respawn closes its predecessor's park as `superseded{by}`
(§2) (brief §4.9; taxonomy D1b: a fresh session converged in 10 minutes where
the resumed one had looped).

**Proposed behavior.** Goal mode is reserved for long-horizon work — hours or
days. Routine dispatch uses plain sessions with vendor compaction (intent items
3, 4). Goal mode is never stripped. Its compaction contract: a goal session that
compacts re-reads its goal after compaction and carries its working state
forward — testable because the next turn's context contains the goal statement
and the carried state (intent item 4). This extends the base case, a plain
session that can compact and finish (intent item 3), and is improved later
behind evals.

## 10. Retired and dispatch deadlines

**Proposed behavior.** `retired` is a legal terminal disposition for goal turns.
Today `goal_turn_retired` is published but the turn vocabulary cannot express
it. All 52 non-terminal turns in the database are exactly this shape, and every
published retirement is such a turn — the match holds in both directions (census
§2, §6.6). Adding `retired` closes all 52.

**Proposed behavior.** Every dispatched session carries a dispatch deadline on
the `dispatched` to `active` transition, config-sourced. Expiry retires the
session with cause `dispatch_deadline_expired`. This closes the 229 zombie
sessions that were created, never ran a turn, and sat idle forever with a
lifespan of 0.0 hours (census §2).

**Proposed behavior.** A held `start_gate` carries its own deadline; expiry
retires the session (brief §4.10).

## 11. Compaction observability

**Proposed behavior.** Every successor turn created after a compaction records
`preceding_compaction_id`, linking it to the compaction that made room for it
(brief §4.11). Today 2,465 compactions exist with no such linkage (census §3f).

**Proposed behavior.** The compaction funnel is fully queryable from durable
state: requested, prepared, applied, failed, each stamped (§3), with input size
and fit result on the failure path. Today the only durable compaction-failure
trace is 23 `goal_execution_failure_recovery` rows, all with the single cause
`context_compaction_input_does_not_fit` (census §3f).

**Proposed behavior.** Every compaction records whether it was vendor or custom,
and on which adapter. The owner ruling covers all four adapters — codex_cli,
claude_code CLI, Anthropic API, OpenAI API — codex first (intent item 5; brief
§1 row 2). This is the eval scaffold the compaction ruling requires.

**Proposed behavior.** Vendor compaction is the default path. The home-rolled
compactor stays as the eval baseline — never deleted, re-added per adapter once
the eval system can reliably measure it (intent item 3). The re-enable gate
itself is a product decision surfaced to the owner at that point, not shipped
silently (intent item 18).

**Proposed behavior.** A compaction-wall event records the session's initial
payload size alongside the wall (§15). Walls are expected at §12's `wall_rate`
threshold or rarer. Their handling is §2's park-and-respawn with cause
`context_compaction_wall`; their rate is §12's `wall_rate` alarm.

## 12. Metrics and the gate

**Proposed behavior.** Five metrics are defined on durable columns, never on
proxies (brief §4.12). They are the acceptance gate for this specification and
the reliability program behind it.

- `session_completion_failure_rate` — the headline. Cohort: owned sessions
  reaching `terminal` in a calendar week. Denominator: the cohort minus
  `stopped` and `superseded` (withdrawn or moved work is not failure).
  Numerator: `failed_retryable`, `failed_structural`, `failed_unknown`,
  `abandoned`, and `retired`. Target: below 10%, then 2–5% (intent item 2).
  Today's equivalent is inverted: 1.8% of sessions ever achieved and 38.7% of
  turns failed (census §2).
- `overflow_incidence` — fraction of the full weekly terminal cohort, before the
  stopped/superseded trim, recording cause `context_headroom_exhausted` on any
  turn.
- `P(finish | overflow)` — of the sessions counted by `overflow_incidence`, the
  fraction whose terminal outcome is `achieved_verified`. The owner's
  observation this measures: a session that starts small and grows into the wall
  through real work almost always succeeds (intent item 13).
- `wall_rate` — fraction of sessions dispatched in the calendar week recording
  cause `context_compaction_wall`. Alarm threshold: 0.1%, config-sourced; the
  owner's expected steady state is 1 in 1,000 or rarer, possibly 1 in 10,000
  (intent item 13). A breached wall rate is a dispatch bug (§15), not a
  lifecycle statistic.
- `cause_completeness` — terminal turns whose typed cause is usable — outside
  the catch-all set: `unrecognized`, absent, or a bare unknown bucket — over all
  terminal turns (target at least 99%; bare presence is 100% by §4's mandate),
  and terminal model calls with a usable cause over all terminal model calls
  (target above 90%) (§4).

**Proposed behavior.** One companion alarm guards the headline's blind spot: a
session that never terminalizes never enters any weekly cohort.
`nonterminal_past_deadline` counts owned sessions whose armed deadline has
expired without its transition firing, plus owned sessions holding no armed
deadline at all — the §1 invariant violation, wired as an alarm with target
zero. The 281-session class this specification opens with is visible here, not
in the headline.

**Proposed behavior.** The substrate-v0 gate requires
`session_completion_failure_rate` below the intent-item-2 target of 10%,
sustained across consecutive weekly cohorts; the number of weeks is
config-sourced (brief §3 fork 5). The dispatch re-enable gate additionally
requires cause classification and cycle budgets to have landed (brief §3 fork
6).

## 13. Watchdog and recovery posture

**Proposed behavior.** Recovery preserves rather than terminalizes whenever the
guarded operation may still be live (brief §4.13). The measured record demands
it: 100 of 777 incident reports (12.9%) are root-caused safety-backfire. The
window's worst population-wide outages were caused by protection machinery: a
1-second reconciliation bound terminalizing live 10-minute model calls, and a
restart chain parking all 24 commissioned sessions at once (taxonomy §2; intent
item 16).

**Proposed behavior.** Recovery bounds are sized per operation class and are
config-sourced (intent item 17). A single bound applied across operation classes
is what produced the 1-second-versus-10-minute backfire (taxonomy C2). While a
declared slow-substrate condition is active — a running backup, a restart in
progress, a detected lock convoy — staleness bounds multiply by a config-sourced
factor; the condition list and the factor live in config. A slow substrate
therefore does not read as a dead session (taxonomy §2: eight healthy turns
reaped during a 6.9 GB backup).

**Proposed behavior.** Every deadline expiry is a transition, and the escalation
target is `parked` — the single enumerable human-attention state (§1). No
deadline expiry is a silent hold, and none terminalizes work whose operation may
be live.

**Proposed behavior.** No staleness machinery exists outside core. Modules and
the future substrate subscribe to deadline events; they do not grow their own
watchdogs (brief §4.13). A new guard, bound, or fence ships only with a written
check against taxonomy §2's safety-caused incident record, surfaced to the owner
(intent items 16, 18). The default posture for a tripped guard is wait, ask, or
hand off to `parked` — never terminalize on local staleness evidence alone
(taxonomy §2).

## 14. Backfill and closure

**Proposed behavior.** If the unconstrained dogfood-DB reset is ratified and
lands first (a leaning, not yet ruled — intent item 20; brief §1), this section
shrinks to the closure semantics alone and no backfill runs. Otherwise, the
5,851 existing sessions receive states by derived backfill: state computed from
the highest-position turn joined with the last goal event — the census's two §2
derivations, merged. Where the two disagree (496 sessions pair a completed last
turn with a blocked goal), the goal event governs, because the goal record is
the finish signal (census §2). Every derived state is stamped with a backfill
marker.

**Proposed behavior.** Provenance backfills under the same conditional: sessions
with a template or a `commissioned_dispatch` row receive
`module_dispatched{repo_watch, dispatch_ref}`; the 19 template-less sessions
receive `interactive`; the census window holds zero `delegated` rows (census §1,
§6.7).

**Proposed behavior.** One-time closure under the new vocabulary: the 52
stranded queued-turn sessions close as `terminal{retired}` via §10's legal
disposition; the 229 zombies close as `terminal{retired}` with cause
`dispatch_deadline_expired`, backfilled (census §2; brief §4.14).

**Proposed behavior.** The dogfood DB never fences a good schema change (intent
item 21). At most, an agent shoehorns old rows later.

**Proposed behavior.** Lifecycle DDL lands as layered statements in the clean
per-domain schema files — sessions, goals, outbox — split by purpose, never
chronologically. Forward changes are small separate migrations that the next
collapse folds back in (intent item 23).

## 15. Dispatch payload budget

**Proposed behavior.** Every session records the size of its initial payload at
creation: token count as estimated for the target model, and byte count, stored
durably on the session row (brief §3 fork 3; intent item 13). No dispatch path
may skip this.

**Proposed behavior.** Each dispatch template carries a config-sourced payload
budget (intent item 17). A dispatch whose payload exceeds the budget is recorded
as a typed dispatch defect attributed to the dispatching module — not as a
session failure. The owner's reframe binds this section: frequent walls mean the
payload was too large to begin with — judge passes and fixup briefs handed too
many comments or too much commit diff history — not organic context growth
(intent item 13). Payload sizing is fixed first; the wall path is the backstop.

**Proposed behavior.** `wall_rate` (§12) is wired as a dispatch-bug alarm: a
rate above §12's threshold pages the dispatch layer's owner with the offending
templates and their recorded payload sizes. Sessions that start around 10% full
and hit the wall through real work almost always succeed and are not the alarm's
subject (intent item 13).

**Proposed behavior.** When a wall still occurs, the session parks with cause
`context_compaction_wall`. The backstop is respawn-fresh, which closes the park
as `superseded{by}`; a park closed as failed records
`failed_structural{context_compaction_wall}` (§2, §9; brief §3 fork 3).
Auto-resume into the same wall is forbidden. The codex goal path already
suppresses automatic resumption on its one wall cause; this rule generalizes
that suppression to every structural cause. The recorded failure shape: 26
block-resume cycles against one wall with zero progress (taxonomy C4, 21
occurrences).

**Proposed behavior.** The web UI shows what a session was handed: the session
view renders the recorded initial payload — size, source, and content. The
sessions list shows each session's state and payload size at a glance (intent
items 13, 14). The existing dashboard could not show the owner that nearly all
dispatched sessions were dead (intent item 14). The sessions list, session-text
reading, and dispatch-payload display are the requirements the first web slice
is defined against (intent item 14).

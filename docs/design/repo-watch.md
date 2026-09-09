# Repository watch design

This committed unbuilt design extends [repository watch](../spec/repo-watch.md)
with programs on the [workflows host](workflows.md).

## Goal

Run repository watch through workflows while preserving accepted events,
dispatch behavior and recovery.

## Design

Path A is committed: the module store remains authoritative, workflows own
orchestration, and one temporary `repository_watch.workflows_enabled` flag
selects the engine during cutover.

### Boundary

Compiled Rust programs and checked effect adapters live in
`apps/signalboxd/src/workflows/repo_watch/`. `ObserveRepository` accepts one
poll/webhook observation unit; `EvaluateRule` evaluates one retained event for
one checked rule revision; `ReactToLifecycle` applies one retained lifecycle
unit; `CleanupCheckout` invokes `repo.cleanupCheckout` for one retained removal
candidate. Runs are finite, with immutable input and journaled results;
subsequent work resumes from module cursors and receipts, including after a
native run ends `ContractRetired` under the host's executable-pinning contract.

Programs sequence checked operations and pure matcher/reaction planning through
`WorkflowContext`. Adapters own I/O, identities, time, template resolution and
command encoding; no database, provider, credential or filesystem handle enters
program code. The module retains its pure reducers and SQL under the existing
ownership seam (`docs/spec/repo-watch.md:272`); it acquires no dependency on the
workflow runtime or core persistence.

### Effects and receipts

One `RepoWatch` capability admits the following methods, with checked request
and result codecs. The host journals requests before effects and deliveries
before resuming programs. Each mutating request carries a stable effect identity
independent of its run and request ordinal. The adapter binds that effect
identity, method and exact input to its module receipt; equal recovery adopts
the retained result and changed input under the same effect identity conflicts.
Run/request identity identifies the journal delivery, not the receipt. Recovery
checks receipts before active configuration. An externally committed operation
without sufficient receipt evidence returns ambiguity; multi-step submission and
checkout are not declared idempotent as a whole.

| Method                                      | Operation and recovery contract                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| ------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `repo.observe`                              | Fetch, normalize, diff and atomically commit one complete observation. Add effect identity and exact frontier/event-range result to the retained frontier commit, including an unchanged observation. Serialize observations per repository and adopt a committed result into the journal before another observation can replace its receipt. Generation and candidate digest alone are not effect receipts (`docs/spec/repo-watch.md:104`).                                                                                                                                             |
| `repo.nextRuleEvent`                        | Return the active checked revision, next retained event and checked matcher/singleton context as journaled bytes. This read reserves no dispatch; activation-tail and cursor selection remain module-owned (`crates/modules/repo-watch-v2/src/dispatch.rs:65`).                                                                                                                                                                                                                                                                                                                          |
| `repo.commitEvaluation`                     | Revalidate the exact event/revision and proposed pure action plan against module authority. Mint identities and freeze complete resolved commands host-side; acquire the existing singleton and commit commands, outcome receipt and evaluation cursor together. Retain the exact nonmatch, suppression or dispatch result on evaluation/dispatch records until journal adoption, without recomputing from changed configuration. Existing command retention and singleton admission remain authoritative (`docs/spec/repo-watch.md:184`, `docs/spec/repo-watch.md:198`).                |
| `repo.submitPending`                        | Submit the retained command identity and exact bytes through core handlers, adopt core command receipts, and complete retained follow-ups before reporting completion. Pull-request creation provisions its checkout inside this effect; there is no separate provisioning effect. The adapter adopts or resumes from ledger location, core session, ownership, provisioned SHA and retirement evidence. Preserve ordered actions, settlement and synchronous conflict rejection (`crates/modules/repo-watch-v2/src/dispatch.rs:304`, `apps/signalboxd/src/repo_watch_dispatch.rs:160`). |
| `repo.cleanupCheckout`                      | Remove checkouts through the existing adapter using ledger location, core session, ownership and removal evidence. Cleanup runs independently of submission and active configuration; recovery adopts or resumes only from retained location and ownership evidence (`docs/spec/repo-watch.md:230`).                                                                                                                                                                                                                                                                                     |
| `repo.nextLifecycle`, `repo.commitReaction` | Journal retained core/seam or close/merge facts and original dispatch context; revalidate terminal state and commit exact release/stop commands and settlement before advancing the module cursor, then acknowledge the seam source. Retain the outcome for lost-answer adoption, including no reaction (`crates/modules/repo-watch-v2/src/dispatch.rs:149`, `apps/signalboxd/src/repo_watch_runtime.rs:716`).                                                                                                                                                                           |

Receipt additions belong to the existing module frontier, evaluation and
dispatch records; there is no second dispatch ledger or singleton table. The
launcher checks receipts awaiting journal adoption before selecting work from
cursors, even when the committing run is `ContractRetired` and its cursor has
advanced. It passes the retained effect identity and exact input to a successor
run. The adapter returns the retained result without repeating the committed
effect or resolving new configuration, and the host journals that result under
the successor's request ordinal. Durable delivery in either run permits receipt
release or replacement, including the next observation; no acknowledgement from
the retired run or rewrite of its journal is required. Read-only context can be
reread before a durable side effect; a replayed delivery uses its recorded
bytes. Host event waits use durable repository/core source positions.

### Retained daemon authority

The retained daemon module owns the launcher for finite runs. Poll ticks and
applied primary webhook receipts launch `ObserveRepository`; accepted repository
events launch `EvaluateRule`; core lifecycle and pull-request close/merge facts
launch `ReactToLifecycle`. At startup and after each completion, the launcher
resumes admitted unfinished runs and launches remaining units from module
cursors and receipts without waiting for another notification; terminal runs are
not reused. It preserves serialized start-to-start polls and queued webhook
wakes (`crates/modules/repo-watch-v2/src/ingest.rs:273`), with the cutover's
single owner. The launcher survives removal of the superseded orchestration; the
host executes and resumes runs but supplies no successor scheduler
(`docs/design/workflows.md:96`).

The retained module also launches `CleanupCheckout` at startup and through a
periodic cleanup sweep using the existing worker cadence
(`apps/signalboxd/src/repo_watch_runtime.rs:542`). It selects retained removal
candidates through the scavenger's eligibility and ownership checks
(`apps/signalboxd/src/repo_watch_dispatch.rs:87`), independently of repository
or lifecycle facts and even when repository watch is disabled or unconfigured.
Pending removals remain eligible on later sweeps until cleanup settles.

The authenticated webhook listener persists deliveries and retains its
primary/shadow meaning; polling credentials remain repository-scoped daemon
inputs (`docs/spec/repo-watch.md:126`, `docs/spec/repo-watch.md:163`,
`apps/signalboxd/src/repo_watch_credentials.rs:63`). Provider paging,
normalization, complete-observation rejection, conditional caches and reviewer
invalidation remain behind `repo.observe` (`docs/spec/repo-watch.md:141`,
`docs/spec/repo-watch.md:312`). Programs receive checked observations and
events, not raw webhook bodies or provider JSON.

`mod_repo_watch` retains its dedicated role, schema authority, accepted events,
revisions, cursors and dispatch lineage (`docs/spec/repo-watch.md:283`). Created
sessions keep the module actor and repository-watch creation cause, with the
same dispatch reference and origin projection; they do not become
workflow-created sessions (`docs/spec/repo-watch.md:325`). The separate
convergence sweep retains its fenced commissions, retry/park behavior and nudge
handoff (`docs/spec/repo-watch.md:79`).

The daemon's model execution path retains automatic context compaction and the
repository-watch successor turn at the context limit
(`docs/spec/model-call-execution.md:48`). Dispatched sessions push through the
configured `git_push_configured` tool, owned by daemon tool composition and its
approval-gated transport; workflows add no push effect
(`docs/spec/tool-loop.md:156`).

## Compatibility constraints

Dispatch preserves independent checked revisions, matchers, ordered actions,
activation tails, singleton scopes and cooldowns (`docs/spec/repo-watch.md:30`,
`docs/spec/repo-watch.md:198`). Complete template defaults remain frozen in held
creation without initial input or a turn; checkout clones the watched repository
at the derived workspace root with the retained branch/SHA before submission
completes (`docs/spec/repo-watch.md:222`, `docs/spec/repo-watch.md:331`).

Goal commissioning/resumption releases the held start; goal achievement or user
stop issues a parent-only sticky stop. A close/merge event later than the
dispatch event retires its live session with the original rule/action and
reason. Durable terminal facts, including those pending at the cursor, prevent
retirement of an already terminal session (`docs/spec/repo-watch.md:208`).
Nonsticky termination releases its action and starts cooldown when the final
action releases; sticky stops retain suppression
(`docs/spec/repo-watch.md:203`).

Rule removal leaves pending commands and lifecycle reactions recoverable;
repository removal retires unprovisioned work as `repository_unconfigured` with
a parent-only nonsticky stop (`docs/spec/repo-watch.md:184`). Provisioning
failure retains its step/status and stops the session; cleanup survives disabled
or absent configuration, restart and cancellation, using retained staging,
device/inode and marker evidence (`docs/spec/repo-watch.md:230`).

Configuration ceilings, observation budgets, webhook body/retention limits,
credential scoping, checkout ownership checks and core session/approval gates
remain host-enforced
(`apps/signalboxd/src/configuration/repository_watch.rs:26`,
`docs/spec/repo-watch.md:118`, `docs/spec/repo-watch.md:155`,
`docs/spec/repo-watch.md:222`, `apps/signalboxd/src/repo_watch_dispatch.rs:411`,
`docs/spec/tool-loop.md:26`). No new guard policy or higher ceiling is added.

## Acceptance criteria

One temporary `repository_watch.workflows_enabled` boolean defaults to false and
selects the orchestrator for the whole configured module. Both engines use the
same store and receipts; exactly one owns observation, event consumption and
command execution. Webhook primary/shadow mode is independent. There is one
listener and one pending-command executor, with no shadow ledger, second cursor,
data copy or backfill.

Named parity fixtures use the exact checked rule definitions in
[repository-watch-rules.example.toml](../../config/repository-watch-rules.example.toml):
rule A `renovate-merge-forward` version 5 and rule B `labeled-review-response`
version 6. The fixture pins every matcher, event kind, ordered action, singleton
scope, cooldown and template.

Rule A's mergeable-state qualifier accepts only `mergeable_state_changed`
payloads (`crates/domain/src/repo_watch/matcher.rs:259`). Its retained
`head_changed` event kind therefore does not fire the rule; parity tests expect
a nonmatch and no dispatch for that event.

Parity tests load those definitions unchanged and compare held commands,
checkout and retirement across equivalent disposable databases. Both use
`EvaluateRule`; neither gets a special engine branch. Acceptance also covers
nonmatch/suppression recovery, lost journal answers, revision activation,
multi-action ordering, removed configuration, terminal facts pending at the
cursor and checkout interruption.

Recovery acceptance interrupts a committed effect before journal delivery,
retires its run on upgrade, and verifies successor adoption without repeating
the effect or blocking the next observation. Cleanup acceptance leaves a pending
removal with no repository or lifecycle fact, then verifies startup and periodic
launches with repository watch disabled or unconfigured.

Reload stops and joins the previous engine before starting its replacement from
retained cursors, receipts and accepted webhook/event backlog. It preserves the
existing listener reload and cleanup contracts (`docs/spec/repo-watch.md:301`).
Cutover and rollback tests include a pending command, webhook backlog, removed
rule, live held session and pending checkout removal; restart resumes one owner.
The flag stays false until observation, dispatch, lifecycle and restart parity
and both handoff directions pass. The owner enables it through ordinary reload.

After the owner confirms successful operation, remove the flag and superseded
repository-task, evaluation and lifecycle orchestration. Workflows become the
sole orchestrator; retain the daemon launcher, pure reducers, module
SQL/receipts, ingestion/cache, checkout adapters, provenance readers, journal
data and the convergence sweep. The removed flag fails ordinary unknown-field
configuration admission.

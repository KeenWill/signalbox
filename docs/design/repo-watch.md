# Repository watch design

This committed unbuilt design extends [repository watch](../spec/repo-watch.md)
with programs on the [workflows host](program-substrate.md).

## Boundary

Compiled Rust programs and checked effect adapters live in
`apps/signalboxd/src/workflows/repo_watch/`. `ObserveRepository` accepts one
poll/webhook observation unit; `EvaluateRule` evaluates one retained event for
one checked rule revision; `ReactToLifecycle` applies one retained lifecycle
unit. Runs are finite, with immutable input and journaled results; subsequent
work resumes from module cursors and receipts, including after a native run ends
`ContractRetired` under the host's executable-pinning contract.

Programs sequence checked operations and pure matcher/reaction planning through
`WorkflowContext`. Adapters own I/O, identities, time, template resolution and
command encoding; no database, provider, credential or filesystem handle enters
program code. The module retains its pure reducers and SQL under the existing
ownership seam (`docs/spec/repo-watch.md:267`); it acquires no dependency on the
workflow runtime or core persistence.

## Effects and receipts

One `RepoWatch` capability admits the following methods, with checked request
and result codecs. The host journals requests before effects and deliveries
before resuming programs. Each mutating adapter binds the run/request identity
and exact input to its module receipt; equal recovery adopts the retained result
and changed reuse conflicts. Recovery checks receipts before active
configuration. An externally committed operation without sufficient receipt
evidence returns ambiguity; multi-step submission and checkout are not declared
idempotent as a whole.

| Method                                           | Operation and recovery contract                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| ------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `repo.observe`                                   | Fetch, normalize, diff and atomically commit one complete observation. Add invocation identity and exact frontier/event-range result to the retained frontier commit, including an unchanged observation. Serialize observations per repository and adopt a committed result into the journal before another observation can replace its receipt. Generation and candidate digest alone are not invocation receipts (`docs/spec/repo-watch.md:104`).                                                                                                                      |
| `repo.nextRuleEvent`                             | Return the active checked revision, next retained event and checked matcher/singleton context as journaled bytes. This read reserves no dispatch; activation-tail and cursor selection remain module-owned (`crates/modules/repo-watch-v2/src/dispatch.rs:65`).                                                                                                                                                                                                                                                                                                           |
| `repo.commitEvaluation`                          | Revalidate the exact event/revision and proposed pure action plan against module authority. Mint identities and freeze complete resolved commands host-side; acquire the existing singleton and commit commands, outcome receipt and evaluation cursor together. Retain the exact nonmatch, suppression or dispatch result on evaluation/dispatch records until journal adoption, without recomputing from changed configuration. Existing command retention and singleton admission remain authoritative (`docs/spec/repo-watch.md:184`, `docs/spec/repo-watch.md:198`). |
| `repo.submitPending`                             | Submit the retained command identity and exact bytes through core handlers, adopt core command receipts, and complete retained follow-ups before reporting completion. Preserve ordered actions, settlement and synchronous conflict rejection (`crates/modules/repo-watch-v2/src/dispatch.rs:304`).                                                                                                                                                                                                                                                                      |
| `repo.provisionCheckout`, `repo.cleanupCheckout` | Use the existing adapter and ledger evidence for location, core session, ownership, provisioned SHA, retirement and removal. Provisioning remains a follow-up of held creation before submission completes; cleanup can run independently. Recovery adopts or resumes only from retained location and ownership evidence (`apps/signalboxd/src/repo_watch_dispatch.rs:160`, `docs/spec/repo-watch.md:222`).                                                                                                                                                               |
| `repo.nextLifecycle`, `repo.commitReaction`      | Journal retained core/seam or close/merge facts and original dispatch context; revalidate terminal state and commit exact release/stop commands and settlement before advancing the module cursor, then acknowledge the seam source. Retain the outcome for lost-answer adoption, including no reaction (`crates/modules/repo-watch-v2/src/dispatch.rs:149`, `apps/signalboxd/src/repo_watch_runtime.rs:660`).                                                                                                                                                            |

Receipt additions belong to the existing module frontier, evaluation and
dispatch records; there is no second dispatch ledger or singleton table. A
successor run recovering business work adopts those records before selecting new
work. Read-only context can be reread before a durable side effect; a replayed
delivery uses its recorded bytes. Host event waits use durable repository/core
source positions; poll timing uses host time/sleep requests and preserves
start-to-start deadlines and queued webhook wakes
(`crates/modules/repo-watch-v2/src/ingest.rs:273`).

## Retained daemon authority

The authenticated webhook listener persists deliveries and retains its
primary/shadow meaning; polling credentials remain repository-scoped daemon
inputs (`docs/spec/repo-watch.md:126`, `docs/spec/repo-watch.md:163`,
`apps/signalboxd/src/repo_watch_credentials.rs:55`). Provider paging,
normalization, complete-observation rejection, conditional caches and reviewer
invalidation remain behind `repo.observe` (`docs/spec/repo-watch.md:141`,
`docs/spec/repo-watch.md:307`). Programs receive checked observations and
events, not raw webhook bodies or provider JSON.

`mod_repo_watch` retains its dedicated role, schema authority, accepted events,
revisions, cursors and dispatch lineage (`docs/spec/repo-watch.md:278`). Created
sessions keep the module actor and repository-watch creation cause, with the
same dispatch reference and origin projection; they do not become
workflow-created sessions (`docs/spec/repo-watch.md:320`). The separate
convergence sweep retains its fenced commissions, retry/park behavior and nudge
handoff (`docs/spec/repo-watch.md:79`).

The daemon's model execution path retains automatic context compaction and the
repository-watch successor turn at the context limit
(`docs/spec/model-call-execution.md:48`). Dispatched sessions push through the
configured `git_push_configured` tool, owned by daemon tool composition and its
approval-gated transport; workflows add no push effect. The executor boundary
exists at `crates/tools-git/src/push_executor.rs:24`; production registration is
unbuilt at this baseline (`docs/spec/tool-loop.md:156`).

## Retirement and guards

Dispatch preserves independent checked revisions, matchers, ordered actions,
activation tails, singleton scopes and cooldowns (`docs/spec/repo-watch.md:30`,
`docs/spec/repo-watch.md:198`). Complete template defaults remain frozen in held
creation without initial input or a turn; checkout clones the watched repository
at the derived workspace root with the retained branch/SHA before submission
completes (`docs/spec/repo-watch.md:222`, `docs/spec/repo-watch.md:326`).

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

## Cutover and removal

One temporary `repository_watch.workflows_enabled` boolean defaults to false and
selects the orchestrator for the whole configured module. Both engines use the
same store and receipts; exactly one owns observation, event consumption and
command execution. Webhook primary/shadow mode is independent. There is one
listener and one pending-command executor, with no shadow ledger, second cursor,
data copy or backfill.

Named parity fixtures preserve the complete configured checked definitions:

| Fixture | Pinned rule and behavior                                                                                                                                                                                 |
| ------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| A       | `renovate-merge-forward`, revision 3: Renovate branches becoming `CONFLICTING` receive one merge-forward session per pull request through template `merge-forward`.                                      |
| B       | `labeled-review-response`, revision 5: pull requests carrying `repo-watch` receive a session through template `review-response-sol` on `checks_completed`, `head_changed` and `mergeable_state_changed`. |

Fixtures copy each rule's exact matchers, ordered actions, singleton scope and
cooldown unchanged, and compare held commands, checkout and retirement across
equivalent disposable databases. Both use `EvaluateRule`; neither gets a special
engine branch. Acceptance also covers nonmatch/suppression recovery, lost
journal answers, revision activation, multi-action ordering, removed
configuration, terminal facts pending at the cursor and checkout interruption.

Reload stops and joins the previous engine before starting its replacement from
retained cursors, receipts and accepted webhook/event backlog. It preserves the
existing listener reload and cleanup contracts (`docs/spec/repo-watch.md:296`).
Cutover and rollback tests include a pending command, webhook backlog, removed
rule, live held session and pending checkout removal; restart resumes one owner.
The flag stays false until observation, dispatch, lifecycle and restart parity
and both handoff directions pass. The owner enables it through ordinary reload.

After the owner confirms successful operation, remove the flag and superseded
repository-task, evaluation and lifecycle orchestration. Workflows become the
sole orchestrator; retain pure reducers, module SQL/receipts, ingestion/cache,
checkout adapters, provenance readers, journal data and the convergence sweep.
The removed flag fails ordinary unknown-field configuration admission.

## Store retention versus rebuild

Path A is the design above: retain module storage and move orchestration into
workflows. Path B is an alternative for owner consideration: remove
`mod_repo_watch` and make journaled workflow state plus host-owned receipts
authoritative. The comparison does not select B or amend the host contract. Both
paths retain the provider client, pure differ/matchers, webhook listener,
credential boundaries, checkout operations and separate convergence sweep.

### State mapping

The migrations below define sixteen module tables. The inventory includes reload
receipts and both poll-cache tables, beyond the thirteen listed at
`docs/spec/repo-watch.md:37`. In B, a journal result is permanent recorded data;
an effect receipt proves a side effect across a lost answer; an event wait
consumes an already durable source. A wait alone stores neither the source nor
shared application state (`docs/design/program-substrate.md:88`).

| Relation and current contents                                                                                                                                                                                                                                                                                                                                                                                                                                                      | Path A                                                           | Path B destination                                                                                                                                                                                                                    |
| ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `repository_state`: repository/default-head projection, full comparison baseline including compact merged subjects, observation generation and last commit digest/predecessor (`crates/persistence/migrations/202609050102_repo_watch_v2_ingest.sql:10`, `crates/persistence/migrations/202609050106_repo_watch_v2_comparison_baseline.sql:5`, `crates/persistence/migrations/202609050107_repo_watch_v2_frontier_commit.sql:4`, `crates/modules/repo-watch-v2/src/ingest.rs:89`). | Retain frontier transaction; add observation invocation receipt. | Carry the complete prior baseline in run input; record the accepted new baseline and event batch in one journal result. A host-owned successor binding must identify the latest accepted generation across runs.                      |
| `pr_state`: current pull-request lifecycle, head/repository/branches, title/body, draft/author and observation times (`crates/persistence/migrations/202609050102_repo_watch_v2_ingest.sql:23`).                                                                                                                                                                                                                                                                                   | Retain atomic projection replacement.                            | Derive the current PR projection from the accepted observation result; no independent authoritative PR table is needed.                                                                                                               |
| `frontier`: recurring event-stream occurrence counters and optional PR association (`crates/persistence/migrations/202609050103_repo_watch_v2_rules.sql:10`).                                                                                                                                                                                                                                                                                                                      | Retain counters in frontier commit.                              | Include counters in predecessor input and successor result; advance them with the accepted event batch, never on an unrecorded fetch.                                                                                                 |
| `gh_event`: permanent checked event payload, UUID/content identity, repository/target, producer, generation, batch position and repository ordinal (`crates/persistence/migrations/202609050103_repo_watch_v2_rules.sql:24`, `crates/persistence/migrations/202609050108_repo_watch_v2_event_lineage.sql:4`).                                                                                                                                                                      | Retain event source and ordinal queries.                         | Accepted observation results hold event batches. Add a typed retained event source over those results for independent rule/lifecycle waits, with catch-up positions and content-identity lookup.                                      |
| `webhook_delivery`: authenticated hook/delivery identity, repository, event/action, body digest, receive time and expiry (`crates/persistence/migrations/202609050102_repo_watch_v2_ingest.sql:52`).                                                                                                                                                                                                                                                                               | Retain intake and duplicate/conflict admission.                  | Host ingress receipt keyed by hook/delivery identity, admitted before HTTP acknowledgement or workflow wake; a run ID alone does not supply this admission contract.                                                                  |
| `webhook_body`: exact authenticated bytes, coupled to delivery expiry (`crates/persistence/migrations/202609050102_repo_watch_v2_ingest.sql:71`).                                                                                                                                                                                                                                                                                                                                  | Retain expiring body storage.                                    | Host-owned expiring payload storage referenced by the ingress receipt; keep raw bodies out of permanent run input/results.                                                                                                            |
| `webhook_disposition`: pending/applied/ignored/rejected outcome and settlement time (`crates/persistence/migrations/202609050102_repo_watch_v2_ingest.sql:83`).                                                                                                                                                                                                                                                                                                                    | Retain one settlement and primary/shadow wake rules.             | Host ingress settlement receipt, with a durable eligible source position for event waits; replay cannot wake a settled delivery again.                                                                                                |
| `rule`: active revision and digest per repository/rule (`crates/persistence/migrations/202609050103_repo_watch_v2_rules.sql:83`).                                                                                                                                                                                                                                                                                                                                                  | Retain checked reconciliation and active lookup.                 | Journal checked configuration activation and carry active definitions in successor input; an authoritative active-generation binding must fence evaluations in other runs.                                                            |
| `rule_revision`: revision digest, activation/retirement times and activation event tail (`crates/persistence/migrations/202609050103_repo_watch_v2_rules.sql:50`, `crates/persistence/migrations/202609050108_repo_watch_v2_event_lineage.sql:18`).                                                                                                                                                                                                                                | Retain history and activation-tail admission.                    | Immutable configuration input/results retain exact checked definitions, tails and retirement facts; host lookup must preserve the highest retired revision and dispatch references.                                                   |
| `rule_field_fingerprint`: ordered identity-field names and digests (`crates/persistence/migrations/202609050103_repo_watch_v2_rules.sql:66`).                                                                                                                                                                                                                                                                                                                                      | Retain field-identity validation.                                | Compute the same fingerprints from retained checked revision bytes; no separate authoritative fingerprint relation is needed.                                                                                                         |
| `reload_activation`: immutable reload-command receipt, rule-set digest and repository activation tails (`crates/persistence/migrations/202609071001_reload_activation.sql:6`).                                                                                                                                                                                                                                                                                                     | Retain atomic reload replay.                                     | Host effect receipt binds the existing reload command to the complete activation result; publishing active definitions and their tails must remain atomic.                                                                            |
| `rule_evaluation_cursor`: last evaluated event per repository/rule/revision, including nonmatches and suppression (`crates/persistence/migrations/202609051700_repo_watch_evaluation.sql:6`, `docs/spec/repo-watch.md:69`).                                                                                                                                                                                                                                                        | Retain cursor; add exact outcome receipt.                        | Evaluation journal results carry outcome and consumed position into the successor input; advance only with retained dispatch intent or recorded nonmatch/suppression.                                                                 |
| `dispatch_ledger`: dispatch origin, action order, exact command bytes/IDs, source event/core trigger, created session, submission follow-up and settlement (`crates/persistence/migrations/202609050104_repo_watch_v2_dispatch.sql:10`, `crates/persistence/migrations/202609051700_repo_watch_evaluation.sql:17`).                                                                                                                                                                | Retain ledger and core receipt adoption.                         | Journal frozen command intents/results; use core command receipts for submission. Add recovery and session-origin lookup across runs by dispatch/session/action, including before the workflow receives its answer.                   |
| `dispatch_ledger` singleton state: key per rule revision, live/sticky suppression, released actions and cooldown start (`crates/persistence/migrations/202609051700_repo_watch_evaluation.sql:17`, `crates/modules/repo-watch-v2/src/lib.rs:2144`, `docs/spec/repo-watch.md:203`).                                                                                                                                                                                                 | Retain atomic singleton acquisition with command/cursor commit.  | Carry the singleton map in one serialized authority's journal state, covering rule scope across repositories; independent runs require a host claim/commit effect instead. Per-run serialization alone is insufficient.               |
| `dispatch_ledger` checkout state: workspace/session, created ownership, device/inode, path/SHA, removed status, failure step/status/reason and stop-command ID (`crates/persistence/migrations/202609071400_repo_watch_checkout.sql:4`, `crates/persistence/migrations/202609072120_repo_watch_checkout_removal.sql:4`).                                                                                                                                                           | Retain filesystem recovery evidence and cleanup scans.           | Host checkout receipts retain location/ownership before filesystem mutation and completion afterward; journal results reference them. Startup cleanup must discover incomplete receipts without an executable old run or active rule. |
| `dispatch_ledger` retirement state: observed session terminal time, later close/merge event, original action and stop reason (`crates/persistence/migrations/202609071500_repo_watch_retirement.sql:4`).                                                                                                                                                                                                                                                                           | Retain retirement scans and terminal checks.                     | Journal lifecycle facts and exact stop intent; adopt core stop receipts. Add lookup of outstanding dispatches by PR/session and preserve terminal checks before effect execution.                                                     |
| `core_event_cursor`: singleton applied-through core lifecycle sequence (`crates/persistence/migrations/202609050102_repo_watch_v2_ingest.sql:97`).                                                                                                                                                                                                                                                                                                                                 | Retain reaction-before-cursor-before-seam-ack ordering.          | Carry the applied position in reaction results/successor input; a core event wait uses it, and an acknowledgement effect runs only after reaction/settlement is durable.                                                              |
| `poll_cache_reviewers`: configured reviewer set per repository (`crates/persistence/migrations/202609071002_poll_cache.sql:6`).                                                                                                                                                                                                                                                                                                                                                    | Retain invalidation at poller composition.                       | Include reviewer identity in accepted observation input/results; changed sets invalidate the next request's cached contribution.                                                                                                      |
| `poll_cache_page`: resource/page key, ETag/Last-Modified, pagination continuation and accepted typed snapshot (`crates/persistence/migrations/202609071002_poll_cache.sql:13`).                                                                                                                                                                                                                                                                                                    | Retain bounded transport cache and pruning.                      | Carry accepted cache snapshots/validators in observation results and successor input, or referenced blobs. Dropping untraversed pages from current state does not delete their historical journal bytes.                              |

Primary/unique indexes enforce the keys in these rows. The additional
`dispatch_singleton`, `dispatch_session_retirement`, `dispatch_live_session`,
`gh_event_pull_request_terminal` and `dispatch_created_session` indexes contain
no independent state; B must replace their admission or lookup function
(`crates/persistence/migrations/202609051700_repo_watch_evaluation.sql:22`,
`crates/persistence/migrations/202609071500_repo_watch_retirement.sql:19`,
`crates/persistence/migrations/202609071502_repo_watch_retirement_indexes.sql:4`,
`crates/persistence/migrations/202609081600_repo_watch_session_origin_index.sql:4`).
Convergence state belongs to the separate sweep and is outside this store
replacement (`docs/spec/repo-watch.md:79`).

### What the host would need

P01 can encode the baselines, checked rules, pure decisions, command intents and
results above as bytes, and replay them. It does not supply the following
cross-run and external-state contracts. These are additions needed by B, not
approved additions to P01.

| Gap in P01                                                                                                                                                                                             | Addition needed for B                                                                                                                                                                                                                                                                                              |
| ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Attempts are serialized per run, not per repository, rule or dispatch (`docs/design/program-substrate.md:95`).                                                                                         | Durable business-key ownership and atomic predecessor-result/successor-input admission. A single module-wide rule coordinator can hold every singleton scope without a generic lock service, but its handoff still needs fencing; per-repository coordinators alone cannot enforce rule scope across repositories. |
| A changed native binary retires unfinished runs; no checkpoint or continuation contract transfers their business state (`docs/design/program-substrate.md:43`, `docs/design/program-substrate.md:95`). | Recoverable finite-run handoff and checked state/result access under new code, including pending effect discovery after cancellation or ContractRetired. A perpetual Rust loop loses executable recovery on deployment; ordinary replay cannot fix that.                                                           |
| Event waits read retained sources; P01 does not create webhook intake or expose other runs' results as repository event streams (`docs/design/program-substrate.md:88`).                               | Authenticated ingress admission/settlement plus typed journal-result event reads, source positions and catch-up. Journaled event batches can supply history without a duplicate event table, but need cross-run indexing or traversal.                                                                             |
| Journal frames are immutable; P01 offload hydrates retained bytes and adds no expiry (`docs/design/program-substrate.md:103`, `docs/spec/program-substrate.md:100`).                                   | Expiring webhook payload receipts outside the journal. Journal-only storage cannot preserve body release at expiry; carrying cache snapshots through journals also retains their history rather than reclaiming old cache rows.                                                                                    |
| Delivery persistence cannot atomically commit a filesystem change or prove its outcome after a lost answer (`docs/spec/program-substrate.md:50`).                                                      | Checked checkout effects with durable preparation/completion receipts and a recovery scan outside run execution. Core session command receipts are reusable; checkout ownership evidence still needs durable storage.                                                                                              |
| Existing session effects create Workflow sessions and submit turns; repository-watch origin resolves module dispatch rows (`docs/spec/program-substrate.md:57`, `docs/spec/repo-watch.md:320`).        | Checked held-create, release and stop adapters plus a dispatch/session origin projection over journal intents and core receipts. Preserve the repository-watch cause, compaction continuation and retirement behavior; merely calling SessionEffects changes those contracts.                                      |
| Module SQL/role isolation and atomic rule-set activation are existing authority contracts, not journal features (`docs/spec/ownership-seam.md:20`, `docs/spec/ownership-seam.md:34`).                  | Approve the replacement authority boundary and implement checked activation/dispatch admission, removed-rule recovery and receipts in host persistence. Deleting the schema cannot silently remove these invariants.                                                                                               |

B removes a separate observation commit: a complete observation and its derived
events become accepted together in one journal delivery. It also removes
separate authoritative PR projections, fingerprint rows and evaluation cursors.
It does not remove webhook, checkout or cross-run ownership state; those become
host-owned contracts. A design forbidding all storage outside the journal cannot
retain the current expiry and external-effect recovery behavior.

### Delivery estimate and cutover

These are estimates of narrow implementation PRs after the shared host work,
excluding this design PR. They assume reuse of provider/checkout code and the
same A/B rule, lifecycle, crash and reload parity tests; they are not
elapsed-time estimates.

| Path            | Estimated implementation PRs                                                                                                                                                                                                                                             | Cutover risk                                                                                                                                                                                                                              |
| --------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| A: retain store | 6: checked effects/receipts; observation; rule dispatch/checkout; lifecycle; handoff/parity; removal of old orchestration.                                                                                                                                               | Lower: both engines resume the same accepted events, commands, singleton state and cleanup evidence. Lost-answer adoption and stop/join ownership still need testing.                                                                     |
| B: rebuild      | 10–12: 4–6 for cross-run ownership/handoff, durable ingress/expiry, result event reads and recoverable effect storage; 1 observation; 1 rule admission/dispatch; 1 lifecycle/checkout; 1 provenance/authority integration; 1 cutover/parity; 1 store/old-engine removal. | Higher: event positions, singleton ownership, live dispatch recovery and session-origin reads change together. Rollback after B accepts work cannot resume A's stale store without importing that work or keeping a compatibility reader. |

A uses the flag handoff above. B can start fresh on an empty installation;
otherwise it needs B-readable accepted events, revision history, sticky
suppression and session origins before dropping their module records. Draining
pending intake, live dispatches and cleanup simplifies transfer but does not
erase that retained history. An undrained cutover also transfers pending
commands and checkout/retirement evidence. Empty initial databases do not erase
work accepted during coexistence. B cannot promise A's immediate rollback while
also dropping the store and forbidding state transfer; schema removal follows
owner acceptance of that cutover constraint.

I recommend A for this change. B gives the program more of the state machine and
removes several projections, but P01 does not yet replace the durable
coordination and effect evidence that the module owns. Rebuilding on it now
would add those contracts to the host while changing the recovery path. Keep the
store, move the orchestration, and judge a later removal by how much state the
host can actually replace.

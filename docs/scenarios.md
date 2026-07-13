# Design scenarios

These scenarios test architectural boundaries; quoted commands and state names are descriptive pseudocode, not final APIs. “Durable commands” means owner intent the hub must commit before acknowledging, not a prescribed event-sourcing design. Invariant identifiers link to [the catalog](invariants.md).

## S01 — Create a new interactive session

- **User intent:** Start an empty conversation from a terminal and make it available on every client.
- **Durable commands:** `CreateSession(cause: owner_initiated, ancestry: none, configuration)` followed by proposed `SubmitInput(delivery: start_when_no_active_turn, ...)`; atomically persist the accepted input, origin turn, and complete frozen effective configuration.
- **State transitions:** No session → active session; no turn → queued origin turn with derived eligibility → atomically fixed starting frontier plus `Active(Running)` and initial prepared attempt.
- **Transient updates:** Optimistic client placeholder and scheduling progress.
- **Owning component:** Hub owns creation and acceptance; Postgres stores the result; the client owns presentation.
- **Failure behavior:** Before commit, return a visible failure and no session identity. After commit, retrying the command returns the same result through idempotency rather than creating a duplicate.
- **Required invariants:** INV-001, INV-003, INV-007, INV-012.
- **Remaining questions:** Acceptance of ADR-0001, ADR-0003, and ADR-0027; final command representation, idempotency scope, first client, and protocol.

## S02 — Stream a centrally called provider response

- **User intent:** Receive a responsive answer while retaining an authoritative final transcript.
- **Durable commands:** Accept input and create a turn with frozen effective configuration; activate it and create a turn attempt; freeze a context frontier; resolve and pin the exact model target; create a model call; finally commit assistant content and call outcome.
- **State transitions:** Turn queued with derived eligibility → active/running with exactly one current attempt → terminal/completed only after the call is classified and the attempt ends; model call prepared → in flight → terminal/completed.
- **Transient updates:** Provider token deltas and progress are relayed as a replaceable draft.
- **Owning component:** The hub resolves and calls the provider; Postgres owns durable provenance and final content; clients render drafts.
- **Failure behavior:** A client disconnect does not cancel the call. Under proposed ADR-0005, every retry is a new model call, including after a call fails before send, and an ambiguous call is not retried automatically; no partial draft becomes final content. Any steering committed into a failed call's frontier remains in the retry frontier.
- **Required invariants:** INV-005, INV-014, INV-015, INV-032, INV-035.
- **Remaining questions:** Acceptance of ADR-0005, provider-specific ambiguity evidence, retry limits, streaming checkpoints, browser transport, and assistant-message commit granularity.

## S03 — Hub restarts after accepting queued work

- **User intent:** Trust an acknowledgement even if the service restarts before work starts.
- **Durable commands:** Under proposed ADR-0027, persist accepted input, origin turn, queue lineage, and frozen effective configuration in one transaction before acknowledgement.
- **State transitions:** Queued turn remains durable across restart; eligibility is recomputed from lineage and slot ownership. After its predecessor becomes terminal, one transaction fixes its outcome-aware starting frontier and either activates it with an initial attempt or terminalizes it as failed if its frozen configuration cannot execute.
- **Transient updates:** Pre-restart queue position and process-local wakeups disappear and are reconstructed.
- **Owning component:** Hub recovery and scheduler coordinate from Postgres.
- **Failure behavior:** Work eventually continues, fails explicitly, is canceled, or requests reconciliation; it never silently vanishes. Duplicate recovery scans do not create duplicate turns.
- **Required invariants:** INV-007–INV-012, INV-034.
- **Remaining questions:** Acceptance of ADR-0004 and ADR-0027; Postgres scheduler mechanics, wake-up strategy, and evidence for whether a prepared attempt crossed an external boundary.

## S04 — Hub restarts during a provider call

- **User intent:** Recover honestly without claiming to resume the lost network stream.
- **Durable commands:** Before send, persist model-call identity, exact hub-resolved provider/model target, frontier, and in-flight state; after restart, record the recovered outcome classification and a recovery decision.
- **State transitions:** Model call in flight → completed/known failed/cancelled/ambiguous according to recovered evidence; restart ends or fences the turn attempt. A non-cancelled ambiguous call deterministically leaves the turn active awaiting a recovery decision; cancellation plus ambiguity terminalizes as reconciliation-required.
- **Transient updates:** Uncommitted deltas and the live provider connection are lost; clients replace drafts from an authoritative snapshot.
- **Owning component:** Hub provider adapter reports evidence; hub recovery classifies it; Postgres records it.
- **Failure behavior:** Do not imply exact-token continuation. Another external call is a new model-call record. It stays in the same turn only through an explicit owner-authorized recovery transition preserving the exact origin, complete configuration, committed context, and effect evidence under ADR-0004 and ADR-0005; the turn retains its slot until that decision and no semantic “same intent” comparison is performed.
- **Required invariants:** INV-004, INV-014–INV-018, INV-032, INV-034.
- **Remaining questions:** Acceptance of ADR-0004 and ADR-0005; whether provider request identifiers make the outcome knowable and provider-specific retry limits.

## S05 — Runner disconnects during a harmless tool

- **User intent:** Complete a read-only workspace query despite runner loss.
- **Durable commands:** Create and authorize a logical tool request; create a tool attempt; dispatch with runner, execution-boundary snapshot, and generation; classify the disconnect.
- **State transitions:** Tool attempt dispatched → lost/known failed; tool request → retry-eligible only if policy classifies it as safely repeatable.
- **Transient updates:** Runner heartbeat, command progress, and partial stdout may disappear.
- **Owning component:** Hub owns policy and recovery; runner owns physical execution; scheduler owns dispatch selection.
- **Failure behavior:** A new physical attempt may be permitted for a proven read-only operation. A late first result is stale and cannot overwrite the current attempt.
- **Required invariants:** INV-011, INV-021, INV-024–INV-026, INV-034.
- **Remaining questions:** Risk/effect taxonomy, evidence needed for harmlessness, output deduplication, and fencing representation.

## S06 — Runner disconnects during a potentially irreversible tool

- **User intent:** Avoid accidentally repeating an external write whose result was lost.
- **Durable commands:** Persist the approved tool request, attempt, dispatch generation, and disconnect evidence; record `ambiguous` when completion cannot be established.
- **State transitions:** Tool attempt in flight → ambiguous and current turn attempt → ambiguous; the non-cancelled turn retains its slot in `AwaitingRecoveryDecision` until explicit owner action or evidence sends it to reconciliation or another permitted disposition.
- **Transient updates:** Last progress text may be shown only as non-authoritative evidence.
- **Owning component:** Hub classifies and blocks automatic retry; the selected runner may later provide evidence; the owner resolves uncertainty.
- **Failure behavior:** No blind retry and no claim that interrupt or disconnect undid the effect. The owner may end the turn as reconciliation-required, but any future authorization to continue must preserve the ambiguous attempt and follow the later tool-effect policy. Reconciliation records why the final classification changed.
- **Required invariants:** INV-019, INV-021, INV-025, INV-026, INV-029, INV-034.
- **Remaining questions:** Reconciliation workflow, idempotency-key support, effect taxonomy, and who may mark an ambiguous attempt resolved.

## S07 — Submit an interrupting message

- **User intent:** Stop current progress and begin different logical work from new input.
- **Durable commands:** Under proposed ADR-0027, atomically persist the interrupting accepted input, its successor turn and frozen configuration, and a cancellation request against the expected active turn.
- **State transitions:** Active turn → cancellation requested → completed/cancelled/failed/reconciliation-required terminal according to raced evidence; interrupt-created turn remains queued, then fixes its successor frontier and becomes eligible.
- **Transient updates:** Cancellation signals to provider or runner and “stopping” progress.
- **Owning component:** Hub owns ordering and state; adapters attempt prompt cancellation; client states intent.
- **Failure behavior:** Issued effects are not rolled back. The interrupted turn retains the progressing slot until every issued operation is classified, its current attempt ends, and any wait is closed. An ambiguous issued effect makes it terminal reconciliation-required; it cannot enter a recovery wait after cancellation. The successor frontier includes completion, cancellation, failure, or ambiguity rather than relying on transient drafts.
- **Required invariants:** INV-007, INV-009, INV-025, INV-028, INV-029.
- **Remaining questions:** Acceptance of ADR-0004, ADR-0005, and ADR-0027; provider/tool-specific cancellation evidence remains open. Child-cancellation propagation is excluded from the baseline and reserved for ADR-0002.

## S08 — Submit safe-point steering

- **User intent:** Refine active work without creating a separate future turn.
- **Durable commands:** Persist the input with `next_safe_point`, its target active turn, acceptance order, and inherited fallback configuration; the command carries no independent configuration request. After every earlier issued physical operation is classified and immediately before a later model call, atomically commit it to turn semantic history, include it in that call's frontier, and record consumption.
- **State transitions:** Accepted input → pending steering → consumed by a later model call, or visibly reclassified as a queued turn origin if the target turn terminates first. Every active wait retains the turn's session slot under proposed ADR-0004.
- **Transient updates:** Client shows “will apply at next safe point”; no mutation of the current provider stream.
- **Owning component:** Hub decides safe-point boundaries and builds context; clients only request and display treatment.
- **Failure behavior:** Restart preserves pending steering. It cannot be consumed while an earlier call or tool attempt is unclassified. If the consuming call later fails before send or ends ambiguously, every retry or continuation retains the consumed steering. If the turn becomes terminal before consumption, the terminal transaction creates FIFO queued work with the captured fallback configuration and records why steering was reclassified.
- **Required invariants:** INV-007, INV-015, INV-016, INV-028, INV-034.
- **Remaining questions:** Acceptance of ADR-0004 and ADR-0027; future safe-point kinds and client rendering of reclassification. Version one does not let tool or orchestration steps consume steering directly.

## S09 — Queue input for the next turn

- **User intent:** Let current work finish, then process a new message separately.
- **Durable commands:** Persist the accepted input with `after_current_turn`, queue lineage, its origin turn, and frozen effective configuration in one transaction.
- **State transitions:** Turn B is queued while turn A remains active; after A reaches a terminal disposition, B fixes an outcome-aware frontier through A and becomes eligible.
- **Transient updates:** Queue position and projected start time may change.
- **Owning component:** Hub owns durable ordering and scheduler eligibility.
- **Failure behavior:** Restart preserves order, identity, and configuration. Cancellation of A does not erase B; B sees committed semantic history plus an explicit completed, failed, cancelled, or reconciliation-required predecessor marker. If B itself cannot execute, it still waits for A, fixes that complete frontier, and only then fails so later C cannot omit A.
- **Required invariants:** INV-007, INV-009, INV-010, INV-012, INV-028.
- **Remaining questions:** Acceptance of ADR-0027; queue admission/resource limits and the semantic rendering of outcome markers. Editing, cancellation, reordering, delivery-policy change, and configuration change remain explicitly unsupported baseline operations.

## S10 — Approve a risky tool

- **User intent:** Permit one clearly presented risky operation.
- **Durable commands:** Create exact tool request; record policy result `confirmation_required`; persist owner approval bound to request, normalized arguments, and material constraints; create an authorized attempt.
- **State transitions:** Tool request proposed under the running turn's required current attempt → that attempt yields and the turn retains its active slot while awaiting approval with no live attempt → approval atomically creates a new turn attempt → tool dispatched/completed.
- **Transient updates:** Confirmation prompt delivery and executor progress.
- **Owning component:** Hub owns policy and approval record; client authenticates and presents; selected executor performs the attempt.
- **Failure behavior:** A hub restart leaves the request waiting without inventing a decision. Duplicate approval is idempotent. Changed arguments or placement constraints require reevaluation and cannot reuse approval.
- **Required invariants:** INV-019, INV-020, INV-024, INV-027.
- **Remaining questions:** Acceptance of ADR-0004; approval expiry, risk classification, scoped standing grants, material constraints, and automated-judge influence.

## S11 — Deny a risky tool

- **User intent:** Prevent the proposed effect while allowing the conversation to continue safely.
- **Durable commands:** Persist denial bound to the exact request and make the denial/result available to orchestration history under the eventual representation.
- **State transitions:** Awaiting approval → denied terminal; turn either continues with that outcome or reaches a terminal state.
- **Transient updates:** Prompt closes and clients receive status.
- **Owning component:** Hub owns denial and prevents dispatch; client captures the owner's decision.
- **Failure behavior:** No physical attempt is created. Duplicate or delayed approval messages cannot reverse the denial without an explicit new decision path.
- **Required invariants:** INV-012, INV-019, INV-020, INV-027.
- **Remaining questions:** Whether reconsideration reopens the request or creates a new one, and how denial becomes model-visible.

## S12 — Receive a stale or duplicated runner result

- **User intent:** Trust current state despite delayed or retried transport delivery.
- **Durable commands:** Validate result envelope against tool-attempt identity and dispatch generation; record duplicate/stale evidence if audit policy requires, without applying it again.
- **State transitions:** Current work remains unchanged; a first valid current result may advance exactly once.
- **Transient updates:** Runner acknowledgement may state “duplicate” or “stale.”
- **Owning component:** Hub transaction and database constraints enforce fencing; runner retries delivery until acknowledged.
- **Failure behavior:** A stale success cannot overwrite a newer failure, result, cancellation, or reconciliation state.
- **Required invariants:** INV-011, INV-012, INV-021.
- **Remaining questions:** Fence representation, retention of rejected evidence, result acknowledgement protocol, and compatibility window.

## S13 — Use an ambient-user runner

- **User intent:** Intentionally run a workspace tool with the same OS authority as the owner.
- **Durable commands:** Select the runner explicitly; snapshot the declared, configured, and verified evidence relevant to the attempt together with the effective ambient boundary; apply tool policy and approval rules.
- **State transitions:** Eligible tool request → placement selected as ambient → authorized/denied → attempted.
- **Transient updates:** UI warning and runner availability.
- **Owning component:** Runner declares its properties; deployment configuration and any accepted verification supply other evidence; hub derives placement and policy; client displays only the effective boundary it may rely on.
- **Failure behavior:** An unsupported isolation claim does not change the effective ambient boundary, and the system never labels this runner isolated on the strength of that claim. Loss or side effects follow the same ambiguity rules, potentially with stricter confirmation.
- **Required invariants:** INV-019, INV-022–INV-026.
- **Remaining questions:** Required warnings, policy differences, verification/attestation, and minimum sandbox requirements for other profiles.

## S14 — Use a restricted runner

- **User intent:** Execute in a deliberately constrained account, container, sandbox, or VM.
- **Durable commands:** Select a runner whose effective typed properties satisfy the request; persist the relevant declarations, configuration, verified evidence, effective-boundary snapshot, and dispatch.
- **State transitions:** Placement evaluation → restricted runner selected → authorized attempt → outcome.
- **Transient updates:** Resource use and progress reported by the runner.
- **Owning component:** Deployment supplies the controls and trusted configuration; runner declares properties; accepted mechanisms may verify them; hub derives and records effective properties; client explains the effective boundary and evidence level.
- **Failure behavior:** A missing or insufficiently evidenced property fails restricted placement explicitly. A “restricted” label or runner declaration alone cannot justify a stronger execution guarantee.
- **Required invariants:** INV-021–INV-024.
- **Remaining questions:** Capability schema, attestation, minimum profiles, resource limits, and whether constraints can change during a connection.

## S15 — Execute a hub-local tool

- **User intent:** Use a centrally available integration such as documentation lookup.
- **Durable commands:** Create a logical tool request, evaluate hub policy, create a hub-local attempt, and persist its result/outcome.
- **State transitions:** Tool request → authorized/denied → hub-local in flight → terminal; turn consumes the durable logical result.
- **Transient updates:** Search progress or partial presentation that is not conversation truth.
- **Owning component:** Hub owns policy and history; a hub-local adapter executes under central credentials.
- **Failure behavior:** Adapter loss is classified with the same known/ambiguous distinction; central placement does not imply safe automatic retry.
- **Required invariants:** INV-019, INV-024–INV-027, INV-035.
- **Remaining questions:** Credential scoping, hub executor isolation, effect classification, and whether centrally hosted MCP is one adapter type.

## S16 — Execute a runner-local tool

- **User intent:** Operate on state available only in a workspace or machine-local application.
- **Durable commands:** Create and authorize the tool request; select/pin a runner; persist boundary snapshot; create and fence the attempt; accept a validated result.
- **State transitions:** Tool request → placement pending → runner dispatched → terminal or lost/ambiguous.
- **Transient updates:** Connection heartbeat, stdout, and progress.
- **Owning component:** Hub coordinates; scheduler places; runner-local executor acts; Postgres stores authoritative state.
- **Failure behavior:** Runner unavailability is visible and does not silently move locality-sensitive work. Stale results fail fencing.
- **Required invariants:** INV-011, INV-019, INV-021–INV-026.
- **Remaining questions:** Pinning/affinity, multi-runner turns, result-size handling, and local MCP capability discovery.

## S17 — Fork from previous transcript state

- **User intent:** Explore an alternative from an earlier point without changing the source session.
- **Durable commands:** Create a session with cause (for example, user-created) independent from ancestry `(source session, immutable frontier)`.
- **State transitions:** New session absent → active with derived initial context; source remains unchanged.
- **Transient updates:** Client may preview the fork point.
- **Owning component:** Hub validates frontier and creates the fork atomically; Postgres preserves source reference and derived history representation.
- **Failure behavior:** Invalid or inaccessible frontier fails before creation. Retrying creation is idempotent. Later source archival does not erase fork identity.
- **Required invariants:** INV-003, INV-012, INV-030.
- **Remaining questions:** Copy versus reference storage, deletion/retention, multiple ancestry sources, and merge semantics (not initially required).

## S18 — Delegate to a child session

- **User intent:** Assign related work to an independently browsable child and receive an explicit result.
- **Durable commands:** Future ADR-0002 commands must create the child with delegation cause and parent-work relation, persist task input, record a typed parent wait/reference, and later persist explicit result delivery.
- **State transitions:** Child creation and execution use a distinct session. Parent wait, resume, result, and cancellation transitions are intentionally not part of the first implementable turn state machine; ADR-0002 must add a typed child-wait phase before delegation ships.
- **Transient updates:** Child progress summaries and presence indicators.
- **Owning component:** Hub owns relationships and scheduling; each session retains independent history.
- **Failure behavior:** Once ADR-0002 defines the feature, restart must restore both child state and parent wait, and child failure must be delivered explicitly rather than disappearing into parent UI state. The current foundation does not permit an implementation to invent those transitions.
- **Required invariants:** INV-003, INV-010, INV-031, INV-034.
- **Remaining questions:** ADR-0002 must define the child-wait variant, result representation, cancellation propagation, detached work, and resource limits. Any accepted child wait retains the parent session slot unless ADR-0002 also defines explicit branching or rebasing.

## S19 — Cancel a parent while child work is active

- **User intent:** Stop parent work with a clear understanding of what happens to the child.
- **Durable commands:** No baseline command is exposed for canceling a parent in a child wait. ADR-0002 must define and atomically persist parent cancellation together with an explicit child-disposition decision before enabling this scenario.
- **State transitions:** Reserved for ADR-0002; the first turn state machine has no `AwaitingChild` phase and therefore no incomplete cancellation edge.
- **Transient updates:** Cancellation progress for each physical attempt.
- **Owning component:** Hub applies the eventual delegation policy; executors only respond to cancellation requests.
- **Failure behavior:** A future policy may keep or cancel the child, but already-issued effects are never undone, the child never silently disappears, and ambiguous child effects remain reconcilable. Implementations must not choose a policy before ADR-0002 is accepted.
- **Required invariants:** INV-010, INV-025, INV-026, INV-029, INV-031, INV-034.
- **Remaining questions:** ADR-0002 remains blocking for propagation, detached-child support, result delivery after parent termination, and the parent/child disposition model; archival coupling remains with ADR-0028.

## S20 — Resolve a curated model alias

- **User intent:** Use a convenient selection such as “latest preferred” while retaining precise requested, resolved, and provider-reported provenance.
- **Durable commands:** Persist requested alias in effective configuration; for the first call resolve it through hub policy and persist the exact hub-resolved provider/model target before send; proposed ADR-0005 pins that target for later calls in the turn; append observable provider identity or mismatch when available.
- **State transitions:** Model call prepared with selection → resolved exact target → in flight → terminal.
- **Transient updates:** Client may show current alias target, clearly separate from historical call facts.
- **Owning component:** Hub model resolver and provider adapter; Postgres stores per-call provenance.
- **Failure behavior:** Alias changes never rewrite previous calls or change the pinned target of an existing turn. Resolution failure is explicit and does not silently choose another model. Historical provenance does not claim which hidden physical backend executed the call when the provider does not reveal it.
- **Required invariants:** INV-008, INV-014, INV-017.
- **Remaining questions:** Acceptance of ADR-0005; alias versioning, cache/transaction boundaries, visibility, and whether a future frozen alias policy may include fallback.

## S21 — Execute an exact pinned model

- **User intent:** Call one exact provider/model reference for reproducibility or control.
- **Durable commands:** Persist the exact requested selection, exact hub-resolved provider/model target, frontier, and model-call record; capture observable provider-reported model identity and mismatch metadata when available.
- **State transitions:** Pinned call prepared → validated/issued → completed, failed, refused, or mismatch-observed.
- **Transient updates:** Provider stream and timing.
- **Owning component:** Hub validates selection and calls provider; adapter reports observed metadata.
- **Failure behavior:** Hub-controlled fallback does not occur unless separately and explicitly authorized. Provider-reported substitution is recorded rather than rewritten as the pinned target; absent provider evidence, Signalbox does not claim knowledge of the hidden physical backend.
- **Required invariants:** INV-014, INV-015, INV-017, INV-018.
- **Remaining questions:** Whether observable substitution fails the call, provider identifier normalization, and reproducibility claims beyond model identity.

## S22 — Apply an availability fallback

- **User intent:** If explicitly configured, continue through a classified capacity/availability failure using an allowed alternate model.
- **Durable commands:** Record the primary call's requested selection, exact hub-resolved target, provider-reported identity when available, and failure classification; evaluate explicit fallback policy; create a distinct model call with an exact hub-resolved fallback target and reason. Each call appends provider-reported identity or mismatch when available.
- **State transitions:** Primary call → known availability failure; turn/attempt → fallback eligible; fallback call → terminal.
- **Transient updates:** Client shows that fallback is being considered/applied.
- **Owning component:** Hub policy authorizes; provider adapters classify evidence but do not silently select targets.
- **Failure behavior:** If version one has no accepted fallback policy, stop explicitly instead. The scenario does not establish automatic fallback as accepted behavior.
- **Required invariants:** INV-014, INV-017, INV-018.
- **Remaining questions:** Whether fallback ships, qualifying failures, configuration, model-change identity, cost limits, and user confirmation.

## S23 — Encounter a model safety refusal

- **User intent:** Understand that the selected model refused and avoid hidden policy evasion.
- **Durable commands:** Persist the model call, requested selection, exact hub-resolved target, observable provider identity or mismatch when available, provider response classification, and refusal outcome; create any follow-up only through an explicit user/policy decision.
- **State transitions:** Model call in flight → refused; in the baseline the refusal becomes an explicit committed conversational outcome and the turn becomes terminal/completed. A future remediation ADR may add a typed wait or continuation policy.
- **Transient updates:** Refusal text may stream but becomes authoritative only when committed.
- **Owning component:** Provider adapter reports; hub classifies and exposes provenance.
- **Failure behavior:** Refusal is not treated as availability failure, does not automatically fall back merely because another model exists, and does not retain the active slot in an undefined wait for input.
- **Required invariants:** INV-014, INV-017, INV-018, INV-032.
- **Remaining questions:** Refusal taxonomy, user-facing remediation, whether any explicit fallback is ever allowed, and provider-reported substitution or mismatch.

## S24 — Reconnect a client during active streaming

- **User intent:** Resume observing current work without corrupting the transcript or relying on every delta having persisted.
- **Durable commands:** Client requests an authoritative snapshot/version and subscribes from a compatible live point; no new logical work is created merely by reconnecting.
- **State transitions:** Client disconnected → synchronized snapshot → live observer; server-side turn remains unchanged.
- **Transient updates:** Previously seen draft may be replaced; new deltas continue with draft identity/version.
- **Owning component:** Hub reconstructs durable truth and streams; client reconciles presentation.
- **Failure behavior:** Gaps cause another snapshot, not guessed tokens. If the call finished while disconnected, the final durable content replaces the draft.
- **Required invariants:** INV-005, INV-012, INV-032, INV-033.
- **Remaining questions:** Snapshot/event protocol, delta sequencing, checkpointing, browser transport, and compatibility window.

## S25 — Archive and restore a session

- **User intent:** Remove a conversation from the active list without losing its identity, provenance, or ability to return.
- **Durable commands:** Subject to the future archive lifecycle policy, persist `ArchiveSession`; later persist `RestoreSession`, each idempotently against the same session identity.
- **State transitions:** An eligible lifecycle state → archived → a policy-selected restored state; eligibility, nonterminal-work handling, and the restore target remain unresolved.
- **Transient updates:** Client list filtering and confirmation.
- **Owning component:** Hub validates lifecycle; Postgres preserves history; clients present archive state.
- **Failure behavior:** Restart preserves archive status. A request made while work is active or otherwise nonterminal must explicitly fail, wait, or request cancellation according to future policy; it never silently abandons work.
- **Required invariants:** INV-010, INV-012, INV-013, INV-034.
- **Remaining questions:** ADR-0028 must define archive eligibility, nonterminal-work handling, restored lifecycle state, and effects on delegated children or related sessions. Destructive retention and purge are separate later scope under ADR-0029, not ordinary archive behavior.

## S26 — Manually regenerate a prior answer

- **User intent:** Ask for another outcome related to a prior turn without erasing what happened before.
- **Durable commands:** No baseline regeneration command is exposed. A future ADR must create a new turn with a typed relation to the original, an explicitly frozen effective configuration, an immutable source frontier, and defined queue placement while retaining the original turn, attempts, calls, and output unchanged.
- **State transitions:** Reserved for the future regeneration ADR. The initial turn-origin enum contains only accepted-input origin and does not encode a half-defined regeneration transition.
- **Transient updates:** A future client may visually group alternatives, but grouping never replaces durable identities.
- **Owning component:** When introduced, the hub validates the source relation and creates new logical work; Postgres preserves both histories; clients choose presentation.
- **Failure behavior:** When introduced, duplicate command delivery must create at most one regeneration turn. A changed model or any changed effective-configuration field belongs to that new turn and is never disguised as recovery of the original.
- **Required invariants:** INV-001, INV-004, INV-006, INV-008, INV-012, INV-014, INV-015.
- **Remaining questions:** A future regeneration ADR must decide command acceptance, FIFO interaction, exact historical source frontier, configuration freeze, and alternative-answer presentation before implementation.

## Coverage note

The proposed foundation ADRs exercise retry identity and baseline input lifecycle but do not become authoritative until accepted. Delegation cancellation, fallback, capability vocabulary, safety policy, queue management, archive behavior, and protocol choices remain open. An ADR that changes a lifecycle should update the affected scenarios and cite the invariant changes it requires.

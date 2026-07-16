# Design scenarios

These scenarios test architectural boundaries; quoted commands and state names are descriptive pseudocode, not final APIs. “Durable commands” means owner intent the hub must commit before acknowledging, not a prescribed event-sourcing design. Invariant identifiers link to [the catalog](invariants.md).

The scenarios are frozen design fixtures. New or changed normative behavior belongs in the record that owns it (an ADR or the [decision log](decisions.md)); a scenario's normative content changes or is added only alongside the decision that motivates it, and a decision introducing a new lifecycle edge adds or amends its scenario fixture in the same change. `Covered by:` lines naming a scenario's integration tests are coverage links rather than normative changes and are added with the tests themselves.

## S01 — Create a new interactive session

- **User intent:** Start an empty conversation from a terminal and make it available on every client.
- **Durable commands:** `CreateSession(cause: owner_initiated, ancestry: none, initial_model_selection_defaults)` establishes defaults version one. `SubmitInput(delivery: start_when_no_active_turn, ...)` resolves its model request against that exact version and atomically persists the accepted input, origin turn, and complete baseline configuration provenance. A later defaults update affects only subsequently accepted origins.
- **State transitions:** No session → active session; no turn → queued origin turn with derived eligibility → atomically fixed starting frontier plus `Active(Running)` and initial prepared attempt.
- **Transient updates:** Optimistic client placeholder and scheduling progress.
- **Owning component:** Hub owns creation and acceptance; Postgres stores the result; the client owns presentation.
- **Failure behavior:** A malformed transport, pre-authority, unconstructible typed-command, or pre-commit infrastructure failure that does not reach committed domain handling returns visibly and claims no command identifier; corrected boundary input may reuse it. Canonically equivalent caller forms compare as the same command. The first committed handling of a well-formed typed command under established owner authority records either its applied result or a typed domain rejection. Replay returns that same result before current-state validation; a command rejected after construction cannot later become valid under the same identifier. Corrected domain intent then needs a new identifier, and reuse of a claimed identifier for another command kind, session, or payload is rejected rather than creating a duplicate.
- **Required invariants:** INV-001, INV-003, INV-007, INV-008, INV-012.
- **Remaining questions:** Final command representation, first client, and protocol. The owner-global idempotency scope is accepted by the foundation set.

## S02 — Stream a centrally called provider response

- **User intent:** Receive a responsive answer while retaining an authoritative final transcript.
- **Durable commands:** Accept input and create a turn with frozen direct or alias model-selection configuration, provider defaults, and disabled retry/fallback; activate it and create a turn attempt; freeze a context frontier; resolve and pin the exact model target; only then create a model call; finally commit assistant content and call outcome.
- **State transitions:** Turn queued with derived eligibility → active/running with exactly one current attempt → terminal/completed only after the call is classified and the attempt ends; model call prepared → in flight → terminal/completed.
- **Transient updates:** Provider token deltas and progress are relayed as a replaceable draft.
- **Owning component:** The hub resolves and calls the provider; Postgres owns durable provenance and final content; clients render drafts.
- **Failure behavior:** A client disconnect does not cancel the call. Target-resolution failure creates no model call. Send preparation failure leaves the already-created call known-failed. Version one performs no automatic retry after any known or ambiguous provider failure; no partial draft becomes final content. A future authorized call must retain steering already committed to turn history.
- **Required invariants:** INV-005, INV-008, INV-014, INV-015, INV-032, INV-035.
- **Remaining questions:** Provider-specific ambiguity evidence, whether a future known-failure retry command is introduced, streaming checkpoints, browser transport, and assistant-message commit granularity.

## S03 — Hub restarts after accepting queued work

- **User intent:** Trust an acknowledgement even if the service restarts before work starts.
- **Durable commands:** Under accepted ADR-0027, persist accepted input, origin turn, immutable acceptance position, any typed priority relation, and frozen baseline model-configuration provenance in one transaction before acknowledgement.
- **State transitions:** Queued turn and order facts remain durable across restart; eligibility is recomputed from the total order and slot ownership. After every earlier turn becomes terminal, one transaction fixes the exact immediate-predecessor starting lineage and outcome-aware frontier, then either activates the turn with an initial attempt or terminalizes it as failed if its frozen configuration cannot execute.
- **Transient updates:** Pre-restart queue position and process-local wakeups disappear and are reconstructed.
- **Owning component:** Hub recovery and scheduler coordinate from Postgres.
- **Failure behavior:** Work eventually continues, fails explicitly, is canceled, or requests reconciliation; it never silently vanishes. Duplicate recovery scans do not create duplicate turns.
- **Required invariants:** INV-007–INV-012, INV-034.
- **Remaining questions:** Postgres scheduler mechanics and wake-up strategy. Whether an individual provider call or tool attempt crossed its persisted issue boundary is classified by its own evidence; an attempt still in `Prepared` has not crossed the orchestration boundary.

## S04 — Hub restarts during a provider call

- **User intent:** Recover honestly without claiming to resume the lost network stream.
- **Durable commands:** Before send, persist model-call identity, exact hub-resolved provider/model target, frontier, and in-flight state; after restart, an idempotent startup scan records the recovered outcome classification. An owner decision, if needed, is a separate command bound to the exact ambiguous-operation set.
- **State transitions:** The startup scan derives the complete evidence and stop-cause set before ending the old turn attempt with disposition `Lost` in the matching terminal variant. A model call in flight becomes completed, known failed, refused where evidence supports it, cancelled, or ambiguous. Unacknowledged ambiguity first leaves the turn active awaiting the exact reference only when neither an applied interrupt nor fatal mismatch prohibits continuation; otherwise it yields `ReconciliationRequired` with the exact set and matching interrupt or fatal reason. Without blocking ambiguity or fatal stop, outcome-authoritative non-mismatched completion or atomic refusal may control; otherwise known failure follows. Only an applied interrupt proof for that predecessor plus proof that it prevented all remaining work produces turn cancellation; cause-free physical cancellation produces turn failure. Completion/refusal raced under a fatal cause remains physical but non-authoritative. Resolving mismatch evidence after a call already became ambiguous preserves that physical disposition and contributes `TerminalAmbiguityResolution` to the complete recovered fatal causes, yielding turn failure when no other ambiguity remains and exact fatal reconciliation otherwise. An owner may preserve an ambiguous call while separately accepting duplicate risk only when no fatal invalidation exists.
- **Transient updates:** Uncommitted deltas and the live provider connection are lost; clients replace drafts from an authoritative snapshot.
- **Owning component:** Hub provider adapter reports evidence; hub recovery classifies it; Postgres records it.
- **Failure behavior:** Do not imply exact-token continuation. Startup creates no cancellation-only or classification-only attempt. It classifies an uncertain call `Ambiguous` while ending the abandoned attempt in the matching `...Lost` branch; live classification ends the attempt in the matching `...Ambiguous` branch only after every other issued operation is classified, and until then the dispatch guard permits no new semantic effect. Nonfatal evidence can clear the blocking ambiguity without rewriting the call; after all issued work is classified, the same still-live attempt may continue without repeating it. Version one does not retry a provider call after resolving evidence establishes known failure or cancellation; those classifications terminalize under the common precedence. The first trusted mismatch observed while the outcome-eligible call is nonterminal immediately selects known failure, makes response material non-authoritative, and requests best-effort cancellation. After terminal ambiguity it leaves the call `Ambiguous`; a still-live attempt gains `TerminalAmbiguityResolution` in its complete fatal causes, while an already-ended attempt remains unchanged. After outstanding classification the turn fails when no other unacknowledged ambiguity remains and requires reconciliation otherwise. After current-authority completion during an active turn it preserves call/history, appends typed invalidation, and stops new/outstanding work before failure or reconciliation; startup preserves the abandoned attempt as `Lost` in the terminal variant carrying every prior or same-scan stop cause. Without fatal stop, a non-mismatched refusal terminalizes call and turn atomically; a continuation refusal raced under fatal stop remains physical evidence while the turn fails or reconciles. After terminal known failure/cancellation it preserves existing state and precedence. After authority transfer it is non-authoritative evidence; after valid turn terminalization it cannot rewrite committed content or successor context. Another provider call may remain in the same turn only after an exact-set owner decision accepts unresolved duplicate risk while preserving origin, configuration, context, and evidence, and no fatal invalidation exists. That decision atomically closes the wait, consumes all eligible steering into the replacement frontier, creates the replacement attempt and prepared call, and transfers outcome authority. Resolving evidence that commits first makes the decision stale; any later prior-call outcome remains audit/reconciliation evidence only. The accepted-risk marker remains visible.
- **Required invariants:** INV-004, INV-009, INV-014–INV-018, INV-032, INV-034.
- **Remaining questions:** Whether provider request identifiers make the outcome knowable and evidence thresholds for each recovered classification.

## S05 — Runner disconnects during a harmless tool

- **User intent:** Complete a read-only workspace query despite runner loss.
- **Durable commands:** Create and authorize a logical tool request; create a tool attempt; dispatch with runner, execution-boundary snapshot, and generation; classify the disconnect.
- **State transitions:** Tool attempt dispatched → known failed when evidence proves no effect occurred, otherwise ambiguous when the effect cannot be established; tool request → retry-eligible only if policy classifies it as safely repeatable.
- **Transient updates:** Runner heartbeat, command progress, and partial stdout may disappear.
- **Owning component:** Hub owns policy and recovery; runner owns physical execution; scheduler owns dispatch selection.
- **Failure behavior:** A new physical attempt may be permitted for a proven read-only operation. A late first result is stale and cannot overwrite the current attempt.
- **Required invariants:** INV-011, INV-021, INV-024–INV-026, INV-034.
- **Remaining questions:** Risk/effect taxonomy, evidence needed for harmlessness, output deduplication, and fencing representation.

## S06 — Runner disconnects during a potentially irreversible tool

- **User intent:** Avoid accidentally repeating an external write whose result was lost.
- **Durable commands:** Persist the approved tool request, attempt, dispatch generation, and disconnect evidence; record `ambiguous` when completion cannot be established.
- **State transitions:** When classified while orchestration is live, tool attempt in flight → terminal/ambiguous and current turn attempt → the matching `...Ambiguous` branch. If the hub crashes before classification, startup makes the tool attempt terminal/ambiguous while ending the abandoned turn attempt in the matching `...Lost` branch. The physical tool outcome remains `Ambiguous` in both cases. Only when neither an applied interrupt nor fatal mismatch prohibits continuation does the turn retain its slot in `AwaitingRecoveryDecision` carrying that tool-attempt reference until explicit owner action or resolving evidence continues the turn or gives it `ReconciliationRequired` with an applied stop-choice proof and that exact wait set. An applied interrupt or fatal mismatch instead preserves the ambiguity while terminalizing with the exact set and matching typed reason.
- **Transient updates:** Last progress text may be shown only as non-authoritative evidence.
- **Owning component:** Hub classifies and blocks automatic retry; the selected runner may later provide evidence; the owner resolves uncertainty.
- **Failure behavior:** No blind retry and no claim that interrupt or disconnect undid the effect. With several ambiguous operations, separately recorded evidence refines the wait to the exact nonempty remainder without rewriting the terminal tool or turn attempt; only resolving evidence for all references or an exact-set owner decision can leave it. Any authorization to continue preserves ambiguous records, adds an accepted-risk marker visible to successor context, and follows the later tool-effect policy.
- **Required invariants:** INV-009, INV-019, INV-021, INV-025, INV-026, INV-029, INV-034.
- **Remaining questions:** Reconciliation workflow, idempotency-key support, effect taxonomy, who may record separate resolving evidence for terminal ambiguity, and which tool effects permit accepted-risk continuation.

## S07 — Submit an interrupting message

- **User intent:** Stop current progress and begin different logical work from new input.
- **Durable commands:** Under accepted ADR-0027, atomically persist the interrupting accepted input, successor configuration provenance, typed priority relation designating its turn as the active turn's immediate successor, and `AppliedInterruptProof` tied to the exact predecessor, plus its transition: end an unsent prepared attempt; directly end running work only when prevention of remaining work and every terminal guard are already proven, otherwise request cancellation; or close the exact durable wait.
- **State transitions:** `Prepared` → ended/cancelled with the predecessor terminal/cancelled and exact applied-interrupt proof. `Running` → directly ended/cancelled only when every guard already holds, otherwise `StopRequested(CancellationOnly)` with that proof while retaining the exact attempt and slot. Approval wait → cancelled with the same proof; recovery wait → reconciliation-required with that proof and the wait's exact operation set. Every direct terminal path atomically records the interrupt-created immediate successor and reclassifies pending steering before releasing the slot. If fatal mismatch already requested stop, the first interrupt populates its interrupt field without reauthorizing work; either event order preserves both facts. A running predecessor then uses the common precedence: unacknowledged ambiguity yields the exact interrupt/fatal reconciliation marker; otherwise sufficient outcome-authoritative non-mismatched completion or atomic refusal controls only without fatal stop, followed by known failure or applied-and-confirmed interrupt cancellation. A raced completion/refusal under fatal stop remains non-authoritative. Resolving mismatch evidence after terminal ambiguity preserves that operation state, producing failure when no other ambiguity remains and exact fatal reconciliation otherwise. An already accepted ambiguity risk remains marked while interruption is classified normally. The interrupt-created turn is always the immediate queued successor; no standalone active-turn cancellation exists in the baseline.
- **Transient updates:** Cancellation signals to provider or runner and “stopping” progress.
- **Owning component:** Hub owns ordering and state; adapters attempt prompt cancellation; client states intent.
- **Failure behavior:** Issued effects are not rolled back. The interrupted turn retains the progressing slot until every issued operation is classified, its current attempt ends, and any wait is closed. If the hub restarts, the startup scan ends the abandoned attempt and classifies operations without creating a replacement; the applied interrupt plus unacknowledged ambiguity produces a proof-bearing reconciliation marker, while previously accepted risk remains explicitly marked under an ordinary terminal outcome. Before releasing the slot, terminalization durably inserts any reclassified steering after the interrupt successor by original acceptance order. No queued successor has fixed a direct predecessor yet, so each later frontier includes every inserted turn.
- **Required invariants:** INV-007–INV-009, INV-012, INV-025, INV-028, INV-029.
- **Remaining questions:** Provider/tool-specific cancellation evidence remains open. Child-cancellation propagation is excluded from the baseline and reserved for ADR-0002.

## S08 — Submit safe-point steering

- **User intent:** Refine active work without creating a separate future turn.
- **Durable commands:** Persist the input with `next_safe_point`, acceptance order, and one binding referencing the source active turn; the command carries no independent configuration request or copied configuration. The source turn remains the canonical immutable configuration source if reclassification is needed. After every earlier issued physical operation is classified, every earlier tool/approval dependency has a durable outcome, and immediately before any later model call—including a duplicate-risk replacement—atomically commit it to turn semantic history, include it in that call's frontier, and record consumption by call identity.
- **State transitions:** Accepted input → pending steering → consumed by a later model call, or visibly reclassified as a queued turn origin if the target turn terminates first. Every active wait retains the turn's session slot under accepted ADR-0004.
- **Transient updates:** Client shows “will apply at next safe point”; no mutation of the current provider stream.
- **Owning component:** Hub decides safe-point boundaries and builds context; clients only request and display treatment.
- **Failure behavior:** Restart preserves the single steering binding. It cannot be consumed while an earlier call or tool attempt is unclassified. If an ambiguity decision prepares a replacement call, that transaction must consume the input; restart cannot reconstruct it as both pending and consumed. If the consuming call later fails, every future authorized call retains the steering. If the turn becomes terminal before consumption, the terminal transaction creates queued work with captured inherited provenance and durable order facts; it does not invent a request or reread session defaults. Any interrupt-created successor is first; later work follows original acceptance order.
- **Required invariants:** INV-007–INV-009, INV-015, INV-016, INV-028, INV-034.
- **Remaining questions:** Future safe-point kinds and client rendering of reclassification. Version one does not let tool or orchestration steps consume steering directly.

## S09 — Queue input for the next turn

- **User intent:** Let current work finish, then process a new message separately.
- **Durable commands:** Persist the accepted input with `after_current_turn`, immutable acceptance position, its origin turn, and frozen baseline model configuration in one transaction; do not freeze a direct predecessor while priority insertions remain possible.
- **State transitions:** Turn B is queued while turn A remains active. When B eventually becomes eligible, it fixes starting lineage and an outcome-aware frontier through the terminal turn immediately before it in durable order, which may be an interrupt or reclassified-steering turn inserted after B's acceptance.
- **Transient updates:** Queue position and projected start time may change.
- **Owning component:** Hub owns durable ordering and scheduler eligibility.
- **Failure behavior:** Restart preserves order facts, identity, and configuration. Cancellation of A does not erase B. An interrupt-created successor precedes B and all reclassified steering; after the interrupt, queued and reclassified inputs retain acceptance order. B waits for every earlier ordered turn, then fixes its frontier through its actual immediate predecessor so none of those outcomes is omitted. If B itself cannot execute, it fixes that same complete frontier before failing, so later C cannot omit B or any inserted work.
- **Required invariants:** INV-007–INV-010, INV-012, INV-028.
- **Remaining questions:** Queue admission/resource limits and the semantic rendering of outcome markers. Editing, cancellation, reordering, delivery-policy change, and configuration change remain explicitly unsupported baseline operations.

## S10 — Approve a risky tool

- **User intent:** Permit one clearly presented risky operation.
- **Durable commands:** Create the exact tool request; record policy result `confirmation_required`; persist owner approval bound to the request, normalized arguments, and material constraints; then create a new turn attempt for the authorized-but-not-yet-dispatched request. Tool-attempt identity is created only at later physical dispatch.
- **State transitions:** Tool request proposed under the running turn's required current attempt → every previously issued operation is terminally classified → that attempt yields and the turn retains its active slot while awaiting approval with no live attempt → approval atomically creates a new turn attempt → tool dispatched/completed.
- **Transient updates:** Confirmation prompt delivery and executor progress.
- **Owning component:** Hub owns policy and approval record; client authenticates and presents; selected executor performs the attempt.
- **Failure behavior:** A hub restart leaves the request waiting without inventing a decision. Duplicate approval is idempotent. Changed arguments or placement constraints require reevaluation and cannot reuse approval. After approval, an authorized request remains a blocking logical dependency while runner scheduling is delayed; orchestration cannot consume steering or prepare a later model call until the request has a durable outcome.
- **Required invariants:** INV-009, INV-010, INV-012, INV-019, INV-020, INV-024, INV-027.
- **Remaining questions:** Approval expiry, risk classification, scoped standing grants, material constraints, and automated-judge influence.

## S11 — Deny a risky tool

- **User intent:** Prevent the proposed effect while allowing the conversation to continue safely.
- **Durable commands:** Persist denial bound to the exact request and make the denial/result available to orchestration history under the eventual representation.
- **State transitions:** Awaiting approval → denial closes the exact wait and atomically creates a new turn attempt with the denial committed to turn history → orchestration continues without a tool attempt and later reaches an ordinary terminal disposition.
- **Transient updates:** Prompt closes and clients receive status.
- **Owning component:** Hub owns denial and prevents dispatch; client captures the owner's decision.
- **Failure behavior:** No physical tool attempt is created. The new turn attempt exists only to continue conversational orchestration with the denial outcome. Duplicate or delayed approval messages cannot reverse the denial without an explicit new decision path.
- **Required invariants:** INV-009, INV-012, INV-019, INV-020, INV-027.
- **Remaining questions:** Whether future reconsideration creates a new request, and the exact semantic rendering of the committed denial. Baseline continuation in a new turn attempt is decided by ADR-0004's proposal.

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
- **State transitions:** Tool request → placement pending → runner dispatched → known failed when evidence proves no effect, otherwise completed/cancelled/ambiguous according to evidence.
- **Transient updates:** Connection heartbeat, stdout, and progress.
- **Owning component:** Hub coordinates; scheduler places; runner-local executor acts; Postgres stores authoritative state.
- **Failure behavior:** Runner unavailability is visible and does not silently move locality-sensitive work. Stale results fail fencing.
- **Required invariants:** INV-011, INV-019, INV-021–INV-026.
- **Remaining questions:** Pinning/affinity, multi-runner turns, result-size handling, and local MCP capability discovery.

## S17 — Fork from previous transcript state

- **User intent:** Explore an alternative from an earlier point without changing the source session.
- **Durable commands:** Create a session with the baseline `OwnerInitiated` cause independent from ancestry `(source session, immutable frontier)`.
- **State transitions:** New session absent → active with derived initial context; source remains unchanged.
- **Transient updates:** Client may preview the fork point.
- **Owning component:** Hub validates frontier and creates the fork atomically; Postgres preserves source reference and derived history representation.
- **Failure behavior:** Invalid or inaccessible frontier fails before creation. Retrying creation is idempotent. Later source archival does not erase fork identity.
- **Required invariants:** INV-003, INV-012, INV-030.
- **Remaining questions:** Copy versus reference storage, deletion/retention, multiple ancestry sources, and merge semantics (not initially required).

## S18 — Delegate to a child session

- **User intent:** Assign related work to an independently browsable child and receive an explicit result.
- **Durable commands:** Future ADR-0002 commands must add a delegated creation-cause variant with an exact durable parent-work identity, create the child with that cause and a parent-work relation, persist task input, record a typed parent wait/reference, and later persist explicit result delivery.
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
- **Durable commands:** At input acceptance, persist the requested alias plus an immutable definition selecting exactly one canonical direct model choice in effective configuration. Before creating the first call, validate and resolve that frozen meaning and pin the exact hub-resolved provider/model target; append observable provider identity or mismatch when available.
- **State transitions:** Turn with frozen alias meaning → exact target pinned → model call prepared → in flight → terminal.
- **Transient updates:** Client may show current alias target, clearly separate from historical call facts.
- **Owning component:** Hub model resolver and provider adapter; Postgres stores per-call provenance.
- **Failure behavior:** Alias changes after input acceptance never alter queued or active work. Resolution failure creates no targetless call and fails the attempt and turn; it does not silently choose another model. A reported identity different from the exact resolved target follows ADR-0005's full timing rule: known failure while nonterminal; preserved ambiguity plus a fatal cause on any still-live attempt, with turn failure only when no other unacknowledged ambiguity remains and reconciliation otherwise; typed invalidation and stop after completion while the turn is active; unchanged known-failure/cancelled state; audit-only evidence after authority transfer; or non-rewriting evidence after turn terminality, including atomic refusal. Historical provenance does not claim which hidden physical backend executed the call when the provider does not reveal it.
- **Required invariants:** INV-008, INV-014, INV-017.
- **Remaining questions:** Alias administration, visibility, and whether a future frozen alias policy may include fallback. Acceptance-time definition freezing and pre-call target resolution are decided by ADR-0005.

## S21 — Execute an exact pinned model

- **User intent:** Call one exact provider/model reference for reproducibility or control.
- **Durable commands:** Persist the exact requested selection, exact hub-resolved provider/model target, frontier, and model-call record; capture observable provider-reported model identity and mismatch metadata when available.
- **State transitions:** Frozen exact selection → resolution succeeds and a pinned call is prepared, or resolution fails before any call exists → validated/issued call → completed, known failed, refused, cancelled, or ambiguous. Provider-reported target observation is adjacent typed evidence, not another state; the first trusted mismatch on a nonterminal call immediately selects known failure and requests best-effort cancellation, while resolving mismatch evidence cannot reopen an already ambiguous call.
- **Transient updates:** Provider stream and timing.
- **Owning component:** Hub validates selection and calls provider; adapter reports observed metadata.
- **Failure behavior:** An unavailable or otherwise unresolvable exact selection is an error: it creates no targetless model call and fails the attempt and turn. Hub-controlled fallback does not occur unless separately and explicitly authorized. Provider-reported substitution is recorded rather than rewritten as the pinned target. First observed while the outcome-eligible call is nonterminal, it selects known failure and makes response/refusal material non-authoritative; after terminal ambiguity it preserves physical state, adds the typed fatal cause to any still-live attempt without losing prior causes, and after outstanding classification fails the turn when no other unacknowledged ambiguity remains or requires reconciliation otherwise. After current-authority completion during an active turn, it preserves call/history, appends typed invalidation, and stops new/outstanding work. Ordinary refusal without fatal stop terminalizes call/attempt/turn atomically; a continuation refusal raced under fatal stop remains physical and non-authoritative while failure/reconciliation controls. After terminal known failure/cancellation it preserves that state and existing turn-outcome precedence. After authority transfer it is non-authoritative; after valid turn terminalization it adds evidence without rewriting committed content. A future allowed fallback must create a separate call with its own exact target; it cannot legitimize substitution on this call. Absent provider evidence, Signalbox does not claim knowledge of the hidden physical backend.
- **Required invariants:** INV-014, INV-015, INV-017, INV-018.
- **Remaining questions:** Provider identifier normalization and reproducibility claims beyond observable model identity remain for ADR-0007; mismatch failure is decided by accepted ADR-0005.

## S22 — Apply an availability fallback

- **User intent:** If explicitly configured, continue through a classified capacity/availability failure using an allowed alternate model.
- **Durable commands:** Record the primary call's requested selection, exact hub-resolved target, provider-reported identity when available, and failure classification; evaluate explicit fallback policy; create a distinct model call with an exact hub-resolved fallback target and reason. Each call appends provider-reported identity or mismatch when available.
- **State transitions:** Primary call → known availability failure; turn/attempt → fallback eligible; fallback call → terminal.
- **Transient updates:** Client shows that fallback is being considered/applied.
- **Owning component:** Hub policy authorizes; provider adapters classify evidence but do not silently select targets.
- **Failure behavior:** If version one has no accepted fallback policy, stop explicitly instead. If fallback is later authorized, the alternate target is attempted only through its distinct call; a provider-reported mismatch against either call's own exact target follows ADR-0005's timing-sensitive failure rule and is never an allowed substitution. The scenario does not establish automatic fallback as accepted behavior.
- **Required invariants:** INV-014, INV-017, INV-018.
- **Remaining questions:** Whether fallback ships, qualifying failures, configuration, model-change identity, cost limits, and user confirmation.

## S23 — Encounter a model safety refusal

- **User intent:** Understand that the selected model refused and avoid hidden policy evasion.
- **Durable commands:** Persist the model call, requested selection, exact hub-resolved target, observable provider identity or mismatch when available, provider response classification, and refusal outcome; create any follow-up only through an explicit user/policy decision.
- **State transitions:** Without fatal stop, model call in flight → call refused, attempt turn-refused, and turn terminal/refused in one aggregate transition when target evidence does not mismatch. Serial orchestration requires all earlier work closed before the call and refusal creates no new dependency, so an ordinary refused-call/active-turn state is invalid. If a continuation already issued before another completed call's invalidation races refusal, the continuation remains physically refused inside `StopRequested(FatalMismatch)`, its content is non-authoritative, and the attempt/turn end only as fatal failure or reconciliation. Mismatch delivered with or before ordinary refusal commit instead makes the call known failed and leaves refusal non-authoritative; after terminal ambiguity it preserves that disposition, adds the fatal resolution to any still-live attempt, and after classification fails the turn only when no other unacknowledged ambiguity remains or requires reconciliation otherwise. A future remediation ADR may add a typed wait or continuation policy.
- **Transient updates:** Refusal text may stream but becomes authoritative only when committed.
- **Owning component:** Provider adapter reports; hub classifies and exposes provenance.
- **Failure behavior:** An ordinary authoritative refusal is not treated as successful completion or availability failure, does not automatically fall back merely because another model exists, and does not retain the active slot in an undefined settlement state. A physical refusal raced under fatal mismatch cannot override that failure. When mismatch is observed with or before ordinary refusal terminalization, or resolves terminal ambiguity, refusal material is audit-only; the turn fails when no other unacknowledged ambiguity remains and otherwise carries the exact fatal reconciliation marker. Mismatch first learned after a valid atomically refused turn adds reconciliation evidence without rewriting that disposition or committed refusal.
- **Required invariants:** INV-014, INV-017, INV-018, INV-032.
- **Remaining questions:** Refusal taxonomy, user-facing remediation, and whether any explicit fallback is ever allowed. Provider-identity normalization remains for ADR-0007, while mismatch disposition is accepted by ADR-0005.

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

The accepted foundation ADRs govern retry identity and baseline input lifecycle. Delegation cancellation, fallback, capability vocabulary, safety policy, queue management, archive behavior, and protocol choices remain open. An ADR that changes a lifecycle should update the affected scenarios and cite the invariant changes it requires.

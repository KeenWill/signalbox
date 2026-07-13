# Design scenarios

These scenarios test architectural boundaries; quoted commands and state names are descriptive pseudocode, not final APIs. “Durable commands” means owner intent the hub must commit before acknowledging, not a prescribed event-sourcing design. Invariant identifiers link to [the catalog](invariants.md).

## S01 — Create a new interactive session

- **User intent:** Start an empty conversation from a terminal and make it available on every client.
- **Durable commands:** `CreateSession(cause: user, ancestry: none, configuration)` followed by the first accepted `SubmitInput(..., after_current_turn)`.
- **State transitions:** No session → active session; accepted input → queued turn → eligible turn.
- **Transient updates:** Optimistic client placeholder and scheduling progress.
- **Owning component:** Hub owns creation and acceptance; Postgres stores the result; the client owns presentation.
- **Failure behavior:** Before commit, return a visible failure and no session identity. After commit, retrying the command returns the same result through idempotency rather than creating a duplicate.
- **Required invariants:** INV-001, INV-003, INV-007, INV-012.
- **Remaining questions:** Final names, creation-cause labels, idempotency scope, first client, and protocol.

## S02 — Stream a centrally called provider response

- **User intent:** Receive a responsive answer while retaining an authoritative final transcript.
- **Durable commands:** Accept input; create a turn and turn attempt; freeze a context frontier; resolve model selection; create a model call; finally commit assistant content and call outcome.
- **State transitions:** Turn eligible → progressing → completed; model call prepared → in flight → completed.
- **Transient updates:** Provider token deltas and progress are relayed as a replaceable draft.
- **Owning component:** The hub resolves and calls the provider; Postgres owns durable provenance and final content; clients render drafts.
- **Failure behavior:** A client disconnect does not cancel the call. A provider failure is recorded; any retry follows the still-open identity policy and never promotes a partial draft to final content.
- **Required invariants:** INV-005, INV-014, INV-015, INV-032, INV-035.
- **Remaining questions:** Provider retry boundaries, streaming checkpoints, browser transport, and assistant-message commit granularity.

## S03 — Hub restarts after accepting queued work

- **User intent:** Trust an acknowledgement even if the service restarts before work starts.
- **Durable commands:** Persist the message, delivery policy, and enough information to recover its pending treatment before acknowledgement; persist logical work and effective configuration at the boundary later selected by ADR-0027.
- **State transitions:** Accepted pending treatment or queued work remains durably represented across restart; recovery applies the eventual queued-turn rules, makes eligible work eligible, and begins an authorized attempt.
- **Transient updates:** Pre-restart queue position and process-local wakeups disappear and are reconstructed.
- **Owning component:** Hub recovery and scheduler coordinate from Postgres.
- **Failure behavior:** Work eventually continues, fails explicitly, is canceled, or requests reconciliation; it never silently vanishes. Duplicate recovery scans do not create duplicate turns.
- **Required invariants:** INV-007–INV-012, INV-034.
- **Remaining questions:** Postgres scheduler mechanics, active-state definition, wake-up strategy, retry identity if an attempt had only been prepared, and the ADR-0027 work-creation/configuration boundary exercised by S09.

## S04 — Hub restarts during a provider call

- **User intent:** Recover honestly without claiming to resume the lost network stream.
- **Durable commands:** Before send, persist model-call identity, exact hub-resolved provider/model target, frontier, and in-flight state; after restart, record the observed interruption classification and a recovery decision.
- **State transitions:** Model call in flight → lost/known failed/ambiguous; turn attempt → interrupted; logical turn → recoverable, failed, or reconciliation-required according to policy.
- **Transient updates:** Uncommitted deltas and the live provider connection are lost; clients replace drafts from an authoritative snapshot.
- **Owning component:** Hub provider adapter reports evidence; hub recovery classifies it; Postgres records it.
- **Failure behavior:** Do not imply exact-token continuation. Another external call is a new model-call record, even if it advances the same turn.
- **Required invariants:** INV-004, INV-014–INV-018, INV-032, INV-034.
- **Remaining questions:** Whether provider request identifiers make the outcome knowable, call retry rules, and when recovery creates a new turn attempt.

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
- **State transitions:** Tool attempt in flight → ambiguous; tool request/turn → reconciliation required rather than automatically retrying.
- **Transient updates:** Last progress text may be shown only as non-authoritative evidence.
- **Owning component:** Hub classifies and blocks automatic retry; the selected runner may later provide evidence; the owner resolves uncertainty.
- **Failure behavior:** No blind retry and no claim that interrupt or disconnect undid the effect. Reconciliation records why the final classification changed.
- **Required invariants:** INV-019, INV-021, INV-025, INV-026, INV-029, INV-034.
- **Remaining questions:** Reconciliation workflow, idempotency-key support, effect taxonomy, and who may mark an ambiguous attempt resolved.

## S07 — Submit an interrupting message

- **User intent:** Stop current progress and begin different logical work from new input.
- **Durable commands:** Persist the interrupting message with `interrupt`; record cancellation request against active work; create new logical work at the appropriate frontier.
- **State transitions:** Active turn → cancellation requested → canceled/failed/ambiguous terminal handling; new turn → queued/eligible.
- **Transient updates:** Cancellation signals to provider or runner and “stopping” progress.
- **Owning component:** Hub owns ordering and state; adapters attempt prompt cancellation; client states intent.
- **Failure behavior:** Issued effects are not rolled back. Their attempts receive honest terminal or ambiguous states before replacement work relies on them.
- **Required invariants:** INV-007, INV-009, INV-025, INV-028, INV-029.
- **Remaining questions:** ADR-0027 must define the exact transcript frontier for the new work. ADR-0004/ADR-0005 cover cancellation and provider outcome handling; ADR-0002 covers child propagation.

## S08 — Submit safe-point steering

- **User intent:** Refine active work without creating a separate future turn.
- **Durable commands:** Persist the message with `next_safe_point`; record its pending relationship to the active turn; when the future safe-point policy makes it eligible for a provider call, create a new frontier that includes it.
- **State transitions:** Steering message accepted → pending safe point → consumed or explicitly reclassified according to future policy; the active logical turn's slot behavior remains open.
- **Transient updates:** Client shows “will apply at next safe point”; no mutation of the current provider stream.
- **Owning component:** Hub decides safe-point boundaries and builds context; clients only request and display treatment.
- **Failure behavior:** Restart preserves pending steering. If the turn becomes terminal before consumption, the hub does not silently drop the message; its exposure or reclassification follows the unresolved input-delivery lifecycle policy.
- **Required invariants:** INV-007, INV-015, INV-016, INV-028, INV-034.
- **Remaining questions:** ADR-0027 must define the safe-point set, terminal-before-consumption behavior, whether a tool or orchestration step may consume steering before another model call, and how unconsumed steering is exposed or reclassified. ADR-0010 must decide whether pending steering retains the session's single-progressing-turn slot.

## S09 — Queue input for the next turn

- **User intent:** Let current work finish, then process a new message separately.
- **Durable commands:** Persist the message with `after_current_turn` and its queue position; create or reserve new logical work under the eventual command model.
- **State transitions:** Message accepted/queued while turn A progresses; after A terminal, turn B becomes eligible.
- **Transient updates:** Queue position and projected start time may change.
- **Owning component:** Hub owns durable ordering and scheduler eligibility.
- **Failure behavior:** Restart preserves order. Cancellation of A does not erase B; policy determines the context B sees after A's terminal outcome.
- **Required invariants:** INV-007, INV-009, INV-010, INV-012, INV-028.
- **Remaining questions:** ADR-0027 must define when logical work and effective configuration are fixed and which context frontier follows each predecessor outcome. Editing, cancellation, reordering, and delivery-policy changes for queued input are explicitly later client-queue scope under ADR-0027 unless brought forward. The accepted input and enough information to recover its pending treatment are durable immediately regardless.

## S10 — Approve a risky tool

- **User intent:** Permit one clearly presented risky operation.
- **Durable commands:** Create exact tool request; record policy result `confirmation_required`; persist owner approval bound to request, normalized arguments, and material constraints; create an authorized attempt.
- **State transitions:** Tool request proposed → awaiting approval → approved → dispatched/completed.
- **Transient updates:** Confirmation prompt delivery and executor progress.
- **Owning component:** Hub owns policy and approval record; client authenticates and presents; selected executor performs the attempt.
- **Failure behavior:** A hub restart leaves the request waiting without inventing a decision. Duplicate approval is idempotent. Changed arguments or placement constraints require reevaluation and cannot reuse approval.
- **Required invariants:** INV-019, INV-020, INV-024, INV-027.
- **Remaining questions:** Approval expiry, risk classification, scoped standing grants, material constraints, and automated-judge influence.

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
- **Required invariants:** INV-011, INV-012, INV-021, INV-033.
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
- **Durable commands:** Create child with delegation cause and parent-work relation; persist task input; record parent wait/reference; later persist an explicit result delivery.
- **State transitions:** Child created → queued/progressing → terminal; parent turn → waiting for delegated result → resumed or terminal.
- **Transient updates:** Child progress summaries and presence indicators.
- **Owning component:** Hub owns relationships and scheduling; each session retains independent history.
- **Failure behavior:** Restart restores both child state and parent wait. Child failure is delivered explicitly rather than disappearing into parent UI state.
- **Required invariants:** INV-003, INV-010, INV-031, INV-034.
- **Remaining questions:** Result representation, cancellation propagation, detached work, resource limits, and whether parent waiting blocks other queued input.

## S19 — Cancel a parent while child work is active

- **User intent:** Stop parent work with a clear understanding of what happens to the child.
- **Durable commands:** Persist parent cancellation and an explicit child-disposition decision once policy is selected; record cancellation signals and resulting outcomes separately.
- **State transitions:** Parent progressing/waiting → cancellation requested → terminal; child may remain active, receive cancellation, or require an owner decision.
- **Transient updates:** Cancellation progress for each physical attempt.
- **Owning component:** Hub applies the eventual delegation policy; executors only respond to cancellation requests.
- **Failure behavior:** Already-issued effects are not undone. The child never silently disappears, and ambiguous child effects remain reconcilable.
- **Required invariants:** INV-010, INV-025, INV-026, INV-029, INV-031, INV-034.
- **Remaining questions:** This scenario intentionally does not choose propagation, detached-child support, result delivery after parent termination, or archival coupling.

## S20 — Resolve a curated model alias

- **User intent:** Use a convenient selection such as “latest preferred” while retaining precise requested, resolved, and provider-reported provenance.
- **Durable commands:** Persist requested alias and effective configuration; at call time resolve it through hub policy; persist the exact hub-resolved provider/model target and resolution metadata before send; append observable provider identity or mismatch when available.
- **State transitions:** Model call prepared with selection → resolved exact target → in flight → terminal.
- **Transient updates:** Client may show current alias target, clearly separate from historical call facts.
- **Owning component:** Hub model resolver and provider adapter; Postgres stores per-call provenance.
- **Failure behavior:** Alias changes never rewrite previous calls. Resolution failure is explicit and does not silently choose another model. Historical provenance does not claim which hidden physical backend executed the call when the provider does not reveal it.
- **Required invariants:** INV-008, INV-014, INV-017.
- **Remaining questions:** Alias versioning, cache/transaction boundaries, visibility, and whether alias policy may include fallback.

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
- **State transitions:** Model call in flight → refused; turn may complete with refusal, wait for input, or follow a future explicit policy.
- **Transient updates:** Refusal text may stream but becomes authoritative only when committed.
- **Owning component:** Provider adapter reports; hub classifies and exposes provenance.
- **Failure behavior:** Refusal is not treated as availability failure and does not automatically fall back merely because another model exists.
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

## Coverage note

The scenarios deliberately leave retry identity, delegation cancellation, fallback, capability vocabulary, safety policy, and protocol choices open. An ADR that changes a lifecycle should update the affected scenarios and cite the invariant changes it requires.

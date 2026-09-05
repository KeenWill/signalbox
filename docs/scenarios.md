# Design scenarios

These scenarios test architectural boundaries; quoted commands and state names
are descriptive pseudocode, not final APIs. “Durable commands” means user intent
the daemon must commit before acknowledging, not a prescribed event-sourcing
design. Invariant identifiers link to [the catalog](invariants.md).

The scenarios are frozen design fixtures. New or changed normative behavior
belongs in the record that owns it (the owning [spec page](spec/README.md) or
implemented test); a scenario's normative content changes or is added only
alongside the maintainer-accepted change that motivates it, and a change
introducing a new lifecycle edge adds or amends its scenario fixture in the same
change. Test coverage is recorded outside this document: tests name the scenario
identifiers they enforce under the rules in [AGENTS.md](../AGENTS.md) and
[testing-style.md](agents/testing-style.md). The
[invariant test index](invariants.md) is generated separately from corresponding
INV-tagged test names and attached doc comments.

## S01 — Create a new interactive session

- **User intent:** Start an empty conversation from a terminal and make it
  available on every client.
- **Durable commands:**
  `CreateSession(cause: interactive, ancestry: none, initial_configuration_defaults)`
  establishes defaults version one.
  `SubmitInput(delivery: start_when_no_active_turn, ...)` resolves its model
  request against that exact version and atomically persists the accepted input,
  origin turn, and complete baseline configuration provenance. A later defaults
  update affects only subsequently accepted origins.
- **State transitions:** No session → durable session with immutable
  user/no-ancestry provenance and complete current defaults version one; no turn
  → queued origin turn with derived eligibility → atomically fixed starting
  frontier plus `Active(Running)` and initial prepared attempt.
- **Transient updates:** Optimistic client placeholder and scheduling progress.
- **Owning component:** Daemon owns creation and acceptance; Postgres stores the
  result; the client owns presentation.
- **Failure behavior:** A malformed transport, pre-authority, unconstructible
  typed-command, or pre-commit infrastructure failure that does not reach
  committed domain handling returns visibly and claims no command identifier;
  corrected boundary input may reuse it. Canonically equivalent caller forms
  compare as the same command. The first committed handling of a well-formed
  typed command under established user authority records either its applied
  result or a typed domain rejection. Replay returns that same result before
  current-state validation; a command rejected after construction cannot later
  become valid under the same identifier. Corrected domain intent then needs a
  new identifier, and reuse of a claimed identifier for another command kind,
  session, or payload is rejected rather than creating a duplicate. The terminal
  client prints a generated command identity before I/O and accepts it again; an
  ambiguous submit is retried with that identity and the same caller-observed
  defaults version rather than silently becoming new work.
- **Required invariants:** INV-001, INV-003, INV-007, INV-008, INV-012.
- **Remaining questions:** Authenticated remote and browser clients remain
  [open](open-questions.md#protocols-and-persistence); the user-global
  idempotency scope, the typed relational command representation, and actor
  attribution in the canonical command payload with its replay equality are
  decided in [identity-and-commands](spec/identity-and-commands.md); the
  baseline accepted-input content value, the long-lived session aggregate, and
  the current-pointer load boundary are decided in
  [sessions-and-transcript](spec/sessions-and-transcript.md).

## S02 — Stream a centrally called provider response

- **User intent:** Receive a responsive answer while retaining an authoritative
  final transcript.
- **Durable commands:** Accept input and create a turn with frozen direct or
  alias model-selection configuration and resolved model settings; activate it
  and create a turn attempt; freeze a context frontier; resolve and pin the
  exact model target and credential profile; only then create a model call;
  finally commit the complete ordered assistant-text and logical
  tool-use-reference sequence with its producing-call provenance and the call
  outcome. A tool-using response yields the attempt but retains the same turn;
  result projection and continuation create later rounds. Only a response with
  no tools ends the attempt and turn and appends the completed-turn marker last.
- **State transitions:** Turn queued with derived eligibility → active/running
  with exactly one current attempt → zero or more tool-yield/wait/continuation
  rounds → terminal/completed only after a no-tool call is classified and its
  attempt ends; each model call is prepared → in flight → terminal/completed.
- **Transient updates:** Client presentation follows the durable-update and
  authoritative-replacement contract in
  [process-protocol](spec/process-protocol.md). Provider-token relay remains
  [open](open-questions.md#protocols-and-persistence).
- **Owning component:** The daemon resolves and calls the provider; Postgres
  owns durable provenance and final content; clients render drafts.
- **Failure behavior:** A client disconnect does not cancel the call.
  Target-resolution failure creates no model call. Send preparation failure
  leaves the already-created call known-failed. No durable authorization is
  retried, and an ambiguous outcome never creates a successor. When pool policy
  selects `switch_now`, a proven availability failure may create the S22
  successor on a new attempt against the same target and a different credential
  profile, under [availability successor calls](spec/model-call-execution.md),
  [the credential-availability machine](spec/credential-availability.md), and
  [credential pools and selection](spec/configuration-and-credentials.md#overview).
  No partial draft becomes final content. A later authorized call must retain
  steering already committed to turn history.
- **Required invariants:** INV-005, INV-008, INV-014, INV-015, INV-032, INV-035.
- **Remaining questions:** Whether a future known-failure retry command is
  introduced, streaming checkpoints, transient provider-delta relay, browser
  transport, rich assistant content, provider/client rendering, and transient
  presentation while an availability successor is selected.

## S03 — Daemon restarts after accepting queued work

- **User intent:** Trust an acknowledgement even if the service restarts before
  work starts.
- **Durable commands:** Under the accepted input-delivery semantics
  ([turn-lifecycle-and-scheduling](spec/turn-lifecycle-and-scheduling.md)),
  persist accepted input, origin turn, immutable acceptance position, any typed
  priority relation, and frozen baseline model-configuration provenance in one
  transaction before acknowledgement.
- **State transitions:** Queued turn and order facts remain durable across
  restart; eligibility is recomputed from the total order and slot ownership.
  After every earlier turn becomes terminal, one transaction fixes the exact
  immediate-predecessor starting lineage and outcome-aware frontier, then either
  activates the turn with an initial attempt or terminalizes it as failed if its
  frozen configuration cannot execute.
- **Transient updates:** Pre-restart queue position and process-local wakeups
  disappear and are reconstructed.
- **Owning component:** Daemon recovery and scheduler coordinate from Postgres.
- **Failure behavior:** Work eventually continues, fails explicitly, is
  canceled, or requests reconciliation; it never silently vanishes. Duplicate
  recovery scans do not create duplicate turns.
- **Required invariants:** INV-007–INV-012, INV-034.
- **Remaining questions:** Scheduler sweep tuning and the optional scheduler
  safeguards listed as open edges in
  [turn-lifecycle-and-scheduling](spec/turn-lifecycle-and-scheduling.md).
  Whether an individual provider call or tool attempt crossed its persisted
  issue boundary is classified by its own evidence; an attempt still in
  `Prepared` has not crossed the orchestration boundary.

## S04 — Daemon restarts during a provider call

- **User intent:** Recover honestly without claiming to resume the lost network
  stream.
- **Durable commands:** Before send, persist model-call identity, exact
  daemon-resolved provider/model target, frontier, and in-flight state; after
  restart, an idempotent startup scan records the recovered outcome
  classification. Definitive recovered success that terminalizes the turn
  commits the complete assistant sequence and completed-turn marker under the
  same atomic boundary as live completion; a partial draft never does. A
  response that would introduce unfinished tool work remains outside the first
  slice until its owning decision defines the required recovery transition. An
  user decision, if needed, is a separate command bound to the exact
  ambiguous-operation set.
- **State transitions:** The startup scan derives the complete evidence and
  stop-cause set before ending the old turn attempt with disposition `Lost` in
  the matching terminal variant. A model call in flight becomes completed, known
  failed, refused where evidence supports it, cancelled, or ambiguous.
  Unacknowledged ambiguity first leaves the turn active awaiting the exact
  reference only when neither an applied interrupt nor fatal mismatch prohibits
  continuation; otherwise it yields `ReconciliationRequired` with the exact set
  and matching interrupt or fatal reason. Without blocking ambiguity or fatal
  stop, outcome-authoritative non-mismatched completion or atomic refusal may
  control; otherwise known failure follows. Only an applied interrupt proof for
  that predecessor plus proof that it prevented all remaining work produces turn
  cancellation; cause-free physical cancellation produces turn failure.
  Completion/refusal raced under a fatal cause remains physical but
  non-authoritative. Resolving mismatch evidence after a call already became
  ambiguous preserves that physical disposition and contributes
  `TerminalAmbiguityResolution` to the complete recovered fatal causes, yielding
  turn failure when no other ambiguity remains and exact fatal reconciliation
  otherwise. A user may preserve an ambiguous call while separately accepting
  duplicate risk only when no fatal invalidation exists.
- **Transient updates:** Uncommitted deltas and the live provider connection are
  lost; clients replace drafts from an authoritative snapshot.
- **Owning component:** Daemon provider adapter reports evidence; daemon
  recovery classifies it; Postgres records it.
- **Failure behavior:** Do not imply exact-token continuation. Startup creates
  no cancellation-only or classification-only attempt. It classifies an
  uncertain call `Ambiguous` while ending the abandoned attempt in the matching
  `...Lost` branch; live classification ends the attempt in the matching
  `...Ambiguous` branch only after every other issued operation is classified,
  and until then the dispatch guard permits no new semantic effect. Nonfatal
  evidence can clear the blocking ambiguity without rewriting the call; after
  all issued work is classified, the same still-live attempt may continue
  without repeating it. Version one does not retry a provider call after
  resolving evidence establishes known failure or cancellation; those
  classifications terminalize under the common precedence. The first trusted
  mismatch observed while the outcome-eligible call is nonterminal immediately
  selects known failure, makes response material non-authoritative, and requests
  best-effort cancellation. After terminal ambiguity it leaves the call
  `Ambiguous`; a still-live attempt gains `TerminalAmbiguityResolution` in its
  complete fatal causes, while an already-ended attempt remains unchanged. After
  outstanding classification the turn fails when no other unacknowledged
  ambiguity remains and requires reconciliation otherwise. After
  current-authority completion during an active turn it preserves call/history,
  appends typed invalidation, and stops new/outstanding work before failure or
  reconciliation; startup preserves the abandoned attempt as `Lost` in the
  terminal variant carrying every prior or same-scan stop cause. Without fatal
  stop, a non-mismatched refusal terminalizes call and turn atomically; a
  continuation refusal raced under fatal stop remains physical evidence while
  the turn fails or reconciles. After terminal known failure/cancellation it
  preserves existing state and precedence. After authority transfer it is
  non-authoritative evidence; after valid turn terminalization it cannot rewrite
  committed content or successor context. Another provider call may remain in
  the same turn only after an exact-set user decision accepts unresolved
  duplicate risk while preserving origin, configuration, context, and evidence,
  and no fatal invalidation exists. That decision atomically closes the wait,
  consumes all eligible steering into the replacement frontier, creates the
  replacement attempt and prepared call, and transfers outcome authority.
  Resolving evidence that commits first makes the decision stale; any later
  prior-call outcome remains audit/reconciliation evidence only. The
  accepted-risk marker remains visible.
- **Required invariants:** INV-004, INV-009, INV-014–INV-018, INV-032, INV-034.
- **Remaining questions:** Whether provider request identifiers make the outcome
  knowable and how any request-status polling participates in recovery.

## S05 — Runner disconnects during a harmless tool

- **User intent:** Complete a read-only workspace query despite runner loss.
- **Durable commands:** Create and authorize a logical tool request; create a
  tool attempt; dispatch with runner, execution-boundary snapshot, and
  generation; classify the disconnect.
- **State transitions:** Prepared or effect-free in-flight tool attempt →
  terminal/known-failed after crash loss; the abandoned turn attempt ends lost
  and the turn fails after proposal-ordered `ToolClosed` materialization for
  every request without an ordinary result.
- **Transient updates:** Runner heartbeat, command progress, and partial stdout
  may disappear.
- **Owning component:** Daemon owns policy and recovery; runner owns physical
  execution; scheduler owns dispatch selection.
- **Failure behavior:** Version one performs no automatic retry, even for an
  effect-free operation. A late result is stale and cannot overwrite terminal
  evidence.
- **Required invariants:** INV-011, INV-021, INV-024–INV-026, INV-034.
- **Remaining questions:** Future explicit retry commands, runner evidence,
  output deduplication, and remote fencing representation.

## S06 — Runner disconnects during a potentially irreversible tool

- **User intent:** Avoid accidentally repeating an external write whose result
  was lost.
- **Durable commands:** Persist the approved tool request, attempt, dispatch
  generation, and disconnect evidence; record `ambiguous` when completion cannot
  be established.
- **State transitions:** When classified while orchestration is live, tool
  attempt in flight → terminal/ambiguous and current turn attempt → the matching
  `...Ambiguous` branch. If the daemon crashes before classification, startup
  makes the tool attempt terminal/ambiguous while ending the abandoned turn
  attempt in the matching `...Lost` branch. The physical tool outcome remains
  `Ambiguous` in both cases. When neither an applied interrupt nor fatal
  mismatch prohibits continuation, the turn retains its slot in
  `AwaitingRecoveryDecision` carrying that tool-attempt reference. The
  implemented applied-interrupt path preserves the ambiguity while terminalizing
  as `ReconciliationRequired` with the exact attempt and proof.
- **Transient updates:** Last progress text may be shown only as
  non-authoritative evidence.
- **Owning component:** Daemon classifies and blocks automatic retry; the
  selected runner may later provide evidence; the user resolves uncertainty.
- **Failure behavior:** No blind retry and no claim that interrupt or disconnect
  undid the effect. Version one has no writer for resolving evidence or
  accepted-risk continuation; the parked wait retains its slot until an applied
  interrupt terminalizes it.
- **Required invariants:** INV-009, INV-019, INV-021, INV-025, INV-026, INV-029,
  INV-034.
- **Remaining questions:** Reconciliation workflow, idempotency-key support, who
  may record separate resolving evidence for terminal ambiguity, and which tool
  effects permit accepted-risk continuation.

## S07 — Submit an interrupting message

- **User intent:** Stop current progress and begin different logical work from
  new input.
- **Durable commands:** Under the accepted interrupt-delivery semantics
  ([turn-lifecycle-and-scheduling](spec/turn-lifecycle-and-scheduling.md)),
  atomically persist the interrupting accepted input, successor configuration
  provenance, typed priority relation designating its turn as the active turn's
  immediate successor, and `AppliedInterruptProof` tied to the exact
  predecessor, plus its transition: end an unsent prepared attempt; directly end
  running work only when prevention of remaining work and every terminal guard
  are already proven, otherwise request cancellation; or close the exact durable
  wait.
- **State transitions:** `Prepared` → ended/cancelled with the predecessor
  terminal/cancelled and exact applied-interrupt proof. `Running` → directly
  ended/cancelled only when every guard already holds, otherwise
  `StopRequested(CancellationOnly)` with that proof while retaining the exact
  attempt and slot. An approval wait remains parked until its canonical decision
  command resolves the approval obligation; deny-and-end records the denial
  first, then applies the interrupt after decision progression opens execution.
  The two commands are not one atomic selection; after execution opens, the
  ordinary dispatch-gate race determines whether remaining work or the interrupt
  commits first. Recovery wait → reconciliation-required with that proof and the
  wait's exact operation set. Every direct terminal path atomically records the
  interrupt-created immediate successor and reclassifies pending steering before
  releasing the slot. If fatal mismatch already requested stop, the first
  interrupt populates its interrupt field without reauthorizing work; either
  event order preserves both facts. A running predecessor then uses the common
  precedence: unacknowledged ambiguity yields the exact interrupt/fatal
  reconciliation marker; otherwise sufficient outcome-authoritative
  non-mismatched completion or atomic refusal controls only without fatal stop,
  followed by known failure or applied-and-confirmed interrupt cancellation. A
  raced completion/refusal under fatal stop remains non-authoritative. Resolving
  mismatch evidence after terminal ambiguity preserves that operation state,
  producing failure when no other ambiguity remains and exact fatal
  reconciliation otherwise. An already accepted ambiguity risk remains marked
  while interruption is classified normally. The interrupt-created turn is
  always the immediate queued successor; no standalone active-turn cancellation
  exists in the baseline.
- **Transient updates:** Cancellation signals to provider or runner and
  “stopping” progress.
- **Owning component:** Daemon owns ordering and state; adapters attempt prompt
  cancellation; client states intent.
- **Failure behavior:** Issued effects are not rolled back. The interrupted turn
  retains the progressing slot until every issued operation is classified, its
  current attempt ends, and any wait is closed. If the daemon restarts, the
  startup scan ends the abandoned attempt and classifies operations without
  creating a replacement; the applied interrupt plus unacknowledged ambiguity
  produces a proof-bearing reconciliation marker, while previously accepted risk
  remains explicitly marked under an ordinary terminal outcome. Before releasing
  the slot, terminalization durably inserts any reclassified steering after the
  interrupt successor by original acceptance order. No queued successor has
  fixed a direct predecessor yet, so each later frontier includes every inserted
  turn.
- **Required invariants:** INV-007–INV-009, INV-012, INV-025, INV-028, INV-029.
- **Remaining questions:** Provider/tool-specific cancellation evidence remains
  open. Delegated-child propagation follows the explicit relationship policy and
  user-selected scope owned by
  [S19](#s19--cancel-a-parent-while-child-work-is-active).

## S08 — Submit safe-point steering

- **User intent:** Refine active work without creating a separate future turn.
- **Durable commands:** Persist the input with `next_safe_point`, acceptance
  order, and one binding referencing the source active turn; the command carries
  no independent configuration request or copied configuration. The source turn
  remains the canonical immutable configuration source if reclassification is
  needed. After every earlier issued physical operation is classified, every
  earlier tool/approval dependency has a durable outcome, and immediately before
  any later model call—including a duplicate-risk replacement—atomically commit
  it to turn semantic history, include it in that call's frontier, and record
  consumption by call identity.
- **State transitions:** Accepted input → pending steering → consumed by a later
  model call, or visibly reclassified as a queued turn origin if the target turn
  terminates first. Every active wait retains the turn's session slot under the
  accepted turn lifecycle
  ([turn-lifecycle-and-scheduling](spec/turn-lifecycle-and-scheduling.md)).
- **Transient updates:** Client shows “will apply at next safe point”; no
  mutation of the current provider stream.
- **Owning component:** Daemon decides safe-point boundaries and builds context;
  clients only request and display treatment.
- **Failure behavior:** Restart preserves the single steering binding. It cannot
  be consumed while an earlier call or tool attempt is unclassified. If an
  ambiguity decision prepares a replacement call, that transaction must consume
  the input; restart cannot reconstruct it as both pending and consumed. If the
  consuming call later fails, every future authorized call retains the steering.
  If the turn becomes terminal before consumption, the terminal transaction
  creates queued work with captured inherited provenance and durable order
  facts; it does not invent a request or reread session defaults. Any
  interrupt-created successor is first; later work follows original acceptance
  order.
- **Required invariants:** INV-007–INV-009, INV-015, INV-016, INV-028, INV-034.
- **Remaining questions:** Future safe-point kinds and client rendering of
  reclassification. Version one does not let tool or orchestration steps consume
  steering directly.

## S09 — Queue input for the next turn

- **User intent:** Let current work finish, then process a new message
  separately.
- **Durable commands:** Persist the accepted input with `after_current_turn`,
  immutable acceptance position, its origin turn, and frozen baseline model
  configuration in one transaction; do not freeze a direct predecessor while
  priority insertions remain possible.
- **State transitions:** Turn B is queued while turn A remains active. When B
  eventually becomes eligible, it fixes starting lineage and an outcome-aware
  frontier through the terminal turn immediately before it in durable order,
  which may be an interrupt or reclassified-steering turn inserted after B's
  acceptance.
- **Transient updates:** Queue position and projected start time may change.
- **Owning component:** Daemon owns durable ordering and scheduler eligibility.
- **Failure behavior:** Restart preserves order facts, identity, and
  configuration. Cancellation of A does not erase B. An interrupt-created
  successor precedes B and all reclassified steering; after the interrupt,
  queued and reclassified inputs retain acceptance order. B waits for every
  earlier ordered turn, then fixes its frontier through its actual immediate
  predecessor so none of those outcomes is omitted. If B itself cannot execute,
  it fixes that same complete frontier before failing, so later C cannot omit B
  or any inserted work.
- **Required invariants:** INV-007–INV-010, INV-012, INV-028.
- **Remaining questions:** Queue admission/resource limits and the semantic
  rendering of outcome markers. Editing, cancellation, reordering,
  delivery-policy change, and configuration change remain explicitly unsupported
  baseline operations.

## S10 — Approve a risky tool

- **User intent:** Permit one clearly presented risky operation.
- **Durable commands:** Create the exact content-authoritative tool request;
  record fail-closed `confirmation_required`; persist a user-global approval
  command bound to that request; then create a new turn attempt for the
  authorized-but-not-yet-dispatched batch. Tool-attempt identity is created only
  at later physical dispatch.
- **State transitions:** Tool request proposed under the running turn's required
  current attempt → every previously issued operation is terminally classified →
  that attempt yields and the turn retains its active slot while awaiting
  approval with no live attempt → approval atomically creates a new turn attempt
  → tool dispatched/completed.
- **Transient updates:** Confirmation prompt delivery and executor progress.
- **Owning component:** Daemon owns policy and approval record; client
  authenticates and presents; selected executor performs the attempt.
- **Failure behavior:** A daemon restart leaves the request waiting without
  inventing a decision. Duplicate approval is idempotent. Changed arguments or
  placement constraints require reevaluation and cannot reuse approval. After
  approval, an authorized request remains a blocking logical dependency while
  runner scheduling is delayed; orchestration cannot consume steering or prepare
  a later model call until the request has a durable outcome.
- **Required invariants:** INV-009, INV-010, INV-012, INV-019, INV-020, INV-024,
  INV-027.
- **Remaining questions:** Approval expiry, per-tool session overrides and
  high-risk guardrails, material constraints, and automated-judge mechanics.

## S11 — Deny a risky tool

- **User intent:** Prevent the proposed effect while allowing the conversation
  to continue safely.
- **Durable commands:** Persist denial bound to the exact request. At the
  continuation or stop boundary append a reference-only denial result entry in
  proposal order.
- **State transitions:** Awaiting approval → denial closes the exact wait and
  atomically creates a new turn attempt when the batch is decision-complete →
  the continuation boundary commits proposal-ordered result history without a
  tool attempt → orchestration later reaches an ordinary terminal disposition.
- **Transient updates:** Prompt closes and clients receive status.
- **Owning component:** Daemon owns denial and prevents dispatch; client
  captures the user's decision.
- **Failure behavior:** No physical tool attempt is created. The new turn
  attempt exists only to continue conversational orchestration with the denial
  outcome. Duplicate or delayed approval messages cannot reverse the denial
  without an explicit new decision path. Deny-and-end records this same denial
  and resolves every earlier approval-order obligation, then composes the
  existing applied-interrupt stop path after decision progression opens
  execution; it does not invent a second cancellation authority or treat an
  interrupt as an approval decision. The composition does not promise that an
  interrupt submitted after execution opens prevents already-eligible work.
- **Required invariants:** INV-009, INV-012, INV-019, INV-020, INV-027.
- **Remaining questions:** Whether future reconsideration creates a new request.
  Baseline continuation in a new turn attempt is decided by
  [tool-loop](spec/tool-loop.md).

## S12 — Receive a stale or duplicated runner result

- **User intent:** Trust current state despite delayed or retried transport
  delivery.
- **Durable commands:** For daemon-local execution, validate the result envelope
  against the authorized tool-attempt dispatch correlation. For runner
  execution, additionally validate the exact runner lease identity, runner
  identity, tool name, and lease-lineage generation; record duplicate/stale
  evidence if audit policy requires, without applying it again.
- **State transitions:** Current work remains unchanged; a first valid current
  result may advance exactly once.
- **Transient updates:** Runner acknowledgement may state “duplicate” or
  “stale.”
- **Owning component:** Daemon transaction and database constraints enforce
  fencing; runner retries delivery until acknowledged.
- **Failure behavior:** A stale success cannot overwrite a newer failure,
  result, cancellation, or reconciliation state.
- **Required invariants:** INV-011, INV-012, INV-021, INV-043.
- **Remaining questions:** Fence representation, retention of rejected evidence,
  result acknowledgement, compatibility, and subscriber observation remain
  [open](open-questions.md#scheduling-and-runners): the retired protocol designs
  carry no current authority, and future protocol work is designed fresh as a
  specification diff; the committing-side update mechanism is decided in
  [persistence-protocol](spec/persistence-protocol.md).

## S13 — Use an ambient-user runner

- **User intent:** Intentionally run a workspace tool with the same OS authority
  as the user.
- **Durable commands:** Select the runner explicitly; snapshot the declared,
  configured, and verified evidence relevant to the attempt together with the
  effective ambient boundary; apply tool policy and approval rules.
- **State transitions:** Eligible tool request → placement selected as ambient →
  authorized/denied → attempted.
- **Transient updates:** UI warning and runner availability.
- **Owning component:** Runner declares its properties; deployment configuration
  and any accepted verification supply other evidence; daemon derives placement
  and policy; client displays only the effective boundary it may rely on.
- **Failure behavior:** An unsupported isolation claim does not change the
  effective ambient boundary, and the system never labels this runner isolated
  on the strength of that claim. Loss or side effects follow the same ambiguity
  rules, potentially with stricter confirmation.
- **Required invariants:** INV-019, INV-024–INV-026.
- **Remaining questions:** Required warnings, policy differences,
  verification/attestation, and minimum sandbox requirements for other profiles.

## S14 — Use a restricted runner

- **User intent:** Execute in a deliberately constrained account, container,
  sandbox, or VM.
- **Durable commands:** Select a runner whose effective typed properties satisfy
  the request; persist the relevant declarations, configuration, verified
  evidence, effective-boundary snapshot, and dispatch.
- **State transitions:** Placement evaluation → restricted runner selected →
  authorized attempt → outcome.
- **Transient updates:** Resource use and progress reported by the runner.
- **Owning component:** Deployment supplies the controls and trusted
  configuration; runner declares properties; accepted mechanisms may verify
  them; daemon derives and records effective properties; client explains the
  effective boundary and evidence level.
- **Failure behavior:** A missing or insufficiently evidenced property fails
  restricted placement explicitly. A “restricted” label or runner declaration
  alone cannot justify a stronger execution guarantee.
- **Required invariants:** INV-021–INV-024.
- **Remaining questions:** Capability schema, attestation, minimum profiles,
  resource limits, and whether constraints can change during a connection.

## S15 — Execute a daemon-local tool

- **User intent:** Use a centrally available integration such as documentation
  lookup.
- **Durable commands:** Create a logical tool request, evaluate daemon policy,
  create and fence a daemon-local attempt, and persist its result/outcome once.
  The initial `current_time` tool defaults to auto approval and is effect-free.
- **State transitions:** Tool request → authorized/denied → daemon-local in
  flight → terminal; turn consumes the durable logical result.
- **Transient updates:** Search progress or partial presentation that is not
  conversation truth.
- **Owning component:** Daemon owns policy and history; a daemon-local adapter
  executes under central credentials. A workspace-root-bound adapter executes
  against the root the requesting session bound: its own derived root where the
  deployment provisioned one, so two such sessions share no tree, and otherwise
  the configured root that every session shares.
- **Failure behavior:** Adapter loss is classified with the same known/ambiguous
  distinction; central placement does not imply safe automatic retry. A session
  whose derived workspace cannot be composed receives a known tool failure and
  is never redirected to another session's root.
- **Required invariants:** INV-019, INV-024–INV-027, INV-035.
- **Remaining questions:** Credential scoping, isolation between a daemon-local
  adapter and the credentials it holds, and whether centrally hosted MCP is one
  adapter type.

## S16 — Execute a runner-local tool

- **User intent:** Operate on state available only in a workspace or
  machine-local application.
- **Durable commands:** Create and authorize the tool request; select/pin a
  runner; persist boundary snapshot; create and fence the attempt; accept a
  validated result.
- **State transitions:** Tool request → placement pending → runner dispatched →
  known failed when evidence proves no effect, otherwise
  completed/cancelled/ambiguous according to evidence.
- **Transient updates:** Connection heartbeat, stdout, and progress.
- **Owning component:** Daemon coordinates; scheduler places; runner-local
  executor acts; Postgres stores authoritative state.
- **Failure behavior:** Runner unavailability is visible and does not silently
  move locality-sensitive work. Stale results fail fencing.
- **Required invariants:** INV-011, INV-019, INV-021–INV-026, INV-042–INV-044.
- **Remaining questions:** Durable lease/affinity orchestration, result-size
  handling, and local MCP capability discovery.

## S17 — Fork from previous transcript state

- **User intent:** Explore an alternative from an earlier point without changing
  the source session.
- **Durable commands:** Create a session with the baseline `Interactive` cause
  independent from ancestry `(source session, immutable frontier)`.
- **State transitions:** New session absent → session durably created with its
  immutable source-session and `TranscriptFrontier` reference. After the fork's
  first input is accepted and its turn becomes eligible, the eligibility
  transition resolves that source boundary, preserves its source-qualified
  semantic-entry identities, appends the new origin entry, and atomically binds
  the new session-owned context snapshot while activating or recording eligible
  failure. The source remains unchanged.
- **Transient updates:** Client may preview the fork point.
- **Owning component:** The daemon validates and atomically creates the session
  with its source reference; the later eligibility transition derives and binds
  the first context snapshot. Postgres preserves both durable boundaries.
- **Failure behavior:** Invalid or inaccessible frontier fails before creation.
  Retrying creation is idempotent. Later source archival does not erase fork
  identity.
- **Required invariants:** INV-001, INV-003, INV-009, INV-012, INV-030.
- **Remaining questions:** Deletion/retention, selectable transcript-frontier
  boundaries, multiple ancestry sources, and merge semantics (not initially
  required). Copy, reference, and shared-prefix storage are permitted
  implementation choices when they preserve the accepted frontier semantics'
  resolved semantic identities
  ([turn-lifecycle-and-scheduling](spec/turn-lifecycle-and-scheduling.md)).

## S18 — Delegate to a child session

- **User intent:** Assign related work to an independently browsable child and
  receive an explicit result.
- **Durable commands:** `spawn_session` creates one child whose delegated cause
  names the exact parent tool request, with ancestry `None`, task input, and a
  background or bound relationship. `await_session` records foreground or
  background delivery; `send_session_message` records either direction, as owned
  by the [delegation tool contract](spec/tool-loop.md).
- **State transitions:** A foreground wait retains the parent's only active turn
  slot until an explicit child result arrives. A background wait registers
  delivery without retaining that slot; result commit creates a durable parent
  wake. The returned value or typed failure becomes delivered parent content,
  never child transcript content, under the
  [delegated-wait contract](spec/turn-lifecycle-and-scheduling.md#boundary-contracts).
- **Transient updates:** Best-effort nudges reduce result/message wake latency;
  the durable eligibility sweep is the restart and lost-wake backstop.
- **Owning component:** Daemon owns relationships and scheduling; each session
  retains independent history.
- **Failure behavior:** Restart restores the relationship, exact wait, messages,
  and undelivered result. Child failure, stop, or cancellation is delivered as a
  typed outcome. A detached result remains durable after parent termination, as
  owned by [session delegation](spec/sessions-and-transcript.md).
- **Required invariants:** INV-003, INV-010, INV-034.
- **Remaining questions:** Multi-source or merged transcript ancestry remains
  separate and unchanged.

## S19 — Cancel a parent while child work is active

- **User intent:** Stop parent work with a clear understanding of what happens
  to the child.
- **Durable commands:** Parent stop/cancel carries `ParentAlone` or
  `ParentAndDescendants`. The latter atomically records a disposition for each
  evaluated relationship from the durable descendant walk defined by
  [session delegation](spec/sessions-and-transcript.md).
- **State transitions:** Background children continue. Bound children apply
  their separately recorded stop/cancel action; `KeepRunning` is itself a typed
  disposition. A child is never deleted and may finish after the parent.
- **Transient updates:** Cancellation progress remains per physical attempt;
  relationship outcomes are durable updates rather than presence hints.
- **Owning component:** Daemon applies the recorded policy and user-selected
  scope; executors respond only to typed stop/cancel authority.
- **Failure behavior:** Every evaluated child has an explicit reason and exact
  spawn, parent-event, and command provenance. Already-issued effects are not
  undone and ambiguous effects remain reconcilable under the
  [delegated-wait contract](spec/turn-lifecycle-and-scheduling.md#boundary-contracts).
- **Required invariants:** INV-010, INV-025, INV-026, INV-029, INV-034.
- **Remaining questions:** Ordinary archive remains independently non-cascading;
  destructive retention remains separate later scope.

## S20 — Resolve a curated model alias

- **User intent:** Use a convenient selection such as “latest preferred” while
  retaining precise requested, resolved, and provider-reported provenance.
- **Durable commands:** At input acceptance, persist the requested alias plus an
  immutable definition selecting exactly one canonical direct model choice in
  effective configuration. Before creating the first call, validate and resolve
  that frozen meaning and pin the exact daemon-resolved provider/model target;
  append observable provider identity or mismatch when available.
- **State transitions:** Turn with frozen alias meaning → exact target pinned →
  model call prepared → in flight → terminal.
- **Transient updates:** Client may show current alias target, clearly separate
  from historical call facts.
- **Owning component:** Daemon model resolver and provider adapter; Postgres
  stores per-call provenance.
- **Failure behavior:** Alias changes after input acceptance never alter queued
  or active work. Resolution failure creates no targetless call and fails the
  attempt and turn; it does not silently choose another model. A reported
  identity different from the exact resolved target follows the accepted full
  mismatch timing rule ([model-call-execution](spec/model-call-execution.md)):
  known failure while nonterminal; preserved ambiguity plus a fatal cause on any
  still-live attempt, with turn failure only when no other unacknowledged
  ambiguity remains and reconciliation otherwise; typed invalidation and stop
  after completion while the turn is active; unchanged known-failure/cancelled
  state; audit-only evidence after authority transfer; or non-rewriting evidence
  after turn terminality, including atomic refusal. Historical provenance does
  not claim which hidden physical backend executed the call when the provider
  does not reveal it.
- **Required invariants:** INV-008, INV-014.
- **Remaining questions:** Alias administration, visibility, and whether a
  future frozen alias policy may include fallback. Acceptance-time definition
  freezing and pre-call target resolution are decided in
  [model-call-execution](spec/model-call-execution.md).

## S21 — Execute an exact pinned model

- **User intent:** Call one exact provider/model reference for reproducibility
  or control.
- **Durable commands:** Persist the exact requested selection, exact
  daemon-resolved provider/model target, frontier, and model-call record;
  capture observable provider-reported model identity and mismatch metadata when
  available.
- **State transitions:** Frozen exact selection → resolution succeeds and a
  pinned call is prepared, or resolution fails before any call exists →
  validated/issued call → completed, known failed, refused, cancelled, or
  ambiguous. Provider-reported target observation is adjacent typed evidence,
  not another state; the first trusted mismatch on a nonterminal call
  immediately selects known failure and requests best-effort cancellation, while
  resolving mismatch evidence cannot reopen an already ambiguous call.
- **Transient updates:** Provider stream and timing.
- **Owning component:** Daemon validates selection and calls provider; adapter
  reports observed metadata.
- **Failure behavior:** An unavailable or otherwise unresolvable exact selection
  is an error: it creates no targetless model call and fails the attempt and
  turn. Daemon-controlled fallback does not occur unless separately and
  explicitly authorized. Provider-reported substitution is recorded rather than
  rewritten as the pinned target. First observed while the outcome-eligible call
  is nonterminal, it selects known failure and makes response/refusal material
  non-authoritative; after terminal ambiguity it preserves physical state, adds
  the typed fatal cause to any still-live attempt without losing prior causes,
  and after outstanding classification fails the turn when no other
  unacknowledged ambiguity remains or requires reconciliation otherwise. After
  current-authority completion during an active turn, it preserves call/history,
  appends typed invalidation, and stops new/outstanding work. Ordinary refusal
  without fatal stop terminalizes call/attempt/turn atomically; a continuation
  refusal raced under fatal stop remains physical and non-authoritative while
  failure/reconciliation controls. After terminal known failure/cancellation it
  preserves that state and existing turn-outcome precedence. After authority
  transfer it is non-authoritative; after valid turn terminalization it adds
  evidence without rewriting committed content. A future allowed fallback must
  create a separate call with its own exact target; it cannot legitimize
  substitution on this call. Absent provider evidence, Signalbox does not claim
  knowledge of the hidden physical backend.
- **Required invariants:** INV-014, INV-015, INV-018.
- **Remaining questions:** Provider identifier normalization and reproducibility
  claims beyond observable model identity remain open
  ([model fallback and provenance](open-questions.md#model-fallback-and-provenance));
  mismatch failure is decided in
  [model-call-execution](spec/model-call-execution.md).

## S22 — Apply an availability fallback

- **User intent:** Continue through a classified capacity/availability failure
  without changing models, using bounded retry-before-action and configured pool
  behavior.
- **Durable commands:** Record the predecessor call's exact requested selection,
  daemon-resolved target, credential profile, provider-reported identity when
  available, failure classification, and typed non-acceptance evidence; evaluate
  the session-pinned pool policy; create a distinct successor attempt and model
  call that pin the same target, the predecessor call, the qualifying cause, and
  either the same profile for retry-before-action or a different eligible
  profile for `switch_now`, as owned by
  [availability successor calls](spec/model-call-execution.md),
  [the credential-availability machine](spec/credential-availability.md), and
  [credential pools and selection](spec/configuration-and-credentials.md#overview).
- **State transitions:** Predecessor call → known availability failure and
  predecessor attempt → known failed; turn → successor eligible; successor
  attempt/call → terminal. A credential's initial call and same-credential
  successors are bounded by
  `numeric_bounds.max_same_credential_attempts_per_turn`; `switch_now`
  chain-excludes the failed profile and selects a different eligible profile. A
  successful call ends that chain before later continuation, while releasing a
  parked wait resumes the chain the wait belongs to
  ([availability successor calls](spec/model-call-execution.md)).
- **Transient updates:** No current client update announces that a successor is
  being considered or selected. The predecessor, cause, and successor are
  committed durable evidence, but no current process-protocol snapshot or
  history message projects that chain to a client.
- **Owning component:** Daemon pool policy authorizes and selects the profile;
  provider adapters supply the typed classification and separate evidence that
  the request was not accepted. Adapters do not select successors.
- **Failure behavior:** Only quota exhaustion, rate limiting, overload, or
  provider-internal failure with distinct non-acceptance evidence may authorize
  the successor. Rate limiting, overload, and provider-internal failure retry
  the same profile below its bound before the pinned pool action applies;
  `switch_now` rotates after the bound. Classification alone is insufficient.
  Ambiguity, refusal, credential resolution failure, and credential rejection
  never authorize a successor. The successor cannot cross adapters or change the
  exact target. Exhausting the pool follows its configured durable park or
  known-failure outcome
  ([credential pools and selection](spec/configuration-and-credentials.md)). A
  provider-reported mismatch against either call's own target follows the
  accepted timing-sensitive mismatch failure rule
  ([model-call-execution](spec/model-call-execution.md)) and is never an allowed
  substitution.
- **Required invariants:** INV-014, INV-018.
- **Remaining questions:** Transient client presentation of successor selection
  and whether a future pool may cross adapter kinds.

## S23 — Encounter a model safety refusal

- **User intent:** Understand that the selected model refused and avoid hidden
  policy evasion.
- **Durable commands:** Persist the model call, requested selection, exact
  daemon-resolved target, observable provider identity or mismatch when
  available, provider response classification, and refusal outcome; create any
  follow-up only through an explicit user/policy decision.
- **State transitions:** Without fatal stop, model call in flight → call
  refused, attempt turn-refused, and turn terminal/refused in one aggregate
  transition when target evidence does not mismatch. Serial orchestration
  requires all earlier work closed before the call and refusal creates no new
  dependency, so an ordinary refused-call/active-turn state is invalid. If a
  continuation already issued before another completed call's invalidation races
  refusal, the continuation remains physically refused inside
  `StopRequested(FatalMismatch)`, its content is non-authoritative, and the
  attempt/turn end only as fatal failure or reconciliation. Mismatch delivered
  with or before ordinary refusal commit instead makes the call known failed and
  leaves refusal non-authoritative; after terminal ambiguity it preserves that
  disposition, adds the fatal resolution to any still-live attempt, and after
  classification fails the turn only when no other unacknowledged ambiguity
  remains or requires reconciliation otherwise. A future remediation decision
  may add a typed wait or continuation policy.
- **Transient updates:** Refusal text may stream but becomes authoritative only
  when committed.
- **Owning component:** Provider adapter reports; daemon classifies and exposes
  provenance.
- **Failure behavior:** An ordinary authoritative refusal is not treated as
  successful completion or availability failure, does not automatically fall
  back merely because another model exists, and does not retain the active slot
  in an undefined settlement state. A physical refusal raced under fatal
  mismatch cannot override that failure. When mismatch is observed with or
  before ordinary refusal terminalization, or resolves terminal ambiguity,
  refusal material is audit-only; the turn fails when no other unacknowledged
  ambiguity remains and otherwise carries the exact fatal reconciliation marker.
  Mismatch first learned after a valid atomically refused turn adds
  reconciliation evidence without rewriting that disposition or committed
  refusal.
- **Required invariants:** INV-014, INV-018, INV-032.
- **Remaining questions:** Refusal taxonomy, user-facing remediation, and
  whether any explicit fallback is ever allowed. Provider-identity normalization
  remains open
  ([model fallback and provenance](open-questions.md#model-fallback-and-provenance)),
  while the mismatch disposition is accepted
  ([model-call-execution](spec/model-call-execution.md)).

## S24 — Reconnect a client during active streaming

- **User intent:** Resume observing current work without corrupting the
  transcript or relying on every delta having persisted.
- **Durable commands:** The server subscribes to process-local fan-out before
  reading an authoritative repeatable-read snapshot of transcript entries, turn
  states, and the outbox cursor, then sends matching events above that cursor;
  no new logical work is created merely by reconnecting
  ([follow synchronization](spec/process-protocol.md)).
- **State transitions:** Client disconnected → synchronized snapshot → live
  observer; server-side turn remains unchanged.
- **Transient updates:** Previously seen draft may be replaced. Version one
  resumes durable progress events; future provider-delta relay must add draft
  identity and sequencing without making deltas authoritative.
- **Owning component:** Daemon reconstructs durable truth and streams; client
  reconciles presentation.
- **Failure behavior:** A bounded-fan-out overrun causes an explicit
  `resync_required` and another snapshot, not guessed updates. If the call
  finished or refused while disconnected, terminal turn state in the snapshot
  prevents a waiter from depending on an already-covered event. Large
  transcripts arrive as validated bounded frames; a partial sequence is never
  authoritative. Final durable content replaces any draft
  ([process protocol](spec/process-protocol.md)).
- **Required invariants:** INV-005, INV-012, INV-032, INV-033.
- **Remaining questions:** Transient updates, retention, later compatibility,
  and browser transport remain
  [open](open-questions.md#protocols-and-persistence).

## S25 — Archive and restore a session

- **User intent:** Remove a conversation from the active list without losing its
  identity, provenance, or ability to return.
- **Durable commands:** Persist `ReplaceSessionMetadata` with `archived = true`;
  later persist another complete replacement with `archived = false`. Each
  command has its own durable identity and replay behavior.
- **State transitions:** The organizational metadata snapshot changes between
  archived and non-archived. Session and turn lifecycle state does not change.
- **Transient updates:** Client list filtering and confirmation.
- **Owning component:** Daemon validates metadata; Postgres preserves history;
  clients present archive state.
- **Failure behavior:** Restart preserves archive status. Archiving never
  cancels, pauses, rejects, or rewrites work and never cascades to another
  session. A missing session is a durable typed rejection.
- **Required invariants:** INV-005, INV-012, INV-013.
- **Remaining questions:** Destructive retention and purge are separate later
  scope under
  [session organization and retention](open-questions.md#session-organization-visibility-and-retention),
  not ordinary archive behavior.

## S26 — Manually regenerate a prior answer

- **User intent:** Ask for another outcome related to a prior turn without
  erasing what happened before.
- **Durable commands:** No baseline regeneration command is exposed. A future
  regeneration decision must create a new turn with a typed relation to the
  original, an explicitly frozen effective configuration, an immutable source
  frontier, and defined queue placement while retaining the original turn,
  attempts, calls, and output unchanged.
- **State transitions:** Reserved for the future regeneration decision. The
  initial turn-origin enum contains only accepted-input origin and does not
  encode a half-defined regeneration transition.
- **Transient updates:** A future client may visually group alternatives, but
  grouping never replaces durable identities.
- **Owning component:** When introduced, the daemon validates the source
  relation and creates new logical work; Postgres preserves both histories;
  clients choose presentation.
- **Failure behavior:** When introduced, duplicate command delivery must create
  at most one regeneration turn. A changed model or any changed
  effective-configuration field belongs to that new turn and is never disguised
  as recovery of the original.
- **Required invariants:** INV-001, INV-004, INV-006, INV-008, INV-012, INV-014,
  INV-015.
- **Remaining questions:** A future regeneration decision must decide command
  acceptance, FIFO interaction, exact historical source frontier, configuration
  freeze, and alternative-answer presentation before implementation.

## S27 — Fatal mismatch with a separately classified ambiguity

- **User intent:** Preserve an independently ambiguous effect exactly while
  allowing a fully closed fatal mismatch to release the session slot without a
  ceremonial stopping phase.
- **Durable commands:** This is a domain-algebra fixture for a future aggregate
  that can own independently issued operations; the implemented serialized tool
  executor cannot produce a live provider call X and tool attempt Y together.
  Given such a running attempt, Y is already physically `Ambiguous`, while X is
  the last unclassified issued operation. One serialized transition records
  trusted target-mismatch evidence for X, classifies X `KnownFailed`, adds its
  exact failure to the complete fatal causes F, records any required best-effort
  cancellation intent, closes or makes non-dispatchable every logical
  dependency, and reclassifies pending steering.
- **State transitions:** With every terminal guard now satisfied and the exact
  unacknowledged ambiguity set U equal to `{Y}`, the same transaction ends the
  attempt `AfterFatalMismatch(Ambiguous)` and terminalizes the turn
  `ReconciliationRequired` with `{Y}` and
  `FatalMismatchRequiresReconciliation(F)`. It does not persist `StopRequested`
  or fabricate a recovery wait. Countercase: if another issued operation Z
  remains unclassified when X mismatches, the attempt must enter
  `StopRequested(FatalMismatch(F))` and retain the slot. After Z is honestly
  classified, the turn fails if U is empty or receives exact fatal
  reconciliation if U remains nonempty.
- **Transient updates:** Delivery and acknowledgement of best-effort
  cancellation may continue after direct terminalization; progress text and late
  provider or runner observations are not outcome authority.
- **Owning component:** The daemon derives F, U, and every terminal guard from
  authoritative aggregate state and atomically commits the result; adapters only
  deliver cancellation intent and report evidence.
- **Failure behavior:** A crash exposes either the prior running aggregate or
  the complete terminal attempt, exact marker, steering dispositions, and
  released slot, never a partial direct transition. Replay cannot insert a
  synthetic stop phase or a second terminal result. Late cleanup or operation
  evidence remains audit/reconciliation evidence and cannot authorize new
  effects or rewrite the terminal turn.
- **Required invariants:** INV-006, INV-009, INV-014, INV-025, INV-026, INV-034.
- **Remaining questions:** Provider-target identity evidence and trust, exact
  cancellation delivery and acknowledgement mechanics, the execution strategy
  that could produce independently issued provider and tool operations, and
  whether direct interrupt-only reconciliation is ever added.

## S28 — Import an external conversation and continue natively

- **User intent:** Preserve an external Claude Code or Codex conversation as
  durable Signalbox history, later select any imported entry boundary, and
  continue in a new resume-style or fork-style session without pretending
  Signalbox executed the imported work.
- **Durable commands:** Pure ingestion converts one explicitly selected
  supported JSONL source through
  [conversation import](spec/conversation-import.md). At any later time, session
  creation selects an imported frontier under
  [sessions and transcript](spec/sessions-and-transcript.md); ordinary native
  input then uses the existing command path.
- **State transitions:** Ingestion, later session creation, imported seed
  projection, and first native execution follow their owning specifications in
  [conversation import](spec/conversation-import.md),
  [sessions and transcript](spec/sessions-and-transcript.md),
  [turn lifecycle](spec/turn-lifecycle-and-scheduling.md), and
  [model-call execution](spec/model-call-execution.md).
- **Transient updates:** None are required for import. Scripted-model deltas, if
  any, follow the ordinary transient/final-content boundary.
- **Owning component:** Each source edge converter owns its JSONL quirks; the
  imported-conversation store owns content-addressed raw records and idempotent
  snapshots; session creation owns later frontier/mode selection and seed
  projection; the existing scheduler and model path own all native execution
  after the seed boundary.
- **Failure behavior:** Malformed or unsupported source content rejects the
  whole conversion. Import, missing-target, replay, and rendering follow the
  same owning specifications linked above; transactional storage, crash, and
  outbox behavior follow [persistence protocol](spec/persistence-protocol.md).
- **Required invariants:** INV-001, INV-002, INV-003, INV-005, INV-007, INV-009,
  INV-012, INV-014, INV-015, INV-026, INV-032, INV-038, INV-039.
- **Remaining questions:** Additional source converters, import discovery and
  bulk policy, rich non-text model rendering, client presentation, and retention
  are outside this scenario. Real-content validation remains opt-in, local, and
  content-silent.

## S29 — Complete a review-workflow pass

- **User intent:** Review one exact repository revision and trust that the
  workflow result is backed by the session execution that produced it.
- **Durable commands:** Record the target, execution-backed pass, exact result,
  finding history, and any publication evidence under the
  [review-workflows contract](spec/review-workflows.md). Publication, when
  requested by later orchestration, reserves before the external call and
  reconciles that same reservation afterward.
- **State transitions:** Follow the closed target, run, pass, finding, and
  external-link machines in the
  [review-workflows specification](spec/review-workflows.md). The S29 fixture
  exercises one queued → running → terminal run/pass path and one finding's
  contiguous event history.
- **Transient updates:** Prompt progress, model drafts, and code-host request
  progress are not workflow evidence.
- **Owning component:** The review-workflow domain validates its projections;
  sessions and turns own execution evidence; Postgres loads and correlates both;
  future orchestration coordinates adapters.
- **Failure behavior:** A reused or foreign accepted input, turn, pass, finding,
  pass-result commitment, referenced-finding status, external-link provider or
  logical target, frontier, non-origin input, unknown policy version, cyclic
  target parent, below-threshold transition, or lifecycle outcome fails
  reconstitution as corruption. Finding-reference cycles fail at admission. A
  pending external reservation does not prove absence of an external effect and
  is not retried automatically. No transcript content or general-purpose
  artifact is copied into workflow rows.
- **Required invariants:** INV-001, INV-002, INV-007, INV-025, INV-026, INV-040,
  INV-041.
- **Remaining questions:** Application commands, scheduling, prompts,
  automation, repair, and stack propagation remain in
  [review-workflow orchestration](open-questions.md#destination-features-target-model);
  a general artifact aggregate remains
  [open](open-questions.md#general-purpose-artifacts).

## S30 — Enroll a runner and pin a session

- **User intent:** Target either one exact runner or a capability class and know
  which logical runner, working directory, tools, credential profile, and
  workspace boundary the session actually received.
- **Durable commands:** A later application stack records logical enrollment and
  validates the runner's advertised names against active enrollment and the
  daemon catalog. Session creation records its class-or-identity selector,
  optional working directory, optional credential-profile selection, and
  workspace requirement. The first runner execution records one exact validated
  registration, pins that runner, and creates any requested initial
  credential-profile grant from that exact registration.
- **State transitions:** Enrollment active → validated registration; placement
  unpinned → pinned on the first eligible runner. The domain foundation in
  [runner protocol and placement](spec/runner-protocol.md) implements these
  transitions without transport or storage.
- **Transient updates:** Connection and registration progress are not
  enrollment, placement, or approval authority.
- **Owning component:** The daemon owns enrollment and policy; the runner
  reports availability; session placement owns affinity.
- **Failure behavior:** Revoked enrollment, unknown or disallowed catalog
  claims, selector mismatch, unavailable credential profile, missing workspace
  capability, or a second ordinary runner fails explicitly without changing
  placement. Hardware or network changes never derive a new identity implicitly.
- **Required invariants:** INV-001, INV-002, INV-024, INV-035, INV-042, INV-044,
  INV-045.
- **Remaining questions:** Authentication exchange, store transactions,
  streaming transport, application session creation, and client presentation
  remain under
  [runner authentication](open-questions.md#identity-credentials-and-resource-governance)
  and [runner transport](open-questions.md#protocols-and-persistence).

## S31 — Recover a lost runner lease

- **User intent:** Resume work that is known safe to repeat without silently
  duplicating a side effect.
- **Durable commands:** A later store stack persists an offered lease before
  streaming it, then persists its exact runner/generation claim before
  acknowledging execution capability or accepting a result. Domain transition
  inputs retain the runner, lease, physical tool attempt, session, tool,
  declaration-derived effect class, and lease-lineage generation. A fresh
  physical attempt begins at its own first tool-dispatch generation.
- **State transitions:** An unclaimed lost lease advances to a checked successor
  generation for every class while retaining the never-executed attempt. A
  claimed lost pure or idempotent lease produces re-lease authority that
  requires a fresh physical attempt identity. A claimed lost side-effecting
  lease produces crash-classification authority and cannot produce re-lease
  authority.
- **Transient updates:** Connection loss, reconnect, and repeated frames do not
  themselves advance the lease.
- **Owning component:** The daemon owns lease and physical-attempt outcomes; the
  runner carries only correlated offers, claims, and observations.
- **Failure behavior:** A stale generation, wrong runner, wrong attempt,
  duplicate completion, or exhausted generation fails closed. Side-effecting
  loss follows the existing ambiguity and reconciliation law and is never
  converted to ordinary tool output or silently dispatched again.
- **Required invariants:** INV-004, INV-006, INV-011, INV-021, INV-024–INV-026,
  INV-034, INV-043.
- **Remaining questions:** Store schema and transactions, exact reconnect
  inventory, transport framing, heartbeat, and stale-evidence retention remain
  in [runner transport](open-questions.md#protocols-and-persistence).

## S32 — Replace a lost runner and credential grant

- **User intent:** Continue a pinned session on an explicitly selected
  replacement while ensuring the model learns the changed logical runner,
  directory, tools, credential profile, and workspace.
- **Durable commands:** A later user command marks the runner lost and supplies
  one complete validated replacement placement. When the placement selects a
  credential profile, runner replacement consumes the prior grant and creates
  its checked successor revision in that same replacement. A separate
  credential-profile replacement changes the selected profile on the same pinned
  runner. Each transition produces exact before-and-after change facts for a
  later injected semantic message and frontier extension.
- **State transitions:** Pinned → runner lost → explicitly replaced and pinned
  at the checked successor revision. Active or revoked prior credential grant →
  checked active successor grant during runner replacement; the consumed prior
  revision remains terminal. On the same pinned runner, active credential grant
  → replaced active grant or revoked terminal grant. A repository-worktree
  requirement creates a new runner-owned provisioned workspace; the old runner's
  workspace is never inherited.
- **Transient updates:** UI progress and connection discovery are not
  replacement authority.
- **Owning component:** The daemon validates and records user intent; the
  selected runner provisions and cleans its workspace; context assembly later
  appends the typed placement-change message.
- **Failure behavior:** Automatic migration is absent. An ineligible runner,
  stale placement or grant revision, unavailable profile, wider tool set,
  missing workspace capability, or attempt to reactivate a revoked grant fails
  unchanged. Revocation gates later lease creation but does not yank or rewrite
  an already offered lease.
- **Required invariants:** INV-005, INV-008, INV-024–INV-026, INV-035, INV-036,
  INV-042, INV-044, INV-045.
- **Remaining questions:** User command shape, atomic store boundaries, exact
  injected semantic content, cleanup recovery, and recovery when no eligible
  replacement exists remain under
  [scheduling and runners](open-questions.md#scheduling-and-runners) and
  [tool safety](open-questions.md#tool-safety).

## S33 — Change the model during a session

- **User intent:** Choose a different configured model for future conversation
  without changing work already accepted or in progress.
- **Durable commands:** `ReplaceSessionDefaults` compare-and-sets the
  caller-observed epoch and appends its complete successor. Each later
  `SubmitInput` binds the epoch current at acceptance.
- **State transitions:** The current defaults pointer advances without changing
  queued or active turns. When the first turn frozen to a different direct model
  starts, its predecessor frontier grows by one model-identity boundary and then
  its ordinary origin. Target and credential pins are established for that turn;
  prior pins remain unchanged.
- **Transient updates:** None. The model learns the boundary from durable
  frontier context, and the terminal client reports only the acknowledged
  replacement receipt.
- **Owning component:** The domain owns epoch and frontier laws; Postgres stores
  and constrains them; the daemon validates against its read-only catalog; the
  process protocol and terminal model verb expose the user command.
- **Failure behavior:** Missing session, stale or exhausted epoch, conflicting
  command reuse, unknown catalog selection, and commit ambiguity retain their
  distinct typed outcomes. Exact replay returns the first recorded result.
- **Required invariants:** INV-008, INV-012, INV-014, INV-015, INV-033, INV-046.
- **Remaining questions:** Richer client model discovery and non-Anthropic
  daemon composition remain outside this scenario.

## S34 — Set the session system prompt

- **User intent:** Give one session standing instructions that every model call
  receives, set at creation or replaced later.
- **Durable commands:** `CreateSession` carries one optional bounded system
  prompt inside its complete initial defaults. `ReplaceSessionDefaults`
  compare-and-sets the caller-observed epoch and installs the complete
  successor, including its optional prompt.
- **State transitions:** The current defaults pointer advances exactly as for a
  model change. Turns keep freezing only the epoch version; model-call
  preparation reads the prompt through that frozen version, and the provider
  bridge sets the runtime operation's system instructions on every call it
  prepares. A prompt-only replacement appends no semantic transcript entry.
- **Transient updates:** None.
- **Owning component:** The domain owns the bounded prompt value and epoch laws;
  Postgres stores and constrains it, including digest-keyed command/defaults
  agreement; the process protocol and terminal create/model verbs expose the
  user surface.
- **Failure behavior:** An empty, U+0000-bearing, over-bound, or omitted prompt
  member fails before any command identity is claimed. Stale epochs, conflicting
  reuse, unknown catalog selections, and commit ambiguity retain their S33
  outcomes.
- **Required invariants:** INV-008, INV-012, INV-033, INV-046.
- **Remaining questions:** Prompt composition from base, per-use-case, and
  instruction-file contributions remains an open configuration-category
  capability.

## S35 — Create a session from a template

- **User intent:** Start a session from one named, versioned bundle without
  repeating its model, system prompt, or dangerous-tool posture.
- **Durable commands:** The daemon resolves the name from its immutable startup
  catalog, copies the resolved bundle into defaults version one, and records the
  template name and content digest with the new session. The durable command's
  template name is its caller-supplied equality fact; replay returns the
  originally copied session even if a later daemon load sees edited template
  content.
- **State transitions:** No session → ordinary durable session with immutable
  defaults version one plus present template provenance. Editing the config and
  restarting the daemon changes only later commands with new identities; no
  existing defaults epoch or provenance record changes.
- **Transient updates:** Template lookup and prompt-file loading are startup
  configuration work, not session state.
- **Owning component:** The daemon owns static configuration and resolution; the
  domain owns provenance and command equality; Postgres stores the copied
  defaults and provenance; the process protocol exposes creation and listing.
- **Failure behavior:** Unknown names and invalid request composition fail after
  durable-command lookup and before a new command claim. Unreadable prompt
  files, invalid paths or prompts, duplicate names, unknown model selections,
  and malformed or unknown config fields are precise typed startup errors.
  Conflicting command reuse and commit ambiguity retain S01 behavior.
- **Required invariants:** INV-002, INV-008, INV-012, INV-033, INV-046, INV-047.
- **Remaining questions:** Durable template objects, CRUD, and agent authoring
  tools are owned by
  [template storage and authoring surfaces](open-questions.md#template-storage-and-authoring);
  richer prompt/tool composition remains owned by
  [configuration categories](open-questions.md#configuration-categories).

## S36 — Scope a session's cross-session reads by placement path

- **User intent:** Place a session in a dotted project directory so it can read
  sibling and descendant native conversations without reading above or outside
  that directory.
- **Durable commands:** Creation records pathless, scoped, or loudly
  acknowledged root-global-read placement as history version one.
  `UpdateSessionPlacement(expected_version, replacement)` appends one explicit
  next-version event and preserves every prior event.
- **State transitions:** Pathless retains legacy read behavior. Scoped placement
  reads the parent directory subtree by one prefix comparison. Root placement
  reads everything only when the current creation or update event records
  explicit global-read intent.
- **Transient updates:** None; a denied selected-transcript read returns typed
  refusal evidence naming the requester's directory and closed reason.
- **Owning component:** Domain owns validated paths, events, and scope decision;
  Postgres owns history and current selection; conversation introspection owns
  enforcement; process surfaces own creation, update, and display.
- **Failure behavior:** Empty, malformed, overlong, and over-deep paths fail
  before command handling. Stale updates are authoritative typed rejections.
  Ancestor, pathless-target, and disjoint scoped reads are typed refusals rather
  than empty successful results.
- **Required invariants:** INV-008, INV-012, INV-050.
- **Remaining questions:** None.

## S37 — Change model and session settings

- **User intent:** Choose a supported reasoning level, fast mode, or service
  tier for a session or one new turn without relying on a provider to repair an
  incompatible request.
- **Durable commands:** Creation establishes the first complete settings
  snapshot from global, model-profile, and session layers.
  `ReplaceSessionDefaults` carries a provenance-preserving settings override and
  compare-and-sets the complete next epoch. Origin-producing `SubmitInput`
  carries an optional per-call override and freezes one complete effective
  value. Steering inherits its source turn and carries no override.
- **State transitions:** An explicit unsupported value rejects before command
  effects or provider preparation. A model-change-induced incompatibility uses
  the greatest supported reasoning level no higher than the prior level, or the
  lowest supported level when none is lower; it disables fast mode or clears
  service tier, then records the exact adjustment. Alias retargeting applies the
  same rule at input acceptance. A declared fast serving target is authorized
  lineage; no undeclared target or suffix is.
- **Transient updates:** None. Capability discovery is a read-only projection;
  settings, provenance, and adjustments are durable facts.
- **Owning component:** Domain owns setting values, precedence, compatibility,
  and adjustment events; daemon configuration owns model capabilities and copied
  global/profile layers; Postgres owns epochs and origin records; the process
  protocol exposes the catalog and commands; provider adapters own exhaustive
  translations.
- **Failure behavior:** Explicit unsupported reasoning, fast, service-tier, or
  adapter-specific combinations remain distinct typed invalid requests. Missing
  or contradictory capability declarations reject configuration. A provider
  CLI's silent clamp, open effort string, or dropped tier is never validation.
- **Required invariants:** INV-008, INV-012, INV-014, INV-051, INV-052, INV-053,
  INV-054.
- **Remaining questions:** Context compaction and the other settings listed
  under [configuration categories](open-questions.md#configuration-categories)
  remain outside this scenario.

## Coverage note

The accepted foundation decisions govern retry identity and baseline input
lifecycle. Fallback, capability vocabulary, safety policy, queue management,
archive behavior, and other protocol choices remain open; the delegation
command, result, message, and descendant-scope protocols are committed by S18
and S19. A decision that changes a lifecycle should update the affected
scenarios and cite the invariant changes it requires.

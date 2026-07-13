# Glossary

This glossary recommends working language for design discussion. “Accepted” means the concept and distinction are accepted; it does not promise a stable API spelling. “Provisional” means the name, boundary, or both still need an ADR.

## Session

- **Definition:** A durable, independently browsable conversation with configuration, ordered semantic history, operational work, and archival state.
- **Status:** Concept accepted; the name and identity boundary are proposed by [ADR-0001](decisions/0001-domain-terminology-and-identity.md). “Session” is preferred over “thread” because it emphasizes durable continuity across clients, though it can be confused with a login session.
- **Do not confuse with:** A client connection, one model context window, one turn, or a runner process.
- **Example:** A user starts “repair garden sensor” on a phone, continues it from a terminal, and archives it next week without losing its history.

## Accepted input

- **Definition:** One user submission made durable with its explicit delivery request and recoverable disposition before acknowledgement.
- **Status:** The durability requirement is accepted; the name, identity boundary, and dispositions are proposed by [ADR-0001](decisions/0001-domain-terminology-and-identity.md) and [ADR-0027](decisions/0027-input-delivery-lifecycle.md).
- **Do not confuse with:** A transport command, transcript entry, turn, or model call. One accepted input may originate a turn or steer an existing turn.
- **Example:** “Use the new log” remains the same accepted input whether it is consumed at a safe point or visibly reclassified as queued work because the active turn ends first.

## Turn

- **Definition:** One durable logical request for Signalbox to produce a conversational outcome from one typed origin under one frozen effective configuration. Typed input-delivery or recovery transitions, never semantic comparison of natural-language intent, determine whether its identity is preserved. It reaches one explicit terminal disposition while surviving zero or more physical orchestration attempts and may use several context frontiers.
- **Status:** The logical/physical distinction is accepted; the name and exact lifecycle are proposed by [ADR-0001](decisions/0001-domain-terminology-and-identity.md) and [ADR-0004](decisions/0004-turn-and-attempt-lifecycle.md).
- **Do not confuse with:** The user message itself, a provider call, an orchestration process, or every item displayed in a transcript.
- **Example:** “Summarize these changes” remains the same logical turn when the hub replaces a lost physical attempt without changing its origin, frozen configuration, or committed effect history.

## Turn attempt

- **Definition:** One exclusive physical orchestration tenure that advances an active running turn until it ends, yields to a durable wait, or is fenced and replaced. Activation and wait resolution atomically create it; a running turn never exists without one.
- **Status:** The physical identity distinction is accepted; the name and lifecycle are proposed by [ADR-0001](decisions/0001-domain-terminology-and-identity.md) and [ADR-0004](decisions/0004-turn-and-attempt-lifecycle.md). “Turn attempt” is preferred to generic “run,” which collides with runners and says little about logical ownership.
- **Do not confuse with:** The durable turn, an individual model call, or an individual tool attempt.
- **Example:** An attempt ends when orchestration enters a durable approval wait; a new attempt continues the same active turn after approval.

## Model call

- **Definition:** One durable hub authorization to attempt a physical interaction with a model provider, recording requested selection, exact hub-resolved provider/model target, context frontier, provider-reported or otherwise observable identity when available, response metadata, and outcome. It may terminate before any request reaches the provider.
- **Status:** Required provenance is accepted; the name, identity, and retry boundary are proposed by [ADR-0001](decisions/0001-domain-terminology-and-identity.md) and [ADR-0005](decisions/0005-model-call-retry-semantics.md). “Model call” is clearer than “completion” because providers and response shapes vary.
- **Do not confuse with:** A turn, an alias resolution, all provider retries as a group, or the assistant message eventually committed to history.
- **Example:** A safe-point steering message leads to a second model call in the same turn; each call records the precise context it consumed.

## Tool request

- **Definition:** A logical request for one named tool operation with normalized arguments, policy state, and eventual logical outcome.
- **Status:** Provisional name; logical/physical split accepted.
- **Do not confuse with:** The model's unvalidated text, an approval prompt, an executor dispatch, or a physical retry.
- **Example:** The request “delete branch `old-demo`” retains one identity while awaiting approval and while a single approved execution is dispatched.

## Tool attempt

- **Definition:** One physical effort by a hub-local or runner-local executor to perform a tool request, including dispatch identity, executor placement, timing, output, and outcome classification.
- **Status:** Provisional. This is preferred to “tool call,” which can obscure the difference between logical request and physical effect.
- **Do not confuse with:** The logical tool request, provider-native function-call syntax, or a scheduler's delivery retry.
- **Example:** A read-only file search attempt is lost with its runner connection; policy may allow a second attempt for the same tool request.

## Creation cause

- **Definition:** The typed reason a session exists. The first implementable value is owner-initiated; application, schedule, delegation, or other causes require feature ADRs that define their exact durable initiator identities.
- **Status:** The independent concept is accepted; immutable owner-initiated baseline and typed extension rule are proposed by [ADR-0003](decisions/0003-session-creation-and-transcript-ancestry.md).
- **Do not confuse with:** Transcript ancestry. Cause answers “why created,” not “where initial context came from.”
- **Example:** An owner-created fork has owner initiation as its cause and a separate source frontier as ancestry. After ADR-0002 defines delegation, a child may instead carry its newly defined delegated cause without necessarily inheriting transcript context.

## Transcript ancestry

- **Definition:** The source frontier from which a session's initial semantic conversation context was derived, or an explicit absence of such a source.
- **Status:** Single-source initial concept is accepted; the immutable `none` or one-source boundary is proposed by [ADR-0003](decisions/0003-session-creation-and-transcript-ancestry.md). Any future multi-source model remains open. “Transcript ancestry” is preferred to “parent session,” because delegation and ancestry are independent.
- **Do not confuse with:** Creation cause, ongoing related-session links, or ownership.
- **Example:** A user-created session forks session A through message 18; its cause is user creation and its ancestry is A at frontier 18.

## Input delivery policy

- **Definition:** The explicit instruction for handling user input relative to authoritative session state: start when no turn is active or, while a turn is active, interrupt, next safe point, or after current turn.
- **Status:** The three active-work intents and durable treatment are accepted; the explicit no-active-turn command and exact lifecycle are proposed by [ADR-0027](decisions/0027-input-delivery-lifecycle.md). “Delivery policy” is preferred to “message priority,” which would not express lifecycle semantics.
- **Do not confuse with:** Transport delivery guarantees or the final determination of which model context consumes the message.
- **Example:** “Use the new error log” with next-safe-point policy becomes durable immediately and is considered before the next model call, not injected into the current request.

## Runner

- **Definition:** An outbound-connected process that declares capabilities and execution-boundary properties, then performs selected runner-local tool attempts under one deployment identity.
- **Status:** Name and boundary accepted at the architecture level; exact protocol provisional.
- **Do not confuse with:** A turn attempt, the central scheduler, a client, or a guarantee of sandboxing.
- **Example:** A runner on a laptop declares access to `/Users/me/project` and truthfully states that commands execute as the logged-in user.

## Runner property evidence

- **Definition:** The evidence distinctions used when selecting and explaining a runner: **declared** properties are reported by the runner; **configured** properties are stated by trusted deployment configuration; **verified** properties are established through an accepted enrollment, attestation, policy, or other mechanism; and **effective** properties are what hub policy permits the scheduler and client to rely on for one dispatch.
- **Status:** Distinctions accepted; evidence formats, enrollment, verification, and attestation mechanisms provisional.
- **Do not confuse with:** Treating a runner's declaration as proof, or assuming configured intent necessarily establishes the deployed physical boundary.
- **Example:** A runner may declare that it uses a container, while the effective boundary shown for dispatch remains no stronger than the configuration and verification evidence support.

## Execution boundary

- **Definition:** The actual identity and isolation properties of a runner deployment, including relevant OS user, container, sandbox, VM, filesystem scope, and other enforceable constraints.
- **Status:** Concept accepted; capability vocabulary provisional. This term is preferred to a single “sandboxed” Boolean.
- **Do not confuse with:** Tool approval, a declared or configured property alone, the effective properties justified for a dispatch, or the trustworthiness of command content.
- **Example:** A restricted runner executes as a dedicated account inside a container with one mounted workspace; the UI shows the effective boundary and the evidence supporting that description before selection.

## Tool policy

- **Definition:** Hub-owned evaluation that determines whether a specific logical tool request is allowed, denied, or requires confirmation, plus any placement or constraint decision.
- **Status:** Ownership accepted; rule model provisional.
- **Do not confuse with:** A model's recommendation, the human approval itself, or executor-level sandbox enforcement.
- **Example:** Reading a repository may be allowed on a selected runner, while publishing a release pauses for confirmation.

## Approval

- **Definition:** A recorded human decision that permits or denies one exact logical tool request with the arguments and relevant constraints presented to the user.
- **Status:** Binding rule accepted; interaction and expiry details provisional.
- **Do not confuse with:** General trust in a runner, approval of a tool name for all future arguments, or an LLM judge recommendation.
- **Example:** Approval to send an email to `a@example.test` does not authorize a retried request addressed to `b@example.test`.

## Executor placement

- **Definition:** The selected location for a physical tool attempt: a hub-local executor or a runner-local executor on an identified runner.
- **Status:** Two-placement concept accepted; selection protocol provisional.
- **Do not confuse with:** Who owns tool policy or the result. Both remain centrally coordinated by the hub.
- **Example:** Documentation lookup uses a centrally credentialed hub integration; a workspace build uses the pinned workstation runner.

## Known failure

- **Definition:** A terminal physical outcome for which the hub has adequate evidence that the intended effect did not complete, or completed with a specific reported failure.
- **Status:** Concept accepted; evidence thresholds provisional.
- **Do not confuse with:** A timeout whose external effect is unknown.
- **Example:** A provider returns a validated authentication error before accepting the request; the call is recorded as a known failure.

## Ambiguous outcome

- **Definition:** A physical outcome where available evidence cannot establish whether an external effect occurred, usually because acknowledgement or observation was lost. A non-cancelled turn with such an issued outcome retains its active slot while awaiting an explicit recovery decision; cancellation preserves the physical `Ambiguous` outcome while the turn terminalizes as reconciliation required.
- **Status:** Concept and no-blind-retry rule accepted; reconciliation states provisional.
- **Do not confuse with:** A known failure, an ordinary retryable read, or “probably failed.”
- **Example:** A runner loses connectivity immediately after submitting a payment-like external write; the hub records ambiguity and requires reconciliation instead of dispatching it again.

## Context frontier

- **Definition:** An immutable reference to the exact ordered semantic content consumed by one model call, including applicable user inputs, consumed steering, committed assistant or tool content, and explicit failure, cancellation, or ambiguity markers.
- **Status:** Per-call provenance is accepted; the starting-frontier and safe-point selection rules are proposed by [ADR-0027](decisions/0027-input-delivery-lifecycle.md). Representation remains provisional.
- **Do not confuse with:** The latest session transcript, an entire turn, or client rendering state.
- **Example:** Model call 1 consumes frontier 42; steering and a tool result become committed, so model call 2 consumes frontier 47 within the same turn. If call 2 fails before send, its retry still retains that committed content.

## Effective configuration

- **Definition:** The complete immutable semantic configuration governing one turn: requested model selection and parameters, semantic instruction policy, enabled tool behavior and placement constraints, owner-visible recovery/fallback/resource choices, and the immutable policy versions needed to interpret them. Every field is identity-significant and equality is semantic value equality.
- **Status:** Durable provenance is accepted; the closed semantic categories, operational exclusions, immutability, equality, and freeze boundary are proposed by [ADR-0004](decisions/0004-turn-and-attempt-lifecycle.md), [ADR-0005](decisions/0005-model-call-retry-semantics.md), and [ADR-0027](decisions/0027-input-delivery-lifecycle.md). Nested subsystem representations remain open without reopening whether they are identity-significant.
- **Do not confuse with:** The exact provider/model target resolved for a model call, current hub defaults, or a client-side draft selection.
- **Example:** A queued turn keeps the complete model and tool configuration accepted with it even if hub defaults change before the predecessor finishes; safe-point steering inherits that value rather than supplying another configuration.

## Dispatch generation

- **Definition:** A monotonic fencing value or equivalent token that identifies which scheduler dispatch is currently authorized to report for a physical attempt.
- **Status:** Behavior accepted; representation and whether it is monotonic are provisional.
- **Do not confuse with:** A runner identifier, attempt identifier, or transport message sequence.
- **Example:** A result for generation 3 arrives after generation 4 was assigned; the hub records or discards it as stale without advancing current state.

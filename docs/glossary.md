# Glossary

This glossary recommends working language for design discussion. “Accepted” means the concept and distinction are accepted; it does not promise a stable API spelling. “Provisional” means the name, boundary, or both still need an ADR.

## Session

- **Definition:** A durable, independently browsable conversation with configuration, ordered semantic history, operational work, and archival state.
- **Status:** Name provisional; concept accepted. “Session” is preferred over “thread” because it emphasizes durable continuity across clients, though it can be confused with a login session.
- **Do not confuse with:** A client connection, one model context window, one turn, or a runner process.
- **Example:** A user starts “repair garden sensor” on a phone, continues it from a terminal, and archives it next week without losing its history.

## Turn

- **Definition:** One durable logical unit of accepted conversational work. It normally begins from user intent and reaches an explicit terminal state, while surviving zero or more physical orchestration attempts.
- **Status:** Name and exact retry boundary provisional; distinction from physical execution accepted. “Turn” is concise, but an ADR must define how regeneration and configuration changes relate to it.
- **Do not confuse with:** The user message itself, a provider call, an orchestration process, or every item displayed in a transcript.
- **Example:** “Summarize these changes” remains the same logical turn when the hub restarts and starts a replacement physical attempt, if the eventual retry policy permits that recovery.

## Turn attempt

- **Definition:** One physical orchestration effort to advance a turn, with its own identity, start/end state, failures, and consumed or produced effects.
- **Status:** Provisional. “Turn attempt” is preferred to generic “run,” which collides with runners and says little about logical ownership.
- **Do not confuse with:** The durable turn, an individual model call, or an individual tool attempt.
- **Example:** Attempt 1 loses its provider connection after a hub crash; attempt 2 later reconstructs context and continues the same turn under a permitted recovery rule.

## Model call

- **Definition:** One physical interaction initiated by the hub with a model provider, recording requested selection, exact resolved provider/model, context frontier, response metadata, and outcome.
- **Status:** Provisional name; required provenance accepted. “Model call” is clearer than “completion” because providers and response shapes vary.
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

- **Definition:** The reason a session exists, such as direct user creation, application creation, schedule, or delegation from parent work.
- **Status:** Concept accepted; labels and representation provisional.
- **Do not confuse with:** Transcript ancestry. Cause answers “why created,” not “where initial context came from.”
- **Example:** A delegated child session has delegation as its cause even if it starts with an empty transcript and only a task brief.

## Transcript ancestry

- **Definition:** The source frontier from which a session's initial semantic conversation context was derived, or an explicit absence of such a source.
- **Status:** Single-source initial concept accepted; final name and any future multi-source model provisional. “Transcript ancestry” is preferred to “parent session,” because delegation and ancestry are independent.
- **Do not confuse with:** Creation cause, ongoing related-session links, or ownership.
- **Example:** A user-created session forks session A through message 18; its cause is user creation and its ancestry is A at frontier 18.

## Input delivery policy

- **Definition:** The accepted instruction for handling user input submitted while a turn is active: interrupt, next safe point, or after current turn.
- **Status:** Three policy meanings accepted; name and default presentation provisional. “Delivery policy” is preferred to “message priority,” which would not express lifecycle semantics.
- **Do not confuse with:** Transport delivery guarantees or the final determination of which model context consumes the message.
- **Example:** “Use the new error log” with next-safe-point policy becomes durable immediately and is considered before the next model call, not injected into the current request.

## Runner

- **Definition:** An outbound-connected process that advertises capabilities and an execution boundary, then performs selected runner-local tool attempts under one deployment identity.
- **Status:** Name and boundary accepted at the architecture level; exact protocol provisional.
- **Do not confuse with:** A turn attempt, the central scheduler, a client, or a guarantee of sandboxing.
- **Example:** A runner on a laptop advertises access to `/Users/me/project` and truthfully states that commands execute as the logged-in user.

## Execution boundary

- **Definition:** The actual identity and isolation properties of a runner deployment, including relevant OS user, container, sandbox, VM, filesystem scope, and other enforceable constraints.
- **Status:** Concept accepted; capability vocabulary provisional. This term is preferred to a single “sandboxed” Boolean.
- **Do not confuse with:** Tool approval, claimed capability alone, or the trustworthiness of command content.
- **Example:** A restricted runner executes as a dedicated account inside a container with one mounted workspace; the UI shows those facts before selection.

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

- **Definition:** A physical outcome where available evidence cannot establish whether an external effect occurred, usually because acknowledgement or observation was lost.
- **Status:** Concept and no-blind-retry rule accepted; reconciliation states provisional.
- **Do not confuse with:** A known failure, an ordinary retryable read, or “probably failed.”
- **Example:** A runner loses connectivity immediately after submitting a payment-like external write; the hub records ambiguity and requires reconciliation instead of dispatching it again.

## Context frontier

- **Definition:** An immutable reference to the exact ordered semantic inputs and eligible steering content consumed by one model call.
- **Status:** Provisional representation; per-call provenance requirement accepted.
- **Do not confuse with:** The latest session transcript, an entire turn, or client rendering state.
- **Example:** Model call 1 consumes frontier 42; steering becomes eligible, so model call 2 consumes frontier 47 within the same turn.

## Dispatch generation

- **Definition:** A monotonic fencing value or equivalent token that identifies which scheduler dispatch is currently authorized to report for a physical attempt.
- **Status:** Behavior accepted; representation and whether it is monotonic are provisional.
- **Do not confuse with:** A runner identifier, attempt identifier, or transport message sequence.
- **Example:** A result for generation 3 arrives after generation 4 was assigned; the hub records or discards it as stale without advancing current state.

# Glossary

This glossary recommends working language for design discussion. “Accepted” means the concept and distinction are accepted; it does not promise a stable API spelling. “Provisional” means the name, boundary, or both still need an ADR.

## Session

- **Definition:** A durable, independently browsable conversation with versioned configuration defaults, ordered semantic history, operational work, and archival state.
- **Status:** Concept accepted; the name and identity boundary are proposed by [ADR-0001](decisions/0001-domain-terminology-and-identity.md). “Session” is preferred over “thread” because it emphasizes durable continuity across clients, though it can be confused with a login session.
- **Do not confuse with:** A client connection, one model context window, one turn, or a runner process.
- **Example:** A user starts “repair garden sensor” on a phone, continues it from a terminal, and archives it next week without losing its history.

## Accepted input

- **Definition:** One user submission made durable with its explicit delivery request and recoverable disposition before acknowledgement.
- **Status:** The durability requirement is accepted; the name, identity boundary, and dispositions are proposed by [ADR-0001](decisions/0001-domain-terminology-and-identity.md) and [ADR-0027](decisions/0027-input-delivery-lifecycle.md).
- **Do not confuse with:** A transport command, transcript entry, turn, or model call. One accepted input may originate a turn or steer an existing turn.
- **Example:** “Use the new log” remains the same accepted input whether it is consumed at a safe point or visibly reclassified as queued work because the active turn ends first.

## Durable command identity

- **Definition:** One owner-global idempotency identity for a durably handled discriminated caller command across all command kinds, sessions, and clients under that owner. Purpose-specific canonical construction produces the comparison payload from the variant and every caller-supplied semantic field other than the identifier; first committed handling records that payload and a terminal applied-or-rejected result. An unconstructible request claims no identifier.
- **Status:** Lookup-before-validation is accepted in principle; the owner-global namespace and cross-kind/session mismatch rule are proposed by [ADR-0001](decisions/0001-domain-terminology-and-identity.md) and [ADR-0027](decisions/0027-input-delivery-lifecycle.md).
- **Do not confuse with:** Accepted-input identity, logical-work identity, a provider idempotency key, or a token reusable in separate session/command namespaces.
- **Example:** Replaying one `SubmitInput` identifier and canonical payload returns its stored acceptance or rejection even after state changes, while reusing that claimed identifier for `ResolveAmbiguity` or another session is rejected before state validation. Equivalent normalized forms compare equal; corrected unconstructible input may reuse an unclaimed identifier, while correction after recorded domain rejection uses a new one.

## Turn

- **Definition:** One durable logical request for Signalbox to produce a conversational outcome from one typed origin under one frozen effective configuration. Typed input-delivery or recovery transitions, never semantic comparison of natural-language intent, determine whether its identity is preserved. It reaches one explicit terminal disposition while surviving zero or more physical orchestration attempts and may use several context frontiers.
- **Status:** The logical/physical distinction is accepted; the name and exact lifecycle are proposed by [ADR-0001](decisions/0001-domain-terminology-and-identity.md) and [ADR-0004](decisions/0004-turn-and-attempt-lifecycle.md).
- **Do not confuse with:** The user message itself, a provider call, an orchestration process, or every item displayed in a transcript.
- **Example:** “Summarize these changes” remains the same logical turn while awaiting an ambiguity decision; resolving evidence or an owner decision may later create a replacement attempt for unfinished work.

## Turn attempt

- **Definition:** One exclusive physical orchestration tenure that advances an active running turn until it ends or yields to a durable wait. Activation creates it; closing a wait creates one only when unfinished work remains and the applicable policy permits continuation, while a closure supported by a terminal outcome creates none. A startup recovery scan is not an attempt.
- **Status:** The physical identity distinction is accepted; the name and lifecycle are proposed by [ADR-0001](decisions/0001-domain-terminology-and-identity.md) and [ADR-0004](decisions/0004-turn-and-attempt-lifecycle.md). “Turn attempt” is preferred to generic “run,” which collides with runners and says little about logical ownership.
- **Do not confuse with:** The durable turn, an individual model call, or an individual tool attempt.
- **Example:** An attempt ends when orchestration enters a durable approval wait; a new attempt continues the same active turn after approval.

## Model call

- **Definition:** One durable hub authorization to attempt a physical interaction with a model provider, created only after requested selection, frozen alias meaning when applicable, exact hub-resolved provider/model target, and context frontier are known. It may terminate during send preparation before any request reaches the provider.
- **Status:** Required provenance is accepted; the name, identity, and retry boundary are proposed by [ADR-0001](decisions/0001-domain-terminology-and-identity.md) and [ADR-0005](decisions/0005-model-call-retry-semantics.md). “Model call” is clearer than “completion” because providers and response shapes vary.
- **Do not confuse with:** A turn, an alias resolution, all provider retries as a group, or the assistant message eventually committed to history.
- **Example:** A safe-point steering message leads to a second model call in the same turn; each call records the precise context it consumed.

## Outcome-authoritative provider call

- **Definition:** The sole model call currently eligible to determine one provider interaction's conversational completion, refusal, failure, or cancellation. Closing a duplicate-risk provider wait atomically creates the replacement attempt and prepared call and transfers this role without deleting or reopening the prior call.
- **Status:** Proposed by [ADR-0005](decisions/0005-model-call-retry-semantics.md) as the deterministic replacement rule.
- **Do not confuse with:** The most recently observed result, every continuation call in a turn, or suppression of audit/reconciliation evidence.
- **Example:** After the owner authorizes a replacement for an ambiguous call, a late answer from the prior call remains visible evidence, while only the replacement can supply the turn's authoritative outcome.

## Provider-target mismatch invalidation

- **Definition:** A typed, unique-by-call value recorded when trusted mismatch evidence is first learned after the currently outcome-authoritative call already completed but before its turn terminalized. It binds the invalidated call and first evidence record; the serialized aggregate validates the call's exact target and current authority from canonical call and transfer records instead of copying them into the invalidation. It preserves the call and committed history and prohibits that material from authorizing new semantic effects. An ordinary outcome-authoritative refusal without fatal stop terminalizes its turn atomically and therefore has no corresponding active-turn invalidation window; a continuation refusal raced under fatal stop remains only physical evidence.
- **Status:** Proposed by [ADR-0005](decisions/0005-model-call-retry-semantics.md).
- **Do not confuse with:** Reopening the terminal call, deleting its content, an allowed fallback, or late audit evidence after authority transfer or turn terminalization.
- **Example:** A response commits as completed while a tool is still running; trusted metadata then reports a different model, so the call remains completed but its typed invalidation stops further effects and forces turn failure or reconciliation.

## Tool request

- **Definition:** A logical request for one named tool operation with normalized arguments, policy state, and eventual logical outcome.
- **Status:** The logical/physical distinction is accepted; the name and boundary are proposed by [ADR-0001](decisions/0001-domain-terminology-and-identity.md).
- **Do not confuse with:** The model's unvalidated text, an approval prompt, an executor dispatch, or a physical retry.
- **Example:** The request “delete branch `old-demo`” retains one identity while awaiting approval and while a single approved execution is dispatched.

## Tool attempt

- **Definition:** One physical effort by a hub-local or runner-local executor to perform a tool request, including dispatch identity, executor placement, timing, output, and outcome classification.
- **Status:** The logical/physical distinction is accepted; the name and boundary are proposed by [ADR-0001](decisions/0001-domain-terminology-and-identity.md). This is preferred to “tool call,” which can obscure the difference between logical request and physical effect.
- **Do not confuse with:** The logical tool request, provider-native function-call syntax, or a scheduler's delivery retry.
- **Example:** A read-only file search loses its runner connection; the tool attempt is `KnownFailed` if evidence proves no effect occurred and otherwise `Ambiguous`, after which policy may allow a second attempt for the same tool request.

## Creation cause

- **Definition:** The typed reason a session exists. The first implementable value is owner-initiated; application, schedule, delegation, or other causes require feature ADRs that define their exact durable initiator identities.
- **Status:** The independent concept is accepted; immutable owner-initiated baseline and typed extension rule are proposed by [ADR-0003](decisions/0003-session-creation-and-transcript-ancestry.md).
- **Do not confuse with:** Transcript ancestry. Cause answers “why created,” not “where initial context came from.”
- **Example:** An owner-created fork has owner initiation as its cause and a separate source frontier as ancestry. After ADR-0002 defines delegation, a child may instead carry its newly defined delegated cause without necessarily inheriting transcript context.

## Transcript ancestry

- **Definition:** The source frontier from which a session's initial semantic conversation context was derived, or an explicit absence of such a source.
- **Status:** Single-source initial concept is accepted; the immutable `none` or one-source boundary is proposed by [ADR-0003](decisions/0003-session-creation-and-transcript-ancestry.md). Any future multi-source model remains open. “Transcript ancestry” is preferred to “parent session,” because delegation and ancestry are independent.
- **Do not confuse with:** Creation cause, ongoing related-session links, or ownership.
- **Example:** An owner-created session forks session A through message 18; its cause is `OwnerInitiated` and its ancestry is A at frontier 18.

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

- **Proposed version-one behavior:** Under [ADR-0005](decisions/0005-model-call-retry-semantics.md), a known provider-call failure is not retried automatically and supplies explicit failure evidence; the turn fails when no unacknowledged ambiguity requires a wait or reconciliation. A physically cancelled provider call without an accepted owner cancellation cause also supplies turn failure while retaining its distinct physical disposition; turn cancellation requires that exact cause plus proof it prevented remaining work. The first trusted provider-reported identity that mismatches the call's exact resolved target while it is nonterminal immediately selects known failure and requests best-effort cancellation. After terminal ambiguity it preserves physical state and fails the turn only when no other unacknowledged ambiguity remains; otherwise the turn requires reconciliation. After completion during an active turn it appends typed invalidation and stops future effects without reopening the call; after terminal known failure/cancellation it preserves that state and existing precedence. Ordinary refusal without fatal stop terminalizes the baseline turn atomically; a continuation refusal under fatal stop remains physical non-authoritative evidence while failure/reconciliation controls. Evidence first learned after authority transfer is audit-only. Tool retry policy remains effect-specific later scope.

## Ambiguous outcome

- **Definition:** A physical outcome where available evidence cannot establish whether an external effect occurred, usually because acknowledgement or observation was lost. Under the proposed lifecycle, unacknowledged ambiguity retains the turn's active slot while awaiting evidence or an explicit recovery decision only when neither accepted cancellation nor fatal mismatch prohibits continuation. Owner-authorized continuation preserves the physical `Ambiguous` outcome and adds a separate accepted-risk marker; accepted cancellation or fatal stop before acknowledgement terminalizes the turn as reconciliation required.
- **Status:** Concept, no-blind-retry rule, and preservation of the ambiguous physical record are accepted; wait, accepted-risk, and reconciliation transitions are proposed by [ADR-0004](decisions/0004-turn-and-attempt-lifecycle.md) and [ADR-0005](decisions/0005-model-call-retry-semantics.md).
- **Do not confuse with:** A known failure, an ordinary retryable read, or “probably failed.”
- **Example:** A runner loses connectivity immediately after submitting a payment-like external write; the hub records ambiguity and requires reconciliation instead of dispatching it again.

## Context frontier

- **Definition:** An immutable reference to the exact ordered semantic content consumed by one model call, including applicable user inputs, consumed steering, committed assistant or tool content, and explicit completion, refusal, failure, cancellation, accepted-risk, typed provider-target-invalidation, or ambiguity markers.
- **Status:** Per-call provenance is accepted; accepted-input-origin starting-frontier and safe-point selection rules are proposed by [ADR-0027](decisions/0027-input-delivery-lifecycle.md). Future non-input origins must define their own starting-frontier rules. Representation remains provisional.
- **Do not confuse with:** The latest session transcript, an entire turn, or client rendering state.
- **Example:** Model call 1 consumes frontier 42; steering and a tool result become committed, so model call 2 consumes frontier 47 within the same turn. If call 2 fails, any future explicitly authorized call still retains that committed content.

## Queue order

- **Definition:** The durable total ordering of known accepted-input-origin work derived from immutable acceptance positions and typed priority relations such as an interrupt's immediate-successor relation.
- **Status:** Proposed by [ADR-0027](decisions/0027-input-delivery-lifecycle.md); persistence representation remains open.
- **Do not confuse with:** A direct predecessor pointer fixed when input is accepted, a mutable user-reorderable queue, or wall-clock scheduler order.
- **Example:** While A is active, after-current B is accepted and then interrupt I is accepted; the priority relation orders I before B without rewriting a fixed predecessor on B.

## Starting lineage

- **Definition:** The immutable first-in-session or exact immediate-predecessor relation fixed for an accepted-input-origin turn when it becomes eligible from durable queue order.
- **Status:** Proposed by [ADR-0027](decisions/0027-input-delivery-lifecycle.md); future non-input origins must define their own lineage rules.
- **Do not confuse with:** Queue order before eligibility, transcript ancestry, or turn-attempt lineage.
- **Example:** After I terminalizes, B fixes `After(I)` and derives its starting context through I even though B was accepted before I.

## Session configuration defaults

- **Definition:** A mutable-by-version session-level value used to resolve configuration requests for future origin input. Creation establishes the first immutable version; each explicit update installs another.
- **Status:** The user/session-level role and version/freeze relationship are proposed by [ADR-0003](decisions/0003-session-creation-and-transcript-ancestry.md) and [ADR-0027](decisions/0027-input-delivery-lifecycle.md).
- **Do not confuse with:** A turn's frozen effective configuration. Updating defaults never changes queued, active, waiting, or recovering work.
- **Example:** The owner changes the session's preferred model after message B was queued; B keeps the defaults version recorded at its acceptance, while message C accepted later uses the new version.

## Effective configuration

- **Definition:** The complete immutable semantic configuration governing one turn. Its minimum algebra explicitly represents direct or frozen-alias model selection, parameters, absent or frozen instructions, disabled tools or a normalized nonempty enabled-tool set with unconstrained/canonical nonempty placement constraints, disabled known-provider-failure retry and fallback, turn resource policy, and interpreting policy versions. Placement has no variant when tools are disabled; empty/no-op forms normalize to `Disabled` or `Unconstrained`. Every constructible field is identity-significant and equality is semantic value equality.
- **Status:** Durable provenance is accepted; the closed semantic categories, operational exclusions, immutability, equality, and freeze boundary are proposed by [ADR-0004](decisions/0004-turn-and-attempt-lifecycle.md), [ADR-0005](decisions/0005-model-call-retry-semantics.md), and [ADR-0027](decisions/0027-input-delivery-lifecycle.md). Nested subsystem representations remain open without reopening whether they are identity-significant.
- **Do not confuse with:** The exact provider/model target resolved for a model call, current hub defaults, or a client-side draft selection.
- **Example:** A queued turn records its request, exact session-defaults version, and effective value. Pending safe-point steering records only its source turn; if reclassified, the new turn derives that source turn's canonical immutable effective value without inventing a request or accepting a conflicting copy.

## Dispatch generation

- **Definition:** A monotonic fencing value or equivalent token that identifies which scheduler dispatch is currently authorized to report for a physical attempt.
- **Status:** Behavior accepted; representation and whether it is monotonic are provisional.
- **Do not confuse with:** A runner identifier, attempt identifier, or transport message sequence.
- **Example:** A result for generation 3 arrives after generation 4 was assigned; the hub records or discards it as stale without advancing current state.

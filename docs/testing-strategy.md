# Testing strategy

This document describes tests expected once implementation begins. It does not select a Rust test framework, container library, client stack, or CI service.

## Principles

Tests should prove domain transitions and recovery semantics before optimizing transport or deployment. A feature's tests should use the same distinct identities as production design and assert durable state, not only a returned HTTP status or rendered screen.

Prefer deterministic inputs: fixed clocks, seeded identifiers, scripted provider/runner behavior, bounded schedulers, and explicitly advanced streams. Tests that depend on real provider availability or timing cannot be the primary merge gate.

Postgres is the test database for persistence behavior. Integration tests may start ephemeral Postgres instances in containers; SQLite is not a substitute for transaction, constraint, locking, or recovery semantics.

## Test layers

### Pure domain transitions

- Test each allowed state transition and reject every invalid predecessor/state combination.
- Use table-driven state-machine cases for turn, attempt, model-call, tool, approval, input-delivery, delegation, and archival lifecycles.
- Property-test invariants such as distinct identity preservation, terminal-state monotonicity, and “at most one progressing turn” at the pure decision level.
- Prove that typed delivery and recovery commands determine turn identity without comparing or classifying natural-language objective text.
- Model the turn and its current attempt as one aggregate transition even if persistence later separates their records. Prove every accepted-input turn retains immutable queue-order facts outside lifecycle state; `Queued` cannot carry a start payload, while every `Active` or `Terminal` variant carries one nonoptional immutable starting lineage/frontier. Prove eligibility transitions preserve order while adding start. Prove that `Active(Running)` carries exactly one `Prepared`, `Running`, or `StopRequested` current-attempt variant; `StopRequested` is either cancellation-only or fatal mismatch with a nonempty failure set and optional accepted cancellation, rejects empty/cross-wired references, prohibits effects, and preserves the matching value in terminal attempt history. Add accepted cancellation and fatal mismatch in both event orders, replay each, crash after each order, and prove equality, disposition precedence, and restart reconstruction are identical. Prove fatal stop cannot construct completed, refused, or cancelled attempt/turn outcomes; cancellation-only cannot yield to a wait; and no-stop cannot claim cancellation. Prove a prepared attempt ends with the turn on cancellation and that waiting phases carry their exact subject and no attempt. Crash with a current `Prepared` attempt after an earlier completed call but before startup first discovers that call's fatal target mismatch; prove the scan constructs `AfterFatalMismatch(Lost)` rather than the live `AfterFatalMismatch(KnownFailure)` or cause-free `WithoutStop(Lost)` branch.
- Reject every transition from running to any durable wait until all operations already issued by the attempt are terminally classified; stop-requested running work cannot enter a wait. Separately reject safe-point consumption while any earlier logical tool/approval dependency lacks a durable outcome, including an authorized request for which scheduling has not created a tool attempt.
- Reject every terminalization while an owned logical tool/approval dependency is not durably closed or terminally non-dispatchable, an issued physical operation is unclassified, a current attempt is nonterminal, or a durable wait remains open. Additionally reject `Completed`, `Refused`, `Failed`, and `Cancelled` while a physical ambiguity has neither resolving evidence nor an exact owner-accepted-risk marker; permit `ReconciliationRequired` only when no durable wait remains open and the unresolved physical ambiguity plus its semantic reconciliation marker are preserved. Cover both direct cancellation-plus-ambiguity from a running attempt, where no wait was created, and cancellation that closes an existing exact recovery wait.
- Treat queue eligibility as a deterministic predicate over durable total queue order and slot ownership. Activation and eligible failure must each construct exact starting lineage and frontier atomically; reject queued-with-start and active/terminal-without-start values.
- Construct every minimum effective-configuration variant, including absent instructions, disabled tools, normalized nonempty enabled tools with unconstrained or canonical nonempty placement, disabled known-failure retry/fallback, and resource policy. Prove empty enabled-tool and empty/no-op constrained-placement values cannot be constructed distinctly and normalize/reject consistently before acceptance; prove disabled tools cannot carry placement; then compare full constructible semantic values rather than record identifiers.
- Prove session-default updates affect only later accepted origins; explicit origins retain request/default-version/effective provenance, while pending steering retains only source-turn provenance. Reclassification must construct the new turn's effective configuration from that canonical immutable source value without an invented request or caller-supplied copy.
- Construct pending steering from exactly one `SteeringBinding` containing only `source_turn`; prove consumption validates the referenced call belongs to that source turn and derives the immutable frontier from the call. Prove reclassification rejects a missing/cross-session source and offers no constructor parameter for a different inherited configuration. No domain constructor accepts a duplicate turn, configuration, or frontier that could be cross-wired.
- For every durable command, prove owner-global same-identifier/same-discriminated-payload replay returns the stored applied or rejected result before current-state validation. First reject a well-formed typed command under established owner authority, change the state that caused rejection, and prove replay still returns the recorded rejection; corrected intent succeeds only under a new identifier. Reuse the claimed identifier from another client for a different command kind, another session, and a changed payload; each must be rejected before either aggregate validates or mutates state. Prove malformed transport, pre-authority, and pre-commit infrastructure failures outside committed domain handling claim no identifier, and separately generated identifiers remain independent even when caller payloads are equal.
- Model effects as requested decisions rather than performing I/O, so tests can assert ordering such as “persist before provider send.”
- Before regeneration becomes implementable, require a future-feature fixture proving one idempotent command creates exactly one new `TurnId`, never adds an attempt to or changes the original turn/output, freezes configuration only on the new turn, and records the exact typed relation, source frontier, and queue placement chosen by the required future ADR.

These tests are required with the first implementation of each state machine.

### Postgres integration

- Exercise real migrations, constraints, transactions, isolation behavior, idempotency keys, and compare-and-set fencing against ephemeral Postgres containers.
- Concurrently submit one unseen `DurableCommandId` through different command kinds, sessions, and storage mappings. Prove exactly one owner-wide transaction records an applied or rejected result, every conflicting payload is rejected without aggregate mutation, and equal replay returns the winner's result. Repeat with the first committed result a typed rejection, change relevant session state, and prove neither replay nor a cross-aggregate mapping can revalidate or reclaim the identifier.
- Race two queued turns to activate work in the same session and prove only the first in durable total order atomically acquires the slot, fixes its exact starting lineage/frontier, and creates its initial attempt; a queued turn has no constructible direct predecessor before eligibility.
- During a scheduler gap with queued turn B and no active slot owner, accept C using `StartWhenNoActiveTurn`; prove B activates first and C remains at the FIFO tail, then fixes its frontier through B.
- Complete turn A, then accept B using `StartWhenNoActiveTurn` while the session has no active or queued work; prove B has `After(A)` lineage and fixes its frontier through A rather than restarting from session ancestry. Reject `FirstInSession` for B.
- Crash or terminate orchestration at transaction boundaries and verify acknowledged input, queued work, confirmation waits, and, once ADR-0002 defines them, delegation waits reconstruct correctly.
- Make a queued turn unexecutable while its predecessor remains active; prove it cannot terminalize early and that its eventual failure frontier includes the predecessor's terminal outcome.
- Use a table-driven frontier fixture for `Completed`, `Refused`, `Failed`, `Cancelled`, and `ReconciliationRequired` predecessors, with and without `DuplicateRiskAccepted` where legal. Prove ordinary queued, interrupt-created, and reclassified-steering successors each fix the complete ordered lineage prefix and the exact disposition/risk markers rather than an implicit latest transcript.
- Accept safe-point steering X, ordinary after-current B, and then interrupt I while A is active; terminalize A before X is consumed and prove durable total order is A, I, X, B. Stop or crash before and after interrupt acceptance, reclassification, slot release, and each successor activation. On every restart, prove only one successor is eligible, I fixes `After(A)`, X fixes `After(I)`, B fixes `After(X)`, and every frontier contains the complete prefix rather than merely executing in the right order.
- Deliver duplicate commands/results and stale generations in different orders; prove state advances at most once and current state is not overwritten.
- Replay an accepted interrupt and an accepted ambiguity decision after the session has advanced; prove deduplication returns the original result rather than applying stale-state validation. Reuse each command identifier with changed payload and prove rejection.
- Accept an origin `SubmitInput`, lose its reply, update session defaults, aliases, and interpreting policies, then replay the identical caller payload; prove lookup returns the original accepted-input identity and stored `OriginConfiguration` before resolver or compare-and-set validation. Reusing its identifier with changed caller choices is rejected.
- Race an unseen input's precomputed configuration candidate against a session-default, alias-definition, or interpreting-policy update. Prove a defaults-version mismatch fails without adopting the replacement, while any other changed resolver input is recomputed against a transactionally validated read set or produces a retryable conflict.
- Keep storage records behind explicit mappings and test unknown/corrupt values fail visibly.

These tests are required for any persistence or concurrency behavior before merge. High-volume contention and multi-region database behavior are later deployment work.

### Provider adapters

Use scripted fake provider adapters that can:

- stream deterministic chunks and finish normally;
- fail before send, after send, or after selected chunks;
- report a model identity, a substitution or mismatch, a safety refusal, capacity failure, or malformed response;
- block until cancellation or simulate an unresponsive connection; and
- expose received context so each call's frontier can be asserted.

Adapter contract tests should run the same provenance cases for every real provider. Live-provider smoke tests may exist behind explicit credentials but are not the sole correctness test and should not be mandatory for ordinary contributions.

Target-resolution fixtures must freeze an alias definition at input acceptance, mutate the current alias, and prove the queued turn still resolves from its frozen meaning during its initial prepared attempt. Resolution failure creates no `ModelCallId`, ends that attempt as known failure, and fails the turn. Separate static pre-activation rejection creates no attempt. Send-preparation failure occurs only after a fully targeted call exists and retains that call identity. For each provider adapter, report a trusted mismatched identity while the call is still in flight, before later completion, refusal, disconnect, cancellation response, and hub crash. Every case must atomically store the typed evidence, select known failure, prohibit new semantic effects, request best-effort cancellation, and keep all later response material audit-only. Unless terminal guards already permit an immediate end, the attempt must carry the exact fatal observation in `StopRequested`; it cannot remain `Running`. An immediate end must preserve that same cause in `AttemptEnd::AfterFatalMismatch`. Replay the evidence identifier with equal and changed payloads to prove idempotency and rejection. After every other issued operation is classified, the attempt/turn end as fatal known failure/failed when no ambiguity remains or fatal ambiguity/reconciliation otherwise; a startup-abandoned attempt uses the matching terminal variant with disposition `Lost`. Matching and absent observations follow their ordinary outcome paths. Separately end the only blocking call/attempt as ambiguous, then deliver resolving mismatch evidence: both physical dispositions remain ambiguous while the waiting turn becomes failed.

Also first terminalize a current-authority call as `Completed` while its turn remains active in each of these states: awaiting approval, carrying an authorized-but-undispatched tool request, running a tool, awaiting recovery for an unrelated ambiguous operation, and before or during a continuation call. Deliver mismatch and prove the call and committed history do not reopen or disappear; exactly one typed invalidation is appended, bound only to the invalidated call and first evidence record. Prove the comparison uses the exact target on the canonical call and serializes against the canonical authority-transfer chain rather than accepting duplicated target or authority fields. Reject wrong-call, conflicting, and duplicate invalidations; make transfer win the race and prove the evidence becomes non-authoritative; structurally equal replay is idempotent. Prove no new semantic effect dispatches, waits/undispatched requests close, issued work receives best-effort cancellation and honest classification, and the turn retains its slot until it fails or reaches reconciliation under ambiguity precedence. Race an already-issued continuation to `Completed`, `Refused`, `Cancelled`, `KnownFailed`, and `Ambiguous`, live and on startup: each physical disposition remains honest, but fatal stop makes its material non-authoritative, preserves `AfterFatalMismatch` with `KnownFailure`, `Ambiguous`, or `Lost`, and permits only turn failure/reconciliation. The unrelated-ambiguity wait must reject owner continuation and close as `ReconciliationRequired` with both facts preserved, while mismatch that resolves the waited call's own ambiguity preserves that call's physical state and permits `Failed` only when no other ambiguity remains. Add a multi-reference wait: mismatch resolves one ambiguous model call while another ambiguous operation remains, and prove the turn closes as `ReconciliationRequired` rather than `Failed` or another wait. A successor frontier includes the invalidated material, typed marker, effect evidence, and final outcome. Separately reject an ordinary `Refused` call paired with `Running`, approval, recovery, or other effect-authorizing/nonterminal state; without fatal stop a matching target must atomically terminalize call/attempt/turn, while mismatch delivered in that transaction selects known failure. Permit `Refused` plus nonterminal turn only inside `StopRequested(FatalMismatch)` until remaining classifications finish. Finally first end calls as `KnownFailed` and `Cancelled`, then learn mismatch and prove their physical state and existing turn-outcome precedence do not change. Learn mismatch only after authority transfer and after a valid turn terminalization, including refusal; prove it appends evidence without changing authority, committed content, disposition, or an already-fixed successor frontier. A durable mismatch observation paired with a nonterminal call is rejected as corrupt because the live observation transition is atomic.

Known-provider-failure fixtures must terminalize the call as known failed, the attempt as known failure, and the turn as failed without creating another call or attempt, including after a prior cancellation request and for provider-reported target mismatch classified before call terminalization. Continuation fixtures must prove that later authorized calls retain steering already committed to turn history.

An after-send unknown-acceptance fixture must end the current attempt as ambiguous, place the turn in `AwaitingRecoveryDecision` only when neither accepted cancellation nor fatal mismatch prohibits continuation, retain the session slot across restart, and reject automatic retry. Owner decisions must carry the exact nonempty operation set with order-insensitive equality and no duplicates: exact match may continue or stop, while stale, partial, expanded, or duplicate-bearing sets are rejected. With several ambiguities, separate evidence must refine the wait to the exact nonempty subset; resolving the final reference terminalizes as failed for a known provider failure or as cancelled for confirmed provider cancellation under the common precedence, and creates a new attempt only for other unfinished work whose operation-specific policy permits continuation. Accepted cancellation or fatal stop plus unacknowledged ambiguity instead terminalizes as reconciliation required.

Run one table-driven classification matrix both live and through startup recovery. Unacknowledged ambiguity wins first and yields a wait or, with accepted cancellation/fatal stop, reconciliation; otherwise sufficient outcome-authoritative non-mismatched completion retains that outcome only without fatal stop, an ordinary outcome-authoritative non-mismatched refusal without fatal stop atomically terminalizes the turn, known failure—including target mismatch observed before physical terminalization—yields failure even after cancellation was requested, and evidence that cancellation prevented all remaining work yields cancellation. Completion/refusal raced under fatal stop remains a physical disposition while failure/reconciliation controls. Resolving mismatch evidence after terminal ambiguity must preserve that physical state while selecting failure only when no other ambiguity remains and reconciliation otherwise. Discover mismatch after a current-authority call completed but before turn terminalization both live and on startup, crash before and after typed invalidation, and prove exactly one marker, preserved stop causes, a `Lost` abandoned attempt in the matching terminal variant, slot retention, and the same failure/ambiguity precedence. Terminal known-failed/cancelled calls must remain unchanged, and post-refusal evidence cannot rewrite the atomically terminal turn. Exercise `ContinueAcceptingDuplicateRisk` followed by completed, refused, failed, and cancelled outcomes, including the direct-terminal case where no work remains. Every case must preserve the physical `Ambiguous` record, record the risk marker once under command replay, and expose that marker in the next turn's frontier.

Duplicate-risk provider fixtures must close the recovery wait, record accepted risk, consume every eligible pending steering input, create the replacement attempt and prepared call with that frontier, and transfer outcome authority in one transaction. Accept steering while the original call is in flight or awaiting recovery, then replay/crash at each transaction boundary; prove each input is consumed exactly once by reference to the replacement call, is present in its frontier, and can never reconstruct as pending or be reclassified later. Race that command against completion, refusal, known-failure, and confirmed-cancellation evidence for the prior call, both live and across restart. Evidence committed first follows the ordinary precedence and makes the command stale; the command committed first leaves no pre-transfer replacement-attempt window and makes every later prior-call outcome audit-only. Exercise prior/replacement pairs of completed/completed, completed/refused, refused/completed, refused/refused, known-failed/completed, cancelled/completed, known-failed/refused, and cancelled/refused in both evidence-arrival orders, including evidence that arrives after terminalization. The replacement alone determines turn disposition and authoritative content; all prior-call outcome evidence remains inspectable, is never inserted as competing semantic content, and cannot change the successor frontier. Repeat through a chain of two replacements and prove only the final replacement is outcome-authoritative.

A live ordinary non-mismatched refusal fixture must atomically preserve the call's `Refused` disposition, end the attempt `TurnRefused`, make the turn `Refused`, release the slot, and reject implicit retry or fallback. A startup fixture must classify the ordinary non-mismatched in-flight call `Refused` and terminalize the turn in one transaction while leaving the abandoned attempt `WithoutStop(Lost)`. Reject refusal with unrelated owned work unless a fatal mismatch already exists or is established from the complete evidence set in the same startup transaction; in that exception, preserve the physical refusal but end the attempt/turn only as fatal failure, loss, ambiguity, or reconciliation. Add the exact crash fixture where completed call A fed already-issued continuation B, neither A's mismatch nor B's refusal was durable before restart, and startup discovers both: B becomes physically refused, A gains invalidation, the attempt ends `AfterFatalMismatch(Lost)`, and the turn fails or reconciles without committing refusal content. A late-evidence fixture must keep an already-ambiguous call and attempt terminal. Mismatch delivered with or before ordinary refusal terminality makes refusal material non-authoritative and selects known failure; mismatch resolving ambiguity preserves that physical state, failing the turn only when no other ambiguity remains and requiring reconciliation otherwise; mismatch learned after atomic refusal changes none of the committed refusal facts.

### Outbound runners and tools

Use fake outbound runners with controlled declarations, trusted deployment configuration, verification evidence, effective properties, connection loss, delayed results, duplicate results, and stale dispatch generations. Tests must prove that an unsupported declaration cannot become a stronger effective guarantee. A fake executor must distinguish:

- a proven no-effect failure;
- a repeatable read;
- a completed write with acknowledgement;
- a write whose acknowledgement is lost; and
- a late result after reassignment or cancellation.

Tool ambiguity tests must prove that ambiguous writes enter `AwaitingRecoveryDecision`, retain the active slot, and are never automatically retried. Explicit owner action may terminalize for reconciliation; accepted-risk continuation must preserve the ambiguous write and its marker, and remains governed by the tool-effect policy. Approval tests must mutate one argument or material constraint and prove the prior approval no longer authorizes execution. They must also delay scheduling after approval and prove the authorized-but-undispatched request blocks safe-point steering consumption and later model-call preparation until its durable outcome. Run the same logical lifecycle contract against hub-local and runner-local executors.

Tool-result ownership fixtures must pair an otherwise valid `ToolAttemptId` and dispatch generation with the wrong logical tool request, the wrong issuing turn attempt, and a stale or non-current ownership relation. Each cross-wired result is rejected without advancing either request, attempt, turn attempt, or turn; only a result whose request ownership, issuing tenure, and current dispatch generation all match may advance logical state.

Interrupt and restart fixtures must also target that authorized-but-undispatched window. Terminalization must atomically make the exact tool request non-dispatchable with a durable outcome, and a later scheduler or stale result must be unable to revive it after the session slot is released.

Denial tests must close the exact approval wait, commit the denial to turn history, create a new turn attempt for conversational continuation, and prove that no physical tool attempt is created. Duplicate or late approval cannot reverse the denial.

These tests are required with tool and runner behavior. Real sandbox escape testing and platform-specific containment certification are required before claiming a production isolation profile, but are later than the first fake-runner slice.

### Streaming and client fixtures

Deterministic streaming tests should assign a draft identity and version, disconnect clients at every chunk boundary, then reconcile from an authoritative snapshot. They must prove that missing deltas are not promoted to final content, that safe-point steering only affects later provider calls after all earlier issued operations are classified, and that consumed steering remains in later call frontiers after the first consuming call fails.

Protocol compatibility fixtures should be language-neutral and cover:

- identifiers and enum/state evolution;
- required versus unknown fields;
- snapshots, transient deltas, and duplicate delivery;
- steering bindings and call-derived consumption ownership/frontiers without duplicate identity fields;
- approval binding and tool arguments;
- runner declaration/evidence distinctions, effective boundaries, and fenced results; and
- requested, hub-resolved, and provider-reported model provenance, refusal, and substitution.

Swift, web, and terminal clients should consume the same canonical fixtures while mapping wire types into client-owned types. Add rendering/UI tests only for client responsibilities: delivery-intent selection, boundary disclosure, confirmation content, draft replacement, and provenance display.

Compatibility fixtures are required before a second independently versioned process or language client merges. A broad device/OS/browser matrix belongs to later release hardening.

### Restart and recovery

Recovery tests should stop the hub at named boundaries rather than random sleeps:

1. before accepting input;
2. after durable acceptance but before scheduling;
3. after attempt creation but before external send;
4. after send but before outcome persistence;
5. while waiting for approval or, once implemented, a delegated result;
6. after outcome persistence but before client acknowledgement.

On restart, assert both the final state and the absence of forbidden effects. The idempotent startup scan derives the complete evidence and stop-cause set before ending every prior-process nonterminal turn attempt with disposition `Lost` in `WithoutStop`, `AfterCancellation`, or `AfterFatalMismatch` as applicable. It separately classifies each recorded physical operation as completed, known failed, refused where applicable, cancelled, or ambiguous; no physical operation receives a generic `Lost` disposition, and the scan creates no turn attempt. Separate fixtures prove sufficient recovered result evidence can complete the turn or, without fatal stop, atomically refuse it once no blocking ambiguity remains. A refusal discovered under a fatal stop already durable or first established by that scan remains physical evidence while failure/reconciliation controls. Otherwise an abandoned tenure without operation ambiguity or accepted cancellation/fatal stop fails the turn, ambiguity retains the slot in `AwaitingRecoveryDecision` only without either stop, and accepted cancellation/fatal stop plus unacknowledged ambiguity terminalizes for reconciliation. Re-running the scan changes nothing.

Include a crash immediately after activation creates a `Prepared` attempt but before it runs. Without a newly established stop cause, startup must end that attempt `WithoutStop(Lost)`, create no model call or replacement attempt, and fail the turn when no ambiguity or stronger outcome evidence exists. Repeat after an earlier completed call caused this prepared continuation and startup first discovers a target mismatch for that call; the scan must append invalidation and end the same abandoned prepared attempt `AfterFatalMismatch(Lost)` without issuing work.

Interrupt recovery tests must separately cover an unsent prepared attempt, an already-running attempt, a fatal-mismatch `StopRequested` value without cancellation, an approval wait, and a recovery wait. Crash after durable accepted cancellation but before classification; prove startup creates no cancellation-only attempt, preserves the exact `CancellationOnly` or `FatalMismatch` value and any ambiguous operation, terminalizes the predecessor appropriately, and does not activate its immediate successor before the scan's classification transaction completes.

Boundary recovery tests are required with each durable effect. Long soak tests, repeated pod eviction, database failover, and Kubernetes disruption suites are later deployment work.

### End-to-end vertical slices

Each major capability should add one narrow end-to-end slice through a real client protocol, hub, Postgres, and fake external boundary. Early slices should cover, in order determined by later planning:

- create session, accept input, make a fake model call, and reconnect to final output;
- request a harmless runner-local tool and reject a stale result;
- wait for and approve a risky tool with exact binding;
- restart with queued work and with an interrupted external call; and
- fork or delegate with an independently browsable child.

End-to-end tests complement rather than replace domain and integration tests. Kubernetes deployment, real remote sandbox providers, real provider failover, load, chaos, and geographic latency are reserved until the corresponding production architecture exists.

## Merge requirements by change

| Change | Tests required before merge | Later scale/deployment tests |
| --- | --- | --- |
| Domain state or invariant | Transition table, invalid transitions, relevant properties, scenario update | Large generated traces if ordinary property runs become insufficient |
| Persistence or scheduler | Real Postgres constraints/transactions, restart boundary, duplicate and stale races | High contention, failover, long soak, multi-pod disruption |
| Provider adapter | Fake contract and provider-specific parsing/provenance cases | Credentialed smoke, rate/capacity drills, multi-provider fallback if accepted |
| Tool or runner | Fake runner lifecycle, approval binding, disconnect ambiguity, fencing | Platform containment audit, resource exhaustion, remote sandbox certification |
| Protocol | Compatibility fixture and old/new behavior for supported window | Broad version matrix and network impairment suites |
| Client | Shared fixtures plus interaction tests for affected responsibilities | Full browser/device accessibility and performance matrix |
| Vertical feature | One deterministic end-to-end success path and its defining failure/restart path | Load, chaos, and production deployment exercises |

## Traceability

Test names or metadata should reference scenario and invariant identifiers when the connection is meaningful, for example `S12_INV011_rejects_stale_generation`. An accepted ADR that changes behavior must identify the tests and fixtures that make its consequences observable.

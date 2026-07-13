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
- Model the turn and its current attempt as one aggregate transition even if persistence later separates their records. Prove that `Active(Running)` carries exactly one `Prepared`, `Running`, or `CancellationRequested` current-attempt variant with no duplicate cancellation flag, that a prepared attempt ends with the turn on cancellation, and that waiting phases carry their exact subject and no attempt.
- Reject every transition from running to any durable wait until all operations already issued by the attempt are terminally classified; cancellation-requested running work cannot enter a wait.
- Reject terminalization while any issued physical operation is unclassified, a current attempt is nonterminal, a durable wait remains open, or a physical ambiguity has neither resolving evidence nor an exact owner-accepted-risk marker.
- Treat queue eligibility as a deterministic predicate over durable lineage and slot ownership; a queued failure must inherit its predecessor frontier before it can terminalize.
- Construct every minimum effective-configuration variant, including absent instructions, disabled tools, unconstrained placement, disabled known-failure retry/fallback, and resource policy. Compare full semantic values rather than record identifiers.
- Prove session-default updates affect only later accepted origins; explicit origins retain request/default-version/effective provenance, while reclassified steering retains source-turn/inherited-effective provenance without an invented request.
- For every durable command, prove same-identifier/same-payload replay returns the stored result before current-state validation, while same-identifier/different-payload reuse is rejected.
- Model effects as requested decisions rather than performing I/O, so tests can assert ordering such as “persist before provider send.”

These tests are required with the first implementation of each state machine.

### Postgres integration

- Exercise real migrations, constraints, transactions, isolation behavior, idempotency keys, and compare-and-set fencing against ephemeral Postgres containers.
- Race two queued turns to activate work in the same session and prove only one atomically acquires the slot, fixes its frontier, and creates its initial attempt.
- Crash or terminate orchestration at transaction boundaries and verify acknowledged input, queued work, confirmation waits, and, once ADR-0002 defines them, delegation waits reconstruct correctly.
- Make a queued turn unexecutable while its predecessor remains active; prove it cannot terminalize early and that its eventual failure frontier includes the predecessor's terminal outcome.
- Accept safe-point steering, ordinary after-current input, and then an interrupt; terminate before steering consumption and prove the interrupt-created turn is first while every remaining successor retains original accepted-input order.
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

Target-resolution fixtures must freeze an alias definition at input acceptance, mutate the current alias, and prove the queued turn still resolves from its frozen meaning during its initial prepared attempt. Resolution failure creates no `ModelCallId`, ends that attempt as known failure, and fails the turn. Separate static pre-activation rejection creates no attempt. Send-preparation failure occurs only after a fully targeted call exists and retains that call identity.

Known-provider-failure fixtures must terminalize the call as known failed, the attempt as known failure, and the turn as failed without creating another call or attempt, including after a prior cancellation request. Continuation fixtures must prove that later authorized calls retain steering already committed to turn history.

An after-send unknown-acceptance fixture must end the current attempt as ambiguous, place a non-cancelled turn in `AwaitingRecoveryDecision`, retain the session slot across restart, and reject automatic retry. Owner decisions must carry the exact nonempty operation set with order-insensitive equality and no duplicates: exact match may continue or stop, while stale, partial, expanded, or duplicate-bearing sets are rejected. With several ambiguities, separate evidence must refine the wait to the exact nonempty subset; resolving the final reference terminalizes a known provider failure under the version-one policy, and creates a new attempt only for other unfinished work whose operation-specific policy permits continuation. Cancellation plus unacknowledged ambiguity instead terminalizes as reconciliation required.

Run one table-driven classification matrix both live and through startup recovery. Unacknowledged ambiguity wins first and yields a wait or, with cancellation, reconciliation; otherwise sufficient completion and refusal retain those outcomes, known failure yields failure even after cancellation was requested, and evidence that cancellation prevented all remaining work yields cancellation. Exercise `ContinueAcceptingDuplicateRisk` followed by completed, refused, failed, and cancelled outcomes, including the direct-terminal case where no work remains. Every case must preserve the physical `Ambiguous` record, record the risk marker once under command replay, and expose that marker in the next turn's frontier.

A live refusal fixture must preserve the call's `Refused` disposition and end the attempt `TurnRefused`. A startup fixture must classify the in-flight call `Refused` while leaving the abandoned attempt `Lost`. A late-evidence fixture must keep an already-ambiguous call and attempt terminal. Every path commits explicit refusal content, makes the turn `Refused`, releases the slot, and rejects implicit retry or fallback.

### Outbound runners and tools

Use fake outbound runners with controlled declarations, trusted deployment configuration, verification evidence, effective properties, connection loss, delayed results, duplicate results, and stale dispatch generations. Tests must prove that an unsupported declaration cannot become a stronger effective guarantee. A fake executor must distinguish:

- a proven no-effect failure;
- a repeatable read;
- a completed write with acknowledgement;
- a write whose acknowledgement is lost; and
- a late result after reassignment or cancellation.

Tool ambiguity tests must prove that ambiguous writes enter `AwaitingRecoveryDecision`, retain the active slot, and are never automatically retried. Explicit owner action may terminalize for reconciliation; accepted-risk continuation must preserve the ambiguous write and its marker, and remains governed by the tool-effect policy. Approval tests must mutate one argument or material constraint and prove the prior approval no longer authorizes execution. Run the same logical lifecycle contract against hub-local and runner-local executors.

Denial tests must close the exact approval wait, commit the denial to turn history, create a new turn attempt for conversational continuation, and prove that no physical tool attempt is created. Duplicate or late approval cannot reverse the denial.

These tests are required with tool and runner behavior. Real sandbox escape testing and platform-specific containment certification are required before claiming a production isolation profile, but are later than the first fake-runner slice.

### Streaming and client fixtures

Deterministic streaming tests should assign a draft identity and version, disconnect clients at every chunk boundary, then reconcile from an authoritative snapshot. They must prove that missing deltas are not promoted to final content, that safe-point steering only affects later provider calls after all earlier issued operations are classified, and that consumed steering remains in later call frontiers after the first consuming call fails.

Protocol compatibility fixtures should be language-neutral and cover:

- identifiers and enum/state evolution;
- required versus unknown fields;
- snapshots, transient deltas, and duplicate delivery;
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

On restart, assert both the final state and the absence of forbidden effects. The idempotent startup scan ends every nonterminal prior-process attempt as `Lost`, classifies each recorded operation as completed, known failed/lost, refused where applicable, cancelled, or ambiguous, and creates no turn attempt. Separate fixtures prove sufficient recovered result evidence can complete or refuse the turn once no blocking ambiguity remains; otherwise loss without ambiguity fails a non-cancelled turn, ambiguity retains the slot in `AwaitingRecoveryDecision`, and cancellation plus unacknowledged ambiguity terminalizes for reconciliation. Re-running the scan changes nothing.

Include a crash immediately after activation creates a `Prepared` attempt but before it runs. Startup must end that attempt `Lost`, create no model call or replacement attempt, and fail the turn when no ambiguity or stronger outcome evidence exists.

Interrupt recovery tests must separately cover an unsent prepared attempt, an already-running attempt, an approval wait, and a recovery wait. Crash after durable running cancellation but before classification; prove startup creates no cancellation-only attempt, preserves any ambiguous operation, terminalizes the predecessor appropriately, and does not activate its immediate successor before the scan's classification transaction completes.

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

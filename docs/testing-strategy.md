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
- Prove that `Active(Running)` always carries exactly one current nonterminal attempt plus cancellation status, that requested cancellation pairs atomically with the attempt's `CancellationRequested` state while a prepared attempt ends atomically, and that waiting phases carry their exact wait subject and no attempt and close atomically on cancellation.
- Reject terminalization while any issued physical operation is unclassified, a current attempt is nonterminal, or a durable wait remains open.
- Treat queue eligibility as a deterministic predicate over durable lineage and slot ownership; a queued failure must inherit its predecessor frontier before it can terminalize.
- Compare semantic values across the complete closed effective-configuration categories for identity and recovery; do not substitute record-identifier equality, and prove that explicitly excluded operational changes preserve turn identity while safe-point input cannot supply independent configuration.
- Model effects as requested decisions rather than performing I/O, so tests can assert ordering such as “persist before provider send.”

These tests are required with the first implementation of each state machine.

### Postgres integration

- Exercise real migrations, constraints, transactions, isolation behavior, idempotency keys, and compare-and-set fencing against ephemeral Postgres containers.
- Race two queued turns to activate work in the same session and prove only one atomically acquires the slot, fixes its frontier, and creates its initial attempt.
- Crash or terminate orchestration at transaction boundaries and verify acknowledged input, queued work, confirmation waits, and, once ADR-0002 defines them, delegation waits reconstruct correctly.
- Make a queued turn unexecutable while its predecessor remains active; prove it cannot terminalize early and that its eventual failure frontier includes the predecessor's terminal outcome.
- Accept safe-point steering, ordinary after-current input, and then an interrupt; terminate before steering consumption and prove the interrupt-created turn is first while every remaining successor retains original accepted-input order.
- Deliver duplicate commands/results and stale generations in different orders; prove state advances at most once and current state is not overwritten.
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

Retry fixtures must consume steering into a prepared call and fail that call both before and after send. Every later retry or continuation must retain the consumed steering in its recorded frontier, while a call that fails before send still retains its own model-call identity.

An after-send unknown-acceptance fixture must end the current attempt as ambiguous, place a non-cancelled turn in `AwaitingRecoveryDecision`, retain the session slot across restart, and reject automatic retry. Separate cases must prove that new evidence, explicit owner-authorized continuation, and owner-selected terminal reconciliation are the only exits. Cancellation plus the same ambiguity must instead terminalize as reconciliation required.

A refusal fixture must preserve the call's `Refused` disposition, end the attempt as `TurnRefused`, commit explicit refusal content, terminalize the turn as `Refused` rather than completed or failed, release the slot, and reject implicit retry or fallback.

### Outbound runners and tools

Use fake outbound runners with controlled declarations, trusted deployment configuration, verification evidence, effective properties, connection loss, delayed results, duplicate results, and stale dispatch generations. Tests must prove that an unsupported declaration cannot become a stronger effective guarantee. A fake executor must distinguish:

- a proven no-effect failure;
- a repeatable read;
- a completed write with acknowledgement;
- a write whose acknowledgement is lost; and
- a late result after reassignment or cancellation.

Tool ambiguity tests must prove that ambiguous writes enter `AwaitingRecoveryDecision`, retain the active slot, and are never automatically retried. Explicit owner action may terminalize for reconciliation, while any future continuation remains governed by the tool-effect policy. Approval tests must mutate one argument or material constraint and prove the prior approval no longer authorizes execution. Run the same logical lifecycle contract against hub-local and runner-local executors.

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

On restart, assert both the final state and the absence of forbidden effects. Every nonterminal pre-restart turn attempt must be ended or fenced before continuation. Provider and tool tests must distinguish a known failure from an ambiguous outcome; they must not assume that losing a connection means the external operation failed. A non-cancelled ambiguity must retain the active slot in `AwaitingRecoveryDecision` with exact operation references. Cancellation must preserve the physical ambiguity while closing any recovery wait and terminalizing the turn as reconciliation required.

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

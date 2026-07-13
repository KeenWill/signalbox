# Invariant catalog

This catalog states constraints the future design must make enforceable. “Accepted” records current direction. “Provisional” means the constraint or its exact boundary needs an ADR. Each invariant has a primary classification; actual enforcement may use several layers.

## Domain and identity

| ID | Invariant | Class | Status | Expected enforcement |
| --- | --- | --- | --- | --- |
| INV-001 | Identifiers for sessions, turns, turn attempts, model calls, tool requests, tool attempts, runners, and messages are distinct and not interchangeable. | Domain/type-level | Accepted | Separate opaque domain types; boundary conversion validates kind and scope. |
| INV-002 | Domain types are separate from transport messages, storage records, and framework types. | Domain/type-level | Accepted | Explicit mapping modules and dependency checks; no generated or ORM type in domain transitions. |
| INV-003 | Session creation cause and transcript ancestry are independent facts. | Domain/type-level | Accepted | Separate fields/types; no single mutually exclusive enum combining both concepts. |
| INV-004 | Logical work and every physical attempt have separate identities and lifecycle records. | Domain/type-level | Accepted | State-machine types prevent a physical retry from overwriting logical identity. |
| INV-005 | Semantic conversation content, operational coordination, audit/recovery evidence, transient streaming, and client presentation are distinct representations even when correlated. | Domain/type-level | Accepted | Purpose-specific domain projections; no universal event type serving all roles. |
| INV-006 | Each transition is valid only from explicitly permitted prior states; terminal physical outcomes do not return to running. | Domain/type-level | Accepted in principle; state sets provisional | Total transition functions over typed states plus persistence guards. |

## Durability and concurrency

| ID | Invariant | Class | Status | Expected enforcement |
| --- | --- | --- | --- | --- |
| INV-007 | Once the hub acknowledges a user message, the message, session identity, delivery policy, and enough information to recover its pending treatment are durable. | Transaction-level | Accepted | Persist in one committed transaction before acknowledgement. |
| INV-008 | Accepted logical work has a durable effective-configuration reference sufficient to explain later model and policy choices. | Transaction-level | Accepted; configuration shape provisional | Create logical work and configuration snapshot/reference atomically. |
| INV-009 | At most one logical turn actively progresses in a session. | Database-level | Accepted for the initial architecture; exact “active” states provisional | Database uniqueness/exclusion or serialized transition, not process memory alone. |
| INV-010 | Queued work, confirmation waits, and delegated-result waits remain durably represented across hub restart. | Database-level | Accepted | Nonterminal states and referenced inputs persist without dependence on a live connection. |
| INV-011 | A stale physical attempt or dispatch generation cannot advance or overwrite current logical state. | Transaction-level | Accepted | Compare-and-set against current attempt and generation in the same result-acceptance transaction. |
| INV-012 | Duplicate command or result delivery cannot create duplicate logical work or apply one physical outcome twice. | Database-level | Accepted in principle; deduplication scope provisional | Durable request identity or equivalent deduplication plus transactional application; exact mechanism open. |
| INV-013 | Archiving changes discoverability/lifecycle state but does not erase durable history; restoration preserves identity. Exact archive and restore transitions remain open. | Database-level | Provisional pending the archive lifecycle decision | Preserve identity and history while ADR-0028 defines permitted transitions; destructive retention remains a separate future policy. |

## Model execution

| ID | Invariant | Class | Status | Expected enforcement |
| --- | --- | --- | --- | --- |
| INV-014 | Every initiated provider interaction records the requested model selection and exact hub-resolved provider/model target; provider-reported or otherwise observable identity and mismatch are appended when available. This provenance does not claim knowledge of an undisclosed physical backend. | Transaction-level | Accepted | Persist call, request, and hub resolution before sending, then append observable provider identity and mismatch evidence when available. |
| INV-015 | Every model call records the exact context frontier it consumed. | Transaction-level | Accepted | Freeze and persist the frontier before issuing the provider request. |
| INV-016 | Safe-point steering never mutates an already-running provider request. | Runtime validation | Accepted | Reject changes to an already-issued request; ADR-0027 defines which future orchestration boundaries may consume or reclassify pending steering. |
| INV-017 | Any hub-controlled fallback is explicit, policy-authorized, and visible in call provenance. | Operational policy | Accepted; whether fallback exists in version one is open | No adapter-local silent fallback; persist reason and source/target resolution. |
| INV-018 | A safety refusal does not by itself authorize automatic fallback or model substitution. | Operational policy | Accepted negative constraint; detailed refusal policy provisional | Classify refusal separately and require an explicit policy transition. |

## Tools, runners, and effects

| ID | Invariant | Class | Status | Expected enforcement |
| --- | --- | --- | --- | --- |
| INV-019 | Approval binds to one exact logical tool request, normalized arguments, and material execution constraints. | Transaction-level | Accepted; material-constraint set provisional | Approval references a content-bound request version; any material change invalidates it. |
| INV-020 | A model or automated judge recommendation never masquerades as human approval. | Domain/type-level | Accepted | Distinct actor and decision types; policy may consume recommendations only through an explicit rule. |
| INV-021 | Every runner result identifies its tool attempt and authorized dispatch generation or equivalent fence. | Protocol compatibility | Accepted; token representation provisional | Required protocol fields and hub-side validation. |
| INV-022 | Declared and configured runner properties must be truthful, but are not automatically proof. Effective properties for a dispatch, and the guarantees presented to a client, never exceed what the available declaration, trusted configuration, and verified evidence support at their distinct evidentiary strengths. | Operational policy | Accepted; enrollment, verification, and attestation mechanisms open | Hub policy derives and durably snapshots effective properties with their evidence basis when work is assigned. |
| INV-023 | Selecting an ambient-user runner is explicit and inspectable, and the system does not label it sandboxed. Any stronger isolation label requires effective properties supported by correspondingly stronger evidence. | Runtime validation | Accepted | Evidence-aware capability matching, UI disclosure, and durable effective-boundary snapshot. |
| INV-024 | Tool policy and durable outcome ownership remain in the hub for both hub-local and runner-local placement. | Domain/type-level | Accepted | Shared lifecycle transitions; executors return evidence rather than editing conversation state. |
| INV-025 | An external side effect may end with an ambiguous outcome when acknowledgement is lost. | Domain/type-level | Accepted | Outcome enum preserves ambiguity rather than coercing it to failure. |
| INV-026 | An ambiguous external write is not automatically retried. | Operational policy | Accepted | Recovery routes to reconciliation or explicit owner action; retry transition rejected by default. |
| INV-027 | A denied tool request cannot create an authorized physical attempt. | Transaction-level | Accepted | Attempt creation checks current policy/approval state atomically. |

## Input, ancestry, and clients

| ID | Invariant | Class | Status | Expected enforcement |
| --- | --- | --- | --- | --- |
| INV-028 | Input submitted during active work declares interrupt, next-safe-point, or after-current-turn treatment; the chosen treatment is durable. | Protocol compatibility | Accepted; default UX provisional | Required command field or negotiated default made explicit before persistence. |
| INV-029 | Interrupt stops future progress as promptly as safely possible but never claims to undo an already-issued external effect. | Operational policy | Accepted | Cancellation signals plus outcome reconciliation; user-facing state avoids rollback language. |
| INV-030 | A fork identifies an immutable source session and transcript frontier; later source changes do not rewrite the fork. | Database-level | Accepted for single-source ancestry | Persist source and frontier identity with the new session. |
| INV-031 | A delegated child is independently browsable and archivable, with an explicit relationship to the parent work. | Domain/type-level | Accepted; result and cancellation semantics provisional | Separate session identity plus typed relationship, never embedded as transient substate only. |
| INV-032 | A reconnecting client can reconstruct authoritative durable state and replace any transient draft without treating lost deltas as final content. | Protocol compatibility | Accepted | Snapshot/version protocol plus clearly marked transient updates. |

## Compatibility and operations

| ID | Invariant | Class | Status | Expected enforcement |
| --- | --- | --- | --- | --- |
| INV-033 | Unknown or incompatible protocol versions and required fields fail explicitly rather than being silently reinterpreted. | Protocol compatibility | Provisional pending protocol ADR | Negotiation rules and compatibility fixtures. |
| INV-034 | Recovery classifies interrupted physical work as completed, known failed/lost, ambiguous, or awaiting reconciliation; it does not silently discard it. | Runtime validation | Accepted; exact taxonomy provisional | Startup reconciliation scans and explicit durable transitions. |
| INV-035 | Provider credentials remain under hub control in the initial architecture and are not sent to clients or general-purpose runners. | Operational policy | Accepted initial direction | Secret access boundary and adapter interfaces; sanitized logs and protocol schemas. |

## Review rule

A feature proposal must cite the invariants it relies on and add a scenario when it introduces a new lifecycle edge. If an implementation cannot enforce an accepted invariant, the change must revise the design through an ADR rather than weakening the invariant implicitly.

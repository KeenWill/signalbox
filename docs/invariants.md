# Invariant catalog

This catalog states constraints the future design must make enforceable. “Accepted” records current direction. “Provisional” means the constraint or its exact boundary needs an ADR. Each invariant has a primary classification; actual enforcement may use several layers.

## Domain and identity

| ID | Invariant | Class | Status | Expected enforcement |
| --- | --- | --- | --- | --- |
| INV-001 | Identifiers for sessions, accepted inputs, turns, turn attempts, model calls, tool requests, tool attempts, runners, and semantic transcript entries are distinct and not interchangeable. | Domain/type-level | Accepted distinction; names and accepted-input boundary proposed in ADR-0001 | Separate opaque domain types; boundary conversion validates kind, ownership, and scope. |
| INV-002 | Domain types are separate from transport messages, storage records, and framework types. | Domain/type-level | Accepted | Explicit mapping modules and dependency checks; no generated or ORM type in domain transitions. |
| INV-003 | Session creation cause and transcript ancestry are independent facts. | Domain/type-level | Accepted | Separate fields/types; no single mutually exclusive enum combining both concepts. |
| INV-004 | Logical work and every physical attempt have separate identities and lifecycle records; every turn has one typed durable origin, while accepted steering remains separately identified. Identity is selected by typed transitions, never by semantic comparison of free-form intent. | Domain/type-level | Logical/physical split accepted; origin precision proposed in ADR-0001 | State-machine types prevent a physical retry from overwriting logical identity or steering from masquerading as a new turn. |
| INV-005 | Semantic conversation content, operational coordination, audit/recovery evidence, transient streaming, and client presentation are distinct representations even when correlated. | Domain/type-level | Accepted | Purpose-specific domain projections; no universal event type serving all roles. |
| INV-006 | Each transition is valid only from explicitly permitted prior states; terminal turns and physical outcomes do not return to running. Cancellation plus unresolved ambiguity cannot enter a recovery wait. | Domain/type-level | Accepted in principle; exact turn/attempt state sets proposed in ADR-0004 | Total transition functions over typed states plus persistence guards. |

## Durability and concurrency

| ID | Invariant | Class | Status | Expected enforcement |
| --- | --- | --- | --- | --- |
| INV-007 | Once the hub acknowledges accepted input, its identity, content, session, explicit delivery request, ordering, and enough disposition/configuration information to recover its promised treatment are durable. | Transaction-level | Accepted durability; exact no-active-turn and active-work lifecycle proposed in ADR-0027 | Persist the accepted input and initial disposition in one committed transaction before acknowledgement. |
| INV-008 | Accepted logical work has a durable immutable effective-configuration reference sufficient to explain later model and policy choices. Origin turns create it atomically with accepted input; safe-point input captures fallback configuration. | Transaction-level | Accepted provenance; freeze boundary proposed in ADR-0027 | Create logical work and configuration snapshot/reference atomically; never reread mutable defaults as accepted intent. |
| INV-009 | At most one logical turn actively progresses in a session; under the proposed lifecycle, every `Active` phase, including durable waits and cancellation, retains the slot. | Database-level | Single-turn rule accepted; exact active-state membership proposed in ADR-0004 | Database uniqueness/exclusion or serialized transition, not process memory alone. |
| INV-010 | Queued work, confirmation waits, and delegated-result waits remain durably represented across hub restart. | Database-level | Accepted | Nonterminal states and referenced inputs persist without dependence on a live connection. |
| INV-011 | A stale physical attempt or dispatch generation cannot advance or overwrite current logical state. | Transaction-level | Accepted | Compare-and-set against current attempt and generation in the same result-acceptance transaction. |
| INV-012 | Duplicate command or result delivery cannot create duplicate logical work or apply one physical outcome twice. | Database-level | Accepted in principle; deduplication scope provisional | Durable request identity or equivalent deduplication plus transactional application; exact mechanism open. |
| INV-013 | Archiving changes discoverability/lifecycle state but does not erase durable history; restoration preserves identity. Exact archive and restore transitions remain open. | Database-level | Provisional pending the archive lifecycle decision | Preserve identity and history while ADR-0028 defines permitted transitions; destructive retention remains a separate future policy. |

## Model execution

| ID | Invariant | Class | Status | Expected enforcement |
| --- | --- | --- | --- | --- |
| INV-014 | Every hub authorization to attempt a provider interaction has a model-call identity and records the requested model selection and exact hub-resolved provider/model target, even if it fails before send; provider-reported or otherwise observable identity and mismatch are appended when available. This provenance does not claim knowledge of an undisclosed physical backend. | Transaction-level | Accepted | Persist call, request, and hub resolution before sending, then append observable provider identity and mismatch evidence when available. |
| INV-015 | Every model call records the exact ordered semantic context frontier it consumed; a turn is not assumed to have one immutable frontier. | Transaction-level | Accepted | Freeze and persist each call frontier before issuing the provider request. |
| INV-016 | Safe-point steering never mutates an already-issued provider request, tool request, approval, or tool attempt, and accepted steering never disappears without a durable disposition. Once consumed, steering is committed turn history and remains in later retry or continuation frontiers. | Runtime validation | No-mutation/no-loss direction accepted; provider-call-only safe point and reclassification proposed in ADR-0027 | Consume steering atomically at an allowed future boundary, preserve it in later frontiers, or reclassify it visibly when the target turn terminates before consumption. |
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
| INV-028 | Input declares an explicit treatment valid for authoritative session state: start when no turn is active or, during active work, interrupt, next safe point, or after current turn; the chosen treatment is durable. | Protocol compatibility | Three active-work intents accepted; no-active command and validation proposed in ADR-0027; default UX provisional | Make the resulting command field explicit before persistence and reject stale state races before acknowledgement. |
| INV-029 | Interrupt stops future progress as promptly as safely possible, retains the predecessor's progressing slot through terminal classification, and never claims to undo an already-issued external effect. An ambiguous issued effect terminalizes the interrupted turn as reconciliation required. | Operational policy | No-rollback direction accepted; slot and successor behavior proposed in ADR-0004 and ADR-0027 | Cancellation signals plus honest outcome classification; replacement work waits for predecessor terminal disposition. |
| INV-030 | A fork identifies an immutable source session and transcript frontier; later source changes do not rewrite the fork. | Database-level | Accepted for single-source ancestry | Persist source and frontier identity with the new session. |
| INV-031 | A delegated child has a distinct session identity, is independently browsable, and has an explicit relationship to the parent work. This invariant does not determine whether related sessions archive independently, cascade, or block archival. | Domain/type-level | Accepted; archival coupling pending ADR-0028, with result and cancellation semantics also provisional | Separate session identity plus typed relationship, never embedded as transient substate only; ADR-0028 must define archival behavior. |
| INV-032 | A reconnecting client can reconstruct authoritative durable state and replace any transient draft without treating lost deltas as final content. | Protocol compatibility | Accepted | Snapshot/version protocol plus clearly marked transient updates. |

## Compatibility and operations

| ID | Invariant | Class | Status | Expected enforcement |
| --- | --- | --- | --- | --- |
| INV-033 | Unknown or incompatible protocol versions and required fields fail explicitly rather than being silently reinterpreted. | Protocol compatibility | Provisional pending protocol ADR | Negotiation rules and compatibility fixtures. |
| INV-034 | Recovery classifies interrupted physical work as completed, known failed/lost, cancelled, ambiguous, or requiring reconciliation; it does not silently discard it or reopen terminal work. | Runtime validation | Accepted classification direction; turn/attempt and model-call state sets proposed in ADR-0004 and ADR-0005 | Startup reconciliation scans, explicit durable transitions, and fenced replacement attempts/calls. |
| INV-035 | Provider credentials remain under hub control in the initial architecture and are not sent to clients or general-purpose runners. | Operational policy | Accepted initial direction | Secret access boundary and adapter interfaces; sanitized logs and protocol schemas. |

## Review rule

A feature proposal must cite the invariants it relies on and add a scenario when it introduces a new lifecycle edge. If an implementation cannot enforce an accepted invariant, the change must revise the design through an ADR rather than weakening the invariant implicitly.

# Architecture

This document records current high-level boundaries, not an implemented system or final API. Candidate names are defined in the [glossary](glossary.md); unresolved choices remain in the [decision ledger](decision-ledger.md). The first domain/lifecycle choices are under review in the [proposed ADR foundation set](decisions/README.md) and are called out below rather than treated as accepted.

## Component view

```text
                                       provider APIs
                                            ^
                                            |
 +----------+    authoritative API    +-----+-----------------------------+
 | terminal |------------------------>|                                   |
 | web      |                         |            CENTRAL HUB            |
 | macOS    |<-- snapshots/events ----|                                   |
 | iOS      |                         | session + turn orchestration      |
 +----------+                         | model resolution + provider calls |
                                      | policy + approvals                |
                                      | scheduler                         |
                                      | hub-local tool executors          |
                                      +----------+------------------------+
                                                 |
                                      transactions|        outbound runner
                                                 v        connections
                                           +----------+       |
                                           | Postgres |       v
                                           +----------+   +----------------+
                                                          | runner         |
                                                          | capabilities   |
                                                          | local tools    |
                                                          | exec identity  |
                                                          +----------------+
```

Clients never need a direct provider or runner connection. Provider calls originate in the hub; runner-local tool execution is dispatched over a runner's outbound connection. Both placements participate in one hub-coordinated logical tool lifecycle.

## Responsibilities

| Component | Owns | Does not own |
| --- | --- | --- |
| Clients | User interaction, presentation, explicit input-delivery intent, replaceable transient views | Canonical transcripts, scheduling, provider credentials, or approval truth |
| Central hub | Durable session semantics, accepted-input dispositions, logical turns, effective configuration, model resolution, provider calls, tool policy, approvals, scheduling, reconstruction, recovery decisions, per-dispatch effective runner properties, and enforcement of the single-owner access boundary | Machine-local capabilities it cannot truthfully provide or an authentication, enrollment, or attestation mechanism selected by this document |
| Postgres | Canonical durable records and transactional constraints | Domain policy, live provider streams, or runner execution |
| Provider adapters | Translation between hub model-call intent and provider APIs; observed provider response metadata | Session lifecycle, fallback policy, or historical alias meaning |
| Central scheduler | Choosing eligible runners, durable dispatch coordination, fencing, and at-most-one progressing turn policy | Tool implementation or an assumed distributed broker |
| Outbound runners | Declaring capabilities and execution-boundary properties, and executing under one deployment identity | Proof of their own claims, conversation truth, provider credentials, or final policy decisions |
| Hub-local tool executors | Centrally placed integrations and centrally credentialed actions | Workstation-local state or automatic privilege on runners |
| Runner-local tool executors | Shell, filesystem, Git, applications, local MCP, hardware, or workspace-specific effects | Approval authority or durable conversation updates |

The hub may initially be one deployable modular monolith. Rows in this table are ownership modules, not a requirement for network services.

## Sources of truth

| Subject | Authoritative source | Replicas or transient projections |
| --- | --- | --- |
| Session content and ancestry | Postgres records governed by hub domain rules | Client caches and model-context projections |
| Accepted user input, explicit delivery request, durable queue order, eligibility-fixed starting lineage, and disposition | Postgres | Client optimistic state |
| Logical turn and attempt state | Postgres | Scheduler memory and client progress views |
| Session configuration defaults | Current immutable version selected by explicit session-level update | Client editing state; affects only subsequently accepted origin input |
| Effective turn configuration and provenance | Durable hub record frozen at the boundary proposed by ADR-0027, including the exact session-defaults version or inherited source turn | Client display and orchestration memory; never current mutable defaults used as historical intent |
| Model alias definition now | Hub configuration; an immutable definition version or value snapshot is copied into accepted effective configuration when an alias is selected | Client selector lists |
| Requested, resolved, and provider-reported model provenance | Per-call durable record containing the requested selection, exact hub-resolved provider/model target, and observable provider identity or mismatch when available | Transcript/audit presentation; no claim about a hidden physical backend the provider does not reveal |
| Tool request, policy decision, and approval | Postgres | Confirmation UI and executor envelope |
| Dispatch generation and current attempt | Postgres | Live runner connection state |
| Declared runner properties | Runner advertisement; records what the runner declared, not the truth of the property | Current connection registry and client display |
| Configured runner properties | Trusted deployment configuration; records what the deployment says should be true, not proof of the physical boundary | Registration inputs and operator-facing deployment views |
| Verified runner properties | Evidence established through an accepted enrollment, attestation, policy, or other verification mechanism; the mechanism remains open | Verification cache and client explanation |
| Effective runner properties for a dispatch | Hub policy applied to available declarations, trusted configuration, and verified evidence; durable snapshot with the attempt | Scheduler memory and client display |
| Final tool result and outcome classification | Hub-accepted durable record | Runner delivery buffer and client presentation |
| Streaming drafts and progress | Live hub process unless selectively checkpointed | Client transient view; never authoritative final content |
| Provider credentials | Hub-controlled secret storage; exact mechanism open | Never client or runner session state |

Postgres is the canonical durable store in development, testing, and production. This does not mean every transient token delta is stored or that database records themselves are domain types.

A declaration is an operationally required claim, not proof of the claimed capability or isolation. Configured properties describe trusted deployment intent, while verified properties are limited to what an accepted mechanism can establish. The scheduler and clients may rely only on the effective properties derived for a particular dispatch, and those properties must never express a stronger execution guarantee than the available evidence supports. Runner enrollment, authentication, verification, and attestation remain unresolved.

## Core flows

### Accepted input and model execution

The proposed [ADR-0027](decisions/0027-input-delivery-lifecycle.md) exercises the accepted durability direction as follows:

1. Session creation establishes an immutable first version of user/session-level configuration defaults. Explicit defaults updates create later versions and affect only origin input accepted afterward; they never rewrite queued or active work.
2. A client submits content with `start when no turn is active` or, against an expected active turn, interrupt, next safe point, or after current turn. Origin-producing modes carry caller choices and an expected exact defaults version; next-safe-point steering cannot request an independent configuration change. A no-active submission joins any existing queued FIFO tail rather than bypassing it.
3. Durable-command deduplication compares the caller-supplied payload before current-state validation or configuration derivation. For an unseen command, the hub validates authoritative session state and the complete versioned resolver read set, derives configuration, and atomically makes the accepted-input identity, content, explicit treatment, ordering, initial disposition, and configuration provenance durable before acknowledgement. A stale state or semantic-input race fails or is recomputed before commit rather than silently changing accepted treatment.
4. No-active-turn, interrupt, and after-current input create origin turns and freeze their complete effective configuration at acceptance. Its minimum typed algebra explicitly represents absent instructions; disabled tools or enabled tools with unconstrained/frozen placement; no known-failure retry; no fallback; resource policy; and immutable policy versions. Placement is nested under enabled tools so a disabled-tool configuration cannot carry identity-significant but behaviorless constraints. Operational connection, scheduling, credential, telemetry, and transport-timing facts remain outside. Safe-point input captures a reference to the source turn as inherited reclassification provenance rather than copying its immutable configuration or inventing a request. These typed delivery transitions determine turn identity; the hub does not compare natural-language intent to choose an identity.
5. Queue eligibility is derived from durable acceptance positions, typed priority relations, and slot ownership. An interrupt-created turn is the active turn's immediate successor; after it, reclassified steering and ordinary queued input retain original acceptance order. Queued turns do not freeze direct predecessor pointers that these insertions would have to rewrite. When an accepted-input-origin turn becomes eligible, it atomically fixes its exact immediate-predecessor starting lineage and outcome-aware context frontier from the resulting total order, then either activates with an initial physical attempt or fails with that complete frontier. Session ancestry is valid only for a first turn. Future non-input origins must define their own lineage and context rules.
6. During the initial prepared turn attempt and before creating the first model call, the hub resolves the frozen model selection meaning and pins the turn's exact provider/model target; resolution failure creates no targetless call, ends the attempt as known failure, and fails the turn. Before every call, it fixes the exact ordered context frontier and attaches that pinned target. Consumed steering remains committed turn history even if later send preparation fails.
7. Provider deltas may stream transiently. The final assistant content, call outcome, and any provider-reported identity or observed mismatch become durable before being treated as authoritative. The first trusted mismatch observed while the outcome-eligible call is nonterminal immediately selects known failure, makes its response material non-authoritative, and requests best-effort cancellation without claiming provider work stopped. After terminal ambiguity, mismatch evidence preserves that physical disposition; it produces turn failure when no other unacknowledged ambiguity remains and reconciliation otherwise. After current-authority completion while the turn remains active, it preserves call/history, appends an invalidation bound to the exact call and first evidence record, prohibits new effects, and stops outstanding work before failure or reconciliation. Exact target and outcome authority are validated from the canonical call and transfer chain rather than copied into that evidence. Without fatal stop, a non-mismatched refusal atomically terminalizes call, attempt, and turn. A continuation that races refusal after another call triggered fatal stop is still physically `Refused`, but its content is non-authoritative and failure/reconciliation controls the turn. Startup derives all recovered outcomes and fatal causes before selecting that same precedence, including when mismatch and refusal are first discovered together. After terminal known failure/cancellation it preserves that call and existing turn-outcome precedence. After authority transfer it is non-authoritative evidence; after valid turn terminalization it cannot demote committed content or rewrite successor context. Allowed fallback would require a separate authorized call. If the owner authorizes a duplicate-risk replacement, one transaction closes the recovery wait, consumes every eligible pending steering input into the replacement frontier, creates the new attempt and prepared call, and transfers conversational-outcome authority to it.

A logical turn need not have one immutable context frontier. Under the proposal, a safe point occurs only before preparing a later provider call, after every earlier issued physical operation for the turn has a durable classified outcome and every earlier logical tool/approval dependency has a durable outcome. An authorized-but-undispatched tool request remains blocking despite having no physical attempt yet. Steering can extend that call's context but cannot mutate an issued provider call, tool request, approval, or tool attempt. Once consumed it remains committed context for later calls; if the target turn terminates before consumption, pending steering becomes visibly queued origin work rather than disappearing.

### Logical tools with two placements

1. The hub durably creates a logical tool request with exact normalized arguments.
2. Hub policy decides whether it may proceed, must wait for human approval, or is denied.
3. An approval, if required, binds to that exact request, its normalized arguments, and its material execution constraints. A material change, including a change in placement constraints, invalidates the approval and requires policy reevaluation before dispatch.
4. The hub creates a physical tool attempt and dispatches it either to a hub-local executor or to a selected runner with a fenced dispatch generation.
5. The hub validates the returned attempt identity and generation, classifies its outcome, and durably advances conversation and operational state.

Placement changes where an effect occurs, not who owns policy or history. A lost result from an external write may require reconciliation rather than retry, regardless of placement.

### Delegation and ancestry

Session creation cause and transcript ancestry are independent immutable facts. Proposed [ADR-0003](decisions/0003-session-creation-and-transcript-ancestry.md) limits the first implementable cause to owner initiation and represents ancestry as none or one exact source frontier. Session configuration defaults are a separate versioned value: creation establishes the first version, while later updates affect only future input acceptance. ADR-0002 must add a delegated-cause variant with an exact parent-work identity before delegation creates related sessions; its parent-side wait, result, and cancellation transitions likewise remain reserved and are not variants in the first implementable turn state machine. Forking initializes an owner-created session from a selected transcript frontier without claiming that the new session was delegated. Future merging remains open.

## Dependency direction

The intended future dependency direction is inward toward domain concepts:

```text
clients / transports    Postgres mappings    provider and runner adapters
          \                   |                     /
           +---------- application orchestration --+
                                |
                         domain state machines
```

- Domain state and transition logic do not depend on Postgres rows, generated wire messages, web frameworks, provider SDK types, or runner implementations.
- Application orchestration requests effects through explicit ports and applies durable domain decisions around those effects.
- Adapters translate identities and validate at boundaries; they do not leak framework types inward.
- Shared protocols describe compatibility at a process boundary, not the canonical persistence schema or the domain model.
- Clients consume stable-enough protocol projections and must tolerate authoritative snapshots replacing transient drafts.

These are architectural dependency rules, not a commitment to a specific Rust module layout.

## Recovery posture

Acknowledged work must not vanish. On restart, a startup scan reconstructs accepted-input dispositions, queued work with frozen configuration provenance, confirmation waits, any future accepted delegation waits, and interrupted attempts from Postgres. It ends each nonterminal turn attempt owned by the prior process with disposition `Lost`; `Lost` describes the abandoned orchestration tenure, not a model-call or generic physical-operation outcome, while the matching terminal variant retains cancellation or fatal causes already durable or first established by the scan. The scan is not itself an attempt and cannot issue semantic effects. In one serialized transition it derives the complete recovered evidence set before classifying each interrupted physical operation as completed, known failed, refused where applicable, cancelled, or ambiguous and selecting turn precedence. Live and startup classification share one precedence: unacknowledged ambiguity first (wait, or reconciliation with cancellation/fatal stop), then sufficient outcome-authoritative non-mismatched completion/refusal only without fatal stop, known failure or mismatch invalidation, and confirmed cancellation. A completion or refusal raced under fatal stop remains honest physical evidence but cannot control the turn. Mismatch never reopens a terminal call: after ambiguity it supports turn failure when no other ambiguity remains and reconciliation otherwise; after completion during an active turn it appends typed invalidation and stops future effects; after terminal known failure/cancellation or an atomically refused turn it preserves that state and precedence. `ReconciliationRequired` is a turn disposition, never a replacement classification for an ambiguous physical operation. Under proposed ADR-0004, an unacknowledged ambiguity retains the turn's active slot in `AwaitingRecoveryDecision` only when neither accepted cancellation nor fatal mismatch prohibits continuation; owner-authorized continuation preserves the physical ambiguity and records `DuplicateRiskAccepted`, which later turn outcomes and successor context retain. Startup creates no cancellation-only or classification-only replacement attempt.

At most one logical turn actively progresses per session initially. Proposed [ADR-0004](decisions/0004-turn-and-attempt-lifecycle.md) defines every implemented active phase, including approval and recovery waits, as retaining the session slot. The turn aggregate owns exactly one current attempt whose `Prepared`, `Running`, or `StopRequested` variant is its single nonterminal state; a stop request is typed as cancellation-only or fatal mismatch with a nonempty failure set and optional accepted cancellation, survives restart, and prohibits new semantic effects. Fatal-stop terminal history cannot represent completion, refusal, or cancellation. Persistence may still use separate records. A prepared attempt ends with the turn on cancellation. A durable wait carries its exact subject and no attempt. Every operation already issued by an attempt must be terminally classified before that attempt yields to any wait. Cancellation from a wait closes that wait and terminalizes atomically. No turn terminalizes or releases its slot until every owned logical tool/approval dependency is closed or terminally non-dispatchable, every issued physical operation is classified, its attempt is ended, and any wait is closed. Future child waits require ADR-0002 and will retain the slot unless that ADR defines explicit branching semantics. Scheduler mechanics remain open.

## Explicitly open boundaries

- Process protocol: Protobuf/gRPC or another approach; browser transport, negotiation, Swift generation, and compatibility policy.
- Scheduler mechanics: precise capabilities, affinity, pinning, fencing, multi-runner participation, and whether Postgres coordination alone is sufficient.
- Workflow infrastructure: an extension boundary is preserved, but no broker or workflow engine is selected.
- Provider evolution: provider calls begin in the hub; a later dedicated service requires an ADR and must preserve provenance and ownership.
- Model fallback: availability fallback is a scenario to design, not accepted automatic behavior.
- Tool safety: risk taxonomy, confirmation thresholds, judge role, sandbox minimums, and retry rules.
- Identity and access: owner/client authentication, runner enrollment and authentication, credential lifecycle, and session revocation.
- Resource governance: initial limits for turns, provider use, tool execution, runner concurrency, and retained artifacts.
- Storage representation: table/event design, streaming checkpoint policy, and stable archival form.
- Client implementation order and web technology.
- Deployment decomposition: modular monolith is acceptable; microservices are not presumed.

The initial direction intentionally does not select Kafka, NATS, Temporal, Restate, SQLite, a Rust HTTP/RPC framework, or a web framework.

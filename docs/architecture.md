# Architecture

This document records current high-level boundaries, not an implemented system or final API. Accepted names are defined in the [glossary](glossary.md); unresolved choices remain in [open questions](open-questions.md). The first domain/lifecycle choices are authoritative in the [accepted ADR foundation set](decisions/README.md).

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
| Session model-selection defaults | Current immutable version selected by explicit session-level update | Client editing state; affects only subsequently accepted origin input |
| Effective turn configuration and provenance | Durable hub record frozen at the boundary accepted by ADR-0027, including the exact session-defaults version or inherited source turn | Client display and orchestration memory; never current mutable defaults used as historical intent |
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

[ADR-0027](decisions/0027-input-delivery-lifecycle.md) is the normative definition of this flow, with model-call semantics in [ADR-0005](decisions/0005-model-call-retry-semantics.md). In outline:

1. Session creation establishes version one of the session's model-selection defaults; explicit updates create later immutable versions that affect only origin input accepted afterward.
2. A client submits content with an explicit delivery treatment: start when no turn is active or, against an expected active turn, interrupt, next safe point, or after current turn.
3. Owner-global durable-command deduplication precedes current-state validation. The first committed handling records the canonical typed payload and its applied-or-rejected result; acknowledgement follows the transaction that makes the accepted input, its treatment, ordering, initial disposition, and configuration provenance durable.
4. Origin-producing input freezes its complete baseline effective configuration at acceptance; safe-point steering instead records inherited source-turn provenance. Typed delivery transitions determine turn identity; the hub never compares natural-language intent.
5. Queue eligibility is derived from durable acceptance order, typed priority relations, and slot ownership. At eligibility a turn atomically fixes its immediate-predecessor starting lineage and outcome-aware context frontier, then activates with an initial attempt or fails with that frontier.
6. Before each provider call the hub resolves and pins the exact provider/model target and fixes the exact ordered context frontier; resolution failure creates no targetless call. Prepared calls consume eligible pending steering atomically, and consumed steering remains committed turn history.
7. Provider deltas may stream transiently. Final assistant content, call outcome, and any provider-reported identity or mismatch evidence become durable before being treated as authoritative, classified under the fixed precedence defined by ADR-0005.

A logical turn need not have one immutable context frontier. A safe point occurs only before preparing a later provider call, once every earlier issued physical operation is classified and every earlier logical tool/approval dependency has a durable outcome. Steering consumed there remains committed context for later calls; pending steering whose target turn terminates first becomes visibly queued origin work rather than disappearing.

### Logical tools with two placements

1. The hub durably creates a logical tool request with exact normalized arguments.
2. Hub policy decides whether it may proceed, must wait for human approval, or is denied.
3. An approval, if required, binds to that exact request, its normalized arguments, and its material execution constraints. A material change, including a change in placement constraints, invalidates the approval and requires policy reevaluation before dispatch.
4. The hub creates a physical tool attempt and dispatches it either to a hub-local executor or to a selected runner with a fenced dispatch generation.
5. The hub validates the returned attempt identity and generation, classifies its outcome, and durably advances conversation and operational state.

Placement changes where an effect occurs, not who owns policy or history. A lost result from an external write may require reconciliation rather than retry, regardless of placement.

### Delegation and ancestry

Session creation cause and transcript ancestry are independent immutable facts. Accepted [ADR-0003](decisions/0003-session-creation-and-transcript-ancestry.md) limits the first implementable cause to owner initiation and represents ancestry as none or one exact source frontier. Session configuration defaults are a separate versioned value: creation establishes the first version, while later updates affect only future input acceptance. ADR-0002 must add a delegated-cause variant with an exact parent-work identity before delegation creates related sessions; its parent-side wait, result, and cancellation transitions likewise remain reserved and are not variants in the first implementable turn state machine. Forking initializes an owner-created session from a selected transcript frontier without claiming that the new session was delegated. Future merging remains open.

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

Acknowledged work must not vanish. On restart, an idempotent startup scan reconstructs durable state from Postgres, ends each nonterminal turn attempt owned by the prior process with disposition `Lost`, and classifies every interrupted physical operation; the scan is not itself an attempt and issues no semantic effects. Live and startup classification share the single outcome precedence that [ADR-0004](decisions/0004-turn-and-attempt-lifecycle.md) and [ADR-0005](decisions/0005-model-call-retry-semantics.md) define normatively. Ambiguous external effects are preserved as ambiguous rather than coerced to failure, and continuing past one requires an explicit owner decision that records accepted duplicate risk.

At most one logical turn actively progresses per session initially; every implemented active phase, including approval and recovery waits, retains the session slot, and the turn aggregate owns exactly one current attempt. The complete state, stop-cause, and terminal-guard algebra is normative in ADR-0004. Future child waits require the delegation decision (reserved ADR-0002) and will retain the slot unless it defines explicit branching semantics. Scheduler mechanics remain open.

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

# Architecture

This document records current high-level boundaries, not an implemented system or final API. Candidate names are defined in the [glossary](glossary.md); unresolved choices remain in the [decision ledger](decision-ledger.md).

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
| Central hub | Durable session semantics, logical turns, effective configuration, model resolution, provider calls, tool policy, approvals, scheduling, reconstruction, recovery decisions, per-dispatch effective runner properties, and enforcement of the single-owner access boundary | Machine-local capabilities it cannot truthfully provide or an authentication, enrollment, or attestation mechanism selected by this document |
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
| Accepted user input and delivery policy | Postgres | Client optimistic state |
| Logical turn and attempt state | Postgres | Scheduler memory and client progress views |
| Effective turn configuration | Durable hub record | Client display and orchestration memory |
| Model alias definition now | Hub configuration | Client selector lists |
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

1. A client submits content plus a delivery policy: interrupt, next safe point, or after current turn.
2. The hub validates the session and atomically makes the message and its intended treatment durable before acknowledging acceptance.
3. Domain transitions either create logical work, queue it, or make it eligible as steering at a future orchestration boundary.
4. Before each provider call, the hub fixes the exact context frontier consumed, resolves the requested model selection to an exact hub-resolved provider/model target, and records the physical call identity.
5. Provider deltas may stream transiently. The final assistant content, call outcome, and any provider-reported identity or observed mismatch become durable before being treated as authoritative. These facts supplement the already-recorded requested selection and exact hub-resolved target; they do not prove an undisclosed physical backend.

A logical turn need not have one immutable context frontier. Safe-point steering can extend context between provider calls, but it cannot mutate the input of a provider request already in flight. Each physical call therefore records its own context frontier.

### Logical tools with two placements

1. The hub durably creates a logical tool request with exact normalized arguments.
2. Hub policy decides whether it may proceed, must wait for human approval, or is denied.
3. An approval, if required, binds to that exact request and argument set.
4. The hub creates a physical tool attempt and dispatches it either to a hub-local executor or to a selected runner with a fenced dispatch generation.
5. The hub validates the returned attempt identity and generation, classifies its outcome, and durably advances conversation and operational state.

Placement changes where an effect occurs, not who owns policy or history. A lost result from an external write may require reconciliation rather than retry, regardless of placement.

### Delegation and ancestry

Session creation cause and transcript ancestry are independent facts. Delegation creates a related, independently browsable session and a parent-side wait or reference. Forking initializes a session from a selected transcript frontier without claiming that the new session was delegated. Initial ancestry is limited to one source frontier; future merging is open.

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

Acknowledged work must not vanish. On restart, the hub reconstructs queued work, confirmation waits, delegation waits, and interrupted attempts from Postgres. A new process does not pretend to resume an old provider stream or unknown runner effect. It marks the physical attempt with a known failure or ambiguous outcome, then applies the eventual retry or reconciliation policy without changing the identity of accepted input.

At most one logical turn actively progresses per session initially. “Active” needs an exact state definition before implementation; queued messages and durable waits do not disappear merely because nothing is executing.

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

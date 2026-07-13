# Vision

## Purpose

Signalbox is a personal, single-owner platform for LLM-assisted work that remains durable, inspectable, and resumable across clients and execution environments. The product should let its owner begin a conversation on one device, continue elsewhere, choose where tools execute, approve risky actions, delegate related work, and reconstruct the observable provenance and outcomes of model and tool interactions.

“Single-owner” describes the account and product scope. It does not grant every process the owner's full operating-system authority, collapse all execution onto one machine, or remove the need for authentication, provenance, audit, and resource controls.

## Target user and deployment

The target user is one technically capable owner operating an always-on hub in Kubernetes with Postgres as the canonical store. Terminal, web, macOS, and iOS clients connect remotely. Runners may live on workstations, personal servers, restricted accounts, containers, or remote sandboxes. Development and test environments use the same Postgres semantics, normally through ephemeral containerized instances.

Multi-user tenancy, organization administration, and collaborative authorization are not first-version goals.

## Why a central hub

The hub gives every client one authoritative view of sessions, accepted input, logical work, approvals, scheduling, model resolution, and final outcomes. It initially owns provider credentials and calls so credentials and model provenance do not fragment across clients or runners. Central ownership also makes crash recovery a product property: acknowledged work is represented durably even when a client disconnects.

The initial hub may be a modular monolith. This boundary does not require microservices and does not prevent provider execution from moving behind a dedicated service after an ADR.

## Why runners are separate

Tools need capabilities and locality that the hub should not assume: a checked-out workspace, a user's desktop applications, special hardware, or a deliberately restricted sandbox. A runner connects outbound, declares capabilities and its execution boundary, and performs work selected by the hub. Deployments are expected to configure and report those properties truthfully, but a declaration is not proof; scheduling and presentation may rely only on execution properties supported by the available deployment and verification evidence. The runner does not become the source of truth for conversation or policy.

Separation makes execution identity explicit. One deployment may intentionally run as the human user; another may run as an unprivileged account or inside a sandbox. The user must be able to see which boundary applies. The initial preference is one process per execution identity, without in-process multi-user privilege switching.

## Why remote execution is foundational

Treating remote runners as foundational prevents the session model, approvals, and scheduler from inheriting a false assumption that UI and execution share a machine. Durable dispatch identity, runner loss, stale results, and ambiguous side effects therefore belong in the first architectural model rather than a later scaling retrofit.

## Why explicit state and strong types matter

Signalbox coordinates durable intent with fallible physical effects. A user message, logical turn, orchestration attempt, model call, tool request, and tool attempt have different identities and retry rules. Conflating them makes stale writes, duplicate effects, misleading history, and silent loss likely.

The future domain should use explicit identities, state machines, and pure transitions where practical. Infrastructure interprets those decisions through Postgres, provider, runner, and transport effects. Domain, storage, wire, and framework representations remain separate so constraints are not accidentally defined by a database row or protocol generator.

## Why start small

Foundational changes have a large blast radius. The repository begins with reviewed vocabulary, scenarios, invariants, and decision scaffolding so later vertical slices can be small enough to reason about. Early pull requests should prove one coherent behavior without pre-building speculative package structures.

## First-version non-goals

- Multi-owner tenancy, teams, or role-based organization administration.
- A general arbitrary session graph, transcript merging, or collaborative editing.
- Exactly-once arbitrary shell commands or external writes.
- Seamless continuation of an interrupted provider network stream at the same token.
- A distributed broker or workflow engine selected in advance of demonstrated need.
- Independently deployed hub microservices.
- Multi-user privilege switching inside one runner process.
- Guaranteed strong isolation for every runner deployment.
- Automatic model fallback until its policy is decided.
- A settled web stack, transport framework, or stable public API during the foundation phase.

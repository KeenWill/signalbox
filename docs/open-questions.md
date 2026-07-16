# Open questions

This is the inventory of unresolved foundational questions. A "leaning" guides exploration but is not a decision. Closing a question requires an entry in the [decision log](decisions.md) or, at foundation weight, an [ADR](decisions/README.md). Accepted decisions live in the ADR foundation set and the decision log; scenario identifiers refer to [scenarios.md](scenarios.md).

Some questions carry an ADR number reserved by earlier planning and cited from accepted records; those numbers remain reserved for their topics.

## Identity representation

- **Identity mechanics left open by the ADR-0001 amendment.** UUID generation and version, caller supply versus hub minting, boundary validation, storage and wire encodings, public formatting, and serialization for the identity newtypes. Blocks persistence and protocol slices. (S01, S02, S04, S08, S10, S12)

## Delegation (reserved ADR-0002)

- **Parent cancellation propagation to active delegated children.** Leaning: explicit relationship policy with visible child outcomes. Blocks delegation. (S18, S19)
- **Detached delegated work in version one.** Leaning: exclude unless a core scenario proves need. Blocks delegation scope. (S18, S19)
- **Representation of child results in the parent conversation.** Leaning: structured durable reference plus explicit delivered content. Blocks delegation. (S18, S19)
- **Waits on delegated children and the progressing-turn slot.** ADR-0004 defers child waits to the delegation decision. Blocks delegation. (S18, S19)
- **Multi-source or merged transcript ancestry.** Accepted baseline is none or one immutable source frontier with an explicit extension boundary. Deferrable. (S17)

## Queue management

- **Editing, canceling, reordering, or changing delivery policy of queued input.** Excluded from the accepted ADR-0027 baseline; any addition needs explicit dispositions. Later scope. (S09)

## Archival and retention (reserved ADR-0028, ADR-0029)

- **Archive eligibility, nonterminal work handling, and restore target.** Leaning: preserve identity and history; never silently abandon work. Blocks archive/restore. (S25)
- **Archive effect on delegated children and related sessions.** Leaning: explicit typed policy with visible outcomes; no implicit cascade or independence rule selected. Blocks archiving related sessions. (S18, S19, S25)
- **Destructive retention or purge beyond ordinary archive.** Kept separate from ordinary archive; exact policy undefined. Later scope. (S17, S25)

## Regeneration

- **Regeneration command acceptance, queue placement, source frontier, and relation representation.** The identity rule is accepted by ADR-0004 (always new logical work; never reopen the original); the rest blocks the regeneration feature. (S26)

## Model fallback and provenance (reserved ADR-0006, ADR-0007)

- **Whether version one supports automatic fallback.** Leaning: none until an explicit policy is justified. Deferrable for the first provider slice. (S22, S23)
- **Which failure classes permit fallback, if it exists.** Leaning: narrow allowlist of classified availability failures; refusal alone never qualifies. Blocks fallback. (S22, S23)
- **Fallback configuration and visibility.** Requires explicit session/turn policy, per-call provenance, and clear UI; no constructible fallback configuration exists in the baseline. Blocks fallback. (S20, S22)
- **Model identifier normalization and detailed provenance representation.** The mismatch disposition itself is accepted by ADR-0005. Blocks the provider provenance schema. (S20–S23)

## Scheduling and runners (reserved ADR-0008, ADR-0009, ADR-0010)

- **Runner capability, evidence, and placement model.** Leaning: typed core properties with explicit evidence levels; effective guarantees never stronger than supporting evidence. Blocks the runner protocol. (S05–S16)
- **Runner pinning and workspace affinity.** Leaning: explicit session/turn pinning where locality matters, with observable failure. Blocks workspace tools. (S05–S16)
- **Multiple runners in one turn.** Leaning: at most one selected runner initially, counting hub-local tools separately. Constrains version one. (S13–S16)
- **Fencing that rejects stale dispatches and results.** Leaning: durable attempt identity plus generation or equivalent compare-and-set. Blocks runner dispatch. (S05, S06, S12)
- **Whether Postgres alone is sufficient for the initial scheduler.** Leaning: start with Postgres if correctness and wake-up tests pass; preserve the adapter boundary. Deferrable until a scheduler prototype. (S03, S05, S12)

## Tool safety (reserved ADR-0011, ADR-0012, ADR-0013, ADR-0014)

- **Tool-risk classification.** Needs an argument-aware effect taxonomy with a conservative unknown class before tool execution. Blocks tool execution. (S05, S06, S10, S11, S15, S16)
- **Which operations require confirmation.** Leaning: hub risk policy considering arguments, placement, and prior scoped grants. Blocks tool execution. (S10, S11, S13–S16)
- **LLM-judge influence on approval policy.** Leaning: advisory or bounded policy signal only, never human approval identity. Deferrable. (S10, S11)
- **Retry policy for side-effecting commands and tools.** Leaning: classify effect and evidence; never auto-retry ambiguous writes. Blocks tool retry. (S05, S06, S12)
- **Initial sandboxing requirements.** Leaning: explicit ambient and restricted profiles only to the strength justified by effective evidence. Blocks runner release. (S13, S14)
- **Ambient-user runner behavior.** Leaning: explicit selection and visible boundary, likely stricter policy for material effects. Blocks the ambient runner. (S13)

## Identity, credentials, and resource governance (reserved ADR-0015 through ADR-0018)

- **Owner client authentication and revocation.** Keep the hub's authorization model single-owner while choosing a remotely safe authentication boundary. Blocks any remote client. (S01, S10, S24, S25)
- **Runner enrollment, authentication, and revocation.** Strong runner identity distinct from capability claims, with rotation. Blocks remote runners. (S05, S06, S12–S16)
- **Provider and integration credential storage and rotation.** Secret values stay outside ordinary session records, exposed only to the responsible hub adapter. Blocks real credentials. (S02, S15, S20–S23)
- **First-release resource limits.** Leaning: explicit bounded concurrency and configurable usage limits at effect boundaries. Blocks public release. (S02–S06, S13–S18)

## Protocols and persistence (reserved ADR-0019 through ADR-0023)

- **Process protocol (Protobuf/gRPC, Connect, JSON/HTTP, other).** Leaning: define semantics and fixtures before selecting transport. Blocks cross-process implementation. (S01, S02, S12, S24)
- **Browser transport.** Preserve authoritative-snapshot-plus-transient-stream semantics; technology open. Blocks the web client. (S02, S24)
- **Protocol version and capability negotiation.** Leaning: version plus capability handshake with explicit incompatibility. Blocks remote clients and runners. (S12, S24)
- **Stable storage representation.** Leaning: Postgres-native schema with explicit migrations; event/log use only where justified. Blocks persistence implementation. (S03, S04, S25)
- **Swift client type generation.** Leaning: generated boundary types mapped to hand-written client domain types. Deferrable until the Swift client. (S01, S24)
- **Cross-release compatibility policy.** Leaning: small documented compatibility window with fixtures; exact window open. Blocks the first public release. (S12, S24)

## Client scope (reserved ADR-0024, ADR-0025, ADR-0026)

- **First client and interface form (CLI, TUI, web, or Swift).** Leaning: the smallest interface that exercises reconnect, approval, and provenance; a thin terminal client is plausible but not accepted. Deferrable until the hub slice is framed. (S01, S02, S10, S24)
- **Apple client code organization.** Defer until the protocol and the first native slice are known. (S01, S24)
- **Web client technology (Rust/Wasm or TypeScript).** No leaning until the browser protocol and product slice are measured. (S01, S02, S24)

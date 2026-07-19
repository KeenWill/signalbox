# ADR-0035: Domain-owned persistence reconstitution

- Date: 2026-07-17
- Supersedes: none
- Superseded by: none
- Refined by: [ADR-0038](0038-session-aggregate-boundary.md) for the complete long-lived session projection and [ADR-0041](0041-evidence-bearing-reconstitution.md) for evidence-bearing active-turn completeness
- Depends on: the accepted foundation set ([ADR-0001](0001-domain-terminology-and-identity.md), [ADR-0003](0003-session-creation-and-transcript-ancestry.md), [ADR-0004](0004-turn-and-attempt-lifecycle.md), [ADR-0005](0005-model-call-retry-semantics.md), and [ADR-0027](0027-input-delivery-lifecycle.md)) and the accepted persistence, frontier, and fatal-terminalization refinements ([ADR-0022](0022-persistence-representation.md), [ADR-0030](0030-context-frontier-snapshots.md), [ADR-0031](0031-direct-fatal-terminalization.md), and [ADR-0034](0034-durable-command-storage-and-equality.md))
- Refines: ADR-0022's storage-record/domain-type mapping and proof-rehydration boundary
- Resolves: the domain/persistence reconstitution-seam question left open by ADR-0022
- Decision questions: complete projection boundaries; reconstruction of opaque and proof-bearing values; validation ownership; corruption and infrastructure-failure behavior

## Context

ADR-0022 keeps Postgres records outside the domain and requires every load to map explicitly and fallibly. That separation is straightforward for independently checked values such as UUID-backed identities and positive ordinals. It is not enough for values whose authority comes from correlations across several durable facts.

An `AppliedInterruptProof` is valid only when the recorded command was an applied interrupt for the exact predecessor and the same transaction created its accepted input and immediate successor. An `AppliedStopForReconciliationProof` similarly depends on one applied exact-set decision. An `AcceptedInputTurnStart` is valid only when durable order, slot state, lineage, semantic entries, and the resolved frontier agree. Fatal stop causes, reconciliation markers, current attempts, calls, and resolved frontiers have their own completeness and ownership constraints. Public constructors from the identifiers stored in one row would let persistence manufacture authority that the accepted ADRs deliberately reserve for matching aggregate facts.

Restart nevertheless must recover those values without replaying effects or treating the in-process Rust layout as a storage format. The missing decision is a pure, domain-owned way to validate complete durable projections and return canonical domain aggregates while keeping SQL rows, SQLx types, and framework errors outside the domain.

## Decision

### Purpose-specific complete projections

Reconstitution uses **purpose-specific complete domain projections**, not raw proof constructors and not one universal persistence document. Each aggregate or immutable fact family that needs reconstruction owns a closed reconstitution input in `crates/domain`. That input contains domain identities, checked scalar values, and closed domain evidence variants, but no SQL row, nullable discriminator encoding, JSON value, SQLx type, or storage error.

The persistence adapter performs only the first boundary step: it reads purpose-specific records, checks column encodings and discriminator/payload shape, maps columns into the domain projection's checked inputs, and supplies the complete related fact set requested by that seam. The domain then performs a pure validation step and returns either one canonical aggregate projection or a typed reconstitution failure. Neither step performs lifecycle effects.

“Complete” is defined by the requested domain seam, not by whichever rows a query happened to return. For example:

- reconstituting an applied-interrupt-bearing aggregate includes the canonical recorded command kind and applied result, the exact predecessor, accepted input, successor, session, and queue-priority correlations required by ADR-0001 and ADR-0027;
- reconstituting a turn includes its immutable order and origin facts, lifecycle discriminator, start and resolved frontier when required, exact current attempt or wait subject, terminal payload when required, owned operation classifications, and every proof source referenced by those values;
- reconstituting a context snapshot includes its complete composite identity and complete ordered-distinct source-qualified membership, while reconstituting a start or call additionally validates the aggregate facts that authorize binding that snapshot;
- reconstituting fatal causes or a reconciliation marker includes the complete typed failure and ambiguity memberships plus each referenced applied-result correlation, rather than accepting a preassembled proof or caller-asserted “complete” flag.

Different read use cases may own different complete projections. A scheduling read need not load unrelated archived audit facts, but it may not omit any fact that can change eligibility, slot ownership, proof validity, or the state it returns. Optional fields represent only accepted optional domain facts; they never stand for an unloaded relation.

### Whole-value reconstruction

Opaque values are constructed only inside their owning reconstitution seam after validation. A seam returns the complete reconstructed aggregate, history value, or resolved snapshot; it does not return a free-standing proof factory or make `from_record_parts` constructors public on proof-bearing types.

Proof values remain nested in the canonical result that established their correlations. Code may observe them through the accessors already required by the domain, but persistence cannot obtain one by supplying only a command identifier, a result discriminator, a frontier identifier, or an arbitrary ordered list. Live transitions and reconstitution share internal correlation and shape validators so restart does not implement a weaker parallel algebra. Reconstitution does not call effect-authorizing live transitions, generate new identities, append history, or claim that a transaction committed.

A resolved context snapshot establishes immutable identity-to-content mapping. It does not by itself establish that the snapshot is the correct one for a particular start or call. That additional authority is reconstructed only by the start- or call-owning aggregate seam from the complete binding and lifecycle projection required by ADR-0030.

Rust visibility is not treated as evidence that bytes came from Postgres. The persistence adapter is trusted hub code, while transport and client inputs cannot invoke a reconstitution port. The domain seam validates semantic shape and correlation; the transaction boundary establishes durable provenance. If Signalbox later loads untrusted in-process extensions, that new trust boundary must prevent them from supplying reconstitution projections rather than relying on constructor naming.

### Load and corruption failures

Persisted data is never normalized into a nearby valid state. A load fails when any required record is missing; an identity or ordinal cannot be decoded; a closed discriminator is unknown; required and forbidden variant columns disagree; a supposedly exact set is empty, duplicated, or incomplete; ownership or continuation references cross aggregates; a frontier has a gap, duplicate reference, or conflicting contents; an applied result does not establish its claimed proof; or the complete projection violates any accepted aggregate shape.

The failure boundary distinguishes:

1. **Infrastructure failure** — the database operation or transaction could not complete. It claims no command identifier, commits no state, and may be retried according to ordinary database policy.
2. **Concurrent staleness** — a projection was valid when read but its guarded write lost a race. It is reloaded and rederived; it is neither corruption nor authority to overwrite the winner.
3. **Durable corruption or incompatible representation** — committed rows cannot construct the accepted domain value. The operation returns a typed internal integrity failure, authorizes no effect, performs no repair write, and does not silently drop the malformed fact.

Persisted input must not cause a panic. Diagnostics may add table, key, migration, and query context outside the domain error, but they must not contain credentials or pretend the invalid rows formed a domain value. During startup, scheduling cannot be enabled for an aggregate whose required projection failed. If the startup scan cannot establish which aggregates are complete, the scan itself has not succeeded. Whether deployment policy stops the whole hub or isolates a known affected session remains an operational decision; either response must remain fail-closed and visible.

Database constraints remain defense in depth. A row set passing SQL constraints can still fail domain correlation, and a successful domain reconstitution does not waive compare-and-set revalidation when a later transaction commits.

## Invariants

- INV-001: reconstitution creates purpose-specific proofs only from complete matching applied-result correlations; raw identities establish no authority.
- INV-002: record structs, SQLx values, and storage errors remain outside domain projections and results.
- INV-006: only an accepted complete aggregate shape is returned; missing children or malformed state/payload combinations do not become optional domain state.
- INV-009 and INV-015: a start or call receives a resolved frontier only through its owning aggregate's complete validation, never from a bare composite reference.
- INV-012: command replay is one consumer of reconstitution; ADR-0034's typed payload/result records must reconstruct structurally equal domain values rather than deserialize bytes into proof.
- INV-029: applied-interrupt authority is recovered from the exact committed applied result and successor/predecessor correlations.
- INV-034: startup recovery operates only on successfully reconstituted complete projections and never fabricates a replacement attempt.

## Strongest alternative

Expose public `from_parts` constructors for every stored domain value and rely on the persistence adapter to call them correctly. This is simple mapping code and keeps aggregate loads local to SQL queries.

It is rejected because the dangerous cases are precisely those in which each individual part looks valid while the cross-record relationship is not. A command identifier and predecessor can look like an interrupt proof; a frontier identifier and ordered list can look like a resolved snapshot; separately valid turn and attempt rows can form an impossible aggregate. Correctness would live in adapter convention, and restart would have a weaker authority path than live transitions.

## Rejected alternatives

- **Deserialize persisted bytes directly into domain structs.** This couples storage to private Rust layout, bypasses checked correlations, and contradicts ADR-0022.
- **Replay commands or lifecycle events to rebuild current state.** ADR-0022 selects guarded current-state rows rather than an event store, and replay could repeat effects or apply obsolete validation context.
- **Put database lookup inside domain transitions.** Hidden I/O would reverse the architecture dependency and make pure validation depend on query timing.
- **Return proofs separately after validating one relation.** Later code could cross-wire a locally valid proof with another aggregate; the seam returns the complete correlated owner value.
- **Repair malformed rows while loading.** Choosing a discriminator, dropping a duplicate, filling a missing fact, or reminting an identity would rewrite authoritative history without an accepted transition.
- **Treat database constraints as sufficient reconstruction.** Constraints cannot express every closed aggregate, exact-set, applied-result, and lifecycle correlation.

## Consequences

Persistence queries must load deliberate aggregate projections and mapping code must distinguish encoding, completeness, correlation, staleness, and infrastructure failures. Some validation is intentionally repeated at the database and domain boundaries.

In return, the domain has one algebra for live and restarted state. Opaque proofs remain opaque, record types stay outside the domain, malformed durable state cannot authorize effects, and restart does not need a special privileged constructor for every private field.

## Scenario walkthroughs

- **S03:** Restart maps the queued turn, accepted input, immutable order and priority facts, frozen configuration provenance, session slot, and any already-fixed start through the scheduling projection. Missing order or a start on a queued turn is corruption; a valid queued projection can rederive eligibility without creating proof or effect.
- **S04:** Startup loads the complete current attempt, calls, evidence, stop causes, and wait membership before applying the accepted recovery transition. A missing issued operation or cross-wired mismatch reference fails reconstruction rather than producing a smaller evidence set.
- **S07:** A stored `(command, predecessor)` pair becomes an applied interrupt proof only when the complete recorded applied result and immediate-successor correlations validate. A rejected or differently targeted command cannot cancel after restart.
- **S17:** A stored context snapshot reconstructs only from one immutable complete ordered source-qualified membership. Its use as a fork start additionally validates the ancestry and consuming-session eligibility projection.
- **S27:** Fatal causes and reconciliation membership reconstruct as exact independent sets. Missing ambiguity or failure membership cannot turn reconciliation into ordinary failure or release the progressing slot.

## Open questions

- ADR-0034 owns canonical durable-command payload/result storage. Reconstitution consumes its typed domain projection without acquiring a storage-shaped proof constructor.
- Exact Rust module names and whether closely related aggregate seams share internal builders are implementation choices; no generic public “rehydrate anything” API is required.
- Operational policy may choose whole-process failure or known-session isolation after reported corruption, provided startup and effect authorization remain fail-closed.

## Explicit non-decisions

This record does not choose SQL queries, repository traits, async APIs, transaction isolation, cache shape, schema names, serialization, migration tooling, corruption repair tooling, observability infrastructure, or a public diagnostic protocol. It does not make storage records domain types, grant clients a reconstitution boundary, or define new lifecycle transitions.

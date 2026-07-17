# Architecture decision records

ADRs record foundation-weight decisions: choices that alter accepted semantics, move a boundary between domain, storage, wire, or framework representations, weaken an invariant, or introduce a technology that constrains several components. Lighter decisions are made in pull requests and recorded in the [decision log](../decisions.md). Unresolved foundational questions live in [open-questions.md](../open-questions.md).

## Accepted foundation set

The repository owner accepted these records atomically on 2026-07-13 after independent adversarial architecture review. ADR-0001 was materially amended on 2026-07-15 to accept the initial private UUID-backed representation and deliberately named UUID conversions for its Rust domain identity newtypes; storage and wire representations remain separate boundaries. ADR-0022 later selects native Postgres columns for the UUID-backed relational identities and references it names, while wire representation remains undecided. The original records are authoritative together:

| ADR | Scope |
| --- | --- |
| [ADR-0001](0001-domain-terminology-and-identity.md) | Core terminology, owner-global applied-or-rejected command identity, purpose-specific applied-command proofs, durable semantic identity boundaries, and the initial private UUID-backed Rust identity representation |
| [ADR-0003](0003-session-creation-and-transcript-ancestry.md) | Independent session creation cause, owner-initiated baseline, initial transcript ancestry, and separation from versioned session defaults |
| [ADR-0004](0004-turn-and-attempt-lifecycle.md) | Turn/attempt lifecycle, aggregate attempt ownership, typed stop causes, applied-interrupt cancellation, proof-bearing reconciliation, startup recovery scan, terminal guards, ambiguity decisions, and regeneration identity boundary |
| [ADR-0005](0005-model-call-retry-semantics.md) | Target-before-call identity, typed reported-target mismatch failure/invalidation, no automatic known-failure retry, ambiguous-call recovery, continuation, refusal disposition, and configuration identity |
| [ADR-0027](0027-input-delivery-lifecycle.md) | Input delivery, versioned model-selection session defaults, constructible baseline effective configuration, explicit steering/configuration provenance, command deduplication, durable queue ordering, eligibility-fixed starting lineage, and context frontiers |

The five records form one coupled baseline: their identity algebras, lifecycle transitions, configuration boundary, and context rules reference one another. A change may correct or supersede an individual record only while preserving or explicitly revising its accepted dependencies. As implementation lands, executable tests become the enforcement of record; the [invariant catalog](../invariants.md) links each invariant to its enforcement.

## Accepted refinements and extensions

The repository owner accepted the following records on 2026-07-17. They depend on and refine the original foundation set; they do not retroactively become part of its five-record atomic acceptance.

| ADR | Scope |
| --- | --- |
| [ADR-0022](0022-persistence-representation.md) | Normalized Postgres persistence records, explicit migrations, storage/domain mapping, database enforcement of the progressing slot and owner-global command identity, and native Postgres encoding for the UUID-backed identity columns it names |
| [ADR-0030](0030-context-frontier-snapshots.md) | Session-owned immutable context-frontier snapshot identity, exact ordered source-qualified resolution, identity versus content equality, trusted construction, and ancestry resolution |
| [ADR-0031](0031-direct-fatal-terminalization.md) | Direct fatal-mismatch failure or exact reconciliation at a closed aggregate boundary, with `StopRequested` retained only while aggregate work remains unfinished |

## Process

- A new ADR starts as `Proposed`; the repository owner's approval makes it `Accepted` and authoritative from its decision date. Acceptance may happen in the pull request that introduces it.
- An accepted ADR is never silently edited into a different decision. Supersession preserves the old record and links both directions. Meaning-preserving corrections are allowed and noted when material.
- Rejected proposals are kept so the same option is not rediscovered without new evidence.
- Filenames are sequential (`NNNN-short-title.md`); a number identifies the record, not its precedence. Numbers cited by accepted records for future decisions (for example ADR-0002 or ADR-0028) stay reserved for those topics.

## Format

Required: Status, Date, Context, and Decision, using precise normative language. Add further sections only where they earn their length: Terminology, Invariants, Alternatives, Consequences, Scenario walkthroughs, Extension implications, Open questions, and Explicit non-decisions. Keep the decision narrow enough to review in one sitting, make it falsifiable against scenarios, and do not use typed pseudocode as if it were a final Rust, Swift, wire, or storage API.

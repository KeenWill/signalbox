# Architecture decision records

Architecture decision records (ADRs) explain durable design choices that affect boundaries, identities, invariants, compatibility, or several future features. The [decision ledger](../decision-ledger.md) inventories accepted decisions and unresolved foundational questions.

## Accepted foundation set

The repository owner accepted these records atomically on 2026-07-13 after independent adversarial architecture review. They are authoritative together:

| ADR | Scope |
| --- | --- |
| [ADR-0001](0001-domain-terminology-and-identity.md) | Core terminology, owner-global applied-or-rejected command identity, purpose-specific applied-command proofs, and durable semantic identity boundaries |
| [ADR-0003](0003-session-creation-and-transcript-ancestry.md) | Independent session creation cause, owner-initiated baseline, initial transcript ancestry, and separation from versioned session defaults |
| [ADR-0004](0004-turn-and-attempt-lifecycle.md) | Turn/attempt lifecycle, aggregate attempt ownership, typed stop causes, applied-interrupt cancellation, proof-bearing reconciliation, startup recovery scan, terminal guards, ambiguity decisions, and regeneration identity boundary |
| [ADR-0005](0005-model-call-retry-semantics.md) | Target-before-call identity, typed reported-target mismatch failure/invalidation, no automatic known-failure retry, ambiguous-call recovery, continuation, refusal disposition, and configuration identity |
| [ADR-0027](0027-input-delivery-lifecycle.md) | Input delivery, versioned model-selection session defaults, constructible baseline effective configuration, explicit steering/configuration provenance, command deduplication, durable queue ordering, eligibility-fixed starting lineage, and context frontiers |

The five records form one normatively coupled baseline: their identity algebras, lifecycle transitions, configuration boundary, and context rules reference one another. A future change may correct or supersede an individual record only while preserving or explicitly revising its accepted dependencies.

## When to write an ADR

Write an ADR before closing a foundational ledger question, changing accepted direction, weakening an invariant, or introducing a technology that constrains several components. Do not use an ADR for local implementation details that are easy to reverse and do not alter a public boundary.

Use sequential filenames such as `0001-domain-terminology.md`. A number identifies the record, not its precedence; status and explicit supersession determine which decision applies.

## Status lifecycle

- **Proposed:** Under review and not authoritative. Implementation may explore it but must not present it as settled.
- **Accepted:** Approved and authoritative from its decision date. Update affected narrative documents, ledger rows, scenarios, and invariants in the same change.
- **Superseded:** Replaced wholly or partly by one or more accepted ADRs. Preserve the old record and link both directions.
- **Rejected:** Considered and declined. Preserve the rationale so the same option is not repeatedly rediscovered without new evidence.

A proposed ADR may become accepted or rejected. An accepted ADR may become superseded, never silently edited into a different choice. Corrections that do not change meaning are allowed and should be noted when material.

## Template

```markdown
# ADR-NNNN: Short decision title

- Status: Proposed
- Date: YYYY-MM-DD
- Owners: ...
- Reviewers: ...
- Supersedes: none
- Superseded by: none
- Decision-ledger questions: ...

## Context

What forces, accepted direction, evidence, and constraints make a decision necessary now?

## Decision

What is chosen, at what boundary, and from when? Use precise normative language.

## Terminology

Which concepts and names are introduced, retained, or changed? What must remain distinct?

## Invariants

Which invariant identifiers are preserved, added, changed, or retired, and how are they enforced?

## Alternatives

Which plausible choices were considered, including the status quo, and why were they not chosen?

## Consequences

What becomes easier, harder, required, or intentionally unsupported?

## Scenario walkthroughs

Walk the decision through affected scenario identifiers, including failure and restart behavior.

## Extension implications

Which future changes remain possible, and which compatibility or migration hooks are intentionally preserved?

## Open questions

What remains unresolved and where is it recorded in the decision ledger?

## Explicit non-decisions

What tempting adjacent choices does this ADR deliberately not settle?
```

## Review standard

An ADR should be independently understandable, narrow enough to decide, and specific enough to falsify with scenarios. Acceptance requires named owner approval, reviewer status, links to affected ledger rows, and corresponding document/test-plan updates. It must not use typed pseudocode as if it were a final Rust, Swift, wire, or storage API.

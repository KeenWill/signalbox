# Architecture decision records

ADRs record foundation-weight decisions: choices that alter accepted semantics,
move a boundary between domain, storage, wire, or framework representations,
weaken an invariant, or introduce a technology that constrains several
components. Lighter decisions are made in pull requests and recorded in the
[decision log](../decisions.md). Unresolved foundational questions live in
[open-questions.md](../open-questions.md).

## Accepted foundation set

The repository owner accepted these records atomically on 2026-07-13 after
independent adversarial architecture review. ADR-0001 was materially amended on
2026-07-15 to accept the initial private UUID-backed representation and
deliberately named UUID conversions for its Rust domain identity newtypes;
storage and wire representations remain separate boundaries. ADR-0022 later
selects native Postgres columns for the UUID-backed relational identities and
references it names, while wire representation remains undecided. The original
records are authoritative together:

| ADR                                                          | Scope                                                                                                                                                                                                                                                            |
| ------------------------------------------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| [ADR-0001](0001-domain-terminology-and-identity.md)          | Core terminology, owner-global applied-or-rejected command identity, purpose-specific applied-command proofs, durable semantic identity boundaries, and the initial private UUID-backed Rust identity representation                                             |
| [ADR-0003](0003-session-creation-and-transcript-ancestry.md) | Independent session creation cause, owner-initiated baseline, initial transcript ancestry, and separation from versioned session defaults                                                                                                                        |
| [ADR-0004](0004-turn-and-attempt-lifecycle.md)               | Turn/attempt lifecycle, aggregate attempt ownership, typed stop causes, applied-interrupt cancellation, proof-bearing reconciliation, startup recovery scan, terminal guards, ambiguity decisions, and regeneration identity boundary                            |
| [ADR-0005](0005-model-call-retry-semantics.md)               | Target-before-call identity, typed reported-target mismatch failure/invalidation, no automatic known-failure retry, ambiguous-call recovery, continuation, refusal disposition, and configuration identity                                                       |
| [ADR-0027](0027-input-delivery-lifecycle.md)                 | Input delivery, versioned model-selection session defaults, constructible baseline effective configuration, explicit steering/configuration provenance, command deduplication, durable queue ordering, eligibility-fixed starting lineage, and context frontiers |

The five records form one coupled baseline: their identity algebras, lifecycle
transitions, configuration boundary, and context rules reference one another. A
change may correct or supersede an individual record only while preserving or
explicitly revising its accepted dependencies. As implementation lands,
executable tests become the enforcement of record; the
[invariant catalog](../invariants.md) links each invariant to its enforcement.

## Accepted refinements and extensions

The repository owner accepted the following records beginning on 2026-07-17.
They depend on and refine the original foundation set; they do not retroactively
become part of its five-record atomic acceptance.

| ADR                                                         | Scope                                                                                                                                                                                                                                                                                                                                                                                                                  |
| ----------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| [ADR-0017](0017-credential-lifecycle.md)                    | Provider and integration credential lifecycle: 1Password/sops-age channel ownership, operator-synced Secret mounted-volume delivery read per send, the in-process credential access port keyed by non-secret references that enforces INV-035, frozen credential reference at target pin, and credential failure semantics mapped onto existing ADR-0005 edges                                                         |
| [ADR-0022](0022-persistence-representation.md)              | Normalized Postgres persistence records, explicit migrations, storage/domain mapping, database enforcement of the progressing slot and owner-global command identity, and native Postgres encoding for the UUID-backed identity columns it names                                                                                                                                                                       |
| [ADR-0030](0030-context-frontier-snapshots.md)              | Session-owned immutable context-frontier snapshot identity, exact ordered source-qualified resolution, identity versus content equality, trusted construction, and ancestry resolution                                                                                                                                                                                                                                 |
| [ADR-0031](0031-direct-fatal-terminalization.md)            | Direct fatal-mismatch failure or exact reconciliation at a closed aggregate boundary, with `StopRequested` retained only while aggregate work remains unfinished                                                                                                                                                                                                                                                       |
| [ADR-0032](0032-postgres-implementation-dependencies.md)    | SQLx Postgres driver, pool, and embedded migrations; Tokio runtime; and explicit Testcontainers-based ephemeral-Postgres integration testing                                                                                                                                                                                                                                                                           |
| [ADR-0033](0033-identity-generation-supply-and-encoding.md) | Caller-supplied durable-command UUIDs, hub-minted UUIDv7 durable-fact identities, opaque UUID semantics, and native Postgres encoding for every UUID-backed baseline identity                                                                                                                                                                                                                                          |
| [ADR-0034](0034-durable-command-storage-and-equality.md)    | Owner-global durable-command registry with versioned typed payload/result records and structural domain replay equality                                                                                                                                                                                                                                                                                                |
| [ADR-0035](0035-domain-owned-persistence-reconstitution.md) | Purpose-specific complete domain projections for validated persistence reconstitution, including opaque proof-bearing values and fail-closed corruption handling                                                                                                                                                                                                                                                       |
| [ADR-0036](0036-initial-semantic-transcript-entries.md)     | Initial origin-accepted-input and failed-turn semantic entries, with eligibility and terminal-failure commit granularity                                                                                                                                                                                                                                                                                               |
| [ADR-0037](0037-baseline-user-content.md)                   | Text-only baseline accepted-input content, exact decoded-text equality, PostgreSQL text mapping, and additive typed extension boundary                                                                                                                                                                                                                                                                                 |
| [ADR-0038](0038-session-aggregate-boundary.md)              | Minimal long-lived `Session` aggregate, separation from creation receipts and associated history, complete checked reconstitution, and current-pointer load-by-identity semantics                                                                                                                                                                                                                                      |
| [ADR-0039](0039-actor-attribution.md)                       | Closed typed actor attribution for durable commands and recorded transitions — owner, turn-scoped model, recovery, and tool-request agency as provenance participating in structural replay equality, with authorization, identity verification, and multi-user scope explicitly excluded                                                                                                                              |
| [ADR-0040](0040-transactional-outbox.md)                    | Transactional outbox as the sole path from committed durable state changes to client-visible update events: in-transaction event append, per-hub commit-ordered sequence as the ADR-0019 subscription cursor, at-least-once idempotent delivery, transient streams excluded, and the ADR-0010 nudge-plus-sweep publisher                                                                                               |
| [ADR-0041](0041-evidence-bearing-reconstitution.md)         | Domain-owned validation of active phases from complete owning-turn stop, wait, operation, and proof facts plus an anchored session acceptance tail that makes omitted accepted input and steering detectable                                                                                                                                                                                                           |
| [ADR-0042](0042-assistant-content-and-completion.md)        | Closed assistant-text and logical tool-use-reference semantic entries with producing-call provenance, atomic final-response commit, and an explicit completed-turn marker                                                                                                                                                                                                                                              |
| [ADR-0043](0043-provider-failure-classification.md)         | Exact scripted-provider disposition declarations and real-provider classification at the full-request-send boundary, with definitive provider error statuses known-failed and unresolved post-send outcomes ambiguous                                                                                                                                                                                                  |
| [ADR-0044](0044-hub-runtime-foundations.md)                 | Tokio as the hub binary's official runtime, the `tracing` facade with hubd-owned subscriber configuration, one shared operator failure taxonomy (infrastructure, corruption, identity collision, caller/hub bug) with mandatory corruption diagnostic keys, and the hubd composition-root contract for configuration, migration-at-startup, per-invocation service concurrency, shutdown, and the scheduler nudge hook |
| [ADR-0045](0045-model-call-execution-orchestration.md)      | Application-owned model-call execution through a prepare-call transaction, one provider interaction outside any database transaction, and an outcome-commit transaction, with exact correlation, authoritative aggregate reloads, and a one-call/no-retry policy at every between-effect failure boundary                                                                                                              |

## Process

- An ADR in this directory is accepted and authoritative. The pull request that
  introduces it is the proposal; while that pull request is open the record is
  under review, and the repository owner's merge is the act of acceptance.
  Records carry no status line, and nothing in a draft may claim acceptance —
  only a merge confers it. Only the repository owner merges ADR pull requests.
- A rejected proposal is closed unmerged and recorded as a dated entry in the
  [decision log](../decisions.md) naming the pull request and the reason, so the
  same option is not rediscovered without new evidence.
- An accepted ADR is never silently edited into a different decision.
  Supersession preserves the old record and links both directions.
  Meaning-preserving corrections are allowed and noted when material.
- Filenames are sequential (`NNNN-short-title.md`); a number identifies the
  record, not its precedence. Numbers cited by accepted records for future
  decisions (for example ADR-0002 or ADR-0028) stay reserved for those topics.

## Format

Header lines: Date (drafting or last material revision; the merge commit is the
authoritative acceptance timestamp), Depends on, Supersedes, Superseded by, and
Decision questions, plus Amended or Accepted-with notes where they carry real
history. Required sections: Context and Decision, using precise normative
language. Add further sections only where they earn their length: Terminology,
Invariants, Alternatives, Consequences, Scenario walkthroughs, Extension
implications, Open questions, and Explicit non-decisions. Keep the decision
narrow enough to review in one sitting, make it falsifiable against scenarios,
and do not use typed pseudocode as if it were a final Rust, Swift, wire, or
storage API.

# ADR-0022: Persistence representation

- Status: Proposed
- Date: 2026-07-16
- Owners: Repository owner
- Reviewers: none yet; this record is authoritative only if the owner accepts it
- Supersedes: none
- Superseded by: none
- Depends on: the accepted foundation set ([ADR-0001](0001-domain-terminology-and-identity.md), [ADR-0003](0003-session-creation-and-transcript-ancestry.md), [ADR-0004](0004-turn-and-attempt-lifecycle.md), [ADR-0005](0005-model-call-retry-semantics.md), [ADR-0027](0027-input-delivery-lifecycle.md))
- Decision questions: stable storage representation for the accepted domain baseline; database enforcement of INV-009 and INV-012; the storage-record/domain-type mapping boundary; UUID column encoding as a linked identity decision; migration discipline and candidate tooling

## Context

The [architecture](../architecture.md) already fixes Postgres as the canonical durable store and its sources-of-truth table names what must be durable: session content and ancestry, accepted input with delivery and queue order, turn and attempt state, effective configuration and provenance, model-call provenance, tool decisions, and dispatch generations. The open question ["Stable storage representation"](../open-questions.md#protocols-and-persistence-reserved-adr-0019-through-adr-0023) blocks persistence implementation (S03, S04, S25), with a recorded leaning toward a Postgres-native schema with explicit migrations and event/log use only where justified.

The accepted foundation set defines exactly what a first persistence slice must be able to hold: the identity kinds and owner-global durable-command registry of ADR-0001, session creation provenance and versioned defaults of ADR-0003 and ADR-0027, the turn/attempt state and stop-cause algebra of ADR-0004, model-call provenance and evidence of ADR-0005, and the accepted-input dispositions, queue-order facts, configuration freeze, and context frontiers of ADR-0027. The domain crate ([`crates/domain`](../../crates/domain/src/lib.rs)) already implements value slices of that algebra — UUID-backed identity newtypes, accepted-input disposition transitions, the closed baseline configuration algebra, immutable queue-order facts with pure total-order derivation, and turn-attempt stop/terminal values — so this proposal is checkable against concrete types, not only pseudocode.

Three catalog constraints shape the representation directly. INV-002 requires database records to stay distinct from domain types. INV-009 and INV-012 are classified database-level: process memory or typed transitions alone are insufficient, so the schema itself must be able to reject a second active turn in a session and a duplicated durable command. This record proposes how; it closes no open question, and nothing below is normative unless the owner accepts it.

## Decision

Signalbox persistence uses a **Postgres-native, normalized relational schema of purpose-specific storage records with explicit, versioned, in-repo migrations**. Lifecycle aggregates are current-state rows carrying typed state discriminators and constraint-enforced exclusivity. Append-only tables are used exactly where an accepted ADR already defines the fact as immutable once committed — the durable-command registry, session creation provenance, session-defaults versions, accepted-input content and order facts, provider target evidence and invalidations, outcome-authority transfers, ambiguity acknowledgements, and semantic history entries. Turn attempts are current-state rows whose terminal end variant is write-once, not a separate append-only table. There is no event store from which current state is replayed: the guarded row is the durable statement of record, which keeps INV-009 and INV-012 enforceable as declarative constraints rather than projection code.

### Storage records are not domain types

Record structs, SQL, and row mapping live in a persistence boundary outside `crates/domain`, per INV-002 and the architecture's dependency direction. Domain types gain no ORM or serialization derives for this purpose, and no generated record type appears in a domain transition. Every load is an explicit fallible mapping that validates identifier kind, ownership, and constructibility at the boundary; every store maps a domain value to record columns deliberately. Two consequences are visible already in the existing types:

- Ordinal values such as `SessionInputPosition` and `SessionConfigurationDefaultsVersion` are `u64` domain ordinals starting at one. Postgres `bigint` is signed and cannot round-trip values above `i64::MAX`, which are still valid domain ordinals; the mapping therefore stores them as `numeric(20, 0)`, which preserves the full `u64` range and its natural ordering. The boundary rejects non-positive, fractional, or out-of-`u64`-range values on load rather than reinterpreting the domain type or silently narrowing it to a signed range.
- Proof-bearing values (`AppliedInterruptProof`, `AppliedStopForReconciliationProof`, stop-cause and reconciliation values) are deliberately opaque and lack public raw constructors. Rehydrating them from rows must go through domain-owned correlation seams that revalidate the recorded applied command result and correlations, so the persistence layer cannot mint lifecycle authority that ADR-0001 reserves for matching applied results. The exact seam design is an open question below.

### Aggregate-to-record map

One table per aggregate or immutable fact family; typed enums become text discriminator columns constrained by `CHECK` to the closed accepted variant set, with variant payload columns constrained to be present exactly when the discriminator requires them. The map below covers every concept the foundation set defines and every type the domain crate implements today. Names are illustrative, not final DDL.

| Domain concept (normative owner) | Storage approach |
| --- | --- |
| Identity newtypes (`SessionId`, `AcceptedInputId`, `TurnId`, `TurnAttemptId`, `ModelCallId`, `ProviderTargetEvidenceId`, `ToolRequestId`, `ToolAttemptId`, `DurableCommandId`; ADR-0001) | Native `uuid` columns. Identity kind is carried by table and column position plus foreign keys, never by inspecting the value; matching bytes across columns establish nothing (INV-001). |
| Durable command registry (ADR-0001, ADR-0027) | Append-only `durable_command`: `command_id uuid PRIMARY KEY`, command-kind discriminator, session or other owner reference, canonical typed payload in a versioned normalized encoding, terminal result discriminator (`applied` or `rejected`) with its typed result payload, and the claim timestamp. Inserted only by the first committed handling transaction. |
| Session creation provenance (ADR-0003) | `session`: `session_id`, creation-cause discriminator (baseline `owner_initiated` only, `CHECK`-closed), ancestry columns (`source_session_id`, source-frontier reference) constrained both-null or both-present. The creation-provenance columns are immutable after insert; they never change once written. |
| Session model-selection defaults (ADR-0027; `VersionedSessionConfigurationDefaults`) | Append-only `session_defaults_version`: `(session_id, version)` primary key plus the complete normalized `ModelSelectionRequest` columns. The current-version pointer is not stored on the immutable `session` row: it lives in a separate one-row-per-session `session_current_defaults` record (`session_id` primary key, `current_version` referencing `session_defaults_version`), advanced only by compare-and-set against the caller's expected version. |
| Accepted input (ADR-0001, ADR-0027; `AcceptedInputLifecycle`, `DeliveryRequest`, `PerInputConfigurationChoices`) | `accepted_input`: identity, session, user content, delivery-request discriminator with `expected_active_turn` and per-input configuration choice columns, acceptance position with `UNIQUE (session_id, acceptance_position)`, and disposition state columns for exactly the four accepted variants (`origin_of` turn, `pending_steering` with the single `SteeringBinding` source-turn column, `consumed_as_steering` with the consuming `model_call_id`, `reclassified_as_turn_origin` with the new turn and typed reason). |
| Queue-order facts (ADR-0027; `AcceptedInputQueueOrder`, `AcceptedInputQueuePriority`) | Immutable columns written at acceptance: acceptance position on `accepted_input`, priority discriminator and optional `interrupt_predecessor_turn_id` on the origin `turn` row. The durable total order is derived at read time from the complete session fact set (mirroring `derive_accepted_input_total_order`); no derived order or direct predecessor is stored before eligibility. |
| Turn lifecycle (ADR-0004, ADR-0027) | `turn`: identity, session, origin accepted input (`UNIQUE`, one origin per input and one typed origin per turn), configuration provenance columns (below), the pinned exact provider/model target that ADR-0005 fixes as a durable turn fact before the first `model_call` is created (write-once columns, null exactly until the target is pinned, thereafter the single canonical target every call in the turn reuses), state discriminator (`queued`, `active`, `terminal`), active-phase discriminator (`running`, `awaiting_approval` with its exact `tool_request_id`, `awaiting_recovery_decision` with its wait set in the child table below), start columns (`lineage` discriminator, `immediate_predecessor_turn_id`, starting-frontier reference) constrained null exactly while `queued` and non-null otherwise, and terminal-disposition columns for the five accepted variants including the `Cancelled` proof pair and the `ReconciliationRequired` marker reason. |
| Effective configuration and provenance (ADR-0027; `EffectiveConfiguration`, `OriginConfiguration`, `TurnConfigurationProvenance`) | Provenance discriminator on `turn`: `explicit_origin` rows store the requested selection, exact checked defaults version, and the complete frozen effective value (frozen direct or alias selection with its frozen definition, plus explicit single-valued policy columns for `ProviderDefaults`, disabled retry, and disabled fallback, `CHECK`-pinned so a future category extension is a visible migration rather than a reinterpretation); `inherited_for_reclassified_steering` rows store the canonical source-turn reference only, never a second copied configuration. |
| Exact operation sets (ADR-0004; `NonEmptyIssuedOperationRefs`, `ReconciliationMarker`) | Child table `turn_wait_operation`: `(turn_id, operation_kind, operation_id)` primary key over the tagged reference columns, one row per member, so set semantics and duplicate rejection are structural. Nonemptiness and exact-set equality with a caller's expected set are validated by the guarded transition; the same shape stores a terminal marker's exact set with the marker's typed reason on `turn`. |
| Turn attempts (ADR-0004; `CurrentTurnAttempt`, `TurnAttemptStopCauses`, `AttemptEnd` and its disposition enums) | `turn_attempt`: identity, turn, nonterminal state discriminator (`prepared`, `running`, `stop_requested`) or terminal end variant (`without_stop`, `after_cancellation`, `after_fatal_mismatch`) with its cause-specific disposition column `CHECK`-restricted to that variant's accepted disposition set, interrupt-proof column pair where the variant carries one, and a child table `turn_attempt_fatal_cause` holding the nonempty fatal failure set as tagged `ProviderTargetMismatchFailureRef` rows. |
| Applied interrupt authority (ADR-0004, ADR-0027; `AppliedInterruptProof`, `AppliedInterruptCommandResult`) | No free-standing proof table. Wherever an accepted value carries the proof, the row stores the `(command_id, predecessor_turn_id)` pair with foreign keys into `durable_command` and `turn`; rehydration revalidates against the recorded applied `SubmitInput::Interrupt` result through the domain's correlation seam. |
| Model calls and provider evidence (ADR-0005) | `model_call`: identity, issuing turn attempt and turn, state and terminal-disposition discriminators, requested selection, frozen alias definition, a reference to the turn's pinned exact provider/model target (read from the `turn` fact above, never recopied as a divergent per-call target), and the call's own context-frontier reference. Append-only `provider_target_evidence` (evidence identity, call, typed observation payload), `provider_target_mismatch_invalidation`, `provider_outcome_authority_transfer` (decision command, from-call, to-call), and `ambiguity_acknowledgement` rows for `DuplicateRiskAccepted`. Exact encodings for selection keys and provider-reported identities remain with the provider-provenance decision (reserved ADR-0006/0007). |
| Semantic history and context frontiers (ADR-0027) | Append-only per-session `semantic_entry` table holding ordered committed semantic facts (inputs, consumed steering, committed assistant/tool content, outcome and accepted-risk markers). A frontier is an immutable reference to exact ordered entries: proposed as a per-frontier reference table `(frontier_id, ordinal, semantic_entry_id)` shared by call frontiers, starting frontiers, and ancestry source frontiers. The entry payload model is blocked by the open transcript-entry question (ADR-0001) and is not fixed here. |
| Tool requests and attempts (ADR-0001) | `tool_request` (identity, owning turn) and `tool_attempt` (identity, owning request, issuing turn attempt) rows whose foreign keys enforce the accepted ownership cardinalities now; state machines, normalized-argument content, approval binding, and dispatch-generation columns are reserved for the tool and runner ADRs. Result acceptance must remain expressible as one compare-and-set against owning request, issuing attempt, and current generation (INV-011). |

### Database enforcement of the progressing slot (INV-009)

At most one `Active` turn per session and at most one live attempt per turn become partial unique indexes over the current-state discriminators:

```sql
-- Illustrative enforcement shape, not final DDL.
CREATE UNIQUE INDEX turn_one_active_per_session
    ON turn (session_id) WHERE state = 'active';

CREATE UNIQUE INDEX turn_attempt_one_live_per_turn
    ON turn_attempt (turn_id) WHERE end_variant IS NULL;

CREATE UNIQUE INDEX turn_one_interrupt_successor_per_predecessor
    ON turn (interrupt_predecessor_turn_id)
    WHERE interrupt_predecessor_turn_id IS NOT NULL;
```

Activation is one transaction that updates the queued row to `active`, writes the non-null start columns, and inserts the initial `prepared` attempt; two racing activations for one session serialize on the first index and the loser aborts, satisfying INV-009's requirement that enforcement not rest on process memory. The indexes guarantee *at most* one; the *exactly-one-attempt-while-running* and *wait-carries-no-attempt* shapes are guaranteed by the aggregate transition updating both rows in the same transaction, with a deferrable constraint trigger available as defense in depth if review wants the database to also reject a torn pair. `CHECK` constraints tie variant payloads to discriminators (start columns null iff `queued`; wait subject present iff waiting; terminal columns present iff `terminal`), so a row the state machine cannot construct is also a row the schema rejects.

### Database enforcement of owner-global command identity (INV-012)

The baseline authority model is single-owner, so one table is the owner-global namespace: the primary key on `durable_command.command_id` spans all command kinds, sessions, and clients. The claim boundary maps directly onto the transaction rules ADR-0001 and ADR-0027 already fix:

```sql
-- Illustrative enforcement shape, not final DDL.
CREATE TABLE durable_command (
    command_id       uuid PRIMARY KEY,
    kind             text NOT NULL,
    session_id       uuid REFERENCES session (session_id),
    canonical_payload jsonb NOT NULL,   -- versioned normalized encoding
    result           text NOT NULL CHECK (result IN ('applied', 'rejected')),
    result_payload   jsonb NOT NULL,
    claimed_at       timestamptz NOT NULL
);
```

The first handling transaction inserts this row together with every domain effect of the command (for `SubmitInput`: the accepted input, its disposition, order facts, any created turn with frozen provenance, any applied-interrupt transition) and acknowledges only after commit (INV-007). A concurrent duplicate submission conflicts on the primary key and rereads the winner. Replay of a recorded identifier compares the stored canonical payload for structural equality and returns the recorded terminal result before any current-state validation; equal payload returns the result, different payload is conflicting reuse and is rejected. A failure before this insert commits claims nothing, exactly as the nonclaiming boundary requires. Because comparison is structural domain equality rather than byte equality, the stored encoding must be canonical and versioned; its exact format is an open question below.

### Transactions and recovery reads

Every atomic transition named by the accepted ADRs — acceptance, defaults replacement, eligibility fixing plus activation or eligible failure, safe-point steering consumption, duplicate-risk replacement, stop-cause application, terminalization with its six-step guard, and the startup scan — is one Postgres transaction whose writes are guarded `UPDATE`/`INSERT` statements with current-state predicates, so a stale writer changes zero rows and fails loudly rather than overwriting (INV-006, INV-011). Startup recovery is purely a read-then-transition over durable rows: nonterminal attempts, issued-operation evidence, stop causes, and wait subjects are all first-class records, so the idempotent scan required by INV-034 re-derives its complete evidence set from tables alone and its guarded transitions no-op on rerun. Nothing durable references a live process, connection, or lease (INV-010); whether the baseline needs a process-incarnation column on attempts, or may rely on the single-hub fact that every nonterminal attempt observed at startup is abandoned, is left to the scheduler decision (reserved ADR-0010).

### Migrations

Schema changes are explicit, forward-only, versioned SQL migration files committed in-repo and reviewed in the pull request of the slice that needs them; the applied-migration ledger lives in the database itself. Candidate tooling, with tradeoffs — **no candidate is selected here**, because each adds a dependency that needs owner approval under the repository dependency rule:

- **`sqlx` migrations (`sqlx-cli` / `sqlx::migrate!`)** — Rust-native, embeds migrations in the binary, checksums applied files; pulls the full `sqlx` stack, which is a large dependency decision that likely arrives anyway with the first Postgres adapter and should be made once, deliberately, at that boundary.
- **`refinery`** — focused Rust migration crate, embedded or CLI use, smaller surface than `sqlx`; a second Rust database dependency if the adapter ends up on `sqlx` or `tokio-postgres` regardless.
- **`diesel_migrations`** — mature, but arrives with an ORM whose generated types would sit uncomfortably close to the INV-002 boundary; heavyweight for a repo that wants hand-written records.
- **External migration binary (`golang-migrate`, `dbmate`, Flyway/Liquibase)** — keeps Rust dependencies untouched and migrations plain SQL; adds a non-Rust toolchain to development and deployment, and Flyway/Liquibase add a JVM.
- **Repo-owned minimal runner** — a small table plus ordered SQL files Signalbox owns outright; no dependency, but Signalbox then owns ordering, checksumming, locking, and failure-recovery correctness that mature tools already test.

### Justified event/log use

The only append-only records are those the ADRs define as immutable facts, listed in the map above. Transient provider streaming deltas are explicitly not persisted in the baseline (the architecture keeps them in the live hub process unless selectively checkpointed); a streaming checkpoint policy would be a separate decision. No universal event type serves conversation, coordination, audit, and presentation at once (INV-005).

### Identity encoding as a linked decision

Persistence forces exactly one fragment of the open [identity-representation question](../open-questions.md#identity-representation): the database encoding of the UUID-backed identity newtypes. This record proposes native `uuid` columns — 16-byte binary storage with database-side format validation — rather than `text`, which doubles storage, loses validation, and invites case/format drift. Acceptance of this ADR would decide that fragment only, recorded as a linked partial resolution against that question; UUID generation version, minting authority, caller supply, boundary validation rules, public textual formatting, wire encodings, and serialization strategy all remain open and are not decided or foreclosed here. Semantic opacity is untouched: shared column type never makes identity kinds interchangeable (ADR-0001).

## Invariants

- INV-002: records, SQL, and mappings live outside the domain crate; every conversion is explicit and fallible. This ADR adds the enforcement approach; the invariant row remains the statement of record.
- INV-007: the acceptance transaction writes input, delivery, order, disposition, and provenance rows together; acknowledgement follows commit.
- INV-008: frozen configuration and provenance are columns written at acceptance and never rewritten from current defaults or alias state.
- INV-009: partial unique indexes plus discriminator `CHECK` constraints make the second active turn, the second live attempt, and the malformed state/payload row database-rejected, not merely type-unrepresentable.
- INV-010: queued turns, wait subjects, and exact operation sets are rows with no dependence on process or connection identity.
- INV-011: result acceptance is a single guarded compare-and-set naming owning request, issuing attempt, and current generation in its predicate.
- INV-012: one owner-global primary key claims the identifier atomically with the command's effects; replay and conflicting reuse are resolved from the stored canonical payload and recorded terminal result.
- INV-030: ancestry source and frontier are immutable columns validated at session insert.
- INV-034: the startup scan reads only durable rows and applies guarded transitions, so reruns are no-ops and no attempt row is ever created by the scan.

## Strongest alternative

A single event-sourced log per session (or per aggregate) with current state derived by replay or maintained projections. It offers a natural audit trail, temporal queries, and one write shape for every transition.

It is rejected as the primary representation because the catalog's database-level invariants are exactly the ones event sourcing makes hard: uniqueness and exclusivity live in projection code instead of declarative constraints, so INV-009 and INV-012 would again depend on application logic — the failure mode those invariants exist to exclude. The accepted ADRs also already mandate durable typed evidence records wherever audit matters, so the audit benefit is largely already required in relational form, and a universal event type would erode INV-005's separation of representations. Append-only tables remain where the domain itself is append-only.

## Rejected alternatives

- **ORM-generated records used as domain types.** Directly violates INV-002 and the dependency direction; domain transitions must not consume framework rows.
- **One JSONB document per aggregate.** Uniqueness, exclusivity, foreign-key ownership, and exact-set semantics become application conventions invisible to the database; INV-009/INV-012 enforcement degrades to hope.
- **Serialized in-process domain values as column payloads.** Couples the schema to Rust layout, defeats querying and constraints, and contradicts ADR-0001's separation of storage encodings from the private Rust representation.
- **A generic work/state table with stringly states.** Collapses the accepted closed variant algebras and makes `CHECK`-level closure impossible; ADR-0004's unrepresentable-state guarantees would not survive storage.
- **A nullable direct-predecessor column on queued turns.** ADR-0027 fixes starting lineage at eligibility, never acceptance; the schema must not reintroduce an unfixed predecessor as a sentinel or nullable column.
- **`text` UUID columns.** Weaker validation, larger storage and indexes, and formatting drift for no boundary benefit.

## Consequences

The schema has many narrow tables and constraints rather than a compact message store, mirroring the deliberate shape of the domain: every lifecycle fact the ADRs make durable is individually queryable, constraint-checked, and reconstructable at restart. Some rules are enforced twice — typed transitions in the domain and constraints in the database — which is intentional for database-level invariants and cheap for the rest. Schema evolution is always a visible migration reviewed with the slice that needs it. Mapping code is a real cost: every aggregate gains an explicit record/domain conversion, which is the price INV-002 sets for keeping the domain free of storage types.

## Scenario walkthroughs

- **S03 (restart after accepting queued work):** Acceptance committed one transaction containing the `durable_command` applied row, the `accepted_input` row with content, delivery, and unique acceptance position, the queued `turn` row with priority facts and frozen explicit-origin configuration columns, against the compare-and-set-checked defaults version — then acknowledged. After restart the hub holds no relevant memory: it reads the session's nonterminal turns and order facts, derives the total order exactly as `derive_accepted_input_total_order` does, and recomputes eligibility. When every earlier ordered turn is terminal, one transaction fixes lineage and starting-frontier references, flips the row to `active` under `turn_one_active_per_session`, and inserts the `prepared` attempt — or, for a structurally unsupported frozen configuration, writes the same start columns with `terminal`/`failed` and no attempt row. A duplicate recovery or scheduler pass re-derives the same order and finds the guarded transitions already applied; the unique indexes make a duplicated activation impossible, so acknowledged work neither vanishes nor duplicates.
- **S04 (restart during a provider call):** Before send, durable rows already exist for the turn (active, running) carrying its pinned exact target, the live attempt, and the model call referencing that target with its frontier reference and in-flight state. After the crash, the startup scan in one transaction selects nonterminal attempt rows, joins their issued operations and evidence (`model_call`, `provider_target_evidence`, stop-cause columns) to derive the complete evidence and stop-cause set, then applies guarded updates: the call becomes `ambiguous` when evidence cannot establish acceptance, the abandoned attempt ends `Lost` in the matching terminal variant, and the turn either enters `awaiting_recovery_decision` with exactly one `turn_wait_operation` row for that call, or terminalizes with a reconciliation marker whose exact set and typed reason are those same rows, per ADR-0004's precedence. The scan inserts no attempt rows, and a rerun selects nothing nonterminal (INV-034). A later `ResolveAmbiguity` command claims its identifier in `durable_command`, validates its expected operation set for equality against the wait's child rows, and either records the appended `ambiguity_acknowledgement` and the atomically created replacement attempt/call with its authority-transfer row, or terminalizes with `OwnerChoseReconciliation` — every step a guarded transition over the rows named here.

## Extension implications

Delegation (reserved ADR-0002), runner dispatch and fencing (reserved ADR-0008–0010), tool lifecycles (reserved ADR-0011–0014), and archive/retention (reserved ADR-0028/0029) add tables, columns, and constraints by migration without reinterpreting existing rows; reserved columns are not created speculatively. A future multi-owner model would add an explicit owner namespace to `durable_command` rather than weakening the uniqueness boundary. Future configuration categories extend the provenance columns additively under ADR-0027's algebra rule. Protocol ADRs (reserved ADR-0019–0021) map wire encodings to these records through their own boundary types; the schema is not a wire contract.

## Open questions

- Migration tooling selection, and whether it is decided together with the Postgres driver/adapter dependency; any choice adds a dependency and needs explicit owner approval.
- The canonical, versioned payload encoding used for durable-command structural comparison (deterministic field ordering, normalization rules, and encoding-version migration).
- The semantic-entry payload model and assistant-content commit granularity (open under ADR-0001), which fix the final shape of `semantic_entry` and whether frontier references can use prefix compression instead of exhaustive per-frontier rows.
- The rehydration seam: how mapping code reconstructs opaque proof-bearing domain values through validated correlation without acquiring a constructor that could mint authority from arbitrary rows.
- Whether attempts carry a process-incarnation column or the single-hub baseline suffices for identifying abandoned tenures; scheduler locking, wake-up, and whether Postgres coordination alone suffices remain with reserved ADR-0010.
- Streaming checkpoint policy, if any, for transient provider drafts.
- Dispatch-generation record placement for INV-011/INV-021 (on `tool_attempt` or a separate dispatch record) — runner ADR scope.
- Archival and destructive-retention storage forms (reserved ADR-0028/0029), including retention behavior for ancestry sources noted open by ADR-0003.
- UUID generation version, minting authority, caller supply, public formatting, and wire encodings — the remainder of the identity question, deliberately untouched.

## Explicit non-decisions

This proposal selects no migration tool, database driver, connection pool, or any other dependency, and adds no code. It does not close the identity-representation, transcript-entry, scheduler, protocol, streaming-checkpoint, or archive open questions; it does not decide provider or runner credential storage beyond restating that secrets stay outside ordinary session records; and it does not define schemas for capabilities the foundation set has not accepted (delegation, regeneration commands, fallback, tool states, queue mutation). Table and column names above are illustrative shapes for review, not a frozen DDL API.

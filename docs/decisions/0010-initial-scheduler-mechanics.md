# ADR-0010: Initial scheduler mechanics

- Date: 2026-07-17
- Owners: Repository owner
- Reviewers: Codex, Copilot, and Cursor Bugbot (automated PR review); no specialist human reviewer assigned
- Supersedes: none
- Superseded by: none
- Accepted with: [ADR-0009](0009-dispatch-fencing.md) as one coupled pair — the fence carried by the dispatch and result transactions this scheduler executes
- Depends on: the accepted foundation set ([ADR-0001](0001-domain-terminology-and-identity.md), [ADR-0003](0003-session-creation-and-transcript-ancestry.md), [ADR-0004](0004-turn-and-attempt-lifecycle.md), [ADR-0005](0005-model-call-retry-semantics.md), [ADR-0027](0027-input-delivery-lifecycle.md))
- Coordinated with: [ADR-0022](0022-persistence-representation.md) (guarded rows, partial unique indexes, and the deferred process-incarnation question) and [ADR-0030](0030-context-frontier-snapshots.md) (the start construction that the activation transaction commits); if either changes, the references here follow that record
- Decision questions: how eligibility is detected and serialized per session against INV-009's database-level enforcement; wake-up strategy and its failure behavior; startup-scan coordination with ADR-0004's recovery semantics; whether Postgres alone coordinates the initial scheduler; the adapter boundary that keeps a future broker possible

## Context

This record closes the foundational question of whether Postgres alone is sufficient for the initial scheduler — previously listed under [scheduling and runners](../open-questions.md#scheduling-and-runners-reserved-adr-0008) (S03, S05, S12) — whose recorded leaning was to start with Postgres if correctness and wake-up tests pass while preserving an adapter boundary. ADR-0004 leaves "scheduler locking, wake-up, leases, and Postgres coordination" to this record, and ADR-0022 defers here whether attempts need a process-incarnation column or the single-hub fact suffices to identify abandoned tenures. The [architecture](../architecture.md) makes the central scheduler a hub-owned module responsible for durable dispatch coordination, fencing, and the at-most-one-progressing-turn policy, allows the hub to be one deployable modular monolith, and deliberately selects no broker or workflow engine.

The accepted semantics the scheduler must execute already exist. ADR-0004 defines eligibility as a derived predicate — every turn earlier in ADR-0027's durable total queue order is terminal and the session has no active turn — and requires that enforcement of the single progressing slot not rest on process memory (INV-009). ADR-0027 fixes starting lineage and the outcome-aware frontier atomically at eligibility, never at acceptance, and ADR-0030 defines the snapshot construction that transaction commits. The domain crate implements the pure order derivation as `derive_accepted_input_total_order` over immutable queue facts ([`crates/domain/src/queue_order.rs`](../../crates/domain/src/queue_order.rs)), and models the closed active-phase and current-attempt shapes the activation transaction creates ([`crates/domain/src/turn_lifecycle.rs`](../../crates/domain/src/turn_lifecycle.rs), [`crates/domain/src/turn_attempt.rs`](../../crates/domain/src/turn_attempt.rs)). This record fixes the mechanics that run those transitions and, on acceptance, closes that question; it is the normative source for the mechanics it decides.

## Decision

### The durable rows are the only queue

The initial scheduler is a hub-internal component with no authoritative memory. Every scheduling effect is one of the guarded transactions the accepted ADRs already define — eligibility fixing with activation or eligible terminal failure (ADR-0027), and dispatch authorization and result acceptance under ADR-0009 — executed against Postgres with current-state predicates. In-process structures (wake queues, per-session tasks, cached orders, timers) are latency optimizations that may be discarded at any moment without losing, duplicating, or reordering work, because acknowledged facts are already durable (INV-007, INV-010) and every transition revalidates current state. A **wake-up is a hint, never authority**: acting on a false wake-up finds nothing eligible and changes zero rows; losing a true wake-up delays work until the sweep below finds it.

### Per-session eligibility detection and serialization

Eligibility evaluation is a per-session pass. The pass loads the session's complete durable queue and lifecycle facts, derives the total order exactly as `derive_accepted_input_total_order` does, evaluates ADR-0004's derived predicate for the head turn, and, when it holds, executes the single eligibility transaction: fix starting lineage and the outcome-aware frontier, then activate with the initial prepared attempt or terminalize as the eligible failure ADR-0027 defines. Passes for different sessions are independent and may run concurrently.

The locking discipline is layered, and the layers have distinct jobs:

- **Enforcement (unchanged, referenced).** INV-009's database-level enforcement is what ADR-0022 defines: guarded `UPDATE`/`INSERT` statements whose predicates revalidate `queued` state and terminal predecessors inside the transaction, plus the partial unique index permitting at most one `active` turn per session. A raced or stale activation changes zero rows or aborts. This layer is sufficient for correctness on its own.
- **Serialization (proposed here).** To make concurrent passes orderly rather than abort-and-retry, every eligibility transaction for a session first takes a row-level lock (`SELECT ... FOR UPDATE`) on one designated per-session scheduler row, so racing passes for the same session queue behind one another and the loser re-reads state that already reflects the winner. The recommended lock subject is a dedicated one-row-per-session scheduler record rather than the immutable `session` provenance row (which other transactions have no reason to lock) or a `pg_advisory_xact_lock` keyed by a hash of the session identity (which shares one untyped 64-bit space with every other advisory use and is invisible to the schema). The exact row shape is persistence-slice scope under ADR-0022's discipline.
- **Coalescing (memory only).** Within the single hub process, a per-session in-memory queue may coalesce redundant wake-ups so at most one pass per session runs at a time. This is an optimization; correctness never depends on it.

### Wake-up strategy

The events that can change a session's eligibility are enumerable and, in the baseline, all committed by the hub itself: input acceptance creating a queued turn, and any transition that gives the active turn a terminal disposition and releases the slot. The proposed baseline strategy is therefore:

- **Primary: same-process nudge.** After the hub commits an eligibility-affecting transaction, it enqueues an in-memory wake-up for that session. Latency is effectively zero and no additional infrastructure or connection exists to fail. Failure behavior: a crash between commit and nudge loses only the hint; the committed fact is found by startup recovery and the sweep.
- **Safety net: periodic reconciliation sweep.** At a configurable interval, one indexed query finds sessions with a queued turn and no active turn — the storage shape of the eligibility precondition — and enqueues passes for them. The sweep is the correctness backstop for every lost hint and missed edge. Failure behavior: a failed sweep is a visible operational error retried at the next interval; it cannot corrupt state because it only enqueues hints.
- **LISTEN/NOTIFY: not adopted in the baseline, documented as the multi-process extension.** While the hub is the only committing process, notifications add a channel that can only tell the hub what it just did. If a second committing process ever exists, `NOTIFY` on commit becomes the cross-process nudge — with known limits that keep it a hint and never the queue: delivery reaches only currently connected listeners, so a listener that reconnects must run a full sweep to cover the gap; `LISTEN` needs a dedicated session (transaction-mode connection pooling silently breaks it); every `NOTIFY`-carrying commit serializes on a global notification-queue append, which degrades under high commit rates; and payloads are capped (about 8 kB), so a notification should carry at most a session hint while the durable rows remain the work. These properties are why the durable-rows-plus-sweep design does not change if notifications are added: they only lower latency.

A polling-only design (no nudges) and a notification-only design (no sweep) were both rejected: the first buys simplicity by making every interaction pay the poll interval even though the committing process already knows the event happened; the second makes progress depend on delivery that Postgres explicitly does not guarantee across disconnects.

### Startup-scan coordination

On process start the hub runs ADR-0004's startup recovery scan to completion before the scheduler activates or dispatches anything. The scan is what turns "a prior process was running this" into honest durable state: it ends every nonterminal prior-process attempt with disposition `Lost`, classifies interrupted operations, reconstructs waits, and terminalizes where the recovered evidence requires — all idempotently and without creating attempts (INV-034). Scheduling before the scan could dispatch new work into a session whose durable state still shows a live-looking attempt; scheduling after it starts from states the accepted lifecycle can actually construct. After the scan completes, a full sweep seeds the scheduler; both steps are idempotent, so a crash between or during them is recovered by simply running both again. Per-session scan gating — letting scanned sessions schedule while others are still being recovered — is a viable latency refinement that this record leaves open rather than requiring.

Resolving the question ADR-0022 deferred: the baseline records **no process-incarnation column and no lease on attempts**. The deployment contract is a single hub process, so every nonterminal attempt observed by the startup scan belongs to a prior incarnation and is abandoned; nothing durable references a live process (INV-010). Running two hub processes against one database is outside this record's contract: introducing any second scheduler-capable process requires revisiting this record, and an incarnation or equivalent fact would arrive then as a visible migration. As an operational safeguard — not proof — the hub may hold one advisory session lock as a singleton guard so a misconfigured second process fails fast; the guard lapses with its connection and remains deployment discipline, not enforcement.

### Postgres alone, behind an adapter boundary

The initial scheduler uses **Postgres coordination alone**: no broker, message queue, or workflow engine, consistent with the architecture's explicit non-selection and the recorded leaning. This choice is falsifiable. It stands only while the following tests pass in the scheduler slice, and failing them reopens technology selection through a superseding record rather than ad hoc infrastructure:

- **S03 correctness:** after restart, eligibility is re-derived from durable order and slot facts, and duplicate recovery or scheduler passes activate exactly once (guarded transitions no-op on rerun).
- **INV-009 enforcement:** two racing activation transactions for one session serialize; exactly one commits an active turn, under both the lock discipline and, with the lock removed, the unique index alone.
- **S09 ordering:** a turn becomes eligible only after every turn earlier in the durable total order — including later-accepted interrupt insertions — is terminal, and fixes lineage through its actual immediate predecessor.
- **Lost-wake-up liveness:** with all nudges disabled, every scenario still completes with only added latency bounded by the sweep interval — demonstrating that hints carry no correctness weight.
- **Fence tests:** the ADR-0009 walkthroughs for S05, S06, and S12 hold against the dispatch and result transactions this scheduler executes.

The adapter boundary that keeps a future broker possible separates three seams, matching the architecture's dependency direction:

- **Domain (never moves):** order derivation, the eligibility predicate, lifecycle transitions, and proof construction stay in `crates/domain`, pure and storage-free.
- **Work-source port:** wake-up subscription and the reconciliation sweep sit behind one application port whose baseline implementation is the in-process nudge plus Postgres sweep. A broker or notification bus replaces the implementation of this port only.
- **Dispatch-transport port:** delivering a fenced dispatch envelope and receiving result envelopes sit behind a second port. Whatever carries the envelopes, dispatch authorization and result acceptance remain hub-owned Postgres transactions under ADR-0009; a broker is transport and wake-up, never the statement of record and never the fence validator.

## Invariants

This record fixes mechanics for INV-009 (the serialization discipline layered on ADR-0022's database enforcement), INV-034 (scan-before-scheduling ordering and the single-hub abandonment rule), and INV-010 (no durable lease or process reference), and relies on INV-007 and INV-011/INV-021 via ADR-0009. The catalog rows remain the statements of record; this acceptance adds the enforcement links there without duplicating these rules.

## Strongest alternative

Adopt a dedicated coordination technology now — a broker for wake-up and dispatch delivery, or a workflow engine for turn orchestration — so the scheduler never needs revisiting as deployment grows. It is rejected for the initial architecture because every correctness property the accepted ADRs demand (durable order, guarded exclusivity, idempotent recovery, fencing) must live in the transactional store regardless, so the second system would add operational surface, a large dependency decision, and a second source of timing without removing any obligation from Postgres. The adapter boundary above is the deliberate, cheaper hedge: it preserves exactly the seams a broker would occupy.

## Rejected alternatives

- **Leases or heartbeats for slot or work ownership.** Expiry-based ownership contradicts INV-010's no-live-process posture and reintroduces split-brain windows; ADR-0004 already rejected keeping attempts live across waits via leases.
- **A durable "eligible" state or scheduler-owned queue table.** ADR-0004 fixes eligibility as a derived predicate, not a second lifecycle state; materializing it invites divergence from the facts it is derived from.
- **`LISTEN`/`NOTIFY` as the primary mechanism.** Delivery is not durable across disconnects and needs dedicated connections; it can only ever be a hint, and in a single-process baseline it is a hint the process already has.
- **Polling as the only mechanism.** Pays the full poll interval on every interactive turn despite the committing process knowing the event instantly; retained only as the safety-net sweep.
- **Advisory locks as the per-session serialization primitive.** One untyped 64-bit keyspace shared across all uses, keyed by a hash of a 128-bit identity, invisible to schema review; a locked row is typed, visible, and collision-free.
- **Cross-process scheduler concurrency in the baseline.** `FOR UPDATE SKIP LOCKED` work-claiming across processes is well understood, but the single-hub contract makes it premature; the ports above are where it would arrive.

## Scenario walkthroughs

- **S01:** Acceptance commits the queued origin turn; the same process nudges the session's scheduler task. The pass derives a one-turn order, finds the predicate holds, and runs one transaction that fixes the start (per ADR-0027/ADR-0030), flips the turn active under the unique index, and creates the prepared attempt. A duplicate nudge finds an active turn and changes nothing.
- **S03:** Restart discards nudges and tasks. The startup scan finds nothing nonterminal to end for a merely queued turn; the post-scan sweep finds the session with queued work and no active turn; the pass re-derives the same total order from immutable facts and activates exactly once — or, for a structurally unexecutable frozen configuration, fixes the same start and terminalizes as the eligible failure, then re-evaluates the successor. Wake-up state is reconstructed, matching the scenario's requirement that pre-restart wakeups disappear and are rebuilt.
- **S07:** The applied interrupt's transaction creates the queued successor with its typed priority relation. When the predecessor reaches its terminal disposition, that committing transaction nudges the session; the successor's pass derives the order with the interrupt insertion first, and activation fixes lineage to the actual terminal predecessor. If the hub instead restarts mid-stop, the scan ends the abandoned attempt and completes terminalization per ADR-0004, and the sweep finds the successor.
- **S09:** Turn B's pass finds an earlier nonterminal turn in the derived order and does nothing, however many nudges or sweeps fire. Only after every earlier ordered turn — including work inserted ahead of B after its acceptance — is terminal does the predicate hold, and B fixes its lineage and frontier through its actual immediate predecessor in the activation transaction.
- **S05/S12:** The scheduler's dispatch authorization and result acceptance are the ADR-0009 transactions; a runner reconnecting after the sweep interval delivers into the same compare-and-set regardless of which wake-up path resumed the session.

## Open questions

- Sweep interval, latency budgets, and any per-session fairness or starvation policy across many sessions are operational tuning left to the scheduler slice.
- Whether the singleton advisory guard is adopted, and per-session scan gating instead of global scan-first, are refinements the slice may propose in the decision log.
- Runner selection, capability matching, placement, and pinning for dispatch remain reserved ADR-0008 scope; this record schedules turns and carries fenced dispatches without choosing their targets.
- The dedicated per-session scheduler row's shape and migration belong to the persistence slice under ADR-0022.
- Adoption timing for `LISTEN`/`NOTIFY` if a second committing process arrives, and any queue admission or resource limits (reserved resource-governance scope), remain open.

## Explicit non-decisions

This record does not decide runner capabilities, placement, or enrollment (reserved ADR-0008, ADR-0015 through ADR-0018), tool execution or retry policy (reserved ADR-0011 through ADR-0014), resource limits, client-protocol encodings ([ADR-0019](0019-process-protocol.md), [ADR-0021](0021-compatibility-and-negotiation.md)), or any storage DDL. It selects no broker, queue, workflow engine, database driver, pool, or other dependency, and adds no code. Named ports, rows, and queries above are illustrative shapes for review, not a final Rust, storage, or wire API.

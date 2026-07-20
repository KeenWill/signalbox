# ADR-0044: Hub runtime foundations

- Date: 2026-07-20
- Amended: 2026-07-20 — post-merge review corrections: (1) the configuration
  clause no longer places the database credential under
  [ADR-0017](0017-credential-lifecycle.md), whose scope is provider and
  integration credentials — the original sentence silently closed the
  database-credential delivery decision
  [ADR-0032](0032-postgres-implementation-dependencies.md) reserves, and the
  correction restores that accepted open reservation, with the hubd slice's
  environment read explicitly provisional; (2) hub-minted aggregate identifiers
  are classified loggable while a caller-supplied command identifier is
  represented only by a derived, non-reversible correlation token; (3) the
  mandatory session corruption key is conditional on session-scoped operations;
  (4) [ADR-0035](0035-domain-owned-persistence-reconstitution.md) concurrent
  staleness is explicitly consumed inside adapters, outside the operator
  taxonomy; (5) an incoming command identifier reused for a different kind is
  classified consistently as a caller or hub bug, while a persisted cross-kind
  relationship that cannot be reconstituted remains corruption
- Supersedes: none
- Superseded by: none
- Depends on: [ADR-0010](0010-initial-scheduler-mechanics.md),
  [ADR-0017](0017-credential-lifecycle.md),
  [ADR-0032](0032-postgres-implementation-dependencies.md),
  [ADR-0034](0034-durable-command-storage-and-equality.md),
  [ADR-0035](0035-domain-owned-persistence-reconstitution.md), and
  [ADR-0040](0040-transactional-outbox.md)
- Refines: ADR-0032's runtime boundary with the hub binary's entrypoint and
  composition-root contract, and ADR-0035's failure boundary with one shared
  operator taxonomy
- Resolves: the typed caller-error family the
  [2026-07-19 audit entry](../decisions.md#2026-07-19--post-milestone-2-audit-corrections-and-tracked-obligations)
  tracks (the classification here; each mapping lands with its slice)
- Decision questions: the hub's official async runtime; the observability facade
  and its boundaries; one operator failure taxonomy across adapters; the
  composition-root construction contract hubd owns

## Context

The hub binary is still empty: `apps/hubd/src/main.rs` is `fn main() {}` with
startup deliberately deferred, while the application crate already exposes five
per-invocation services (`CreateSessionService` through
`ReplaceSessionDefaultsService` in `crates/application/src/lib.rs`) and the
persistence crate exposes `Clone`, `PgPool`-backed repositories plus an embedded
`MIGRATOR` and explicit `migrate` operation (`crates/persistence/src/lib.rs`).
The first slice that wires these into a running process will improvise whatever
this record leaves open — an ad-hoc stderr logger, a bespoke error mapping, a
shared service behind a lock — and, exactly as ADR-0040 observed for publishing,
every later slice copies the improvisation.

Three accepted mandates already require operational visibility without naming a
mechanism. ADR-0010 makes a failed sweep "a visible operational error"; ADR-0035
requires corruption handling to be fail-closed *and visible*, and allows
diagnostics to add table, key, and query context outside the domain error;
ADR-0017 forbids credential values in any log line. Meanwhile the adapter error
families have grown apart: `SubmitInputRepositoryError` distinguishes
`Database`, `DifferentCommandKind`, identity collision, and `Corruption`, while
its siblings draw different lines, and the 2026-07-19 audit entry tracks that
preparation failures which can only be caller bugs currently conflate into
`Corruption::Inconsistent`. ADR-0032 selected Tokio 1 as the runtime SQLx and
the hub share but wired nothing into the binary and left the
migration-invocation and configuration questions open.

## Decision

### Hub runtime entrypoint

[ADR-0032](0032-postgres-implementation-dependencies.md) owns the selection of
Tokio 1 as the hub's asynchronous runtime and the runtime-free domain boundary.
This record decides the remaining entrypoint contract: hubd owns the runtime
entrypoint, and application-adjacent runtime code (task spawning, shutdown
signals, nudge channels) uses that selected runtime. Tokio is already
transitively present through SQLx's `runtime-tokio` feature; the hubd wiring
slice must add a direct dependency with narrowly selected features under
ADR-0032's dependency discipline.

### Observability through the `tracing` facade

The application, persistence, and hubd crates emit operational telemetry through
the `tracing` facade. Subscriber selection, formatting, filtering, and
configuration live in hubd alone: library crates emit events and spans and never
install a subscriber. This is the mechanism behind ADR-0010's visible
operational error and ADR-0035's fail-closed-and-visible mandate.

The domain stays rendering-free and dependency-free. Domain errors are never
logged directly; adapters and runtime code translate them into a taxonomy
classification plus diagnostic keys at the boundary, per ADR-0035's allowance.
Operational telemetry never rides the ADR-0040 outbox, which carries
client-visible update events only; a log line is not a client fact.

The hubd wiring slice must add `tracing` as a new dependency crossing the
repository's large-dependency gate; the owner approved its adoption when
commissioning this record, and merging the record is the recorded acceptance.
Explicit non-goals: metrics and OpenTelemetry are deferred, and no log-content
policy exists beyond [ADR-0017](0017-credential-lifecycle.md)'s credential
redaction plus one rule this record adds — full user content never appears in
logs. The opaque UUID-backed aggregate identifiers (`SessionId`, `TurnId`, and
their [ADR-0033](0033-identity-generation-supply-and-encoding.md) siblings) are
loggable — they are semantically opaque references under ADR-0033, not content —
but raw `DurableCommandId` values are not: ADR-0033 permits callers to supply
arbitrary non-sentinel UUIDs, so telemetry represents them only with a stable,
non-reversible correlation token derived by the observability boundary. The
redaction prohibition targets user content, credential material, free-form
payload values, and raw caller-supplied identifiers. Lengths and taxonomy
classifications may likewise appear.

### One shared operator failure taxonomy

The application crate owns one closed operator-facing failure classification,
and every adapter error family maps into it. Concurrent staleness per
[ADR-0035](0035-domain-owned-persistence-reconstitution.md) is consumed inside
adapters by reload-and-rederive and is never surfaced through the operator
taxonomy; the four categories classify only genuine failures after staleness
handling:

- **Infrastructure** — the operation could not complete; carries a
  commit-ambiguous flag for failures (a connection lost around commit) where the
  transaction's fate is unknown and recovery must re-read durable state rather
  than assume either outcome.
- **Fail-closed corruption** —
  [ADR-0035](0035-domain-owned-persistence-reconstitution.md)'s durable
  corruption: committed rows cannot construct the accepted domain value; no
  effect, no repair.
- **Identity collision** — a hub-minted candidate identity collided; the
  operation retries with fresh candidates rather than failing the work.
- **Caller or hub bug** — a request that can only be a defect (for example
  [ADR-0034](0034-durable-command-storage-and-equality.md)'s conflicting
  command-id reuse, or an activation guard that cannot fail honestly), distinct
  from corruption. This is the typed caller-error family the tracked conflation
  awaits; slices migrate the `Corruption::Inconsistent` conflations as they
  touch them.

Domain rejections stay recorded applied-or-rejected results under
[ADR-0001](0001-domain-terminology-and-identity.md) and
[ADR-0034](0034-durable-command-storage-and-equality.md) — never errors, never
taxonomy members. Diagnostics attach aggregate keys at the adapter/runtime
boundary, and for corruption events the discipline is mandatory: every
corruption-classified event names the session identity when the failing
operation is session-scoped, plus the durable command and/or turn identity when
the failing operation is scoped to one — and never a credential value or user
content. Registry-level and pre-claim corruption events carry the derived
durable-command correlation token, rather than the raw caller-supplied identity,
without a session key. Reuse of an incoming command identifier for a different
kind remains the caller-or-hub-bug classification above. A persisted cross-kind
relationship discovered while reconstituting accepted state is fail-closed
corruption under [ADR-0035](0035-domain-owned-persistence-reconstitution.md).

### Composition-root contract

hubd is the composition root and owns construction:

- **Configuration.** `DATABASE_URL` arrives as deployment configuration supplied
  to the process environment. The delivery channel for database credentials
  remains open per [ADR-0032](0032-postgres-implementation-dependencies.md),
  which reserves that decision for a separate future record —
  [ADR-0017](0017-credential-lifecycle.md)'s channel split governs provider and
  integration credentials, not this one — and until that decision lands the hubd
  slice uses an explicitly provisional deployment-configuration read from the
  process environment, never a 1Password runtime credential and never a durable
  record. Production connections use the persistence crate's verify-full
  options.
- **Migration at startup.** The baseline resolves ADR-0032's open wiring: the
  hub process itself runs the embedded migrations at startup, before ADR-0004's
  recovery scan, which completes before ADR-0010 permits scheduling. A failed
  migration is a failed startup, visibly.
- **Concurrency.** Repositories are cheap `Clone` values over the shared
  `PgPool`; services are cheap, take `&mut self`, and are built per invocation
  over a cloned repository. Each task clones what it needs; a shared
  `Mutex`-guarded service instance is prohibited — it would serialize unrelated
  sessions behind process memory that ADR-0010 says carries no authority.
- **Graceful shutdown.** On a shutdown signal hubd stops admitting new work,
  gives in-flight transactions a bounded window to commit or abort, and exits.
  Abrupt exit is safe at any point: durable rows plus the startup scan (INV-034)
  recover whatever a window abandoned, so shutdown polish is latency, never
  correctness.
- **Nudge channel.** Command outcomes reach the scheduler through a typed hook:
  after an eligibility-affecting commit, the committing path hands a session
  hint to ADR-0010's work-source port. It is an explicit typed boundary, not
  runtime introspection of domain results. Its exact shape stays loose here; the
  scheduler slice refines it.

## Rejected alternatives

- **Std-only stderr logging.** No dependency, but it establishes the ad-hoc path
  every slice copies: unstructured lines, no levels, no keys, and a migration to
  structured telemetry that grows harder with each caller.
- **Logging through the outbox.** Violates ADR-0040's scope — the outbox carries
  client-visible events; operational telemetry in it would leak operator
  diagnostics toward clients and bloat the cursor stream.
- **Per-service bespoke error classification.** The status quo the taxonomy
  replaces; it already produced the tracked caller-bug/corruption conflation and
  gives operators a different vocabulary per adapter.
- **Shared mutable service instances.** A `Mutex`'d long-lived service
  contradicts the cheap-per-call construction the crates already exhibit and
  puts a process-memory lock where ADR-0010 requires none.

## Open questions

- Metrics, OpenTelemetry, log shipping, and retention are deferred to a later
  operational decision.
- Subscriber format, level policy, and the shutdown window are implementation
  tuning in the hubd slice.
- The nudge hook's exact shape and the work-source port API belong to the
  scheduler slice under ADR-0010.
- Migration-role separation (schema-owner versus steady-state credentials)
  remains open from ADR-0032.

## Explicit non-decisions

This record adds no code by itself and writes no manifests; its dependency
requirements land with the hubd wiring slice. It does not decide scheduler
mechanics beyond naming the hook boundary (ADR-0010 owns them), the protocol
server or transport startup (ADR-0019 scope), pool sizing or timeouts, log
content schemas, or any storage DDL. It selects no metrics, tracing-subscriber
ecosystem beyond hubd's private choice, or process supervisor. Names above
(`SessionRepository`, `migrate`, error variants) describe today's code for
falsifiability, not a frozen API.

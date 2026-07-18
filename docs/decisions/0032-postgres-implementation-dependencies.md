# ADR-0032: Postgres implementation dependencies

- Date: 2026-07-17
- Owners: Repository owner
- Reviewers: Codex (dependency selection and boundary review); no specialist human reviewer assigned
- Supersedes: none
- Superseded by: none
- Depends on: [ADR-0022](0022-persistence-representation.md)
- Coordinates with: [ADR-0009](0009-dispatch-fencing.md) and
  [ADR-0010](0010-initial-scheduler-mechanics.md)
- Decision questions: Postgres driver and pool; migration tooling; async runtime;
  ephemeral-Postgres integration tests; Docker-free default validation

## Context

ADR-0022 selects a normalized Postgres schema, hand-written storage records,
explicit fallible mappings, and forward-only in-repository migrations. It
deliberately leaves the driver, pool, and migration tool open because each
choice adds a dependency requiring owner approval. ADR-0009 and ADR-0010 then
require guarded concurrent transactions for activation, dispatch, and result
acceptance. The first persistence slice cannot implement or test those
requirements until one compatible stack is selected.

The choice crosses several components. A driver determines the asynchronous
runtime and database value mappings. A pool participates in every application
transaction. Migration tooling runs against development, test, and production
databases. The integration harness must exercise real Postgres constraints and
locking while keeping the ordinary repository validation usable on machines
without Docker. This is therefore a foundation-weight technology decision,
rather than an implementation detail hidden in the first code pull request.

The domain boundary remains fixed. ADR-0022 requires that records, SQL, and row
mapping live outside `crates/domain`; domain types gain no ORM, row, or
serialization derives. Selecting a toolkit does not move that boundary.

## Decision

### One Postgres stack

Signalbox uses the SQLx 0.9 series for its Postgres driver, connection pool, and
embedded migration runner. It uses the Tokio 1 series as the hub's asynchronous
runtime. The implementation enables narrowly selected features instead of
either crate's default or `full` feature set.

The initial SQLx feature set is:

```text
postgres
runtime-tokio
tls-rustls-ring-native-roots
migrate
macros
uuid
rust_decimal
time
```

Default SQLx features are disabled. `postgres` excludes drivers Signalbox does
not use. `uuid`, `rust_decimal`, and `time` support the native UUID,
`numeric(20, 0)`, and timestamp columns ADR-0022 requires. The Rustls
native-roots TLS backend avoids a platform OpenSSL dependency. Production TCP
connections configure `PgSslMode::VerifyFull` so certificate and hostname
verification use the host trust store; ephemeral local Postgres configures
`PgSslMode::Disable`.

`macros` is enabled only because
[`sqlx::migrate!`](https://docs.rs/sqlx/0.9.0/sqlx/macro.migrate.html) embeds the
reviewed SQL migration set. Persistence queries use SQLx's runtime query API,
static SQL, `Row::try_get`, hand-written record structs, and explicit fallible
record/domain conversions. The persistence boundary does not use `query!`,
`query_as!`, `FromRow` derives, SQLx type derives, or an ORM-generated domain or
record model. Implementation verification of the SQLx 0.9 feature graph
clarified that `macros` necessarily activates the query-macro, `derive`, and
offline feature surfaces transitively even though Signalbox does not select
`derive` or offline support directly. Availability of those surfaces is not
authority to use them: repository code uses only `migrate!`, and query macros
and SQLx derives remain prohibited by this boundary. The stack does not enable
`any`, another database driver, or JSON support before ADR-0022's canonical
durable-command payload encoding is decided.

SQLx's built-in
[`PgPool`](https://docs.rs/sqlx/0.9.0/sqlx/postgres/type.PgPool.html) is the only
application connection pool. No Deadpool or BB8 layer is added.
Pool size, acquisition timeout, idle policy, and lifetime policy are explicit
deployment configuration with conservative defaults chosen by the
implementation slice; they are not domain semantics and are not fixed here.
Each accepted atomic transition uses one acquired SQLx transaction, and pool
ownership never enters a domain API.

Tokio supplies the runtime SQLx and the hub share. The initial direct Tokio
features are `rt-multi-thread`, `macros`, `sync`, and `time`; a later slice adds
`signal`, `process`, `fs`, or another feature only when its behavior needs it.
No Tokio task, channel, timer, lock, or error type crosses into
`crates/domain`. The multi-thread runtime permits independent session work and
concurrent database race tests, while ADR-0010's per-session database
serialization remains the correctness boundary.

Exact compatible patch releases are locked in `Cargo.lock`. Patch upgrades and
an explicit Postgres test-image patch update are ordinary dependency
maintenance when they preserve these boundaries. A change outside any selected
dependency series, a different driver/runtime, or a migration-system
replacement requires a new foundation review.

### Embedded, forward-only SQL migrations

ADR-0022 remains the normative owner of the forward-only, versioned,
in-repository SQL-file discipline. One static SQLx `Migrator` embeds that
governed file set and uses SQLx's database migration ledger, checksums, and
default migration locking. The repository adds a build script that emits
`cargo:rerun-if-changed=<migration-directory>` so stable Rust rebuilds when a
migration is added, and `.gitattributes` fixes migration files to LF line
endings so checksums do not vary by checkout platform.

The migration library exposes one explicit operation that both production
startup wiring and integration tests can invoke. This record does not decide
whether a deployment calls it through a future hub subcommand, an init job, or
the main process. Whichever wiring is chosen must finish schema migration
before ADR-0004's startup recovery scan, and that scan must finish before
ADR-0010 permits scheduling.

SQLx applies rather than generates the ADR-0022-governed schema-source SQL
files.

### Migration-candidate evaluation

ADR-0022 names five candidates. Four add a dependency or external tool, while
the fifth is the no-dependency control. They are resolved as follows:

| Candidate | Decision | Reason |
| --- | --- | --- |
| SQLx migrations | Selected | Reuses the chosen driver and pool, embeds reviewed SQL, and supplies checksums, a database ledger, and migration locking without a second database stack |
| `refinery` | Rejected | Focused and smaller in isolation, but duplicates driver and migration surface once SQLx is already selected |
| `diesel_migrations` | Rejected | Mature, but brings a second driver/ORM ecosystem next to a boundary that deliberately requires hand-written records and mappings |
| External binary (`golang-migrate`, `dbmate`, Flyway, or Liquibase) | Rejected | Keeps Rust smaller but adds a separately versioned development and deployment prerequisite; the JVM candidates add another runtime |
| Repository-owned minimal runner | Rejected | Avoids a dependency only by making Signalbox own ordering, checksums, concurrent locking, and failure recovery |

The strongest complete alternative is `tokio-postgres` plus
`deadpool-postgres` and `refinery`. Each component is focused, and the direct
driver exposes Postgres closely. It is rejected because Signalbox would select
and integrate three independent layers where SQLx supplies one coherent
transaction, pool, mapping, and migration stack. That extra separation does
not strengthen INV-002: explicit record/domain mapping is enforced by
Signalbox's module and API boundary, not by using the lowest-level driver.

### Ephemeral Postgres integration tests

Container-backed integration tests use the
[`testcontainers-modules`](https://docs.rs/testcontainers-modules/0.15.0/testcontainers_modules/)
0.15 series with its Postgres module and asynchronous runner. Its default
features are disabled; only `postgres` and the `ring` crypto backend are
enabled. The synchronous `blocking` runner and unrelated service modules remain
disabled. Each test binary starts an explicitly tagged supported Postgres
image, enables the module's `with_fsync_enabled()` setting rather than
inheriting its performance-oriented `fsync=off` default, uses that container's
isolated database, applies the embedded migrations, and closes its SQLx pool
before the container is dropped. The implementation pins an explicit image tag
rather than inheriting a module default or using `latest`; the supported
production major and test major must match.

The container dependency is isolated behind a `postgres-integration` Cargo
feature and a dedicated integration-test target. Every container-backed test
is marked `#[ignore = "requires ephemeral PostgreSQL"]`. Therefore the root
validation sequence, including `cargo test --workspace --all-targets
--all-features`, compiles the integration surface but never contacts a
container runtime.

The explicit integration invocation enables the feature and runs only the
ignored target, for example:

```bash
cargo test -p signalbox-persistence \
  --features postgres-integration \
  --test postgres_integration \
  -- --ignored --test-threads=1
```

That invocation does not silently skip when Docker or a compatible container
runtime is unavailable: failure to start Postgres is a test failure. CI adds a
required Postgres-integration job on a runner with Docker support, while the
existing repository-wide validation job remains Docker-free.

The first persistence stack proves at least:

- a fresh migration and an idempotent second migration run;
- migration checksum rejection and concurrent migration serialization;
- UUID and full-domain-ordinal (`numeric(20, 0)`) mapping boundaries, including
  zero, maximum, fractional, negative, and out-of-range cases;
- INV-009's one-active-turn and one-live-attempt database enforcement under
  racing transactions;
- INV-012's owner-global first command claim, equal replay, conflicting reuse,
  and concurrent duplicate submission;
- transaction rollback and stale guarded-update behavior; and
- the ADR-0009 result-acceptance races when that stacked slice lands.

This explicit target is the enforcement mechanism for
[CONTRIBUTING's Postgres-testing rule](../../CONTRIBUTING.md#testing). Pure
mapping tests may still use constructed record values without a database, and
ordinary domain tests remain storage-free.

## Invariants

- INV-002: SQLx types stop at the persistence boundary. Domain types gain no
  SQLx, Tokio, migration, test-container, ORM, or serialization traits.
- INV-009: real Postgres partial indexes, constraints, row locks, and racing
  transactions enforce the progressing slot; a pool or task lock is not the
  authority.
- INV-010: the runtime and pool introduce no durable process, connection,
  heartbeat, or lease identity.
- INV-011 and INV-021: SQLx transactions can express ADR-0009's one
  compare-and-set predicate; transport and runtime types remain outside it.
- INV-012: the owner-global database key and canonical stored payload decide
  command replay under concurrency.
- INV-034: migrations complete before recovery, and recovery completes before
  scheduling; the runtime does not invent a recovery attempt.

## Strongest alternative

Use `tokio-postgres`, `deadpool-postgres`, and `refinery`, with the same Tokio
and Testcontainers choices. This reduces the abstraction in each database
operation and keeps migration tooling separately replaceable.

It is rejected because the three-package composition creates more integration
and version surface without removing any Signalbox-owned mapping or
transaction rule. SQLx's runtime query API permits the same hand-written rows
and SQL, while its pool and migrator avoid redundant dependencies. SQLx has a
larger compile-time and transitive-dependency cost; the owner accepts that cost
only through merging this record.

## Rejected alternatives

- **Diesel or another ORM as the adapter model.** Generated schema and row types
  invite the storage representation across INV-002's explicit mapping boundary
  and add machinery Signalbox does not need.
- **SQLx compile-time query macros.** They add build-time database or offline
  metadata coupling and generated row shapes; static runtime queries plus
  explicit decoding keep validation in the checked-in mapping code.
- **A separate pool.** Deadpool or BB8 duplicates SQLx's pool without a
  demonstrated requirement.
- **A different async runtime.** Async-std or Smol would split the database
  runtime from the Tokio-centered ecosystem expected by the hub without
  changing domain semantics.
- **CI service containers as the only test harness.** They are useful
  infrastructure but do not give local integration tests ownership of
  per-test lifecycle and configuration.
- **Docker Compose or shell-managed test databases.** They move cleanup,
  port allocation, and parallel isolation outside the Rust test process.
- **Silently skip when Docker is absent.** It can make an explicit integration
  job green without running its claimed checks. Ignored-by-default and
  fail-when-explicit separates the two modes honestly.

## Consequences

This is a substantial dependency choice. SQLx, Tokio, Rustls, Rust Decimal, and
Testcontainers increase download, compile, audit, and upgrade surface. In
return, Signalbox does not own a pool, migration ledger, migration lock, TLS
stack integration, or container lifecycle wrapper. Default-feature suppression
keeps unused database drivers and JSON mapping out of the initial graph. The
`macros` feature necessarily makes SQLx query macros, derives, and offline
support available transitively, but repository code uses only `migrate!`; those
other surfaces remain prohibited rather than directly selected.

The persistence crate stays hand-written and Postgres-specific. Tests exercise
the same database semantics as production and make Docker use explicit.
Developers without Docker can run the complete ordinary validation sequence;
developers and CI with Docker run the additional integration target.

Embedding migrations makes the reviewed binary aware of its schema set and
keeps test and production migration bytes identical. Deployment still must
choose when and under which database role to run them; this record does not
grant the steady-state hub schema-owner privileges.

## Scenario walkthroughs

- **S03:** The Postgres integration job migrates an empty database, commits an
  accepted queued turn, discards process memory, and reruns the guarded
  eligibility path. Competing activation transactions prove the unique index
  and row lock permit one active turn. Tokio wake tasks are hints only.
- **S04:** One SQLx transaction durably records call issue state before the
  simulated crash boundary. A fresh pool reconstructs the same rows; recovery
  classifies them before scheduler startup. No test double supplies Postgres
  locking or constraint behavior.
- **S12:** Concurrent result deliveries use separate pool connections against
  one current generation. Exactly one compare-and-set advances state; equal
  replay is acknowledged idempotently, while stale and conflicting deliveries
  cannot overwrite it.

## Open questions

- ADR-0022's canonical durable-command payload encoding, opaque-proof
  rehydration seam, exact ADR-0030 snapshot layout, cancellation-intent
  delivery, and archival form remain open.
- Production migration invocation, migration-role separation, pool sizing,
  timeouts, connection observability, and the exact supported Postgres image
  tag are implementation and deployment questions within this selected stack.
- Provider and integration credential delivery is governed by ADR-0017; the
  future owner-authentication and database-credential decisions remain
  separate.
- The scheduler sweep interval, singleton safeguard, and per-session scan
  gating remain the operational refinements ADR-0010 leaves open.

## Explicit non-decisions

This record adds no dependency or code by itself. It does not freeze DDL,
canonical command-payload encoding, proof rehydration, semantic-entry payloads,
context-frontier physical layout or identifier encoding, dispatch-generation
placement, cancellation delivery, pool tuning, sweep timing, production
database credentials, or a deployment migration command. It does not adopt
`LISTEN`/`NOTIFY`, a broker, a workflow engine, an ORM, SQLite, or generated
domain/storage mappings.

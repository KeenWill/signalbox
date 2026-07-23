# Configuration and credentials

This page describes the implemented configuration and credential behavior of
Signalbox, verified against merged `main` at `c0db59c` (hubd configuration
loading in `apps/hubd/src/configuration.rs` and `apps/hubd/src/main.rs`, the
static TOML catalog, and the provider bridge in `crates/model-provider-runtime`)
together with the model-runtime crates it composes
(`crates/model-runtime/src/credential.rs` and the redaction pipeline in
`crates/model-runtime-anthropic/src/runtime.rs`). It distills ADR-0017 and the
configuration clauses of ADR-0044 and ADR-0047. Invariant law lives in
[docs/invariants.md](../invariants.md), cited here by tag.

## Process configuration

`signalbox-hubd` reads exactly three deployment values from the process
environment at startup:

- `DATABASE_URL` — complete PostgreSQL connection URL. Production connections
  force `sslmode=verify-full` regardless of URL parameters. This environment
  channel is explicitly provisional under ADR-0044; the database-credential
  delivery decision remains reserved (ADR-0032).
- `SIGNALBOX_CONFIG_FILE` — path to the static model/alias catalog (below).
- `ANTHROPIC_API_KEY_FILE` — path to the file whose bytes are the current
  Anthropic API key value.

A missing or empty value, an unreadable or invalid catalog file, or a failed
Anthropic runtime construction fails startup at the `Configuration` phase,
before any database contact. Startup and shutdown logs carry the phase, an
operator failure class, and small typed fields where present (blocker count,
session and turn ids, recovered-turn count, grace-window seconds) — never
configuration values, paths, or URLs. The typed configuration error does not
survive to the log: `run_hub` collapses every catalog-parse and
Anthropic-construction variant (and likewise connection and migration errors)
into a generic `Infrastructure` class carrying only its phase, so an operator
cannot distinguish an unreadable catalog from an unknown field, bad version, or
invalid limit (see Open edges). The two file paths are accepted without I/O at
configuration time; only the catalog file is actually read during startup. The
key file is never read at startup (see credential lifecycle below).

The Anthropic endpoint parameters are composition-fixed at the adapter defaults
(public API base URL, `anthropic-version: 2023-06-01`, no connect or exchange
timeout, 8 MiB SSE record cap); no deployment knob exists for them. Startup
ordering, migration, the recovery scan, and shutdown policy are the
[runtime-substrate](runtime-substrate.md) page's material.

The local `signalbox-debug` harness reads `SIGNALBOX_DEBUG_DATABASE_URL` plus
the same two file variables in its `--anthropic` mode; it is a development
driver, not the client protocol.

## The static model and alias catalog

The file named by `SIGNALBOX_CONFIG_FILE` is a versioned TOML document
(`config/hubd.example.toml` is the checked-in example). Parsing is fail-closed:

- The root must carry `version = 1`; any other or absent version is rejected.
- At least one `[[models]]` entry is required: an absent, mistyped, or empty
  models array is rejected (`MissingModels`), so a document containing only
  `version = 1` fails startup.
- Unknown fields are rejected at the root and inside every table. Why: a
  silently ignored key would let a typo change model meaning invisibly, so
  unrecognized content fails explicitly instead.
- Parse errors are typed, sanitized values; no file content appears in error
  text. (hubd erases the type before logging, as described above.)

Each `[[models]]` entry defines one direct selection:

- `selection_id` — UUID of the immutable `DirectModelSelection` key.
- `target_id` — UUID of the exact normalized provider/model identity
  (`ResolvedProviderTarget`). Identity encoding is
  [identity-and-commands](identity-and-commands.md) material.
- `provider` — must be `"anthropic"`; the only provider this composition slice
  admits.
- `provider_model` — the exact provider-native model spelling; must be nonempty
  and unpadded.
- `max_output_tokens` — required positive `u32` output-token ceiling.

Each optional `[[aliases]]` entry defines one alias: `alias_id` (UUID of the
`ModelAlias`) and `selection_id`, which must name a configured model (dangling
aliases are rejected). Duplicate selection keys, duplicate aliases, and
conflicting runtime meanings for one target are all rejected.

One valid document yields two immutable in-memory catalogs:

- the domain `ModelTargetCatalog`, mapping each `DirectModelSelection` to its
  exact `ResolvedProviderTarget`, used by execution-time target resolution;
- the `RuntimeModelCatalog`, mapping each target to its provider-native spelling
  and output-token ceiling, used by the provider bridge
  ([runtime-substrate](runtime-substrate.md)).

The file is read once at startup and never reread; changing the catalog is a
process restart. Why: pinned targets and frozen selections must not change
meaning mid-flight, so the restart is the visible unit of configuration change.
Keeping a selection key immutable is deployment discipline that code enforces
only partially: removal makes new resolution fail, but nothing prevents an
edited document from pointing an existing `selection_id` at a new `target_id`
across a restart — new turns would silently resolve to the new target (see Open
edges). Where a stored call exists, code does enforce consistency: ordinary-path
reconstitution cross-checks every stored call's target against the configured
`ModelTargetCatalog` and fails closed as corruption (`CallTargetMismatch`) when
the catalog now resolves that selection to a different target. The startup-scan
restart path instead rebuilds its target catalog from the stored calls
themselves, deliberately not from configuration — part of why recovery of
acknowledged work is configuration-independent (INV-034).

## Model-selection validation

Validation happens at two boundaries, on frozen semantic meaning only —
credential presence is never consulted (ADR-0017, INV-008):

- **At acceptance.** `SubmitInput` freezes the requested selection into the
  turn's effective configuration. A direct selection freezes without catalog
  consultation. An alias request consults an acceptance-time definition
  resolver; an unknown alias is a recorded `UnknownModelAlias` rejection, not an
  error. In the current wiring the persistence path supplies an empty resolver,
  so every alias request rejects (see Open edges); acceptance semantics are
  [turn-lifecycle-and-scheduling](turn-lifecycle-and-scheduling.md) material.
- **At execution.** When the attempt pins its target, the frozen selection is
  resolved against the `ModelTargetCatalog`. An unresolvable selection fails the
  turn as a known failure before any model call exists; a credential or send
  failure occurs only after the call exists. Why: keeping configuration absence
  distinct from provider failure, with no silent model substitution, is what
  INV-017 and INV-018 require. Lifecycle detail is
  [model-call-execution](model-call-execution.md) material.

In the provider bridge, a durably resolved target with no `RuntimeModelCatalog`
mapping is a typed adapter defect (`UnconfiguredTarget`), never provider
evidence; both catalogs derive from the one document, so this indicates a
composition bug. The debug harness additionally pre-validates its requested
selection against the catalog before creating a session.

## Credential lifecycle

ADR-0017's hub-side contract is implemented as follows. Deployment channel
ownership (which store is a secret's source of truth, mounted-secret delivery,
rotation overlap discipline) is operational policy recorded in ADR-0017 and is
not enforced by code.

- **Reference/value split.** A `CredentialReference` is the non-secret durable
  name of one credential; a `CredentialValue` carries the secret bytes.
  References are safe in configuration, errors, logs, and durable records;
  values are safe only at the adapter boundary. Why: rotation preserves the
  durable name so no record or log ever needs the secret (INV-035). One
  reference exists today: the composition constant `anthropic-primary`.
- **File-based supply, reread per preparation.** `FileCredentialAccess` binds
  the reference to the `ANTHROPIC_API_KEY_FILE` path and reads the file for
  every request preparation; nothing is cached. Why: atomic file replacement
  rotates the key without restarting hubd, and an in-flight call keeps the value
  it authenticated with (ADR-0017). Resolution is reference-scoped: a foreign
  reference fails typed `Unmapped`; a missing file is `Unavailable`; an
  unreadable file is `Unreadable` — all reference-only errors.
- **No startup preflight.** hubd never reads the key file at boot, so a missing
  or unsynced credential cannot block startup or the recovery scan. Why:
  recovery of acknowledged work must not depend on any provider's credential
  (INV-034; ADR-0017).
- **Resolution timing.** The adapter resolves the pinned reference during send
  preparation of exactly one physical request — after the durable `Prepared`
  record, before send authorization — and the resulting value is scoped to that
  request (INV-002 boundary type). The adapter races resolution against its
  cancellation signal so a blocked read cannot hold a cancelled operation; in
  the composed hubd this race is inert, because the provider bridge passes
  `CancellationSignal::never()` to both preparation and execution and nothing
  constructs a firing signal (see Open edges).
- **Failure behavior.** A failed resolution, or a value that cannot form an HTTP
  header (empty, non-UTF-8, non-header-safe bytes), is a typed known preparation
  failure: the call ends `KnownFailed`, the attempt ends with a known failure,
  the turn fails — no automatic retry, no fallback (INV-014, INV-017, INV-018).
  Why: a missing credential is deployment misconfiguration, and retry or
  substitution would hide it. A provider rejecting the credential after send is
  ordinary outcome evidence ([model-call-execution](model-call-execution.md),
  ADR-0043).
- **Durable references, never values.** Postgres never stores a credential
  value. Each new model call durably pins its non-secret credential reference at
  the `Prepared` insert (`model_call.credential_reference`), immutable
  thereafter under the authorization-facts trigger; the column is nullable only
  for rows predating the migration. Resuming a stored `Prepared` call
  re-supplies the stored reference, and a stored call with no reference fails
  closed as corruption.

## Redaction and logs

The following never appear in logs, error text, or durable records: credential
values, the key file path, `DATABASE_URL`, and raw catalog file content. For
provider-controlled evidence the guarantee is mechanism-bounded: text is
scrubbed of the exact preparation-time credential value, as described below.
Enforcement as implemented:

- `CredentialValue` implements no `Display` or serialization and its `Debug`
  form is always `[REDACTED]`; the outbound `x-api-key` header is marked
  sensitive. `FileCredentialAccess`'s `Debug` redacts its path;
  `AnthropicRuntime`'s `Debug` redacts its credential source and version header.
  Access errors carry reference and typed failure class only.
- hubd logging is a compact INFO tracing subscriber; startup and runtime errors
  log phase, failure class, counts, and aggregate ids only.
- Every provider-controlled text that leaves the Anthropic adapter — stream text
  and thinking deltas, tool-argument JSON, tool proposals, native error bodies,
  provider request ids, reported model identity, stop-sequence and finish
  tokens, transport detail — is scrubbed with the exact preparation-time
  credential value before crossing the boundary. Streamed deltas additionally
  withhold a trailing credential prefix and, when ordering forces a flush,
  replace the withheld bytes with `[redacted]`. Why: provider chunk boundaries
  are arbitrary, so a reflected secret must not escape split across two deltas —
  the pipeline fails closed. Native error bodies get JSON-aware redaction before
  truncation so an escape-encoded secret cannot survive. The scrub covers the
  exact value, its JSON-string-escaped form in error bodies, and chunk-split
  prefixes in deltas; a reflection the provider re-encodes in any other form
  (base64, say) is outside these code paths. INV-035-tagged tests in
  `crates/model-runtime/src/credential.rs`,
  `crates/model-runtime-anthropic/tests/loopback.rs`, and
  `apps/hubd/src/configuration.rs` enforce this boundary.

## Open edges

- Catalog alias definitions are parsed and validated but not wired into input
  acceptance: the live SubmitInput path supplies an empty alias resolver, so
  every alias request is rejected as unknown until
  `HubModelConfiguration::resolve_alias` reaches the acceptance transaction.
- Selection-key retargeting across a restart is not prevented by code:
  reconstitution's `CallTargetMismatch` cross-check fails closed only for a
  session with a live stored call; for everything else, not retargeting a
  `selection_id` is deployment discipline.
- Model calls predating the credential-reference migration carry a NULL stored
  reference; resuming such a `Prepared` call fails closed as corruption rather
  than re-deriving the reference from configuration.
- Multi-provider support and the reference-to-provider-component mapping are
  undecided (reserved ADR-0007); today `provider = "anthropic"` and
  `anthropic-primary` are hard-coded.
- ADR-0017's deployment obligations (channel ownership, optional mounted Secret,
  auto-restart disablement, revoke-last rotation, retain-on-outage verification)
  are operational policy with no code enforcement.
- `DATABASE_URL` via process environment is explicitly provisional (ADR-0044);
  the database-credential delivery channel remains reserved (ADR-0032
  follow-up).
- hubd erases typed configuration diagnostics before logging: catalog-parse and
  Anthropic-construction variants (and connection and migration errors) collapse
  to a generic `Infrastructure` class plus phase, so startup logs cannot
  distinguish failure causes within the `Configuration` phase.
- No connect or exchange timeout is configured in hubd composition (ADR-0043
  assigns the budget to the caller); a hung provider exchange is bounded only by
  process shutdown — the 30-second grace window, then abandonment to startup
  recovery.
- No cancellation channel exists in the hubd composition: the provider bridge
  passes `CancellationSignal::never()` to both runtime preparation and
  execution, so the adapter's cancellation-dependent guarantees (credential-read
  race, cancelled-before-send) are inert capability in the live system.
- In-memory credential hygiene (zeroization or equivalent) is an ADR-0017 open
  question with no implementation.

# Configuration and credentials

This page describes the implemented configuration and credential behavior of
Signalbox, verified against the implementing stack through PR #217
(`agent/credential-reference-total`). This includes signalboxd configuration
loading in `apps/signalboxd/src/configuration.rs` and
`apps/signalboxd/src/main.rs`, the static TOML catalog, and the provider bridge
in `crates/model-provider-runtime`, together with the model-runtime crates it
composes (`crates/model-runtime/src/credential.rs` and the redaction pipeline in
`crates/model-runtime-anthropic/src/runtime.rs`); the database-channel refusals
in [process configuration](#process-configuration) were verified through PR #237
(`agent/fix-pg-env-surface`), in `production_connection_options` under
`crates/persistence/src/lib.rs`; the `signalboxd` binary name, its
`apps/signalboxd` code homes, and the `config/signalboxd.example.toml`
checked-in example path were verified through PR #258
(`agent/signalboxd-rename`). The daemon-held GitHub credential channel and its
code-host result redaction are verified through PR #270
(`agent/tool-batch-tier1`). The per-turn pinning behavior at a mid-session
defaults boundary was verified through PR #272 (`agent/mid-session-model`). The
credential-file value narrowing and the credential-shaped code-host detail were
verified through PR #285 (`agent/dev-instance-code-host-credential`). Invariant
law lives in [docs/invariants.md](../invariants.md), cited here by tag.

## Process configuration

`signalboxd` reads exactly five deployment values from the process environment
at startup:

- `DATABASE_URL` — complete PostgreSQL connection URL. Production connections
  force `sslmode=verify-full` regardless of URL parameters. This environment
  channel is explicitly provisional; the database-credential delivery decision
  remains open (see Open edges).
- `SIGNALBOX_CONFIG_FILE` — path to the static model/alias catalog (below).
- `ANTHROPIC_API_KEY_FILE` — path to the file holding the current Anthropic API
  key value.
- `GITHUB_TOKEN_FILE` — path to the file holding the current GitHub code-host
  token value.
- `SIGNALBOX_SOCKET_PATH` — local Unix-socket path for the version-one
  [process protocol](process-protocol.md), which owns its binding and trust
  semantics.

`DATABASE_URL` is the whole database configuration channel. The SQLx driver
would otherwise seed anything the URL omits from the ambient libpq-style `PG*`
variables — host, port, user, database, TLS material, application name, and
runtime options, plus `PGPASSWORD` and `PGPASSFILE` as a second credential
channel — so the production connection path refuses to parse when any of them is
present in the environment, whatever its value. `SSL_CERT_FILE` and
`SSL_CERT_DIR` are refused on the same terms: the driver's selected TLS backend
takes its root certificates only from what those two name whenever either is
set, and adds an `sslrootcert` the URL states to that set rather than replacing
it, so a root named by the environment would verify the production server even
under an explicit root certificate. The driver also falls back to libpq's
default password file when the URL carries no password and `PGPASSFILE` is
unset, so the same path refuses when `~/.pgpass` exists under the process home
directory; presence alone decides and the file is never opened. With those
closed the driver still completes an incomplete URL from outside it — an omitted
user name from the process account, an omitted host by probing the local socket
directories and then `localhost` — so the same path refuses a URL that states
either nowhere the driver reads it: the authority, or the `user`, `host`, and
`hostaddr` query parameters. Port and database name stay with the driver and the
server, which derive them from the URL alone: an omitted port is the fixed 5432,
and an omitted database name is the user name the URL states. The refusal names
the offending channel and never its contents, and it happens before any database
contact. A deployment carries every connection parameter in the URL. The
separate local test connection path is unchanged and keeps SQLx's behavior; it
is a development and test channel by intent — the integration suites and
`signalbox-debug`, which reads its own `SIGNALBOX_DEBUG_DATABASE_URL` — and no
check confines the URL it is given to a local cluster, so the refusals above are
what stand between a production cluster and ambient configuration, not that
path's name.

A missing or empty value, an unreadable or invalid catalog file, or a failed
Anthropic or GitHub transport construction fails startup at the `Configuration`
phase, before any database contact. Startup and shutdown logs carry the phase,
an operator failure class, and small typed fields where present (blocker count,
session and turn ids, recovered-turn count, grace-window seconds) — never
configuration values, paths, or URLs. The typed configuration error does not
survive to the log: `run_hub` collapses every catalog-parse and
adapter-construction variant (and likewise connection and migration errors) into
a generic `Infrastructure` class carrying only its phase, so an operator cannot
distinguish an unreadable catalog from an unknown field, bad version, or invalid
limit (see Open edges). The three file paths are accepted without I/O at
configuration time; only the catalog file is actually read during startup.
Neither credential file is read at startup (see credential lifecycle below).

The deployed daemon supplies no Anthropic endpoint or timeout knob; it
constructs the adapter with its defaults. The
[runtime-substrate](runtime-substrate.md) page owns those transport defaults,
positive caller-level exchange-timeout overrides, and the whole-exchange bound.
Startup ordering, recovery scanning, and shutdown policy are
[turn-lifecycle-and-scheduling](turn-lifecycle-and-scheduling.md) scope;
migration behavior is [persistence-protocol](persistence-protocol.md) scope, and
the socket boundary and single-daemon guard are
[process-protocol](process-protocol.md) material.

The local `signalbox-debug` harness reads `SIGNALBOX_DEBUG_DATABASE_URL`,
`SIGNALBOX_CONFIG_FILE`, and `ANTHROPIC_API_KEY_FILE` in its `--anthropic` mode.
It does not compose the daemon tool catalog and does not read
`GITHUB_TOKEN_FILE`; it is a development driver, not the client protocol.

## The static model and alias catalog

The file named by `SIGNALBOX_CONFIG_FILE` is a versioned TOML document
(`config/signalboxd.example.toml` is the checked-in example). Parsing is
fail-closed:

- The root must carry `version = 1`; any other or absent version is rejected.
- At least one `[[models]]` entry is required: an absent, mistyped, or empty
  models array is rejected (`MissingModels`), so a document containing only
  `version = 1` fails startup.
- Unknown fields are rejected at the root and inside every table. Why: a
  silently ignored key would let a typo change model meaning invisibly, so
  unrecognized content fails explicitly instead.
- Parse errors are typed, sanitized values; no file content appears in error
  text. (signalboxd erases the type before logging, as described above.)

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
credential presence is never consulted (INV-008):

- **At acceptance.** `SubmitInput` freezes the requested selection into the
  turn's effective configuration. A direct selection freezes without catalog
  consultation. An alias request consults an acceptance-time definition
  resolver; an unknown alias is a recorded `UnknownModelAlias` rejection, not an
  error. The live process runtime supplies the immutable `HubModelConfiguration`
  alias catalog to the acceptance transaction; acceptance semantics are
  [turn-lifecycle-and-scheduling](turn-lifecycle-and-scheduling.md) material.
- **At execution.** When the attempt pins its target, the frozen selection is
  resolved against the `ModelTargetCatalog`. An unresolvable selection fails the
  turn as a known failure before any model call exists; a credential or send
  failure occurs only after the call exists. Why: keeping configuration absence
  distinct from provider failure, with no silent model substitution, is what
  INV-017 and INV-018 require. Lifecycle detail is
  [model-call-execution](model-call-execution.md) material.

Each accepted origin retains the selection frozen from its defaults epoch.
Replacing session defaults imposes no same-provider restriction: any selection
admitted by the immutable catalog may become the next epoch, while the current
daemon composition still configures only Anthropic targets. The first subsequent
turn resolves and pins its own provider target and credential reference at its
model-call boundary. A prepared or in-flight predecessor retains its pins. This
re-establishes credential affinity where the new defaults take effect and keeps
provider prompt-cache prefixes stable for work already in progress (INV-046).

In the provider bridge, a durably resolved target with no `RuntimeModelCatalog`
mapping is a typed adapter defect (`UnconfiguredTarget`), never provider
evidence; both catalogs derive from the one document, so this indicates a
composition bug. The debug harness additionally pre-validates its requested
selection against the catalog before creating a session.

## Credential lifecycle

The daemon-side credential contract is implemented as follows, and the
deployment-side rules that code cannot enforce are stated in
[Credential operations policy](#credential-operations-policy) below.

- **Reference/value split.** A `CredentialReference` is the non-secret name of
  one credential; a `CredentialValue` carries the secret bytes. References are
  safe in configuration, errors, logs, and durable records; values are safe only
  at the adapter boundary. Why: rotation preserves the stable name so no record
  or log ever needs the secret (INV-035). Two references exist today: the
  composition constants `anthropic-primary` and `github-primary`.
- **File-based supply, reread per preparation.** `FileCredentialAccess` binds
  each reference to its corresponding deployment path and reads the file for
  every model-call or code-host operation preparation; nothing is cached. Why:
  atomic file replacement rotates either credential without restarting
  signalboxd, and an in-flight operation keeps the value it authenticated with.
  Resolution is reference-scoped: a foreign reference fails typed `Unmapped`; a
  missing file is `Unavailable`; an unreadable file is `Unreadable` — all
  reference-only errors.
- **The value is the file's bytes less trailing line termination.** The read
  drops trailing `\n` and `\r` bytes and retains every other byte exactly,
  including leading and interior whitespace. Why: the tools that write a
  credential file — `gh auth token`, `op read`, `pass`, a shell redirect —
  terminate the line they print, so the terminator is how the file ends rather
  than part of the secret; without this, a routine deployment step produced a
  value no HTTP header could carry. The narrowing happens once, at the file
  channel, so every adapter and the redaction scrub all see the same value. A
  file holding nothing but termination narrows to an empty value, which the
  adapter boundary then refuses exactly as it already refuses an empty file;
  narrowing never invents a credential.
- **No startup preflight.** signalboxd never reads either credential file at
  boot, so a missing or unsynced credential cannot block startup or the recovery
  scan. Why: recovery of acknowledged work must not depend on any provider or
  integration credential (INV-034).
- **Resolution timing.** A model adapter resolves the durably pinned reference
  during send preparation — after the durable `Prepared` record, before send
  authorization — and scopes the resulting value to that request (INV-002
  boundary type). The shared cancellation contract for preparation and execution
  is owned by [model-call-execution](model-call-execution.md#staged-execution).
  A code-host tool resolves its fixed `github-primary` reference only after the
  durable tool attempt is authorized `InFlight` and immediately before its typed
  transport call; no model argument, client, or runner can select or receive the
  credential.
- **Failure behavior.** A failed resolution, or a value that cannot form an HTTP
  header (empty, non-UTF-8, non-header-safe bytes), is a typed known preparation
  failure: the call ends `KnownFailed`, the attempt ends with a known failure,
  the turn fails — no automatic retry, no fallback (INV-014, INV-017, INV-018).
  Why: a missing credential is deployment misconfiguration, and retry or
  substitution would hide it. A provider rejecting the credential after send is
  ordinary outcome evidence ([model-call-execution](model-call-execution.md)).
  For a code-host tool, resolution or header failure is fixed known-failure
  evidence naming the credential rather than the code host — the request never
  left the daemon; definitive code-host rejection is likewise fixed under its
  own detail, while an uncertain mutation acknowledgement follows the tool
  loop's external-effect ambiguity contract.
- **Durable references, never values.** Postgres never stores a credential
  value. Each model call durably pins its non-secret credential reference at the
  `Prepared` insert (`model_call.credential_reference`), immutable thereafter
  under the authorization-facts trigger; the column is total (`NOT NULL` and
  non-empty), because every insert writes it and no database predates the stack.
  Resuming a stored `Prepared` call re-supplies the stored reference. Tool
  attempts store neither integration references nor values: the immutable
  compiled code-host declaration selects `github-primary` again when execution
  resumes.

## Redaction and logs

The following never appear in logs, error text, or durable records: credential
values, credential file paths, `DATABASE_URL`, and raw catalog file content.
Full user content never appears in logs: every tracing site logs phase, failure
class, counts, daemon-minted aggregate identifiers, and closed classification
tokens, never conversation content (which identifiers and tokens may appear is
[identity-and-commands](identity-and-commands.md) material). For
provider-controlled evidence the guarantee is mechanism-bounded: text is
scrubbed of the exact preparation-time credential value, as described below.
Enforcement as implemented:

- `CredentialValue` implements no `Display` or serialization and its `Debug`
  form is always `[REDACTED]`; the outbound `x-api-key` header is marked
  sensitive. `FileCredentialAccess`'s `Debug` redacts its path;
  `AnthropicRuntime`'s `Debug` redacts its credential source and version header.
  The GitHub adapter marks its `Authorization` header sensitive and retains no
  credential value. Access errors carry reference and typed failure class only.
- signalboxd logging is a compact INFO tracing subscriber; startup and runtime
  errors log phase, failure class, counts, and aggregate ids only. The
  `crates/application` tracing sites emit the same typed fields, plus the closed
  tool error kind and the daemon-authored catalog tool name at the failed
  tool-attempt site; no call site in the codebase passes accepted-input,
  assistant content, tool arguments, or tool error detail to `tracing`.
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
  `apps/signalboxd/src/configuration.rs` enforce this boundary.
- Every checked string in a successful code-host result is scrubbed of the exact
  request-scoped token and its JSON-string-escaped form before the result can
  cross into tool evidence. Code-host transport failures and malformed responses
  expose only fixed details, never response bodies. INV-035-tagged tests in
  `apps/signalboxd/src/code_host/mod.rs` and
  `apps/signalboxd/tests/offline_tool_loop.rs` enforce the executor and durable
  transcript boundaries.

## Credential operations policy

Operational rules the deployment must honor; code cannot enforce them (retained
here because the surviving daemon-side mechanics depend on them):

- **One source of truth per secret.** 1Password owns runtime credentials: the
  vault item a reference resolves to is the source of truth, and rotation is an
  edit to it. sops-age-in-git owns bootstrap and deployment material (including
  the operator's own credential): the encrypted file in git is the source of
  truth, and rotation history is git history. Maintaining the same value in both
  channels is a defect. Kubernetes Secret objects are delivery artifacts of
  whichever channel produced them, never sources of truth; hand-editing one is a
  defect because the next sync overwrites it. This split governs exactly the
  provider and integration runtime credentials plus the bootstrap and deployment
  material the channels themselves depend on, not every cluster-delivered
  secret: owner-client authentication, runner enrollment, and the database
  credential are separate open decisions outside it (see Open edges).
- **Acyclic bootstrap chain.** The owner-held age identity (custodied outside
  git and outside operator sync) decrypts the sops channel; the sops channel
  delivers the operator's credential; the operator syncs the 1Password channel;
  the daemon consumes mounted artifacts. No cluster workload may reach the age
  identity through the 1Password channel.
- **Mounted-volume delivery, never environment variables.** Runtime credentials
  arrive as an operator-synced Secret mounted as a volume and read per use
  (`subPath` mounts are prohibited — they never refresh). Rotation therefore
  propagates within the operator polling interval plus the kubelet sync period,
  without a restart. The daemon deployment must explicitly set
  `operator.1password.io/auto-restart: "false"` — the operator inherits
  auto-restart from wider scopes, and a restart-per-rotation deployment
  terminalizes in-flight work as `Lost` on every rotation.
- **Optional mount; a missing Secret never gates boot.** The credential Secret
  volume is declared `optional` (or an equivalently non-gating mount) so the pod
  starts even when the Secret object is absent or a first sync has not completed
  — during a restore, a deleted Secret, or bootstrap. A required volume would
  turn a missing or unsynced credential into a boot failure and so block the
  startup recovery scan that signalboxd's no-startup-preflight behavior protects
  (INV-034); an absent credential surfaces at the effect boundary that needs it.
  The deployment likewise verifies that the operator retains last-synced Secrets
  across a manager outage, so a paused sync delays rotation propagation only,
  never startup.
- **Least-privilege Secret access.** The synced credential Secret's RBAC is
  scoped to the daemon's identity; no other cluster principal may be able to
  read it.
- **Revoke-last rotation.** Install the new value at the source of truth, wait
  out the propagation bound plus the longest expected in-flight provider call,
  then revoke the old value at the provider. Where a provider allows only one
  active key, rotation has an honest known-failure window; the mitigation is
  narrower propagation configuration, never silent retry.

## Open edges

- Selection-key retargeting across a restart is not prevented by code:
  reconstitution's `CallTargetMismatch` cross-check fails closed only for a
  session with a live stored call; for everything else, not retargeting a
  `selection_id` is deployment discipline.
- Multi-provider support and the reference-to-provider-component mapping are
  undecided; today `provider = "anthropic"` and `anthropic-primary` are
  hard-coded.
- The [credential operations policy](#credential-operations-policy) is
  operational discipline with no code or CI enforcement; violating it cannot be
  caught by any test.
- `DATABASE_URL` via process environment is explicitly provisional; the
  database-credential delivery channel remains an open decision.
- signalboxd erases typed configuration diagnostics before logging:
  catalog-parse and Anthropic-construction variants (and connection and
  migration errors) collapse to a generic `Infrastructure` class plus phase, so
  startup logs cannot distinguish failure causes within the `Configuration`
  phase.
- [Identity, credentials, and resource governance](../open-questions.md#identity-credentials-and-resource-governance)
  owns the unresolved in-memory credential-hygiene question.

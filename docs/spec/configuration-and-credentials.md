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
verified through PR #285 (`agent/dev-instance-code-host-credential`). The static
copy-on-create session-template catalog was verified through PR #311
(`agent/session-templates-spec`). The static web-fetch egress allowlist is
verified through PR #330 (`agent/audit-verified-fixes`). The opt-in telemetry
export contract is verified through PR #347 (`agent/telemetry-export`).
Invariant law lives in [docs/invariants.md](../invariants.md), cited here by
tag. The runner configuration and credential paragraphs are the foundation
proposal at the bottom of their implementing stack and become verified only with
those child pull requests.

## Process configuration

`signalboxd` reads exactly seven required deployment values from the process
environment at startup and also consults `HOME`:

- `DATABASE_URL` — complete PostgreSQL connection URL. Production connections
  force `sslmode=verify-full` regardless of URL parameters. This environment
  channel is explicitly provisional; the database-credential delivery decision
  remains open (see Open edges).
- `SIGNALBOX_CONFIG_FILE` — path to the static model/alias catalog (below).
- `SIGNALBOX_TEMPLATE_CONFIG_FILE` — path to the static session-template catalog
  (below).
- `HOME` — consulted during production database configuration validation to
  locate the default PostgreSQL password file. When a template uses a `$HOME/`
  prompt-file reference, the environment value is additionally required to be a
  nonempty absolute path; absence, an empty value, or a relative value is a
  typed template-configuration failure.
- `ANTHROPIC_API_KEY_FILE` — path to the file holding the current Anthropic API
  key value.
- `GITHUB_TOKEN_FILE` — path to the file holding the current GitHub code-host
  token value.
- `SIGNALBOX_SOCKET_PATH` — local Unix-socket path for the version-one
  [process protocol](process-protocol.md), which owns its binding and trust
  semantics.
- `SIGNALBOX_RUNNER_SOCKET_PATH` — distinct local Unix-socket path for the
  runner wire. It uses the same private-node discipline but has an independent
  lock, identity, vocabulary, and listener.

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
directory; presence alone decides and the file is never opened. Locating that
default consults `HOME` even when every template prompt is inline or
config-relative; an earlier ambient-variable refusal can end validation before
that lookup. With those closed the driver still completes an incomplete URL from
outside it — an omitted user name from the process account, an omitted host by
probing the local socket directories and then `localhost` — so the same path
refuses a URL that states either nowhere the driver reads it: the authority, or
the `user`, `host`, and `hostaddr` query parameters. Port and database name stay
with the driver and the server, which derive them from the URL alone: an omitted
port is the fixed 5432, and an omitted database name is the user name the URL
states. The refusal names the offending channel and never its contents, and it
happens before any database contact. A deployment carries every connection
parameter in the URL. The separate local test connection path is unchanged and
keeps SQLx's behavior; it is a development and test channel by intent — the
integration suites and `signalbox-debug`, which reads its own
`SIGNALBOX_DEBUG_DATABASE_URL` — and no check confines the URL it is given to a
local cluster, so the refusals above are what stand between a production cluster
and ambient configuration, not that path's name.

A missing or empty required value, an unreadable or invalid model or template
catalog, an invalid or unreadable referenced prompt file, or a failed Anthropic
or GitHub transport construction fails startup at the `Configuration` phase,
before any database contact. Startup and shutdown logs carry the phase, an
operator failure class, and small typed fields where present (session and turn
ids, recovered-turn count, grace-window seconds) — never configuration values,
paths, or URLs. The typed configuration error does not survive to the log:
`run_hub` collapses every catalog-parse and adapter-construction variant (and
likewise connection and migration errors) into a generic `Infrastructure` class
carrying only its phase, so an operator cannot distinguish an unreadable catalog
from an unknown field, bad version, or invalid limit (see Open edges). The five
deployment paths are accepted without I/O at environment parsing time; both
catalogs and every template prompt file are read during startup. Neither
credential file is read at startup (see credential lifecycle below).

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

## Telemetry export

Telemetry export is implemented but opt-in. With every setting below absent,
`signalboxd` constructs neither an OpenTelemetry provider nor a Prometheus
registry or listener: local compact tracing, startup, request handling, and
network egress are unchanged. When Signalbox OTLP is enabled, presence of any
standard general `OTEL_EXPORTER_OTLP_` or trace-specific
`OTEL_EXPORTER_OTLP_TRACES_` setting with the suffix `ENDPOINT`, `HEADERS`,
`TIMEOUT`, `PROTOCOL`, or `COMPRESSION` fails sanitized configuration without
accepting or rendering its value; the explicit process settings are the only
telemetry configuration channel. With Signalbox OTLP disabled, those unrelated
ambient settings are ignored.

Presence of `SIGNALBOX_OTLP_ENDPOINT` enables span export. The complete OTLP
surface is:

- `SIGNALBOX_OTLP_ENDPOINT` — an HTTP or HTTPS collector base URL, at most 2,048
  bytes, with a host and without user information, query, or fragment. For
  `http/protobuf`, the exporter appends `/v1/traces`; for gRPC it uses the
  configured authority. HTTPS authenticates the collector against the platform
  trust roots. The HTTP client ignores ambient proxy configuration and refuses
  redirects; the derived traces endpoint is its only route. Its absence disables
  OTLP and causes all other OTLP settings to be ignored.
- `SIGNALBOX_OTLP_PROTOCOL` — optional exact `grpc` or `http/protobuf`; omission
  selects `grpc`.
- `SIGNALBOX_OTLP_HEADERS_FILE` — optional path to a file read once at startup.
  Each line is one `name=value` collector transport header. The file is at most
  16 KiB and 16 headers; names are at most 64 ASCII alphanumeric, hyphen, dot,
  or underscore bytes and are case-normalized, values are 1 through 1,024
  printable ASCII bytes, and duplicate or malformed names fail startup. Names
  ending in `-bin` are binary gRPC metadata and remain ordinary HTTP header
  names under `http/protobuf`. Header values are sent only as OTLP transport
  metadata. They never become a span, event, resource attribute, metric, or log
  field, and errors never render the path or contents. A nonempty header file
  requires an HTTPS endpoint; a header-free local collector may use HTTP. This
  prevents collector credentials from crossing the network without transport
  protection.
- `SIGNALBOX_OTLP_SAMPLING_RATIO` — optional finite number from `0` through `1`
  inclusive; omission selects `1`. Sampling is parent-based with the configured
  trace-id ratio.
- `SIGNALBOX_OTLP_SERVICE_NAME` — optional `service.name`; omission selects
  `signalboxd`. The admitted overrides are exactly `signalboxd.development`,
  `signalboxd.staging`, and `signalboxd.production`. This closed deployment
  vocabulary cannot encode a credential, prompt, completion, or tool material.

For example, a gRPC collector configuration is:

```text
SIGNALBOX_OTLP_ENDPOINT=https://otel-collector:4317
SIGNALBOX_OTLP_PROTOCOL=grpc
SIGNALBOX_OTLP_SAMPLING_RATIO=1
SIGNALBOX_OTLP_SERVICE_NAME=signalboxd.production
SIGNALBOX_OTLP_HEADERS_FILE=/run/secrets/signalbox-otlp-headers
```

An OTLP/HTTP deployment instead sets the endpoint base, conventionally port
4318, and `SIGNALBOX_OTLP_PROTOCOL=http/protobuf`. A collector may route the
vendor-neutral OTLP stream to Tempo, Jaeger, or another tracing backend; the
daemon contains no backend-specific protocol or attribute.

Presence of `SIGNALBOX_PROMETHEUS_BIND` enables Prometheus independently. Its
value is an exact IP socket address such as `127.0.0.1:9464`; hostnames are not
resolved. The daemon binds a distinct plaintext HTTP listener, never the process
protocol socket. `GET /metrics` returns the registry, other paths return 404,
and there is no authentication or TLS. Therefore every peer that can reach the
configured address can read the metrics, and deployment network policy owns that
reachability. At most 16 connections are served concurrently; an excess
connection is dropped immediately. Each request is bounded to 8 KiB, and a
connection is abandoned after two seconds. A bind failure disables only metrics;
it does not fail request handling. An accept failure emits one static warning
per failure streak and retries after 250 milliseconds, so sustained failure is
rate-limited and a transient failure does not stop later scrapes.

```text
SIGNALBOX_PROMETHEUS_BIND=127.0.0.1:9464
```

The initial registry contains exactly three metric names:

- `signalbox_turns_started_total`, with no labels, counts durable turn
  activations. An operator graphs it as the workload-rate denominator and
  compares it with terminalization to spot work that is not closing.
- `signalbox_turns_terminalized_total{outcome}`, whose only label values are
  `completed`, `failed`, `refused`, `cancelled`, and `reconciliation_required`,
  counts durable terminal turn outcomes. It earns its place as the user-visible
  success, failure, refusal, cancellation, and owner-intervention rate.
- `signalbox_model_calls_terminalized_total{disposition}`, whose only label
  values are `completed`, `known_failed`, `refused`, `cancelled`, and
  `ambiguous`, counts durable terminal model calls. It separates provider-call
  health and refusal from ambiguity that requires recovery handling.

All label children are allocated from those closed enums at registry
construction. The metric API accepts no string, session id, turn id, model-call
id, prompt, completion, or tool value. The source is the already-committed typed
outbox transition, and content-bearing input events are ignored. The dispatcher
retains only the last observed durable sequence, so a retry of that sequence is
not counted twice and deduplication has constant memory. Metric help and type
lines are fixed strings; sample values are counters. There are no tool,
scheduler, queue-depth, or database-duration metrics in this initial surface:
the daemon-owned durable transition path can state the three metrics above
without inventing an inexact observation or instrumenting an adapter or another
crate's boundary.

The complete OTLP record inventory is:

- Span name `session_work`, with the sole `session_id` attribute, and span name
  `turn_work`, with `session_id` and `turn_id`. These are daemon-minted UUIDs.
  OpenTelemetry-generated trace id, span id, optional parent span id,
  timestamps, internal span kind, and unset status are protocol structure, not
  application values. Source location, thread fields, target, level, tracked
  busy/idle time, links, and arbitrary error conversion are disabled. The fixed
  instrumentation scope name is `signalboxd`; it has no version or schema URL. A
  per-layer export filter registers interest only in candidate Signalbox schemas
  at `DEBUG` or above, so dependency trace callsites remain disabled and do not
  evaluate their fields while non-exported records remain available to the local
  compact tracing layer.
- The sole resource attribute is `service.name`, admitted by the checked
  `SIGNALBOX_OTLP_SERVICE_NAME` grammar above and never derived from a
  credential, request, provider response, model content, or tool material. The
  resource starts empty, so host, process, environment, and SDK attributes are
  not added.
- Event name `turn activated`, with `session_id` and `turn_id`;
  `turn terminalized`, with those ids and the closed `terminal_outcome`;
  `turn parked awaiting owner reconciliation`, with those ids;
  `model call dispatched`, with `session_id`, `turn_id`, `model_call_id`, and
  `turn_attempt_id`; and the event names
  `model runtime reported a trustworthy capability-preparation failure`,
  `model call completed`, and `model call produced no assistant material`, each
  with `session_id`, `turn_id`, `model_call_id`, and the closed `cause_code`.
  Every exported event additionally has OpenTelemetry's `level` and `target`:
  level is a tracing enum, while target is one of the exact compile-time
  Signalbox module names admitted by the value-validating layer.
- `terminal_outcome` is one of `completed`, `failed`, `refused`, `cancelled`,
  `cancelled_with_tool_response`, `target_unavailable`,
  `capability_known_failure`, or `continuation_target_unavailable`. `cause_code`
  is one of the fixed tokens produced by `ModelCallCauseCode`: `completed`,
  `provider_refused`, `provider_credential_rejected`,
  `provider_permission_denied`, `provider_invalid_request`,
  `provider_target_not_found`, `provider_request_too_large`,
  `provider_rate_limited`, `provider_quota_exhausted`, `provider_overloaded`,
  `provider_internal`, `provider_unrecognized_error`,
  `provider_cancellation_confirmed`, `cancelled_before_send`, `connect_failed`,
  `send_incomplete_proven_unacceptable`, `boundary_loss_cancellation_requested`,
  `boundary_loss_timed_out`, `boundary_loss_transport_failed`,
  `boundary_loss_response_body_lost`, `boundary_loss_response_unintelligible`,
  `boundary_loss_unexpected_http_status`, `boundary_loss_stream_incomplete`,
  `boundary_loss_stream_protocol_violation`, `unsupported_operation`,
  `credential_unmapped`, `credential_unavailable`, `credential_unreadable`,
  `credential_unusable`, `provider_target_substituted`,
  `unrepresentable_tool_material`, `finish_contradicts_content`,
  `unconfigured_target`, `preparation_defect`, `correlation_mismatch`,
  `authorization_mismatch`, `observation_correlation_mismatch`,
  `unsupported_completion_material`, `invalid_assistant_text`,
  `invalid_tool_schema`, or `invalid_tool_proposal`. These are closed enum
  projections, never error messages. Admission parses the runtime-owned
  `ModelCallCauseToken` vocabulary, and the exhaustive `ModelCallCauseCode`
  projection makes a new cause require a deliberate compiler-checked token. The
  event message is the fixed event name and is not repeated as an attribute. Any
  other event name, module target, field set, malformed UUID, token value, or
  any `error` field is rejected before the OpenTelemetry layer.

Consequently no admitted dynamic value can be a credential, prompt, completion,
or tool argument: dynamic event and span values are UUIDs or exhaustive enum
tokens, the one resource value has a dedicated checked namespace, and metrics
have only preallocated closed labels. Tests capture the actual OpenTelemetry
`SpanData` and Prometheus text boundary, inject synthetic credential- and
content-shaped values into otherwise matching event fields and span identifier
fields, plus a content-bearing outbox event, and require their absence while a
valid record still exports. They also require arbitrary `error` fields to be
absent and reduce a synthetic exporter error to logged success.

Completed spans enter a dedicated batch worker through a bounded 512-span queue.
Exports are serial, at most 128 spans per batch, every five seconds or when the
batch fills, with a five-second export timeout. Queue insertion is nonblocking;
when full, the just-completed newest span is dropped and the SDK records the
overflow rather than evicting old work or waiting on the daemon. Collector and
transport errors emit one static, content-free local warning and the failed
batch is dropped; they are returned to the SDK as success and never reach a turn
or request. Shutdown waits at most one second for the provider and then logs and
abandons remaining export work. Thus an unreachable collector can consume at
most the fixed queue and one serial in-flight batch, cannot block a turn, and
cannot fail the daemon.

## Runner configuration

`signalbox-runner` accepts exactly one configuration path from either
`SIGNALBOX_RUNNER_CONFIG_FILE` or one `--config PATH` argument. Both, neither,
an empty path, another positional argument, or an unknown option fails before
opening a socket. It reads the file once at startup as strict versioned TOML;
the checked-in example is `config/signalbox-runner.example.toml`. Root version
other than `1`, an unknown field, duplicate key, wrong type, or invalid nested
entry fails with a sanitized path-free error.

The root contains:

- `daemon_socket_path`, the exact dedicated runner socket;
- `runner_root`, one absolute runner-owned directory used for enrollment state,
  result spool, `sessions/`, staging, and `trash/`;
- `bubblewrap_path`, one absolute executable path;
- `read_only_paths`, a bounded nonempty list of absolute toolchain or cache
  paths admitted read-only to `workspace-restricted`;
- `allowed_network_hosts`, a bounded subset of the fixed `github.com`,
  `crates.io`, and `api.anthropic.com` entries;
- one checked Git author name and email used by Git tools;
- `[repositories.<name>]` tables mapping checked repository keys to an exact
  credential-free GitHub HTTPS clone URL and an optional configured
  credential-profile name; and
- `[credentials.<name>]` tables mapping checked profile names to `file` and
  `injection_env` strings.

Names use the runner-protocol checked-name grammar. A version-one clone URL has
scheme `https`, exact lowercase host `github.com`, no user information, port,
query, fragment, percent encoding, empty component, or dot component, and an
exact owner/repository path with an optional terminal `.git`. Its named
credential profile, when present, must exist. Absence means that the entry
admits anonymous HTTPS access only; it never asks the runner or daemon to select
a credential. Any repository requires `github.com` in the effective network
list. Environment names use `[A-Z_][A-Z0-9_]*`, cannot name runner control,
model-provider, or dynamic-loader variables, and are unique. Absolute paths are
canonicalized without following a final credential symlink; duplicate, nested,
writable/read-only-overlapping, or runner-root-overlapping allowlist paths fail
closed. Configuration may narrow network entries but cannot add a hostname.

The shipped example contains exactly one credential entry:
`credentials.github-runner`, whose `file` names a fine-grained repository-scoped
PAT file and whose `injection_env` is `GH_TOKEN`. The parser and resolver are
otherwise name-generic: adding another credential shape is a configuration
entry, not a code branch. The runner advertises the exact configured credential
names, and each configured repository key paired with the optional profile name
its own entry carries, as availability; the daemon records no
credential-specific effect or approval policy, and the daemon may grant only a
name the current registration advertised. Reserved model-provider profile and
environment names are rejected. Because arbitrary secret bytes have no
self-describing type, file contents cannot be classified as a provider key; the
runner has no model-provider config field or daemon path that supplies one.

Startup opens or creates `runner_root` as an effective-user-owned real `0700`
directory without following its final component, retains its device/inode
identity and dirfd, takes the exclusive lock through that root, checks socket
and bubblewrap prerequisites, and loads only non-secret structure. It never
reads a credential file at startup and never logs configuration paths,
repository URLs, or values. Each lease admission checks that a granted name
exists, and each dispatch rereads and validates its file as specified under
[runner credential lifecycle](#runner-credential-lifecycle). The enrollment
request identity and daemon-issued receipt are runtime state below the root, not
operator-authored configuration.

## The static model, alias, and web-fetch catalog

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

The optional `[web_fetch]` table has exactly one `allowed_origins` array. It
contains at most 64 distinct bare HTTP(S) origins: scheme, host, and optional
port only, with no user information, path beyond `/`, query, or fragment. The
loader canonicalizes the effective port and hostname before duplicate checks. An
absent table or empty array admits no outbound `web_fetch` request. Every
request must match one configured canonical origin before dispatch, so automatic
approval cannot silently egress to an arbitrary host. Paths and queries remain
unrestricted request data at an admitted origin.

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

## The static session-template catalog

The file named by `SIGNALBOX_TEMPLATE_CONFIG_FILE` is a separate versioned TOML
document (`config/session-templates.example.toml` is the checked-in example). It
is read once at startup, after the model catalog, and never reread within a
process. Its root requires exactly `version = 1`, an optional array of
`[[templates]]` tables, and one optional `[review_library]` table; a
version-only document is a valid empty catalog. Unknown root and nested fields,
a mistyped templates or review-library value, duplicate names, and every invalid
field fail as precise sanitized `SessionTemplateConfigurationError` variants
without including file paths, prompt content, or document text.

The version-one `[review_library]` table is closed and carries exactly:

- `source_version` — the positive bundle version applied to every generated
  template;
- `concern_set_version` — the exact nonempty `ReviewKey` copied into each
  orchestration attempt;
- exactly one of `model` or `alias`, under the same canonical configured-UUID
  rules as an ordinary template;
- one required `dangerous_tool_auto_approval` Boolean shared by the library;
- one required nonempty `shared_header` string;
- the four required nonempty `import_body`, `judgment_body`, `repair_body`, and
  `publication_body` strings; and
- one `[review_library.concerns]` table containing exactly the five required
  nonempty keys `correctness`, `interface-and-type-design`, `test-quality`,
  `security`, and `documentation-code-drift`.

An absent concern table, an empty or partial five-key inventory, an extra
concern key, a missing or empty body, both or neither model choices, a
noncanonical or unknown model identity, a missing approval Boolean, or any
unknown field rejects the complete catalog. The initial concern inventory is
closed: a different set requires a later accepted catalog contract rather than
an extra version-one key.

Loading a library generates exactly nine ordinary resolved session templates:
`review-import`, `review-judgment`, `review-repair`, `review-publication`,
`review-concern-correctness`, `review-concern-interface-and-type-design`,
`review-concern-test-quality`, `review-concern-security`, and
`review-concern-documentation-code-drift`. Those names are reserved even when no
library is configured, so an ordinary `[[templates]]` entry cannot shadow one.
Each generated prompt is the exact `shared_header` bytes, two LF bytes, and the
corresponding stage or concern body bytes, in that order. Loading performs no
trimming, newline normalization, variable expansion, or interpolation. The
assembled value must satisfy the ordinary `SessionSystemPrompt` bound.

Each template table carries exactly:

- `name` — 1 through 128 ASCII bytes matching `[a-z0-9][a-z0-9._-]*`, unique in
  the document;
- `version` — a positive TOML integer bundle version, from 1 through
  9,223,372,036,854,775,807 inclusive;
- exactly one of `model` or `alias` — the canonical UUID of a direct selection
  or alias present in the already-validated model catalog;
- exactly one of `system_prompt` or `system_prompt_file`; and
- `dangerous_tool_auto_approval` — the required Boolean encoding of the complete
  `Disabled`/`ApproveAll` blanket.

An inline prompt is the exact TOML string value. A prompt-file reference is
either a relative path resolved from the template document's parent directory,
or `$HOME/` followed by a relative suffix resolved from the process's `HOME` at
load. Every component after either root must be normal and nonempty: absolute
paths, `.` or `..`, another `$`, any other variable spelling, and a missing,
empty, or non-absolute `HOME` for a `$HOME/` reference fail typed validation.
The target must be a regular file containing readable UTF-8, and its complete
contents must construct the same nonempty, U+0000-free, 1,048,576-byte-bounded
`SessionSystemPrompt` as an inline value. The loader reads at most 1,048,577
bytes even if the file changes during loading; a file already larger than the
bound is rejected before its contents are read. There is no newline trimming or
interpolation.

One valid table becomes an immutable resolved bundle containing the exact model
request, system prompt, and dangerous-tool blanket. Its content digest is
domain-separated SHA-256 over length-framed canonical values. Each frame is an
unsigned 64-bit big-endian byte length followed by that many exact bytes. The
frames, in order, are: ASCII `signalbox/session-template/content-digest/v1`; the
template version as eight unsigned big-endian bytes; ASCII `direct` or `alias`;
the selected UUID as its 16 network-order bytes; ASCII `disabled` or
`approve_all`; and the exact UTF-8 prompt bytes. The name and source form are
excluded: an inline and file-backed prompt with the same version and bundle have
the same digest, while changing any copied value or the template version changes
it. The stable vector for version 7, alias
`30000000-0000-4000-8000-000000000003`, `ApproveAll`, and prompt
`Review the change and report concrete findings.` is hexadecimal
`00c08275577e73f1565716b5c886861a0f19ea4f2c9cb9e8f93034d030b9796d`. The daemon
exposes only sorted name/version summaries to clients; clients never receive
prompt text or parse this file.

Review orchestration retains a second digest for each generated template. It is
domain-separated SHA-256 over the same unsigned-64-bit length framing. Its
frames, in order, are ASCII `signalbox/review-template/orchestration-digest/v1`;
the exact stage or concern key; the source version as eight unsigned big-endian
bytes; ASCII `direct` or `alias`; the selected UUID's 16 network-order bytes;
ASCII `disabled` or `approve_all`; SHA-256 of the exact shared-header bytes; and
SHA-256 of the exact body bytes. The key frame makes equal prompt bytes used for
different stages or concerns distinct orchestration inputs. This orchestration
digest does not replace the ordinary content digest: template provenance uses
the complete assembled prompt digest above, while the immutable orchestration
attempt uses the header/body/key-aware digest.

Creation by template name first consults the owner-global durable-command
registry by command identity. An existing create-session claim is reconstituted
and compared using the caller-supplied creation mode and template name before
the current catalog is consulted; an equal replay returns its stored session,
including when that name is absent or changed in the current catalog. Only an
unclaimed command identity resolves against this process-lifetime catalog and
copies the complete bundle into the session's immutable defaults version one.
The session separately records the template name and ordinary content digest; it
retains no live catalog reference. Generated review templates follow this same
copy-on-create path: the complete assembled prompt, model selection, approval
blanket, reserved name, and content digest become immutable session evidence. An
edit therefore requires a daemon restart and affects only creation commands
first handled under the new catalog. Equal replay of an already handled command
and template name returns the original copied session rather than comparing
against the current bundle (INV-047).

Why: a separate file lets operators change the reusable creation surface without
mixing it into immutable model-identity definitions, while one load boundary
keeps validation fail-closed. Config-relative paths make a catalog directory
portable; explicit `$HOME/` supports machine-local long prompts without
committing a machine's absolute home path. Copying preserves ordinary defaults
epoch authority and makes configuration edits forward-only.

## Model-selection validation

Validation happens at two boundaries, on frozen semantic meaning only —
credential presence is never consulted (INV-008):

- **At acceptance.** `SubmitInput` freezes the requested selection into the
  turn's effective configuration. A direct selection freezes without catalog
  consultation. An alias request consults an acceptance-time definition
  resolver; an unknown alias is a recorded `UnknownModelAlias` rejection, not an
  error. The live process runtime supplies the immutable `HubModelConfiguration`
  alias catalog to the acceptance transaction. These model-selection freeze
  semantics are this page's material; the surrounding input-delivery lifecycle
  is [turn-lifecycle-and-scheduling](turn-lifecycle-and-scheduling.md) scope.
- **At execution.** When the attempt pins its target, the frozen selection is
  resolved against the `ModelTargetCatalog`. An unresolvable selection fails the
  turn as a known failure before any model call exists; a credential or send
  failure occurs only after the call exists. Why: keeping configuration absence
  distinct from provider failure, with no silent model substitution, is what
  INV-018 requires. Lifecycle detail is
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
  the turn fails — no automatic retry, no fallback (INV-014, INV-018). Why: a
  missing credential is deployment misconfiguration, and retry or substitution
  would hide it. A provider rejecting the credential after send is ordinary
  outcome evidence ([model-call-execution](model-call-execution.md)). For a
  code-host tool, resolution or header failure is fixed known-failure evidence
  naming the credential rather than the code host — the request never left the
  daemon; definitive code-host rejection is likewise fixed under its own detail,
  while an uncertain mutation acknowledgement follows the tool loop's
  external-effect ambiguity contract.
- **Durable references, never values.** Postgres never stores a credential
  value. Each model call durably pins its non-secret credential reference at the
  `Prepared` insert (`model_call.credential_reference`), immutable thereafter
  under the authorization-facts trigger; the column is total (`NOT NULL` and
  non-empty), because every insert writes it and no database predates the stack.
  Resuming a stored `Prepared` call re-supplies the stored reference. Tool
  attempts store neither integration references nor values: the immutable
  compiled code-host declaration selects `github-primary` again when execution
  resumes.

## Runner credential lifecycle

Runner credential profiles are non-secret checked names granted by the daemon
and resolved only by `signalbox-runner`. The daemon, client, database,
transcript, workspace manifest, and runner wire never receive a runner
credential path or value (INV-035, INV-045).

A session may hold no credential at all, and no boundary infers one. When the
placement selected no profile the daemon issues no grant, the lease carries no
credential dispatch authorization, and the runner resolves no path and injects
no value for that session's dispatches. A repository entry with no profile then
uses anonymous HTTPS, while an entry that names a profile fails with the typed
`credential_unavailable` class rather than resolving a profile the placement
never selected. Conversely, a named profile is granted to a session with no
repository and no workspace, because the credential is scoped to that session's
dispatches rather than to a clone
([runner protocol and placement](runner-protocol.md#session-composition)).

At lease admission the runner requires the exact granted name in its startup
configuration; absence rejects the claim before any executable capability is
issued. Immediately before each provisioning or tool dispatch, it opens the
configured path without following symlinks, requires a regular file owned by the
effective user with exact `0600` mode, reads at most 65,536 bytes, and drops
trailing `\n` and `\r` bytes while retaining all others. Empty, containing a NUL
byte, unreadable, oversized, wrong-owner, wrong-mode, or
replaced-with-nonregular files are typed unavailable failures. The value is
scoped to that dispatch and never cached, so atomic file replacement rotates it
between operations.

The value is supplied only under the configured environment name inside the
bubblewrap namespace. It does not appear in command arguments, Git remote URLs,
Git configuration, inherited host environment, error details, or logs. Git tools
use a fixed runner-owned credential helper inside the namespace, and its
authorization is bound to the repository entry that dispatch resolved rather
than to the session's provisioned workspace. For a Git tool operating on an
existing worktree that entry is the repository key the workspace manifest
records; for `git_clone` it is the checked `repository` argument, whose
configured entry the runner resolves before the invocation. The helper returns
the selected value only when the query's protocol, exact `github.com` host, and
owner/repository path all match that entry's configuration-validated canonical
URL and that entry names exactly the granted credential profile; every other
query returns no credential. Every guarded Git command that installs this helper
also forces `credential.useHttpPath=true` on the same command line as the
transport and helper configuration. Why: Git otherwise defaults that setting to
false and strips the owner/repository path before calling the helper, so the
required exact-path check would reject every authenticated clone, fetch, and
push; preserving the path is the authorization boundary, not a convenience.
Binding the helper to the provisioned workspace would also leave the operation
that introduces a session's first repository unauthorizable, because a clone
runs in a writable root whose manifest names no repository key at all
([runner protocol and placement](runner-protocol.md#workspace-provisioning-and-recovery)).
The runner scrubs the exact value and its JSON-string-escaped form from admitted
stdout, stderr, and result text before forwarding. This reduces accidental echo;
it cannot prevent model-controlled code from transforming or using the value
within its granted repository scope, which is an accepted restricted-profile
cost.

Unknown profiles fail before lease claim. A credential failure after a claimed
dispatch is a fixed `ExecutionFailed` observation naming only the profile and
failure class. A transport or supervisor loss remains effect-class ambiguous;
credential failure never authorizes an automatic repeat of side-effecting work.
Model-provider credentials are daemon-only and cannot be granted or injected to
a runner. Explicit `ambient` nevertheless retains same-user filesystem powers
and therefore does not promise those files are unreadable; that access is
outside the credential-grant channel.

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
  `crates/tools-code-host/src/code_host/mod.rs` and
  `apps/signalboxd/tests/offline_tool_loop.rs` enforce the executor and durable
  transcript boundaries.

## Credential operations policy

Operational rules the daemon deployment must honor; code cannot enforce them
(retained here because the surviving daemon-side mechanics depend on them).
Runner credential files instead follow the same-host `0600` contract above and
are outside this cluster-delivery policy:

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

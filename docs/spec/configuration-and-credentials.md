# Configuration and credentials

The blob catalog and input-modality grammar below are proposed and not
implemented.

This page describes the implemented configuration and credential behavior of
Signalbox. This includes signalboxd configuration loading in
`apps/signalboxd/src/configuration.rs` and `apps/signalboxd/src/main.rs`, the
static TOML catalog, and the provider bridge in `crates/model-provider-runtime`,
together with the model-runtime crates it composes
(`crates/model-runtime/src/credential.rs` and the redaction pipeline in
`crates/model-runtime-anthropic/src/runtime.rs`), and the database-channel
refusals in [process configuration](#process-configuration) under
`production_connection_options` in `crates/persistence/src/lib.rs`. Invariant
law lives in [docs/invariants.md](../invariants.md), cited here by tag. The
runner credential use during provisioning or execution remains committed
unimplemented functionality as labeled below. The credential-profile and
credential-pool grammar, its fail-closed admission, the deliveries this build
supplies, the fail-closed rejection of reserved Codex deliveries, and the
operator-chosen model-provider profile names are implemented in
`apps/signalboxd/src/credential_pools.rs` and
`apps/signalboxd/src/configuration.rs`. Preparation-time pool selection, durable
trigger actions and chain exclusions, the availability successor calls owned by
[the credential-availability machine](credential-availability.md), together with
durable per-call pool-policy snapshots, are implemented. Codex `codex_home`
admission and the per-member `CODEX_HOME` the selected profile delivers to each
Codex CLI child are implemented in `apps/signalboxd/src/credential_pools.rs` and
`crates/model-runtime-codex-cli/src/runtime.rs`. Codex `file` and `oauth`,
capacity reservations, and legacy family-to-reference migration remain committed
unimplemented functionality as labeled below. Every other paragraph on this page
describes implemented behavior.

## Process configuration

`signalboxd` reads six unconditionally required deployment values, the optional
runner-socket override, and the two optional browser HTTP values below from the
process environment at startup, and also consults `HOME`. Model-provider
credential paths are not among them: this build composes `FileCredentialAccess`
from the profile catalog, so those paths come only from each `file` profile's
delivery configuration in the static catalog below, on the same pattern
`[credentials.<name>]` already uses for the runner. `ANTHROPIC_API_KEY_FILE` and
`OPENAI_API_KEY_FILE` are not read and supplying them has no effect. Why this
direction: one environment variable cannot name the paths of several accounts,
and a deployment holding two keys for one provider must be able to say so.

The complete set of unconditional process settings, including the two
integration credentials of which there is exactly one each, is below. This list
is the complete set of deployment values the daemon reads from the environment;
`PATH`, `RUST_LOG`, and the telemetry variables are read for their own purposes
and are stated where each is owned.

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
- `BRAVE_API_KEY_FILE` — path to the file holding the current Brave Search API
  key value used by the daemon-composed `web_search` tool.
- `GITHUB_TOKEN_FILE` — path to the file holding the current token shared by the
  GitHub-backed code-host and pull-request tool adapters.
- `SIGNALBOX_SOCKET_PATH` — local Unix-socket path for the version-one
  [process protocol](process-protocol.md), which owns its binding and trust
  semantics.
- `SIGNALBOX_RUNNER_SOCKET_PATH` — optional distinct local Unix-socket path for
  the runner wire. When absent, signalboxd replaces the process socket's final
  extension with `.runner.sock`. The two canonical public paths and their
  adjacent `.lock` and `.identity` artifacts must be disjoint; any intersection,
  including one reached through parent-directory aliases, is a typed
  configuration failure. Otherwise the runner socket uses the same private-node
  discipline but has an independent lock, identity, vocabulary, and listener.
- `SIGNALBOX_WEB_BIND` — optional browser HTTP socket address. Absence binds
  `127.0.0.1:37231`, keeping the listener on loopback; an explicit socket must
  also use a loopback address because this browser surface has no application
  authentication. A non-loopback value fails configuration. A valid loopback
  socket address is the deployment's opt-in override. An invalid or non-Unicode
  value fails the `Configuration` phase without logging the value.
- `SIGNALBOX_WEB_ASSET_ROOT` — optional path to a static production web build.
  An explicitly empty path fails the `Configuration` phase. When absent, non-API
  paths return `404 Not Found`; when present, the daemon serves files from that
  root and uses its `index.html` for client-side routes.

### Browser HTTP listener and generated contract

The browser application and `/api/**` share the configured listener and origin.
API routing takes precedence over static files: an unknown `/api/**` path
returns a structured API `404` and never the web application's `index.html`. The
daemon does not emit permissive CORS headers and adds no account, login,
bearer-token, application-session, TLS, proxy, VPN, or ingress machinery. The
listener therefore rejects non-loopback binds. Unauthenticated session reads —
the session catalog, session descriptor, session timeline, live snapshot and
follow stream, bounded lexical search, bounded usage summary and usage-call
detail, operator attention snapshot and its follow stream, and the blob
descriptor and content routes — additionally require a loopback `Host`
authority: `localhost` or an IPv4 or IPv6 loopback address, with an optional
port. Another authority receives a structured `403 Forbidden` transport error
with code `non_loopback_host_rejected` before session data, search results,
usage and cost results, blob metadata, or blob bytes are read, and before a
descriptor read may start image derivation work.

`GET /api/bootstrap` describes the production browser contract. It returns the
exact contract family `signalbox.web-http`, version `2`, the `bounded_json`,
`same_origin_json_mutations`, `ndjson_streaming`, `import_discovery`,
`imported_continuations`, and `bounded_session_timeline` capabilities, the
`bounded_session_timeline_detail` and `bounded_session_live` capabilities, the
32-turn live queue-preview ceiling, the `immutable_blob_content`,
`blob_derivations`, and `image_derivatives` capabilities, the
`bounded_lexical_search` and `bounded_usage_cost` capabilities, the effective
65,536-byte JSON-body and NDJSON-item hard ceilings, the 256-item and
65,536-projected-byte timeline ceilings, the 128-item and
65,536-projected-body-byte timeline-detail ceilings, and the 512-byte query,
100-item page, and 512-byte snippet search ceilings. It also advertises the
256-group usage summary and 100-call usage-detail ceilings. Version two adds the
bounded import DTOs and routes owned by
[conversation import](conversation-import.md). The generated browser decoder
rejects an unknown field, wrong shape, different family, or different version
rather than interpreting it as the local process protocol. No process-protocol
frame is a browser DTO. The descriptor, historical-window, and lexical-search
route shapes and semantics are owned by
[Sessions and the transcript](sessions-and-transcript.md) and its
[sessions and transcript](sessions-and-transcript.md). The open-workspace live
snapshot, follow route, and resynchronization semantics are owned by
[sessions and transcript](sessions-and-transcript.md). The descriptor, content,
and download routes beneath `/api/blobs/{digest}` are the same-origin surface
owned by [blob storage](blob-storage.md).

`GET /api/attention` returns at most 32 session summaries from one read-only
repeatable-read snapshot, ordered by session identity. A continuation names the
last session identity and opens the next keyset page; it is not a count-based or
fixed-tail feed. Each summary carries the current turn classification, the
session's lifecycle state, exact operator action when one is owed, a typed
blocked-goal reason and a need summary of at most 128 Unicode scalar values,
approval-judge outcome counts, and the last publication-timestamped durable
activity fact. Exact blocked-goal need text remains available from the session
detail read rather than entering the attention summary.

Runner loss, model-call recovery ambiguity, tool recovery, reconciliation,
approval wait, blocked goal, parked, active, queued, and idle remain distinct
states. Tool recovery carries no reconciliation action because no current
command writes that wait. The projection uses one set query over the selected
identities and never constructs the fleet by following individual sessions.

`GET /api/attention/follow` begins with the first coherent attention page and
its durable change-journal cursor, then emits summary replacements only for
changed session identities. One incremental read examines at most 32 journal
records. A larger cursor gap emits `resync_required` with the current cursor and
ends that stream; it never skips records or continues from a partial gap. The
HTTP producer retains only the item currently being encoded and waits between
empty polls. An initial projection failure returns a typed HTTP error before
streaming begins. The append-only change journal timestamps commits explicitly;
historical creation is seeded only from the durable command claim time and never
inferred from UUID bits.

Five read-only repository-watch routes expose the durable operator projection:
`GET /api/repository-watch/repositories`, `pull-requests`, `work`, `sessions`,
and `activity`. They are read-only projections of the records
[repository watch](repo-watch.md) describes. The activity route exposes
independently selectable event and webhook cursors; an excluded feed cannot
carry a cursor.

Rust serde DTOs and their schemars schemas under `crates/web-contract` are the
authority. The checked-in `web-contract.mjs` runtime decoders and
`web-contract.d.mts` TypeScript declarations under `clients/web/src/generated`
are generated from that authority. A workspace test fails when either generated
file or the generated round-trip fixture drifts, and CI executes the generated
decoder against the Rust-produced fixture.

The transport foundation admits ordinary JSON bodies only through a 65,536-byte
buffering ceiling. Browser mutation routes use `POST`, require
`application/json`, and, when the browser supplies `Origin`, require its HTTP(S)
host and effective port to equal the request `Host` authority. Because the
listener is plain HTTP, a `Host` without an explicit port has effective port 80;
the daemon never derives that port from the supplied origin. A missing origin is
admitted for non-browser and same-origin clients; an invalid, opaque,
missing-authority, or cross-origin pair receives a structured transport error.
Only crossing the buffering ceiling produces `413 Payload Too Large`; another
failure while reading the request body produces a distinct `400 Bad Request`
transport error. Application errors occupy a separate error kind and are not
inferred from HTTP status alone.

Incremental responses are `application/x-ndjson`: each item is serialized only
when the response body polls it, carries one trailing newline, and is at most
65,536 bytes before that newline. The source is a caller-supplied stream, so its
own bounded channel supplies backpressure; dropping the browser response drops
that stream and closes its receiver, cancelling a blocked producer. Static files
use ordinary HTTP bodies rather than JSON wrapping.

### Bounded browser usage and cost reads

The bounded `/api/usage/summary` and newest-first `/api/usage/calls` routes read
the usage projection [model-call-execution](model-call-execution.md) owns; their
response shapes, bounds, and cost labels are the web contract under
`crates/web-contract`, and the daemon's web HTTP layer parses their filters and
cursors.

`deterministic_test_router` supplies a database-free page plus bounded read,
mutation, and two-item stream routes. It composes the same bootstrap, mutation
guard, bounded decoder, DTOs, and NDJSON encoder, but the production daemon does
not mount its `/api/test/**` paths.

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
parameter in the URL. The separate local test connection path keeps SQLx's
behavior; it is a development and test channel by intent — the integration
suites and `signalbox-debug`, which reads its own `SIGNALBOX_DEBUG_DATABASE_URL`
— and no check confines the URL it is given to a local cluster, so the refusals
above, not that path's name, are what keep ambient configuration away from a
production cluster.

A missing or empty required value, an unreadable or invalid model or template
catalog, an invalid or unreadable referenced prompt file, or a failed Anthropic,
OpenAI, or GitHub transport construction fails startup at the `Configuration`
phase, before any database contact. A present invalid static tool mapping fails
during that same pre-database configuration pass. After the database connects,
an invalid configured workspace root or any failed tool-suite construction also
fails at the `Configuration` phase. A derived per-session root is composed on
first use rather than at startup, so its failures are per-session tool failures
described under [the mapping registry](#derived-session-workspace-roots). All
tool dependencies are supplied by parsed configuration, the already-constructed
database pool, or explicit credential and transport values; no tool family
discovers ambient authority. Startup and shutdown logs carry the phase, an
operator failure class, and small typed fields where present (session and turn
ids, recovered-turn count, grace-window seconds) — never configuration values,
paths, or URLs. The typed configuration error does not survive to the log:
`run_hub` collapses every catalog-parse and adapter-construction variant (and
likewise connection errors) into a generic `Infrastructure` class carrying only
its phase, so an operator cannot distinguish an unreadable catalog from an
unknown field, bad version, or invalid limit (see Open edges). A failed
migration is the one exception. It carries the same generic class and phase and
additionally records the database's own rejection text in a structured field,
because the phase alone cannot separate a rejected constraint from an
unreachable database, and that text names schema objects rather than
configuration. The six unconditional deployment paths are accepted without I/O
at environment parsing time; both catalogs and every template prompt file are
read during startup. Provider and integration credential files remain lazy. A
currently routed S3 blob store is the sole static-file exception: after database
connection and the configuration-independent recovery scan, startup reads that
explicit credential to perform the marker and lifecycle checks owned by
[blob storage](blob-storage.md), before socket admission or scheduling.

The deployed daemon supplies no Anthropic or OpenAI endpoint or timeout setting;
it constructs each adapter with its defaults. The
[runtime-substrate](runtime-substrate.md) page owns those transport defaults,
positive caller-level exchange-timeout overrides, and the whole-exchange bound.
Startup ordering, recovery scanning, and shutdown policy are
[turn-lifecycle-and-scheduling](turn-lifecycle-and-scheduling.md) scope;
migration behavior is [persistence-protocol](persistence-protocol.md) scope, and
the socket boundary and single-daemon guard are
[process-protocol](process-protocol.md) material.

The local `signalbox-debug` harness reads `SIGNALBOX_DEBUG_DATABASE_URL` and
`SIGNALBOX_CONFIG_FILE` in its `--anthropic` mode, taking the Anthropic key path
from the configured profile exactly as the daemon does. It does not compose the
daemon tool catalog and does not read `GITHUB_TOKEN_FILE`; it is a development
driver, not the client protocol.

## Configuration reload

**Committed unimplemented functionality.** No present surface re-reads a
configuration file after startup. Reload is one admin verb,
[`reload_configuration`](process-protocol.md#planned): it re-reads the
configured paths, validates the complete replacement exactly as startup does,
and swaps the in-memory catalogs atomically on success. Any failure, and any
replacement whose startup-only sections differ, leaves the running configuration
in place. File watching and polling are external tooling that calls that verb.

The reloadable sections are the model and alias catalog with its rate windows,
the session-template catalog, and the repository-watch configuration, whose
reload transaction is owed to [repo-watch](repo-watch.md); every other section
is startup-only.

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

The registry contains exactly nine metric names:

- `signalbox_turns_started_total`, with no labels, counts durable turn
  activations. An operator graphs it as the workload-rate denominator and
  compares it with terminalization to find work that is not terminalizing.
- `signalbox_turns_terminalized_total{outcome}`, whose only label values are
  `completed`, `failed`, `refused`, `cancelled`, and `reconciliation_required`,
  counts durable terminal turn outcomes. It is the user-visible success,
  failure, refusal, cancellation, and user-intervention rate.
- `signalbox_model_calls_terminalized_total{disposition}`, whose only label
  values are `completed`, `known_failed`, `refused`, `cancelled`, and
  `ambiguous`, counts durable terminal model calls. It separates provider-call
  health and refusal from ambiguity that requires recovery handling.
- `signalbox_session_lifecycle_rate_parts_per_million{metric}` publishes each
  latest complete weekly rate under the closed labels
  `session_completion_failure_rate`, `failed_unknown_share`,
  `overflow_incidence`, `finish_given_overflow`, `wall_rate`,
  `turn_cause_completeness`, and `model_call_cause_completeness`.
- `signalbox_sessions_nonterminal_past_deadline` is the current count of owned
  non-terminal sessions past their armed deadline obligation.
- `signalbox_session_lifecycle_export_fresh` is one after a successful
  configured refresh and zero after a failed refresh.
- `signalbox_scheduler_passes_in_flight`, with no labels, is the current count
  of authoritative scheduler passes holding admission slots.
- `signalbox_scheduler_oldest_in_flight_pass_age_seconds`, with no labels, is
  the scrape-time age of the oldest admitted pass, or zero while idle.
- `signalbox_scheduler_oldest_in_flight_pass_info{session_id}` identifies that
  oldest pass by its daemon-minted session UUID. It has zero or one series and
  removes the prior series whenever the oldest pass changes or the loop becomes
  idle.

Counter label children are allocated from closed enums at registry construction.
The only free-form metric label is the scheduler information gauge's
daemon-minted `session_id`; no turn id, model-call id, prompt, completion, or
tool value is accepted. The durable counters use already-committed typed outbox
transitions, and content-bearing input events are ignored. The dispatcher
retains only the last observed durable sequence, so a retry of that sequence is
not counted twice and deduplication has constant memory. Metric help and type
lines are fixed strings. There are no tool, queue-depth, or database-duration
metrics in this surface.

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
  `turn parked awaiting bounded reconciliation`, with those ids;
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
entry, not a runner code branch. The runner advertises the exact configured
credential names, and each configured repository key paired with the optional
profile name its own entry carries, as availability. The registration-only
daemon admission follows the
[advertised catalog and daemon authority](runner-protocol.md); another
otherwise-valid configured profile is rejected until that authority admits it.
Reserved model-provider profile and environment names are rejected. Because
arbitrary secret bytes have no self-describing type, file contents cannot be
classified as a provider key; the runner has no model-provider config field or
daemon path that supplies one.

Startup opens or creates `runner_root` as an effective-user-owned real `0700`
directory without following its final component, retains its identity, takes the
exclusive lock through that root, checks socket and bubblewrap prerequisites,
and loads only non-secret structure. The bubblewrap path must resolve to an
executable regular file. Git author fields reject empty or padded text, control
characters, and angle-bracket delimiters. An existing credential path whose
final component is a symlink fails startup before containment checks; a missing
credential path remains admissible. Startup never reads credential bytes and
never logs configuration paths, repository URLs, or values. The enrollment
request identity and daemon-issued receipt are atomically fsynced runtime state
below the root, not operator-authored configuration.

## The static model, alias, and web-fetch catalog

The file named by `SIGNALBOX_CONFIG_FILE` is a versioned TOML document
(`config/signalboxd.example.toml` is the checked-in example). Parsing is
fail-closed:

- The root must carry `version = 1`; any other or absent version is rejected. A
  document naming `credential_profile`, or declaring profiles without `adapter`
  and `delivery`, is rejected at startup rather than migrated. Why not a version
  2: a version discriminator is needed only when two shapes must be accepted at
  once, and nothing here needs that. The catalog is a deployment-owned file with
  no in-place upgrade path and a single operator who edits it; a second decoder
  would preserve a shape no deployment is entitled to keep using. The rejection
  is typed — `UnknownField` for an unrecognized key, `InvalidField` for a
  missing or mistyped one — but neither variant carries the offending field's
  name, so the operator's guide to the edit is `config/signalboxd.example.toml`
  rather than the error: that file declares `adapter` and `delivery` on every
  profile and maps each family through a `[[credential_pools]]` entry.

- The `[numeric_bounds]` table is required and contains every deployment-owned
  numeric policy listed in `config/signalboxd.example.toml`. Integer policies
  use nonnegative TOML integers and duration policies use an unsigned integer
  followed by `ms` or `s`. Every field also accepts the single exact string
  `"none"` for an unbounded deployment policy. Missing fields are one typed
  startup failure whose sanitized message lists every absent field in schema
  order; mistyped values, alternate spellings of `"none"`, and unknown fields
  fail startup. The loader supplies no default for any member of this table.

- At least one `[[models]]` entry is required: an absent, mistyped, or empty
  models array is rejected (`MissingModels`), so a document containing only
  `version = 1` fails startup.

- At least one `[[adapter_mappings]]` entry is required. Each entry gives one
  exact `model_family`, the build-provided `adapter`, and the non-secret
  `credential_pool` whose members may authenticate that family. Those three are
  the whole of the implemented entry: the workspace-instruction capability
  fields are specified only in their committed-unimplemented block below, and an
  operator writing them here receives an unknown-field startup failure. The pool
  must name one declared `[[credential_pools]]` entry, and every member of that
  pool must carry the mapping's adapter. Duplicate families, an adapter this
  daemon build does not provide, an undeclared pool, and an adapter disagreement
  between a mapping and its pool are typed startup failures. Nothing is inferred
  from model spelling.

- At least one `[[credential_profiles]]` entry is required. Each exact `name`
  carries the build-provided `adapter` it authenticates, one closed
  `billing_kind` (`api_metered` or `subscription`), and one closed `delivery`
  whose own fields [credential deliveries](#credential-deliveries) owns. The
  name is 1 through 256 UTF-8 bytes, unpadded, and NUL-free. Duplicate names,
  unknown adapters, unknown kinds, an unknown delivery, a delivery its adapter
  does not admit, and unknown fields are rejected. Where a delivery determines
  the authentication kind, a disagreeing `billing_kind` is rejected too: `file`
  authenticates with a provider API key, so it admits only `api_metered`, and
  `oauth` constructs a subscription login, so it admits only `subscription`.
  `ambient` and `codex_home` name a login the operator established and admit
  either, because the daemon cannot tell which one it is. The refusal names the
  profile and both disagreeing spellings, and it is taken before the undelivered
  decision, so a reserved delivery's contradiction is refused as a contradiction
  rather than masked by the refusal that follows it. Why reject rather than
  infer: the field is what terminal cost derivation trusts to choose between a
  real charge and a metered equivalent, so an accepted contradiction silently
  misreports spend, and inferring it would overwrite an operator's statement
  about the two deliveries where the answer genuinely varies. Parsing opens no
  credential-profile path and contacts no provider, so every admitted profile
  credential stays lazy, matching the no-preflight rule below. Billing kind
  belongs to authentication, not to the adapter a mapping selects. A profile
  name is otherwise opaque to code: no build-provided constant is compared
  against it, so a deployment names its accounts as it chooses.

- At least one `[[credential_pools]]` entry is required.
  [Credential pools and selection](#credential-pools-and-selection) owns its
  complete grammar and admission rules.

- Unknown fields are rejected at the root and inside every table. Why: a
  silently ignored key would let a typo change model meaning invisibly, so
  unrecognized content fails explicitly instead.

- Parse errors are typed, sanitized values; no file content appears in error
  text. (signalboxd erases the type before logging, as described above.)

The required `numeric_bounds.scheduler_pass_admission_cap` policy bounds
concurrent authoritative per-session passes, not the durable queue: excess
eligible sessions remain recorded and are admitted as passes finish. Zero pauses
authoritative session execution while the scheduler task and the daemon's
ingestion and process services remain live; `"none"` admits every currently
eligible session. A `[scheduler]` table is an unknown root field.

The required finite, positive `numeric_bounds.codex_cli_version_probe_bound`
policy bounds the credential-free startup probe that asks a configured Codex CLI
executable for its version. A missing, malformed, unbounded, zero, unsuccessful,
or mismatched probe fails configuration before the socket opens; the executable
must report the exact version compiled into the adapter from the installation
manifest.

The required `numeric_bounds.fenced_pool_min_connections` policy controls how
many fenced PostgreSQL sessions are established before daemon work begins;
`"none"` preserves SQLx's zero-session floor. A finite value above the daemon's
compiled pool ceiling is rejected during configuration rather than silently
clamped. A positive floor also requires finite, positive
`fenced_pool_floor_reconciliation_interval` and
`fenced_pool_floor_reconciliation_attempt_bound` policies. The runtime
periodically observes sessions retired after startup without consuming any idle
service capacity. Once ordinary demand has consumed the idle inventory, one
bounded attempt adds one missing physical session and returns it; failed,
timed-out, or concurrently invalidated attempts retry after the configured
interval. A zero or `"none"` floor disables that reconciliation.

The optional `[model_settings]` table supplies the deployment-global settings
overlay. Each `[[model_settings_profiles]]` entry gives an exact unique `name`
and an overlay that a selectable model may name with `settings_profile`. Both
overlay forms admit `reasoning_level`, `fast_mode`, and `service_tier` only.
Omission means inherit; reasoning and service tier also accept
`provider_default`, while fast mode accepts `enabled` or `disabled`. A service
tier is a provider-tagged inline table. Duplicate profile names, unknown profile
references, malformed values, or a configured lower-layer value outside the
selected model's capabilities fail startup. A lower-layer combination that the
selected adapter cannot enforce also fails startup, including a global
combination masked by the selected profile. The precedence and durable
provenance of these layers are owned by
[Model and session settings](model-session-settings.md).

### Workspace-instruction roots

The optional `[workspace_instructions]` table owns the explicitly registered
daemon directories used by
[workspace-instruction discovery](workspace-instructions.md). Its exact
version-one grammar is:

```toml
[workspace_instructions]
version = 1
registered_roots = ["/absolute/canonical/path"]
```

An absent table means no configured roots. When present, `version` and
`registered_roots` are required and no other field is admitted. The array has at
most 64 distinct strings. Each string must be a nonempty, absolute, lexically
canonical UTF-8 path of at most 4,096 bytes with no NUL: it begins with `/`,
followed by one or more nonempty components separated by single `/` characters,
and no component is `.` or `..`. Thus the filesystem root itself and a trailing
separator are not admitted. Equal canonical strings are duplicates and fail
startup. A wrong version, wrong type, relative or noncanonical path, duplicate,
excess entry, and unknown field are typed configuration failures.

Configuration validation does not require a registered root to exist or be
readable. Discovery reports a typed root-unavailable finding instead, so an
operator can provision the path after validating the static file without an
unavailable directory being reported as an empty successful scan. The catalog is
read once at daemon startup; changing it requires a restart and never rewrites
an earlier discovery snapshot.

**Committed unimplemented functionality — configured-root identities.** A
configured root carries two distinct identities, because one value cannot serve
both purposes without leaking.

`ConfiguredInstructionRootId` is the template-selector identity. Its value is
SHA-256 over literal UTF-8 `signalbox-configured-instruction-root-v1`, followed
by the canonical path as an unsigned 64-bit big-endian byte length and that many
exact UTF-8 bytes, displayed as 64 lowercase hexadecimal characters. Deriving it
from the path is what lets a template distinguish two configured roots with the
same root-relative bundle path without placing an absolute daemon path in the
selector, and what keeps a template's content digest reproducible from
configuration alone. It is daemon- and template-side only and never reaches a
model or a provider.

The provider-safe root reference is the identity a model may see, and it is
therefore not derived from the path. A public unkeyed path hash is not
provider-safe at all: a reader who guesses a conventional home or checkout
directory can hash candidates and compare them against the reference exposed by
`instructions_list` and every configured-root wrapper, recovering usernames and
repository layout the reference was supposed to withhold. It is therefore
operator-assigned. `[workspace_instructions]` is extended so that an entry of
`registered_roots` may be written as a table with exactly `path`, validated as
the string form is, and `provider_reference`: exactly 64 lowercase hexadecimal
characters naming 32 opaque bytes, which the operator generates randomly once
and then keeps stable, since provider-visible ordering and rendered wrapper
bytes depend on it. Startup rejects a missing reference, a duplicate across
roots, and one equal to any root's `ConfiguredInstructionRootId`, which would
reintroduce the derivation it exists to avoid. Randomness is not verifiable, so
those rejections catch the distinguishable mistakes and the grammar states the
requirement plainly for the rest.

Those checks compare values within one configuration, which is not enough on its
own: the same path restarted with a different reference keeps its path-derived
`ConfiguredInstructionRootId` while durable registrations and their alias
records still hold the old reference, so reuse would either leave two aliases
for one selector identity or silently swap the value that determines catalog
order, wrapper bytes, eligibility hashes, and scopes. The daemon therefore
persists the association from each root's `ConfiguredInstructionRootId` to its
provider-safe reference, and startup rejects a configuration presenting a known
root with a different reference before discovery or registration reuse runs. A
reference is stable for the life of a root's stored evidence; changing one means
retiring that root's registrations, not editing a value the durable rows already
name.

The association is a reservation in both directions, and for as long as any
stored evidence names it. A reference retired with its root is not free for
another: were root A removed and its former reference later assigned to root B,
the within-configuration duplicate check would pass and so would the forward
association, since B has its own `ConfiguredInstructionRootId` — while durable
aliases and authority-qualified eligibility entries written for A still carry
that reference. Authority-qualified pairs would become ambiguous, and
root-removal revalidation would read B's live reference as authority to reread
A's path. Startup therefore rejects a reference that retained evidence still
names when it is offered for any root other than the one owning the persisted
association, and the reservation is released only when the last of that evidence
is gone. The exception for the owning root is not a weakening: re-presenting its
own reference is what an ordinary restart does, and is exactly the stability the
rule above demands, so rejecting it would stop configured-root discovery from
surviving its first restart. What the reservation forbids is a reference moving
to a different root while evidence written under the first still names it. No
present parser admits the table form; only the bare-string form above is
accepted, and a root without a reference cannot become provider-visible.

No present configuration, template, or runtime surface materializes either
identity.

Each `[[models]]` record declares its capability surface with
`reasoning_levels`, `fast_mode`, `service_tiers`, and `input_modalities`.
`input_modalities` is a nonempty array from the closed set `text`, `image`, and
`document`; it rejects duplicates and must contain `text`. Omission means
exactly `["text"]`. Every `[[serving_targets]]` record admits the same member
and default. The model-capability process projection always materializes the
selectable record's modalities in the closed order `text`, `image`, `document`,
and call preparation uses the effective serving record's set. Omitted reasoning
and service-tier arrays are empty, and omitted fast mode means `unsupported`.
`request_control` authorizes the adapter's request-level fast control.
`alternate_target` additionally requires `fast_target_id`; that identity must
name a non-selectable `[[serving_targets]]` record with its own exact
`model_family`, provider model, `max_output_tokens`, and
`context_window_tokens`. Every serving record states its family, and that family
must name one declared `[[adapter_mappings]]` entry; the mapping, not the
selectable record naming the target, supplies the serving record's adapter and
credential pool, so nothing is inferred from the pointing model. At preparation
the enabled call resolves that family's pinned reference from the session's
credential history and pins it on the call exactly like any other resolved
target; preparation then selects from that serving family's admitted pool
exactly as it does for any other resolved target. A serving record omitting
`model_family`, or naming an unmapped one, is a typed startup failure. Startup
rejects a missing, selectable, cross-adapter, or otherwise conflicting alternate
target. An enabled call uses that serving record's provider identity and
output-token request limit, while the client's durable selection remains
unchanged. Capability values are validated against the selected adapter's
explicit mapping table during startup, so an adapter cannot silently drop a
configured setting. Input guarding, output reservation, and post-response usage
enforcement use the effective serving record's limits for that enabled call
rather than the selectable source record's limits.

**Committed unimplemented functionality — workspace-instruction capability.**
Every `[[models]]` and `[[serving_targets]]` record is extended with an
all-or-none pair: `workspace_instruction_transport = "typed_system"` and
`workspace_instruction_capacity_bytes`, a positive `u32` measured over the exact
serialized `WorkspaceInstructionRegion` bytes. Omission of both means
unsupported; supplying only one, another transport spelling, or a capacity below
the fixed 65,536-byte version-one region ceiling is a typed startup failure. The
effective serving record, including an alternate fast target, is authoritative
for a call. Its adapter mapping must declare support for the same typed-system
transport and at least that byte capacity, or startup rejects the configuration.
Context-window tokens are an independent limit and are never converted into this
byte value. Each `[[adapter_mappings]]` entry accepts those same two optional
all-or-none fields in addition to its three implemented keys. The mapping's
capacity is the adapter implementation's maximum exact serialized region bytes
for that family; it must be at least 65,536 and at least every model or serving
target in the family. A mapping omitting the pair can map only targets that also
omit it. These declarations are static adapter capability, not values inferred
from model token windows. No present parser or adapter exposes these fields.

The optional `[conversation_import]` table has exactly one `max_source_bytes`
positive integer. It bounds both a single-shot source and the exact source bytes
retained while one per-connection chunked import is assembled. An absent table
uses 268,435,456 bytes (256 MiB). Single-shot import rejects a source above the
configured value before conversion. Begin rejects a declaration above the
configured value before assembly, append rejects the first observed size above
it, and commit rechecks the value against the actual appended byte count.

The optional `[blob_storage]` table, its one-through-32 store catalog, distinct
store-name and namespace-UUID bindings, exact filesystem and S3 fields, static
credential grammar, routes, bounds, and absent-state compatibility are owned by
the [blob-storage configuration contract](blob-storage.md). This configuration
loader rejects every disagreement before runtime composition and applies the
ordinary protected-file checks to the explicit S3 credential file; no ambient
credential source enters the resulting adapter configuration.

The optional `[web_fetch]` table has exactly one `allowed_origins` array. It
contains at most 64 distinct bare HTTP(S) origins: scheme, host, and optional
port only, with no user information, path beyond `/`, query, or fragment. The
loader canonicalizes the effective port and hostname before duplicate checks. An
absent table or empty array admits no outbound `web_fetch` request. Every
request must match one configured canonical origin before dispatch, so automatic
approval cannot silently egress to an arbitrary host. Paths and queries remain
unrestricted request data at an admitted origin.

<a id="daemon-tool-mapping-registry"></a>

Production signalboxd composition requires exactly one mapping for each of the
four deployment-mapped tool families in the same closed-table style as
`[[adapter_mappings]]`:

- `code_host` selects adapter `github`, credential profile `github-primary`, and
  egress policy `github_api_only` for authenticated GitHub API requests; the
  credential-free job-log redirect remains the bounded public-HTTPS exception
  owned by [tool-loop](tool-loop.md);
- `github` selects the same adapter, profile, and policy;
- `workspace` selects adapter `local` and supplies one nonempty, absolute,
  lexically canonical UTF-8 `workspace_root` of at most 4,096 bytes with no NUL;
  it begins with `/`, has one or more nonempty components separated by single
  `/` characters, and has no `.`, `..`, or trailing-separator component; and
- `conversations` selects adapter `application` and has no credential, egress,
  or filesystem field.

The `[[tool_mappings]]` array may be absent for compatibility with deployments
that have not enabled the configured composition. In that case production
preserves the base catalog, including the code-host suite, without constructing
pull-request, workspace, conversation, local Git, or execution dependencies.
When the array is present it must already be complete: an unknown, missing, or
duplicate family; an unknown field; any fixed value with another spelling; a
relative, filesystem-root, noncanonical, overlong, or NUL-containing workspace
root; or a dependency field on the wrong family is a sanitized configuration
failure.

The complete mapped composition also requires one `[git_identity]` table with
exactly `author_name` and `author_email`. Both are nonempty, at most the Git
identity bound of 256 UTF-8 bytes, have no leading or trailing whitespace,
control character, `<`, or `>`, and are injected as both author and committer
identity; no ambient Git configuration or process environment supplies either
value. A missing table, unknown field, invalid value, or identity construction
failure is a sanitized configuration failure.

The complete mapped composition also requires one `[daemon_tools]` table with
`exec_supervisor_executable` and admits the optional `cargo_registry_cache`. The
executable value is an absolute path to an existing file naming the separately
packaged `signalbox-exec-supervisor` program. The cache value, when present, is
an absolute path to an existing directory. A missing table, unknown field,
relative path, or wrong path kind is a sanitized configuration failure.
Production resolves admitted paths to their canonical targets; the execution
suite pins both the program and optional cache during construction. The daemon
never derives either from its own executable path or home directory.

The configured root is opened once during tool construction and its pinned
authority is cloned into both workspace suites. The local Git suite
independently binds that same root and requires a direct main worktree whose
`.git` directory is inside the root. The three execution tools bind that root
and share the one pinned supervisor runner. A nonexistent, non-directory,
final-symlink, non-repository, linked, or externally administered configured
root, or one with no lexical final component for the per-session derivation
below to append its suffix to, therefore fails startup for the complete mapped
composition. The mapping-free base composition admits no root and constructs no
Git or execution suite, so existing base-only deployments remain valid.

<a id="derived-session-workspace-roots"></a>

Each session binds its own workspace root, derived from the configured root by a
fixed formula: the derived root is `<name>.sessions/<session identifier>` beside
the configured root, where `<name>` is the configured root's own final path
component and the session identifier is its UUID text. A session names no path
and no configuration field or durable column supplies one, so the set of roots
the daemon can open is fixed by the configured root alone. The derived parent is
a sibling of the configured root rather than a child, because a per-session root
inside the configured root would be readable, writable, and executable by every
session still bound to the configured root. One root per session, and derivation
from the configured root alone, are properties of this version rather than
permanent limits; several bound roots per session and explicit operator rebinds
are routed through [tool safety](../open-questions.md#tool-safety).

<a id="durable-workspace-records"></a>

A workspace also has a durable record — an identity and the canonical root it
was minted for — because authority grants must be scoped to something stabler
than a path. The record is written *from* this derivation, never read *by* it:
nothing consults the table to decide which root to open. Committed but
unimplemented: no present surface records a derived root. The table and its
constraints exist; the daemon-side write does not, as
[identity and commands](identity-and-commands.md) states for `WorkspaceId`
generation. What the identity is for is the other direction — a grant such as a
minted Git push destination is keyed by it, so two spellings of one directory
cannot become two scopes. The root is canonicalized once, when the record is
minted, and stored in canonical form; no later comparison normalizes anything.
The tiers that mint these records, and the one grant that currently uses them,
are stated under [remote destination authority](git-authority-threat-model.md).

Provisioning that directory is deployment work: creating a direct main worktree
there is what makes a session use it. Only a reported absence at the derived
path is unprovisioned, and such a session binds the configured root, so an
unprovisioned deployment is unchanged. A present non-directory, a symlink, or a
path the daemon cannot classify at all is a misprovisioned session rather than
an unprovisioned one and fails closed. This decides the sessions whose binding
is still open; a session that already bound the configured root is governed by
the recorded-binding rule below instead.

**Committed unimplemented functionality — pre-activation instruction binding.**
A session template carrying a `workspace` instruction selector makes workspace
binding a prerequisite to that session's first turn activation. The daemon uses
this section's same configured-versus-derived resolution, misprovisioning
refusal, identity checks, and sticky process-lifetime binding; after the binding
is established, instruction discovery and selector resolution run against that
bound root before activation can freeze eligibility. It does not probe and scan
a candidate pathname while leaving the binding open. No present template field
or session-creation path requests this eager binding; when one does, it must
compose with this binding rather than a second resolver.

The derived parent is classified the same way and before the session's own
directory, because it is the one intermediate component this derivation
introduces and every no-follow open after it declines to follow only the
component it names. A symlink standing at the parent would otherwise be followed
by all of them, placing every derived root wherever it points — inside the
configured root, say, where every session still bound to that root can read,
write, and execute it. A parent that is itself one of the configured
composition's directories is refused for the same reason: `<name>.sessions`
bind-mounted onto the configured root presents a real directory rather than a
symlink, so the classification admits it while every child beneath it is nested
inside the configured workspace — which the bound pair cannot show, since
ancestry is not equality. The pinned and the standing configured pairs are both
compared, since the configured pathname is never re-resolved. A composed
workspace that is the parent itself, rather than a directory inside it, is
refused on the same comparison: a session's identifier directory bind-mounted
onto that parent composes to the directory holding every sibling session's root,
which neither the configured comparison nor another session's bound pair can
show. Either composed directory standing on the parent is refused, since a
`.git` there nests the siblings inside this session's administration directory
just as a root there nests them inside its worktree. An accepted residual: a
parent that is a real directory whose contents are a bind mount of a tree inside
the configured root presents no symlink and no shared directory identity, and is
admitted.

Classifying the parent is a statement about one instant, so its identity is
captured with that classification and revalidated wherever the pathname is
walked again: once the composition has built, and on every later request. The
identity is recorded beside the pair the session bound and compared apart from
it, because a parent is traversed rather than bound — two sessions legitimately
share one, so it is never a collision, while a different directory standing
there means the pathname no longer leads where it led when the session bound. A
parent renamed away and replaced, with the session's own directory moved under
the replacement, leaves both bound directories intact at the same pathname and
is caught by this comparison alone. An accepted residual: a replacement undone
between two adjacent comparisons is not distinguished, which would require
holding the parent descriptor and resolving every family's root relative to it.

The configured root must have a lexical parent and final component, since the
formula appends the suffix to that component. A root without one —
`/srv/workspace/child/..` is absolute, is accepted by the mapping registry, and
can name a valid worktree — is rejected at composition rather than treated as a
deployment where every session is unprovisioned, which would silently return
every session to the one shared root this derivation exists to replace.

Which root a session bound is recorded the first time it invokes a
workspace-root-bound tool and does not change for the process's lifetime. A
session that bound the configured root is not moved onto a directory provisioned
later, and is not failed by a misprovisioned entry appearing there either: it
never opens that pathname, so nothing arriving at it is reachable by that
session or can change the tree it already uses. A session that bound a derived
root is never returned to the configured root by that directory's removal: its
next request fails closed instead. The first record written wins, so two
concurrent first requests for one session converge on one root rather than the
later one overwriting the earlier. Convergence covers the request that observed
nothing: a probe taken before the state lock can report an absence that a
concurrent first request has already resolved by binding a derived root, and the
resuming request retakes the probe under the lock rather than failing on the
stale observation. A directory that is genuinely absent reads identically, so
the retaken probe is what distinguishes them, and a removal still fails the next
request closed.

A derived record names the filesystem identities of the worktree and of the
`.git` directory inside it, not only the fact that a derived root was bound, so
a different directory standing at the same pathname is refused rather than
resumed as though it were the same workspace. Two identities rather than one
because a workspace is a worktree and a repository: two roots exposing one
`.git` are one workspace even where the roots differ. Every request revalidates
the pathname against that record before dispatching, including a request served
from a retained composition, so a removed or replaced directory fails the next
request rather than being reached through a descriptor pinned to the directory
it replaced. The same request also remakes the comparison against the configured
root described below, because admission is not a durable answer: the configured
composition is never re-resolved, so what its pathname names can change after a
session was admitted, and a retained composition returned on the strength of the
comparison made at admission would leave both reaching one tree under separate
serialization domains. A request binding the configured root remakes that
comparison too, against every other session's derived record, since the same
replacement is reachable from the configured composition and comparing only on
the derived branch would protect only the requests taking it. A deployment where
no session was ever provisioned a root of its own has nothing to compare and
captures nothing.

The record holds one session identity, one discriminant, and those identities,
so it is kept apart from the descriptor-holding composition and is never
evicted. A daemon restart clears it, after which a removed directory again reads
as unprovisioned.

**Committed unimplemented functionality — instruction-selector binding.** A
session carrying a workspace instruction selector extends that process-lifetime
record with durable pre-activation correlation. Its instruction-eligibility
initialization records the selector-set hash, complete discovery identity,
resolved workspace root path, and the exact worktree and `.git` filesystem
identities captured above. After the scan, one session-scheduler transaction
revalidates the live process binding against that evidence, installs the initial
allow-list, copies it into the first turn's eligibility snapshot, and activates
that turn. The transaction commits all three state changes or none, so no crash
can leave installed identities awaiting an unrelated later activation.

After restart, an already-active first turn may proceed only after the binding
resolver reconstructs its process record by comparing the current path with the
durable correlation. Missing or different filesystem identities fail closed;
recovery neither rescans selectors nor substitutes newly registered bundle
identities. Configured-root-only selectors carry no workspace correlation but
use the same atomic install-and-activate transition. No present template field
or activation path supplies this behavior.

A derived root is opened, layout-checked, and supervisor-bound the first time
that session invokes a workspace-root-bound tool, not at startup, because no
session exists at startup. Every family in one composition resolves the same
pathname, so the root's filesystem identity is captured on both sides of the
composition and compared: a pathname that did not resolve to one directory
throughout rejects the whole composition rather than leaving one family bound to
a directory and another to its replacement. The pair a composition records is
the one its Git suite pinned, which that suite accepted on either side of its
own repository open, rather than a further resolution of the pathname once the
composition is built: an administration directory replaced between the
repository open and that later resolution would otherwise be recorded while the
Git executor stays bound to the repository it opened. The Git suite's worktree
root is compared against the pathname every other family resolved, so a Git
suite bound elsewhere rejects the composition. The composed executors are
retained per session under a bound of eight, the least recently used idle entry
released first, which is what keeps open descriptors and pinned repositories
finite. A set a request is still holding is never released to make room, because
releasing it would let that session's next request compose a second set beside
the one already mutating its tree. The retained set may therefore exceed eight
by the number of sessions executing a workspace-bound tool at that moment. That
excess drains rather than persisting: each retention releases idle entries until
the set is back under the bound, so one burst of concurrent sessions does not
leave it permanently above.

Isolation is checked against the directories rather than the pathname, since two
pathnames can name one workspace: a composed root either of whose directories is
either directory of the configured root or of one another session already bound
is refused. Every pairing is compared rather than worktree against worktree and
administration against administration alone, because one composition's worktree
root can be the directory another administers — a nested repository exposed by a
bind mount — and the first composition's mutation and execution tools would
otherwise write the second's repository administration state. The configured
composition is compared both as it pinned itself at startup and as its pathname
resolves now, since it is the one binding no later request re-resolves: its
worktree descriptor is pinned, but its mutation and execution tools reach `.git`
through that descriptor by name, so a `.git` renamed and recreated under it is
reachable from the configured root while the pinned pair still names the
displaced one. A configured pathname whose pair cannot be captured at all fails
the request closed rather than falling back to the pinned pair: the configured
adapter still holds its root descriptor and still reaches whatever stands under
it, so a failed capture is less than the comparison needs rather than more, and
comparing against the startup pair alone would admit exactly the sharing the
comparison exists to refuse. An accepted residual: a filesystem may reuse a
device and inode pair after the directory that held them is removed, so a
derived directory removed and recreated while its composition is not retained
can present the identities the record names. Distinguishing that would require
holding a descriptor for every session ever bound, which is the descriptor
growth the retained bound exists to prevent. Failure to compose or bind a
derived root — an unopenable directory, a rejected repository layout, a root
replaced during composition or since the session bound it, a root reached
through a parent that is no longer the classified one, a root shared with
another session or with the configured root, a configured root whose own
directories could not be captured to decide that sharing, or a repository whose
object format disagrees with the one the process-lifetime catalog compiled —
closes that tool request as a known failure whose sanitized detail names the
closed reason. No second operator event is emitted for it: the tool loop's
single failed-attempt admission site owns that telemetry, and the reason travels
in the durable result. It never falls back to another root. The GitHub policy
admits exactly `https://api.github.com:443` for authenticated requests. The
code-host `change_request_ci_job_log` operation retains the tool-loop-owned
exception for one credential-free download from its validated, pinned, bounded
public HTTPS redirect destination; the pull-request suite has no such exception.
Model arguments cannot widen either admission rule.

The optional `[tool_approval_postures]` table maps an exact composed tool name
to one of `auto`, `delegated`, or `human`. The parser rejects non-string or
unknown posture values, and startup rejects a structurally valid name that is
absent from the selected composition. That name check runs in the pre-database
configuration pass. An absent table or omitted tool name preserves that
declaration's permission-default and session-blanket behavior exactly. Subject
to the `AlwaysConfirm` rule owned by
[Approval policy and decision sources](tool-loop.md#approval-policy-and-decision-sources),
an explicit posture supersedes that result for the request: `auto` records
policy automation and `human` parks for a user even when the session blanket is
enabled. `delegated` parks the request, invokes the approval judge, and exposes
the ordinary user-decision path only after escalation or a terminal judge
failure — except where the escalation is judged under repository-watch dispatch
authority and takes the unattended terminal path
[repository watch](repo-watch.md) owns, which fails the turn instead of exposing
that path to a user who is not there.

The optional `[approval_judge]` table has exactly one `selection_id`, and the
configuration parser requires it to name a configured direct selection. The
daemon uses that selection through the ordinary adapter, credential-profile,
target-resolution, and usage-limit machinery. When the table is absent, the
judge call uses the request-producing call's direct selection unchanged, never a
hardcoded lower tier.

When no explicit posture is configured, composition preserves each compiled
declaration's permission default and feeds it unchanged into the existing
durable approval flow. Exact-revision code-host and pull-request reads and all
workspace reads default to `Auto`; code-host mutations, GitHub review
publication, and every workspace mutation default to `Confirm`. Reading the
invoking session's transcript defaults to `Auto`, while listing conversations
and reading another native or imported conversation default to `Confirm`.
`web_search` and `web_fetch` also default to `Confirm`; the checked-in example
maps both exact names to `human`. The three execution tools complete the
enumeration. `unsandboxed_exec` is compiled `AlwaysConfirm`. Every posture value
remains configurable for it, but which of them changes its resolved approval is
owned by
[Approval policy and decision sources](tool-loop.md#approval-policy-and-decision-sources).
`sandboxed_exec` defaults to `Confirm`, because it accepts an arbitrary program
and argument vector. That is a compiled default and not a floor: an explicit
`auto` posture resolves it to policy-automatic, and with no posture mapped a
session blanket approves it without a per-call decision. Only the `human`
posture parks it for a person whatever the blanket says. `cargo_diagnostics`
defaults to `Auto`: its arguments carry no program, so a turn selects neither
the binary nor its argument vector, and the tool issues only the fixed Cargo
check, clippy, and test passes it builds itself. Those passes still compile and
run the workspace's own build scripts, procedural macros, and test binaries, so
an automatic diagnostics call executes whatever code the workspace already
contains, under the profile described below. The runtime meaning and precedence
of those declaration defaults, the explicit posture, the session blanket, and
the durable approval wait are owned by
[Approval policy and decision sources](tool-loop.md#approval-policy-and-decision-sources).
Only the explicit `[tool_approval_postures]` table changes a declaration's
resolved posture; family composition itself does not.

On Linux, `unsandboxed_exec` pins the requested host working directory before
launch. When the direct program is Git and that directory is a linked worktree
whose `.git` marker names an administration directory below either the
sandbox-only `/workspace` path or the injected host workspace root, execution
pins the corresponding directory below that root and supplies the pinned
administration and worktree paths through Git's environment. Discovery stops at
the first `.git` entry, so a nested clone or submodule never inherits an outer
worktree's environment, and explicit Git repository selectors (`-C`,
`--git-dir`, or `--work-tree`) suppress injection. The repository-creating
commands `init` and `clone` also suppress injection, because each establishes a
new repository rather than operating on the current one, and an inherited
`GIT_WORK_TREE` makes `clone` refuse its destination. Before injection,
execution atomically rewrites the linked-worktree administration directory's
sandbox-only `gitdir` backlink to the corresponding host marker path so
host-side worktree maintenance does not prune the live worktree. That durable
metadata write exposes the host workspace path to the sandbox-side view and can
make sandbox-side worktree maintenance unable to resolve the backlink; callers
needing that view must recreate the linked worktree there. Other programs and
other `.git` marker shapes receive no Git-specific environment or metadata
mutation.

On Linux, direct Git through `sandboxed_exec` recognizes the same host- and
sandbox-rooted linked-worktree markers, under the same selector, nested-marker,
and ownership guards. It pins the administration directory, binds it over its
corresponding path below `/workspace`, and supplies that path and the requested
`/workspace` worktree through Git's environment. A marker already written in
host form — the ordinary case for a worktree created on the host — is verified
and left alone, so it stays valid without granting another host path or
rewriting repository state. A marker still in sandbox-only form is rewritten to
the host form exactly as the unsandboxed path does, with the same trade-off
recorded above. Other sandboxed programs and marker shapes receive no
Git-specific mount, environment, or metadata mutation.

`sandboxed_exec` and `cargo_diagnostics` share one daemon-local bubblewrap
profile. This page states the launch and then lists separately what the profile
does not provide.

The default launch is this. Bubblewrap receives `--die-with-parent`,
`--new-session`, `--unshare-user`, `--unshare-pid`, `--unshare-ipc`,
`--unshare-uts`, and `--unshare-net`, and mounts a fresh `/proc`. An explicit
container-process-namespace variant omits `--unshare-pid` and instead read-only
binds the existing `/proc`; it is admissible only when an outer container
runtime already isolates that PID namespace and procfs from the host. Both
variants mount a fresh `/dev` and a `tmpfs` at `/tmp`; create `/etc`; and
read-only bind `/usr`, `/bin`, `/lib`, `/lib64`, `/nix/store`,
`/etc/alternatives`, `/etc/hosts`, `/etc/nsswitch.conf`, and `/etc/ssl`, each
where it is present. It does not bind `/etc/resolv.conf`. It binds the calling
session's bound workspace root read-write at `/workspace`, read-only binds the
pinned execution supervisor — a host path that need not lie under that root — at
`/signalbox-exec-dispatch`, and changes directory to `/workspace` or to the
requested directory beneath it. The child environment is cleared and then set to
`LANG`, `LC_ALL`, `PATH`, and `HOME=/workspace`. When `cargo_registry_cache` is
configured, the profile additionally creates a private writable tmpfs at
`/cargo-home`, read-only binds the pinned cache at `/cargo-home/registry`, and
sets `CARGO_HOME=/cargo-home`; replacement or removal of the configured cache
fails sandbox setup closed. Every command is dispatched through the supervisor.

The profile does not provide the following, and no other daemon-local control
supplies them:

- Two transports outside the network namespace's reach stay available. An
  `AF_UNIX` *pathname* socket inside the workspace root is connectable, because
  it is reached through the filesystem, and one fronting a proxy carries egress
  with it; `AF_VSOCK` to a host CID remains available wherever the platform
  provides it. Abstract `AF_UNIX` sockets are not in this class — that namespace
  is scoped by the network namespace, so unsharing it does isolate them.
- A credential inside the workspace root is readable. Credential settings are
  admitted on presence alone and are never checked against that root, so one
  configured inside it is bound along with the workspace, as is any secret the
  repository itself carries.
- The fresh `/proc` in the default variant, the container-isolated `/proc` in
  the explicit variant, and `/dev` are not a private surface. They carry kernel-
  and host-derived data — `/proc/cpuinfo`, `/proc/meminfo`, and the boot
  identifier among it — that no workspace bind governs, so the readable surface
  is wider than the bound paths alone.
- `HOME` is the workspace root, so home-relative configuration discovery —
  `~/.config` and anything else a program resolves that way — is inside the
  writable workspace rather than at a host location. Cargo alone uses the
  private `/cargo-home` when an explicit registry cache is configured; its
  registry is read-only while its lock and other transient state remain private
  to the process.
- Everything under the workspace root is writable, including the repository's
  `.git`.
- `cargo_diagnostics` compiles and runs the workspace's own build scripts,
  procedural macros, and test binaries under this profile, so an automatic call
  executes whatever code the workspace already contains.
- No resource limit, uid or gid drop, seccomp policy, or landlock policy
  applies, so the profile does not contain a deliberately hostile program.

This is not the runner's `WorkspaceRestricted` profile described by
[sandbox profiles and approval](runner-protocol.md#planned), which additionally
drops capabilities and brokers egress through a hostname allowlist, and which no
present runner surface provides.

The conversation adapter uses the existing application listing service and the
established persistence projections for native semantic transcripts and
immutable imported conversations. It exposes only persisted visible semantic
content in tool results: source-attested imported text remains text, while
unattested, non-text, thinking, redacted-thinking, document, and absent-content
entries are content-silent typed markers. Native reads stream from the
repeatable-read projection. A selected native read carries the trusted invoking
session separately from the model-selected target. The adapter loads both
current placement epochs before opening the transcript in that same snapshot; an
out-of-directory target returns typed refusal evidence naming the requesting
directory and `outside_requesting_directory_subtree`, never an empty page.
Pathless requesters retain the pre-placement behavior and a loudly acknowledged
root placement reads every target. Imported reads materialize the complete
immutable aggregate, including its persisted raw source records, before the
adapter projects normalized visible entries and enforces the tool page's entry
and byte bounds; raw source records are never returned in the tool result.

Each `[[models]]` entry defines one direct selection:

- `selection_id` — UUID of the immutable `DirectModelSelection` key.
- `target_id` — UUID of the exact normalized provider/model identity
  (`ResolvedProviderTarget`). Identity encoding is
  [identity-and-commands](identity-and-commands.md) material.
- `model_family` — exact key of one `[[adapter_mappings]]` entry.
- `provider_model` — the exact provider-native model spelling; must be nonempty
  and unpadded. One spelling routes to exactly one adapter across the document:
  declaring it under two families whose mappings name different adapters is a
  typed startup failure, so a deployment serving one provider through two
  surfaces gives each surface its own spelling.
- `max_output_tokens` — required positive `u32` output-token ceiling.
- `context_window_tokens` — required positive `u32` usable context ceiling after
  any provider or adapter reservation, not the provider's larger raw advertised
  window, and not smaller than `max_output_tokens`.
- zero or more `[[models.rate_windows]]` entries, each one dated price window
  over this entry's own `provider_model`: `provider` (the commercial provider
  that published the rates, `anthropic` or `openai`), `channel` (`api` or
  `batch_api`), `effective_from`, optional `effective_until`, the all-or-none
  `input_usd_per_million_tokens`, `output_usd_per_million_tokens`,
  `cache_creation_input_usd_per_million_tokens`, and
  `cache_read_input_usd_per_million_tokens` as nonnegative decimal USD strings
  per million tokens, and the provenance pair `source_url` and `retrieved_on`.
  Both bounds are canonical `YYYY-MM-DD` strings: a window covers a call
  timestamp from `effective_from` at 00:00:00 UTC inclusive to `effective_until`
  at 00:00:00 UTC exclusive, and windows resolved for one target and channel may
  not overlap. A window's identity is exactly its `provider`, `provider_model`,
  `channel`, and `effective_from`, and that identity is what a derived cost
  names. A published window's rates and bounds do not change; the one admitted
  edit is closing an open window, setting its absent `effective_until` to the
  `effective_from` of the successor installed with it. Declaring only part of a
  window's rate set is a configuration error; declaring no window yields no
  dollar figure for that model.

The document root may carry an optional `[verified_through]` table mapping a
provider name to one date. It is provenance metadata, never a resolution gate.

The daemon provides exactly `anthropic`, `openai`, `claude_cli`, and
`codex_cli`. No adapter pins a profile name, and a pool may hold several
profiles for any one adapter. **This sentence is the closed set of admitted
`(adapter, delivery)` pairs, and startup rejects every pair outside it:**
Anthropic and OpenAI admit `file`; Claude CLI admits `ambient` and `file`; and
Codex CLI admits `ambient`, `file`, `codex_home`, and `oauth`. Each delivery's
own section below states the *route* the secret takes for the adapters admitted
here, and states no admission of its own — a pair is admitted here and routed
there. [The `file` delivery](#the-file-delivery) routes Claude CLI's `file`
pair. Admission is not delivery: of the pairs above, the daemon supplies a
surface for Anthropic and OpenAI `file`, Claude CLI `ambient` and `file`, and
Codex CLI `ambient` and `codex_home`, and validates then refuses the rest as
undelivered. OpenAI admits the reasoning levels `none` through `max` — `ultra`
is the Codex effort value and is rejected — and the provider-tagged tiers
`auto`, `default`, `flex`, `scale`, `priority`, and `fast`.

A Codex mapping also requires `[codex_cli]` with an absolute executable path
naming an existing regular file and an absolute, existing `working_directory`;
construction validates that shape and platform support without invoking Codex or
inspecting login state. The Codex CLI owns its external login exactly as the
adapter contract specifies.

Claude Code mappings follow that same CLI shape. A Claude mapping requires
`[claude_cli]` with three deployment-named paths: `executable` and
`mcp_bridge_executable` must each resolve to an absolute path naming an existing
regular file, and `working_directory` must name an existing directory. The
bridge is the separate `signalbox-claude-mcp-bridge` program the adapter spawns
as Claude Code's only tool server; the deployment names it exactly the way it
names the CLI, so the daemon derives no executable path from its own image.
Because that program is one this workspace builds and installs rather than one
the operator already placed, `mcp_bridge_executable` alone admits a second
spelling: a bare program name — a value equal to its own final path component —
is resolved once at startup against the daemon's own `PATH`, entry by entry in
configured order, to the first entry holding a regular file of that name the
daemon's own effective credentials may execute, which a file only another user
may run does not satisfy. Only absolute search entries participate; a relative
entry, including the empty entry POSIX reads as the working directory, is
skipped, because the resolved path is written into the MCP server configuration
Claude Code spawns from a working directory of its own. A name `PATH` does not
resolve is a typed startup failure distinct from a malformed path. Any other
value is a path, consults no `PATH`, and faces the absolute-existing-file rule
unchanged, so a configured path never silently resolves to a different program.
Both spellings yield the same absolute path downstream: what the adapter
receives, and writes into that MCP server configuration, is always the resolved
absolute path. Construction validates that shape and platform support without
invoking Claude Code or inspecting login state. An ambient profile leaves login
resolution inside Claude Code; a file profile resolves its value per preparation
as described below. Because Claude Code exposes no service tier, any
`service_tiers` entry on a Claude model is a typed startup failure, while its
reasoning set and either fast-mode form are admitted.

Each optional `[[aliases]]` entry defines one alias: `alias_id` (UUID of the
`ModelAlias`) and `selection_id`, which must name a configured model (dangling
aliases are rejected). Duplicate selection keys, duplicate aliases, and
conflicting runtime meanings for one target are all rejected. If more than one
model entry names the same target, its complete set of rate windows or complete
rate absence must also agree; a rated and unrated entry cannot share a target.

One valid document yields correlated immutable in-memory catalogs:

- the domain `ModelTargetCatalog`, mapping each `DirectModelSelection` to its
  exact `ResolvedProviderTarget`, used by execution-time target resolution;
- the `RuntimeModelCatalog`, mapping each target to its provider-native spelling
  and token ceilings, used by the provider bridge
  ([runtime-substrate](runtime-substrate.md)).
- the exact provider-model-to-adapter routing table and target-to-family table.
  The former selects Anthropic HTTP, OpenAI HTTP, Claude CLI, or Codex CLI for
  each operation; the latter selects a session-pinned credential entry. A
  provider model routed to different adapters or a target assigned conflicting
  families is rejected at startup.
- the profile-to-billing-kind registry and target-to-dated-rate-window catalog
  used only when a read surface derives dollar cost. Rates are never written to
  a model-call row.

The file is read at startup and re-read by the
[configuration reload](#configuration-reload) verb. Keeping a selection key
immutable is deployment discipline that code enforces only partially: removal
makes new resolution fail, but nothing prevents an edited document from pointing
an existing `selection_id` at a new `target_id` across a restart — new turns
would silently resolve to the new target (see Open edges). Where a stored call
exists, code does enforce consistency: ordinary-path reconstitution cross-checks
every stored call's target against the configured `ModelTargetCatalog` and fails
closed as corruption (`CallTargetMismatch`) when the catalog now resolves that
selection to a different target. The startup-scan restart path instead rebuilds
its target catalog from the stored calls themselves, deliberately not from
configuration — part of why recovery of acknowledged work is
configuration-independent (INV-034).

## Credential deliveries

A `[[credential_profiles]]` entry accepts exactly `name`, `adapter`,
`billing_kind`, and `delivery`, plus whichever fields the selected delivery
owns, and rejects every other key as unknown
(`apps/signalboxd/src/credential_pools.rs:54`). Which adapter a profile
authenticates is the profile's own `adapter`, and the secret reaches the
provider through the delivery this section describes rather than through a
process environment variable. Claude and the direct HTTP adapters receive their
complete adapter-scoped profile catalogs and resolve each operation's selected
reference from them, so two model families of one of those adapters may prefer
different profiles. Codex still carries a single retained reference into its
runtime, so two `codex_cli` families preferring different profiles are a typed
startup failure rather than an admitted pair.

A profile's closed `delivery` states how its secret reaches the provider. Four
are admitted. Which of them a given adapter accepts is **not** a table stated
here: an `(adapter, delivery)` pair is admitted exactly when that adapter's own
delivery contract defines how the secret reaches its provider, and startup
rejects every pair no such contract defines. Why a permission rather than a
matrix: a matrix would have to be edited in two places every time an adapter
gained a delivery, and the delivery contract is what defines the route.

The contracts defined here are `ambient`, for the CLI adapters that take a
non-secret reference; `file`, for the direct HTTP adapters and for `claude_cli`,
whose `env_key` and materialized settings store are described below, and for
`codex_cli`, whose own `env_key` spelling is described there too; and the Codex
`codex_home` and `oauth` deliveries. Every other pair is rejected because no
contract here says how the secret would reach that provider: `ambient`,
`codex_home`, and `oauth` for a direct HTTP adapter, and `codex_home` and
`oauth` for `claude_cli`. Defining a pair and supplying a surface for it remain
separate questions — the deliveries refused despite being defined are enumerated
below.

Each `[[credential_profiles]]` entry is one flat TOML table: `delivery` is a
required TOML string discriminant, common fields are exactly `name`, `adapter`,
`billing_kind`, and `delivery`, and the selected variant admits only its fields
below. A field owned by another variant is unknown and rejected.

Admitting a pair and supplying a surface for it are separate questions:
`ambient` is delivered for both CLI adapters, `file` for `anthropic`, `openai`,
and `claude_cli`, and `codex_home` for `codex_cli`. The `codex_cli` spellings of
`file` and `oauth` are admitted by their sections and then rejected as
`UndeliveredCredentialDelivery`, so such a document fails startup rather than
running with an inert setting. Their contracts are stated under
[credential-home and reserved deliveries](#credential-home-and-reserved-deliveries)
below.

#### Distinct members are distinct authorizations

Rather than reject alias spellings one at a time, this contract states the one
property a pool's membership must satisfy, and then says for every delivery
whether the daemon establishes it or requires it of the deployment.

**Every two members of one pool that a successor may substitute between denote
authorizations the provider meters, throttles, and rejects independently.** The
daemon establishes that where it can and requires it of the deployment where it
cannot; this contract says which for every delivery, and there is no third case.

- `ambient` — *established by rejection.* At most one `ambient` profile exists
  per CLI adapter, so no pool can hold two of them, and mixing `ambient` with
  **any** other credential-bearing delivery for that adapter — `codex_home` or
  `oauth` — is rejected. The reason is the same in both cases and is a property
  of `ambient` rather than of what it is mixed with: the daemon never reads the
  ambient login store, so that profile has no account identity at admission and
  never acquires one, and static configuration cannot prove it differs from a
  named directory or from an authorization provisioning later mints. The
  provisioning-time serialization does not help here, because it compares
  *stored* identities and `ambient` contributes none — so a mixed pair would
  reach `switch_now` and retry into the one metering and rejection domain this
  property exists to prevent. Rejecting the pair is the only check available
  that does not require reading the store. `file` is not one of these pairs: a
  deployment-owned key file is an artifact independent of whatever login the CLI
  resolves for itself, so it carries its own admission-time identity rather than
  contesting `ambient`'s. The `ambient`/`oauth` pair cannot be written, since
  every `oauth` profile is refused as undelivered; the mixed
  `ambient`/`codex_home` case is rejected directly when the home is admitted.
- `file` — *required.* The daemon rejects only equal lexically normalized paths.
  An ordinary copy of the key file is admissible and indistinguishable from a
  second credential. This is an accepted limit, and its reason is the
  provider/integration no-startup-preflight rule: a `file` profile credential is
  never opened before preparation, so no filesystem identity exists at
  admission, and obtaining one would give up that rule for a guarantee an
  ordinary `cp` defeats anyway.
- `codex_home` — *admitted by normalized path.* Two profiles may not name the
  same normalized directory. Independence of the token families inside distinct
  directories remains a deployment assertion, because the daemon never reads the
  authentication material that could reveal a copied login.
- `oauth` — *established by the delivery, which is not delivered*, by the
  provider account identity that provisioning harvests and stores alongside the
  refresh token. That identity is what the provider meters, throttles, and
  rejects against, so two members are independent exactly when their stored
  account identities differ. Static pool admission cannot decide that relation,
  because no member has a stored identity until an operator provisions it; the
  relation is therefore enforced at the provisioning commit that first makes the
  identity knowable, as [the `oauth` delivery](#the-oauth-delivery) states. The
  provisioning tuple is deliberately **not** this relation, and it fails in both
  directions: two independently metered accounts reached through one OAuth
  client, endpoints, and scopes have identical tuples and are nonetheless
  independent, while two grants for one account obtained under different clients
  or scopes have different tuples and are nonetheless one authorization. The
  tuple has a separate purpose — binding a stored token to the configuration it
  was minted under, so an edited endpoint cannot receive a token issued for
  another — and for that job it is compared as parsed canonical components:
  scheme, lowercased host, effective port, path, and query, never configured
  bytes, with fragments and user information rejected at admission because
  neither reaches the request target at all. Of all of this the present parser
  performs only that endpoint admission check: every `oauth` profile is then
  refused as undelivered, so no account identity is ever stored, no provisioning
  commit ever runs, and no two members are ever compared.

Two exceptions, and only these two. `quarantine` excludes a member from every
pool rather than from the one that observed it, so an authorization that turns
out to be shared is removed everywhere at once instead of surviving under
another pool's name. The operations policy applies to `codex_home` from the
Codex CLI's typed terminal classification; the daemon does not inspect the home
to invent a second signal.

Why this is stated as one property with a per-delivery disposition rather than
as a list of rejected spellings: a list of rejections can only ever be as long
as the shapes someone has already thought of, and each new spelling admitted — a
raw duplicate path, a lexical alias, a symlink, a hard link, an ambient alias —
is one the previous list did not name. The property is what a pool actually
needs, so a newly proposed alias shape is either closed by construction, as it
is for `oauth`, or already covered by the stated accepted limit, as it is for
`file`.

#### The `ambient` delivery

`ambient` is spelled `delivery = "ambient"` and is fieldless. The CLI resolves
the one login already visible in the daemon user's process environment; the
daemon supplies no credential value or profile-specific home. A profile
declaring `ambient` therefore rejects every delivery-specific field. Because one
CLI adapter process environment exposes only one such authentication context, a
document may declare at most one `ambient` profile for `claude_cli` and at most
one for `codex_cli`, regardless of which pools contain it. Giving that same
login two profile names would not make two credentials and could not authorize a
successor call. A Codex document declaring `ambient` therefore rejects every
`codex_home` profile in the same document, because static admission cannot prove
the two stores are distinct.

#### The `file` delivery

`file` is spelled `delivery = "file"` with required TOML string `file` naming an
absolute deployment-owned path and, only for a CLI adapter, required TOML string
`env_key`. The path is 1 through 4,096 UTF-8 bytes and NUL-free; startup rejects
every other string before any credential preparation. The path is read per
preparation and never cached, narrowed by the trailing-line-termination rule
below. Which adapters admit `file` is the closed set stated with the adapter
inventory above; for each of them the route is fixed with no third case: a
direct-HTTP adapter forms an HTTP header from it — `anthropic` its `x-api-key`
header and `openai` its `Authorization: Bearer` header — while a CLI adapter
routes it to the fresh process by the adapter contract stated below, which for
`claude_cli` keeps the value out of the child environment entirely. A
direct-HTTP adapter rejects `env_key` because it does not use a child
environment. A CLI adapter requires the one credential variable its adapter
contract names — `ANTHROPIC_API_KEY` for `claude_cli` and `OPENAI_API_KEY` for
`codex_cli` — and rejects every other value, including forwarded and
process-control names such as `HOME`, `CLAUDE_CONFIG_DIR`, `CODEX_HOME`, and
`PATH`. A CLI adapter whose contract names no such variable admits no `file`
profile at all, and startup rejects the pair: the route is the admission here,
so there is nothing to validate `env_key` against until that adapter's contract
names its variable. Both CLI adapters name one, so the pair is admitted for
each; whether a *surface* honors it is the separate question, and the
`codex_cli` spelling is validated and then rejected as undelivered, with
[credential-home and reserved deliveries](#credential-home-and-reserved-deliveries)
owning its contract.

Claude file delivery receives the complete adapter-scoped catalog of declared
`claude_cli` file-profile references and resolves the operation's selected
reference during cancellable request preparation, so a historical session keeps
the profile it was created with. The value reaches the CLI through a private
request-scoped settings store rather than the child environment: the key itself
is never added to the environment the adapter assembles, and the store is
removed when the prepared capability is dropped. How that store is constructed
and applied — its file modes, the `apiKeyHelper` script and its fixed
interpreter, and the one allowlisted child value the adapter replaces — is owned
by the
[credential-access boundary](runtime-substrate.md#credential-access-boundary)
and is not restated here. What this page fixes is which value seeds redaction:
the exact resolved credential, retained in the one-shot capability, is what
provider-controlled observations and terminal evidence are scrubbed against.

This is the delivery for every credential that has an external source of truth —
provider API keys, and any long-lived bearer token a provider's own tooling
mints for unattended use. Before comparing paths, startup lexically normalizes
each absolute path by removing redundant separators and `.` components and
folding each `..` component without permitting it to cross the root; that
operation performs no filesystem lookup and follows no symlink. For one adapter,
one normalized absolute file path may appear on only one profile in a document:
two spellings of one path are not independent credentials and cannot authorize
two attempts in one successor chain. That test is deliberately lexical only.
signalboxd opens no provider or integration credential file before preparation,
so a startup identity check would trade the no-startup-preflight rule in
[credential lifecycle](#credential-lifecycle) for a guarantee an ordinary copy
defeats anyway. Two distinct paths that a symlink, a hard link, or a copy
resolves to the same secret therefore remain two members. The accepted cost is
bounded: such a pair can spend one extra successor attempt that fails exactly as
its predecessor did, after which that member is excluded and the chain ends. It
admits no credential the pool did not already grant and cannot lengthen a chain
beyond the pool's member count.
[Credential operations policy](#credential-operations-policy) applies to it
unchanged.

### Credential-home and reserved deliveries

Codex CLI `codex_home` is delivered: parsing admits the directory and the
runtime supplies it to the selected member's child, as
[the `codex_home` delivery](#the-codex_home-delivery) states.

**Committed unimplemented functionality.** Codex CLI `oauth` and `file` have no
present delivery surface: parsing validates their fields and then rejects the
profile. The agreement between a delivery and its `billing_kind` is enforced for
every spelling, including these reserved ones, as
[the credential catalog](#the-static-model-alias-and-web-fetch-catalog) states.

#### The `codex_home` delivery

`codex_home` is spelled `delivery = "codex_home"` with required TOML string
`codex_home` naming the login directory the provider's CLI owns. The path is 1
through 4,096 UTF-8 bytes, NUL-free, absolute, and lexically normalized without
filesystem traversal. Startup then requires it to name an existing directory and
enumerates at most its first entry to prove it is readable and nonempty. A
relative or malformed path, a missing or non-directory path, an enumeration
failure, and an empty directory are distinct typed per-profile startup failures.
Error display and debug output carry the profile reference and closed cause but
never the path.

The daemon treats the directory only as a path reference: it never opens,
copies, parses, serializes, or logs authentication material inside it. Delivery
replaces each Codex CLI child's inherited `CODEX_HOME` with the admitted path of
the profile that operation's credential reference names, and leaves every other
member's path absent from that process environment; the CLI itself owns every
read and write beneath the selected home. The runtime re-checks each configured
home's shape at construction under the same four conditions startup applies, so
a home that has ceased to qualify fails construction rather than reaching a
spawn. An operation whose credential reference names neither the runtime's own
ambient profile nor a configured home is a typed unavailable-credential
preparation failure and starts no child.

Two `codex_home` profiles for Codex must name different normalized paths, and a
Codex document may not combine an `ambient` profile with any `codex_home`
profile because startup cannot prove those names represent distinct
authorizations. Distinct paths are a deployment assertion that the contained
provider accounts are independently metered; the daemon cannot verify that
assertion without reading precisely the authentication material this boundary
forbids it to inspect.

**Committed unimplemented functionality — bounded home concurrency.**
`max_concurrent_invocations` is a reserved field with the range 1 through 1,024,
and every profile that supplies it is rejected. Capacity reservations,
contention waits, and refresh-race coordination become admissible together; no
accepted bound is inert.

#### The `oauth` delivery

`oauth` is spelled `delivery = "oauth"` with exactly four required fields: TOML
strings `client_id`, `token_url`, and `device_authorization_url`, plus TOML
array-of-strings `scopes`. It is a rotating authorization the daemon owns. These
values are configuration, never build-provided constants: which OAuth client a
deployment presents is the operator's decision and is recorded in the operator's
own document, not asserted by the daemon. `client_id` is 1 through 1,024 UTF-8
bytes with no NUL; its bytes are preserved exactly, including whitespace.
`scopes` contains 1 through 64 strings, each 1 through 256 bytes. Every byte of
every element must be an RFC 6749 `scope-token` character — `%x21`, `%x23-5B`,
or `%x5D-7E` — which admits ordinary ASCII graphics while excluding the space,
the double quote, the backslash, every control byte including NUL, and every
non-ASCII byte. Why the whole set rather than only NUL: OAuth transmits `scope`
as a space-delimited sequence, so an accepted element containing a space would
become two scopes on the wire and make `["read write"]` and `["read", "write"]`
request the same authorization while the exact-duplicate rule and the persisted
provisioning tuple treat them as different values. Declared order is request
order, exact duplicate strings are rejected, and no trimming, case folding,
sorting, or other normalization occurs. Both endpoint values must be absolute
`https` URLs carrying no fragment and no user information — an empty username
and an empty password component. The tuple comparison is over parsed canonical
components — scheme, lowercased host, effective port, path, and query — and
never over the configured bytes. This comparison binds a stored token to the
configuration it was minted under; the relation that decides whether two members
are independent authorizations is the stored provider account identity, which
[the identity property](#distinct-members-are-distinct-authorizations) owns and
which this tuple is deliberately not. Two spellings a URL parser sends to the
same request target are therefore one stored binding, so respelling an endpoint
as `https://issuer.example:443/token` where it was
`https://ISSUER.example/token` does not quarantine a working authorization.
Fragment and user information are rejected at admission rather than normalized
away, and for a reason the canonical comparison does not supply: neither is a
component of that comparison at all, so silently discarding them would accept a
document whose text states something the daemon does not do. User information
carries a second objection of its own: a URL is logged, persisted in the
provisioning tuple, and echoed in diagnostics, so a password embedded there is a
credential carried outside the daemon-owned boundary this delivery exists to
establish — and on an HTTP stack that does honor it, it becomes an undeclared
authentication credential the contract never admitted. Startup rejects every
other scheme and provides no plaintext or local-host exception.

##### OAuth delivery and administration

No present configuration composition, runtime path, API, process message, CLI
command, or separate administrative endpoint provisions, re-provisions, deletes,
or clears quarantine for an `oauth` profile. The parser rejects this admitted
delivery as undelivered. The paragraphs below state the delivery's contract; the
delivery must add an operator-authorized administrative boundary, idempotency
and response contract before an OAuth profile can be usable. The closed
process-protocol inventory is complete and supplies none of these operations.

Provisioning is explicit and never automatic, and the daemon performs the
device-authorization exchange itself against the profile's configured
`device_authorization_url`, `token_url`, `client_id`, and `scopes`. It does not
drive the provider CLI's own login. That is not a preference: the pinned Codex
CLI constructs its authorization from issuer, scope, and endpoint values baked
into the binary, so a login driven through it would mint whatever tuple the CLI
carries rather than the one the profile declares — and the profile's tuple is
exactly what the storage contract below persists and later compares before every
refresh. Driving the CLI would therefore either misbind the harvested token to a
tuple it was not minted under, or persist a tuple that disagrees with the
operator's document from the moment it is written. Doing the exchange in the
daemon keeps one authority for what a stored authorization means, and it is the
same authority that already sends every refresh POST.

An operator-invoked command therefore requests a device authorization from the
configured endpoint, relays the returned user code and verification URI to the
operator, polls the configured token endpoint under the same
one-POST-per-attempt and no-redirect rules the refresh path uses, and on success
harvests the refresh token, the identity token, and non-secret account metadata
into one transaction.

That transaction is also where account-level independence is decided, because it
is the first moment an account identity exists to compare. Provisioning consults
every profile that shares a pool-policy revision with this one — its co-members
in each revision the profile is pinned into, and no profile outside them — and
fails typed, storing nothing, when any of those co-members already stores the
harvested account identity. Re-provisioning a member against its own previously
stored identity is not a collision; the rule concerns a *different* profile
already holding it. The consulted set is per revision because independence is
what a pool needs, and a profile in no pool has no co-member to contradict. Two
provisionings of one account must not both commit, so this consultation is
serialized against any concurrent provisioning commit of a consulted profile;
the lock protocol that achieves it is owned by
[persistence protocol](persistence-protocol.md).

Provisioning is not the only moment co-membership arises, and checking only at
provisioning would leave the property unenforced by the other one. Two profiles
provisioned to one account while neither shared a pool pass this rule correctly
— they had no co-member — and a later configuration can then intern a revision
naming both. **Interning a pool-policy revision therefore applies the same rule
to the membership it is about to freeze**, under the same locks and against the
same stored identities, and fails to intern when two members resolve to one
account. Between them the two moments are exhaustive: a pair can only become
co-members holding one account by a provisioning that joins an existing
membership or by a membership that joins existing provisionings, and each is
checked where it happens. An interning failure is a configuration error,
reported like any other rejected registration — the alternative, admitting the
revision and discovering the collision at a `switch_now`, is exactly the
account-level rejection domain the rule exists to prevent. Without that
serialization each would read an unclaimed identity and both would commit,
leaving a pool holding two members the provider meters, throttles, and rejects
as one — so a later `switch_now` would retry into the same account-level
rejection domain the pool exists to leave, which is exactly the independence
this delivery claims to establish.

The identity token is stored durably, with the refresh token and under the same
protections, because dispatch requires one on every invocation while a refresh
happens about once per access-token lifetime — so publishing it only as a
refresh result would leave the first preparation after any restart with no
source for it. Every refresh that returns a new identity token replaces the
stored one in the same commit that replaces the refresh token; a refresh that
returns none leaves the stored one in place, since a provider that omits it on
refresh has not invalidated it. Provisioning that returns none is a typed
provisioning failure and stores nothing: an authorization that cannot supply the
account header the CLI requires is not usable for this delivery, and failing at
provisioning is where an operator can still act on it. It is bearer material for
the same account, so it is never written anywhere the refresh token would not
be, and it seeds the redactor with every other value placed in the scratch home.
No scratch credential home is involved, because no child runs: the CLI is
involved only at dispatch, when it is handed a minted access token. Provisioning
depends on no other login for that account: it authorizes through its own
configured client and stores what it harvests, reading nothing an operator's CLI
already holds. Whether it *disturbs* one is the authorization server's to decide
and not something this contract can promise — a server that issues one grant per
client and account, or that revokes an earlier grant on a new authorization,
will invalidate an operator's existing login, and the exchange gives the daemon
no way to detect or prevent that. Deleting the profile's stored authorization
likewise ends the daemon's own grant and whatever else that server ties to it.
Where grant independence matters, it is a property of the configured
authorization server that the operator must establish, not one this delivery
provides.

A stored authorization is bound to the tuple it was minted under. Provisioning
persists, in the same transaction as the token generation, the exact
`client_id`, `token_url`, `device_authorization_url`, and ordered `scopes` the
authorization used. Every later refresh and every dispatch first compares that
stored tuple with the profile's current registration by the same canonical
components stated above, under the profile row lock and before any request is
formed. A mismatch never sends the stored token: the generation quarantines and
re-provisioning is the only recovery, exactly as for a rejected refresh. Why
this is a storage rule rather than a review rule: a refresh token is bearer
material for one authorization server, so a mistaken or hostile edit of
`token_url` in a document that ordinary restart deliberately honors would
otherwise disclose it to a host the operator's authorization never named, and a
changed `client_id` or scope set would corrupt the family the operator believes
they hold.

The daemon is the sole refresher of a stored authorization. Before contacting
the provider, it locks the profile row, reads the stored token, and
transactionally marks that generation's refresh in progress. The refresher that
wins that transition owns one process-shared single-flight keyed by profile and
generation. The durable marker excludes another refresher after the lock is
released for the network exchange. A concurrent preparation observing that
marker joins the same single-flight; it never starts another exchange or treats
the marker as a credential failure.

The limit is one POST per **attempt**, not one per generation, and at most one
attempt in flight per generation at a time. A refresh client sends exactly one
POST for that attempt to the configured `token_url`'s exact scheme, host,
effective port, path, and query, and redirect following and automatic HTTP,
transport, and protocol retries are disabled at every layer, so an attempt is
one request and never a family of them. The single-flight is what keeps two
preparations from attempting the same generation concurrently; it is not a count
of how many attempts that generation may ever have.

A failure that **definitively did not rotate** the stored token — one whose
response or transport outcome establishes that the exchange never reached the
point of issuing a new token — clears the marker and leaves the generation
available to a later attempt. Counting attempts per generation instead would
permanently strand a profile after one transient token-endpoint outage, which is
a worse outcome than the one the limit exists to prevent, and the daemon has the
evidence to tell that case apart.

**Replay after an ambiguous exchange is forbidden.** This is a separate rule and
not a qualification of the counting above, because it is the property the whole
protocol exists to protect: once any request bytes may have been written, a
connection loss, a redirect response, or an indeterminate response leaves the
rotation outcome unknown, and a token that may already have been rotated must
never be presented again. The daemon does not send again, whatever the attempt
count says, and follows the quarantine path below. No relaxation of the
attempt-counting rule reaches this one. A second transaction re-locks and
matches that generation, compares the account identity the response carries
against the one stored with that generation, persists the returned token, and
clears the marker before the new access token is used anywhere. An identity that
differs is not persisted: the generation quarantines and re-provisioning is the
only recovery. Why quarantine rather than adopt the new identity — dispatch
pairs each minted access token with the stored account identity to form the
CLI's per-account header, so silently keeping the old identity would send a
valid token under the wrong account, and silently adopting the new one would
re-scope a profile the operator declared for a specific account without the
operator saying so. Neither is a decision a refresh is entitled to make. A
definitely committed replacement overwrites the previous refresh token rather
than retaining it: a superseded token is unusable, and keeping one would only
preserve material whose sole remaining effect is to invalidate the live
authorization if it were ever replayed. If the exchange fails after possible
provider rotation, its persistence commit is ambiguous, or the daemon restarts
with the marker still present, it never replays the stored token. It first
rereads the durable generation: a committed replacement is adopted; an uncleared
marker quarantines the profile and requires re-provisioning. After a successful
replacement commit, the refresh task publishes the one in-memory access token to
every joined preparation. A definitely non-rotating failure first clears the
marker, then publishes its one typed result. An ambiguous exchange, ambiguous
commit, or refresh-task loss first commits quarantine from the retained marker,
then publishes that typed result and wakes every joiner. Cancellation follows
the same evidence boundary: before possible request bytes it is definitely
non-rotating, and afterward it is ambiguous. Process exit needs no durable
waiter: startup resolves the retained marker to replacement or quarantine before
admitting work, and a later preparation observes that durable result. No joiner
can wait past its own cancellation or the single-flight's one published terminal
result. Access tokens are held in memory. A clean restart discards them without
contacting any provider; the first later call preparation that needs a profile
lazily refreshes it. This keeps access tokens out of the database and preserves
configuration-independent recovery even when a token endpoint is unavailable.

Dispatch supplies each invocation a scratch credential home carrying the
complete authentication state the CLI needs to form a request, minus the refresh
token. That is the daemon-minted access token, the identity token the
authorization issued with it, and the non-secret account metadata harvested at
provisioning. The rule is stated as *completeness minus one exclusion* rather
than as a list of fields, because a field list is incomplete: a store holding
only an access token cannot form the per-account header, and one holding only
the access token and account identity still cannot supply the plan and
deployment-environment claims the CLI decodes from the identity token to choose
its request headers and routing. Whatever else that CLI's stored shape requires
and the daemon holds is included on the same terms; only the refresh token is
withheld, and withholding it is what permits the concurrency.

Every token written into a scratch home seeds the adapter's exact-value redactor
before it is written, not only the access token. An identity token is a bearer
credential for the same account, and one that reached a log or a debug rendering
would be exactly the disclosure the scratch-home discipline exists to prevent.
Dispatch is the only path that builds one. Scratch homes live beneath a single
daemon-owned `0700` root, are themselves `0700`, contain only daemon-owned
`0600` regular files, and are created and removed through descriptor-relative
operations that reject symlinks; normal completion removes the home before the
invocation returns. Before accepting work at every startup the daemon scavenges
every entry it can prove is an owned scratch home beneath that root, and an
ownership, type, or containment mismatch fails startup and removes nothing — so
a host or daemon crash can leave only effective-user-restricted residue until
the next startup, never an indefinitely trusted login store. Dispatch also
explicitly forces the CLI's file or ephemeral backend to that home while
disabling ambient, keyring, helper, and external stores. Failure to enforce that
selection is a typed pre-send delivery failure and starts no CLI child. The
access token is otherwise retained only in memory. The refresh token stays with
the daemon and is never copied into a scratch home. Withholding it is what
permits the concurrency: a CLI process holding a refresh token could decide to
refresh, so N concurrent processes could race exactly as they do under
`codex_home`. Holding none, they share no mutable authorization state, and the
daemon refreshes once under its row lock on behalf of all of them.

Before anything is written or any child starts, preparation seeds the CLI
adapter's exact-value redactor with **every token it is about to place in the
scratch home** — the access token, the identity token, and any further bearer
value that shape requires — not the access token alone. The identity token is
bearer material for the same account and the CLI reflects it, so seeding only
one of them leaks the other through provider-controlled output. How the adapter
installs and applies that scrub — the representations it covers, its behaviour
across output chunk boundaries, and where it sits relative to parsing and
persistence — is owned by
[the Codex CLI provider adapter](runtime-substrate.md#codex-cli-provider-adapter)
and is not stated here. A path that cannot install it fails preparation before
writing the scratch home or spawning the CLI, which is this contract's
obligation rather than the adapter's.

A daemon-minted access token can expire while a long invocation is still
running, and that is not an authorization failure. The daemon minted the token
and therefore knows its expiry, so an invocation whose credential lapsed while
running leaves the profile eligible for a later, separately authorized call with
a fresh token; it does not quarantine the profile. It does not automatically
retry the failed call, because token expiry does not by itself prove whether the
provider accepted that call. Why the distinction is stated rather than left to
classification: treating a mid-run lapse as a rejected credential would
quarantine a healthy account because it was given a long task, and automatic
repetition could duplicate an accepted request.

The daemon supplies `file` for the `anthropic` and `openai` direct-HTTP adapters
and for `claude_cli`; it supplies `ambient` for the `claude_cli` and `codex_cli`
process adapters, and `codex_home` for `codex_cli`. A Codex profile naming
`file` or `oauth` parses and is then rejected at startup as undelivered, on the
same principle as the capacity-dependent pool keys below — configuration whose
effect no surface provides is refused rather than admitted inert. The grammar
admits all four so that supplying one of the reserved Codex deliveries needs no
configuration contract change.

A refresh rejected as expired, reused, or revoked is permanent. The profile is
quarantined and re-provisioning is the only recovery, which is the same operator
command as first provisioning. Database restore is therefore an explicit
restore-workflow responsibility: before signalboxd may start against restored
state, that workflow transactionally quarantines every restored `oauth` profile.
An ordinary restart does not. This pre-start fence prevents dispatch from
attempting to resume a refresh chain the provider may already have moved past
without requiring the daemon to guess whether identical stored rows came from a
restore.

## Credential pools and selection

This build maps a model family to exactly one credential pool. Each implemented
`[[adapter_mappings]]` entry accepts exactly `model_family`, `adapter`, and
`credential_pool`; committed unimplemented workspace-instruction admission also
accepts exactly the all-or-none transport and byte-capacity pair defined in
[model capability configuration](#model-selection-validation). Every other key
is rejected (`apps/signalboxd/src/configuration.rs:706`). The pool must name one
declared `[[credential_pools]]` entry whose adapter agrees with the mapping's.

Selection happens for each model-call availability chain. Configuration parsing
derives the session's initial preferred reference, while preparation loads the
target's admitted pool and skips durable chain exclusions, pending next-turn
displacements, membership exclusions, and global quarantines. A `switch_now`
failure starts after the failed member and cannot select any member already
tried in that chain. The settings whose effect this build cannot supply are
typed startup failures rather than retained-and-inert — `round_robin`,
`least_used`, any `headroom_reserve_percent`, a non-`stay` `on_headroom_low`,
and a `switch_now` whose adapter cannot prove the cause. What each admitted
value is defined to mean is stated below, and what a selection attempt can end
as is owned by
[the credential-availability machine](credential-availability.md).

A credential pool is the set of profiles that may substitute for one another for
one model family. Its name is 1 through 256 UTF-8 bytes, unpadded, and NUL-free,
and it contains 1 through 1,024 members. These bounds keep the complete,
duplicated exhaustion evidence and authoritative policy read below the process
protocol's 8 MiB frame limit even under worst-case JSON escaping. Each
`[[credential_pools]]` entry carries:

- `name` — the exact pool key, unique in the document.
- `members` — a nonempty array of inline tables; the array-of-tables spelling
  `[[credential_pools.members]]` is rejected. Each names one declared `profile`,
  its `priority` within this pool as an integer from 1 through 4,294,967,295
  where a lower value is preferred, and an optional `headroom_reserve_percent`
  overriding the pool value for that member alone.
- `tie_break` — one closed value resolving equal priorities. This build admits
  `first_listed`; `round_robin` and `least_used` remain reserved spellings and
  fail startup because no durable cursor or capacity observation supplies them.
- `on_pool_exhausted` — one closed value, `park` or `fail`. This grammar admits
  the value and nothing more; what each one does is owned by
  [the credential-availability machine](credential-availability.md), where the
  value acts only by selecting whether an exhaustion parks: `fail` never parks,
  and `park` parks only while some member's every active exclusion is one a wake
  can clear.
- `headroom_reserve_percent` — an optional pool-wide integer from 0 through 99.
- the five closed trigger keys `on_quota_exhausted`, `on_rate_limited`,
  `on_overloaded`, `on_credential_rejected`, and `on_headroom_low`, each
  carrying one closed action. An omitted trigger key selects `stay`.

Priority is a property of the membership rather than of the profile. Why: one
account holds different ranks in different pools — first choice for interactive
work, last resort for batch — and a single rank on the profile cannot state
both. Priorities need not be unique or contiguous: equal values are exactly what
`tie_break` resolves, and gaps let a later profile take an intermediate rank
without renumbering the rest.

For rate-limit and overload failures, the credential-availability machine
creates an authorized same-member retry before applying the pinned action while
that member remains below the configured attempt bound. Provider-internal
failures use the same retry rule but have no trigger key; they terminalize at
the bound.

The five admitted actions are:

| Action               | Effect when its trigger fires for a member                                                                                                                                                                                                                                                                            |
| -------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `stay`               | The member keeps the session. A failure terminalizes as it would with no pool.                                                                                                                                                                                                                                        |
| `switch_next_turn`   | A failure terminalizes as it would with no pool; low headroom does not fail or replace the current turn. The next turn's preparation excludes this member.                                                                                                                                                            |
| `switch_now`         | The turn creates a successor attempt against the next admitted member ([model-call-execution](model-call-execution.md)).                                                                                                                                                                                              |
| `avoid_new_sessions` | Sessions with a prior completed call through the member keep it; preparation for a session without one on this pool excludes it.                                                                                                                                                                                      |
| `quarantine`         | The member is excluded from every selection, in every pool and across restarts, until an explicit operator command clears it — or, where an adapter offers one, until a zero-cost probe that calls no model reports availability. Never by a timer and never by a restart; the clearing rule is stated in full below. |

### Pool selection and trigger effects

Model-call preparation resolves a pool and persists the selected member and
immutable call-pinned policy. Once no same-member retry is authorized,
observation commit translates the closed quota, rate-limit, and overload
classifications through that frozen policy and applies `stay`,
`switch_next_turn`, `switch_now`, `avoid_new_sessions`, or `quarantine`. The
durable trigger effects below are what this build appends and reads.
Capacity-derived selection remains committed unimplemented functionality where
called out below, and so does the whole exclusion lifecycle — reset-aware
expiry, operator clear, probe recovery, action-head generations, correlation
coalescing, and origin-aware clearing — which
[committed unimplemented functionality — credential-exclusion lifecycle](#committed-unimplemented-functionality--credential-exclusion-lifecycle)
states in full.

#### Durable trigger effects

Each adapter maps native terminal evidence to a closed `ProviderErrorKind`, and
model-call execution consumes that typed evidence without reinterpreting prose.
The observation transaction stores the frozen action together with its exact
correlation; preparation reads the resulting chain exclusion, pending next-turn
displacement, membership exclusion, or global quarantine. `stay` writes no
action row.

`switch_next_turn` creates a durable pending displacement scoped to the session,
pool-policy snapshot, member, and exact source turn that observed the trigger.
It survives restart and is ignored by every later preparation inside that source
turn, including a tool-round continuation. The first distinct turn in the
session that prepares against that pool excludes the member; the transaction
that successfully prepares that turn through another member consumes the
displacement. If no other member is admissible, the displacement remains pending
while the pool's exhaustion policy runs; it cannot expire merely because a
preparation found no successor.

An `avoid_new_sessions` exclusion is durable and scoped to the membership that
observed it, not to every pool containing the profile. Nothing in this build
ends one: no reported reset expires it, no restart clears it, and no clear or
probe path exists to release it. The session test in the table is evaluated
against that pool: a completed call through another pool does not make this
member sticky or exempt from the exclusion.

Every trigger action is a derived effect of its exact classified observation.
The observation-commit transaction atomically stores the terminal observation
and any profile quarantine, pending displacement, or membership exclusion it
causes. The correlation runs one way only: an action record names the
observation that caused it and cannot exist without it. It does not run the
other way. `stay` is the action every omitted trigger key selects, and it
creates no quarantine, no displacement, no membership exclusion, and no chain
exclusion. An action that creates no durable state writes no record, and the
observation commits alone; requiring a record from it would make the default
configuration unable to commit a terminal observation at all, so no turn under
it could ever terminalize. No generic applied-action row exists for that case
either, because a row recording that nothing happened would be written on every
ordinary failure and read by nothing. Delivery-layer quarantine that occurs
before a provider request instead names its own typed refresh or credential-home
failure and commits that evidence atomically with quarantine.

Two sessions can observe the same trigger for one profile at the same moment,
and their session-scheduler locks do not serialize that. Every transaction that
writes an exclusion therefore first takes the affected profile's action-head
lock — keyed by the profile reference alone, because quarantine and membership
exclusion are global to the profile rather than to one pool — after its session
scheduler lock, before any bounded-profile capacity row or policy cursor row,
and in profile-reference byte order when it touches more than one. Each
observation appends its own durable row naming the exact observation that caused
it, so a repeated trigger can never make a uniqueness conflict block the
terminal observation that requires it, and two observations excluding one member
still leave one excluded member at selection.

Preparation is the other side of that race and joins the same protocol. Before
it reads any member's exclusion state, it locks the action head of every member
of the policy it may select, at the ordering position and in the modes
[persistence protocol](persistence-protocol.md) fixes, and holds those locks
through the `Prepared` insert. The modes are not restated here, because they are
not uniform across members: a preparation writes the exclusion state of any
member whose pending displacement it consumes, and reads the rest. The share and
exclusive modes conflict, so one of the two transactions waits: a call is either
prepared before the exclusion commits or prepared against a member it has
already observed as excluded. Without this rule selection takes no lock the
exclusion writer takes — an unbounded `first_listed` member acquires neither a
capacity row nor a cursor row — and a preparation that read a member as
admissible could then dispatch a provider request on a credential quarantined in
the interval.

`switch_now` is admitted only for `on_quota_exhausted`, `on_rate_limited`, and
`on_overloaded`, because only those causes carry proof that the request was not
accepted. Selecting it for `on_credential_rejected` or `on_headroom_low` is a
typed startup failure: a rejected credential is deployment misconfiguration that
substitution would hide, and low headroom is not a failure at all. The
`on_credential_rejected` trigger classifies rejection of an issued provider
request. A rejected daemon-owned OAuth refresh occurs in the delivery layer
before any such request, is typed as its own refresh failure by the daemon that
performed the exchange, and bypasses pool trigger policy: it quarantines the
profile unconditionally as specified above.

A `codex_home` refresh race is deliberately not given that bypass. The Codex CLI
reports one undifferentiated authentication failure — the adapter classifies
from rendered message text and collapses refresh-token phrases, `unauthorized`,
and invalid keys into the same `CredentialRejected` kind — so nothing can tell a
lost refresh race from an ordinary rejection of an issued request. Every
`codex_home` credential rejection therefore follows one policy, the pool's
configured `on_credential_rejected`, and a deployment that wants a refresh race
to quarantine configures `quarantine` there. Splitting the branch on evidence
the adapter cannot produce would force an implementation to either quarantine
ordinary rejections against `stay` or miss the race entirely. A future typed
refresh-failure variant from that adapter would let the delivery-layer bypass
apply here too.

`switch_now` is further admitted only where the pool's adapter can supply the
typed non-acceptance proof for that exact trigger's cause. Every pool's members
already agree on one adapter, so the check is per adapter *and* per trigger, not
once per adapter.

Two conditions make a response carry that proof; a successor authorized without
either is a second paid call for a request the provider may already have
performed. First, the adapter must have decoded its own documented error
envelope and the decoded native token must name that cause in the adapter's
exhaustive mapping; a status-derived fallback carries no proof. Second, the
response must be a *pre-stream error response*: an error-status exchange whose
body is that envelope, decoded before any stream began. **An SSE error record
never carries the proof, whatever native token it holds.** Both in-repository
stream decoders classify a mid-stream `error` record through the same
native-token mapping they use for an error response, so the token is identical;
what differs is that by the time such a record arrives, `message_start`,
content, reported usage, or a finish token has already been observed, and the
provider has demonstrably accepted and begun processing the request.
Non-acceptance is exactly what that disproves. A mid-stream or post-finish
availability failure therefore stays an ordinary terminal known failure and
authorizes no successor. For HTTP adapters this admits exactly `on_rate_limited`
and `on_overloaded` for an `anthropic` pool, and `on_rate_limited` and
`on_quota_exhausted` for an `openai` pool. `on_quota_exhausted` under
`anthropic` and `on_overloaded` under `openai` are typed startup failures
because those adapters' mappings carry no native token for those causes and can
reach them only by status-derived fallback, which carries no proof. Claude Code
exposes no machine-readable terminal envelope, so `switch_now` on a `claude_cli`
pool is rejected for all three triggers. Codex is different: rendered failure
text supplies the narrower cause, but the JSONL `turn.failed` lifecycle envelope
independently proves the request ended without a successful stream. The Codex
adapter therefore admits all three availability triggers and never authorizes a
successor from a malformed, incomplete, or contradictory event stream. Why
reject an unprovable pair rather than accept and ignore it: a configured
`switch_now` that can never fire reads as failover the deployment does not have.

#### Pool-based preparation

Preparation selects from the admitted members and every availability successor
starts immediately after its failed predecessor in policy order. Each call
stores the policy and member it used; every chain exclusion, delayed successor,
next-turn displacement, membership exclusion, and quarantine survives restart.
Capacity reservations, `round_robin`, `least_used`, and parking remain committed
unimplemented functionality; admission rejects capacity-dependent choices this
build cannot observe.

Selection happens at model-call preparation, never at session creation. For the
resolved target's family, preparation reads the session's current immutable
pool-policy snapshot from its credential history. It first selects the sticky
member when that member remains admissible. Without an admissible sticky member,
it traverses members in priority order, skipping any excluded by an action
above, and breaks a priority tie by the snapshot's rule. `round_robin` owns one
durable global cursor per immutable pool-policy revision and priority value. The
repository interns the policy's complete canonical structural value — pool name,
ordered members, each member's expected adapter and delivery kind, membership
settings, tie-break, exhaustion rule, and trigger actions — under a uniqueness
constraint on that value. An unchanged document therefore reuses the same
immutable surrogate revision across restarts, while any changed field creates a
new revision; a later exact reversion reuses the old one. Hashes may accelerate
lookup but never establish equality without comparing the complete value. Every
session history entry that copied that validated revision refers to the same
cursor, rather than creating a session-local one. When that rule must choose
among two or more admitted equal-priority members, the cursor names one member
ordinal in that priority's relative declaration order. Selection starts there
and walks that declared order cyclically, skipping each inadmissible member,
until it finds the first admitted member; it never renumbers or indexes into a
filtered member list. The transaction that commits the selected call's
`Prepared` record advances the cursor to the next declared member of that same
priority after the selected member, wrapping even when that next member is
currently excluded. Before reading a cursor or choosing by it, preparation locks
that policy-and-priority cursor row `FOR UPDATE` after its session scheduler,
the candidate members' action heads, and any candidate bounded-profile capacity
rows, then rereads the cursor and the admissibility facts protected by those
locks. The same transaction selects, inserts `Prepared`, and advances the locked
cursor; no path acquires a capacity row while holding a cursor row. A priority
with no admitted member cannot select; selection continues according to the
pool's contention and exhaustion rules. A failed preparation advances nothing;
restart preserves the cursor. Stickiness and a sole admitted member require no
tie-break and do not advance it. Stickiness needs no separate durable state:
preparation prefers the member the session's most recent `Prepared` call on that
pool pinned, including a call that later failed under `stay`, so a session stays
on one account until a trigger displaces it. When the pool admits no member,
which ending the attempt reaches is owned by
[the credential-availability machine](credential-availability.md). Quarantine is
durable and scoped to the profile rather than to the pool that observed it,
because a rejected credential is a property of the account: a profile ranked in
two pools is excluded from both. It is cleared only by an explicit operator
command, or by a probe that costs nothing and calls no model where the adapter
offers one — never by a timer, since a revoked credential does not heal on a
schedule, and never by a restart. Why an operator command rather than
rediscovery: for a `codex_home` or `oauth` profile the repair is an interactive
re-authorization the operator performs, so the operator knows the moment it is
fixed, and rediscovering it instead would spend a real model call to learn what
they could have said. Reading a quarantine record is never on the recovery path
for acknowledged work, so INV-034 is unaffected.

The exact future operator-clear request, target correlations, replay behavior,
and receipt are owned by
[credential-exclusion administration](process-protocol.md#planned). No present
process or application surface implements that request, so every explicit-clear
path above remains committed unimplemented functionality and no indefinite wait
can presently be released.

A session's credential history event carries a complete family-to-pool-policy
snapshot. Each immutable policy includes the pool name, ordered members, every
member's expected adapter and delivery kind, their membership settings,
tie-break and exhaustion rules, and all trigger actions; preparation never
resolves that snapshot through the current document's pool table. Before
credential resolution, preparation requires the selected member's frozen adapter
to equal the resolved target's adapter and requires the current profile
registration to retain both that adapter and delivery kind. Absence or a
mismatch is a typed pre-send credential-configuration failure, so reusing a
profile name cannot route another adapter's or delivery's credential through an
old policy. Each model call pins both the exact profile that authenticated it in
`model_call.credential_reference` and the selecting immutable `pool_policy_id`
at the `Prepared` insert. Observation commit reloads that call-pinned policy,
never the session's later credential-history head, before deriving any action. A
historical read therefore still resolves that call's billing kind and rates from
the reference the call itself pinned, whatever selection chose it, and a pool
edited across a restart can neither broaden an existing session's admitted
credentials nor relabel a stored call.

What this build presently delivers differs from that record in one way.
Preparation selects among the admitted members and a qualifying failure rotates
the chain, so a multi-member pool does not behave as its preferred member alone.
What is missing is the interned immutable policy identity: a session's
credential history event stores the preferred reference rather than the complete
policy, so only an intra-chain successor reloads its predecessor call's frozen
policy while a fresh availability chain resolves the pool from the current
document. A pool edited across a restart can therefore change which members an
existing session admits, and call-free exhaustion records the pool name rather
than the exact per-member evidence that proved it. Both gaps close with the
`pool_policy_id` record described above and remain committed unimplemented
functionality until then.

Two families of one adapter may prefer different profiles wherever that adapter
resolves the session-pinned reference from a complete adapter-scoped catalog —
both direct HTTP adapters and `claude_cli` do. Only `codex_cli` carries a single
reference into its runtime, so two `codex_cli` families resolving to different
profiles is a typed startup failure rather than a silent pin of whichever parsed
last. A profile declared for another adapter is unmapped in every case, even if
a later configuration routes the same model family through this adapter.

Admission is fail-closed. Startup rejects a pool with no members, a duplicate
member profile, a member naming an undeclared profile, a mapping naming an
undeclared pool, members disagreeing on adapter, a priority outside the integer
range 1 through 4,294,967,295, an unknown tie-break or exhaustion value, an
unknown action, an action on a trigger that does not admit it, and any unknown
field. It also rejects `headroom_reserve_percent`, `tie_break = "least_used"`,
and any `on_headroom_low` action other than `stay` in this build, because no
composed runtime observes remaining capacity, and `switch_now` on any
adapter-and-trigger pair whose adapter cannot prove non-acceptance for that
cause — every trigger under `claude_cli`, `on_quota_exhausted` under
`anthropic`, and `on_overloaded` under `openai`. The admission gate is the
capacity report alone, so an adapter that reports capacity admits `least_used`
and a headroom reserve with no further parser change; an adapter that reports
capacity must therefore also define the normalized quantity, observation
lifetime, and deterministic secondary tie-break. Why: a configured reserve or
selection rule that silently never fires — or whose metric varies by
implementation — would read as protection the deployment does not have. The keys
are admitted by the grammar so that supplying that later contract needs no
configuration grammar change; the observation itself is routed through
[model fallback and provenance](../open-questions.md#model-fallback-and-provenance).

A one-member pool is the ordinary single-account deployment and requires no
trigger keys, since no member can succeed another.

### Committed unimplemented functionality — credential-exclusion lifecycle

Durable exclusions are appended and read; no present composition expires,
coalesces, or clears one. The runtime writes one bare durable action row per
observation — pool, member, action kind, the observing session, turn, and model
call, and the classified cause — and the only row it ever updates is a
`switch_next_turn` displacement that a later prepared call on that pool
consumes. The four topics below state the lifecycle that record is meant to
carry; none of them describes behavior available from this build.

- **Reset-aware exclusion expiry.** A membership exclusion is reset-aware: a
  reported reset time clears it when that time passes, and only an exclusion
  carrying no reported reset is indefinite. Attaching a correlation also
  accumulates its reset evidence, because a reset-aware exclusion clears itself
  when its reported reset passes and a generation carrying a stale deadline
  would re-admit a member the provider is still refusing. The generation's
  effective reset is the latest reset any correlation attached to it reported,
  and an observation reporting no reset makes the generation indefinite.
  Indefinite is absorbing: once any correlation reported no reset, a later
  correlation carrying one does not restore a deadline, since the observation
  that reported none is evidence the provider named no recovery time. No present
  durable record carries a reported reset beside an exclusion and no present
  composition expires one, so every exclusion this build writes is indefinite in
  fact, whatever its observation reported.

- **Operator clear and probe recovery.** An explicit operator clear may remove a
  pending `switch_next_turn` displacement or an `avoid_new_sessions` exclusion,
  exactly as it clears a quarantine, and only an operator clear, an availability
  probe that costs nothing and calls no model, or another durable availability
  update ends an indefinite generation. The clear request itself is owned by
  [credential-exclusion administration](process-protocol.md#planned) and
  described under [pool-based preparation](#pool-based-preparation); no present
  process or application surface implements it and no composed adapter exposes
  such a probe, so no exclusion this build writes is ever cleared.

- **Action-head generations and correlation coalescing.** Each profile carries a
  durable action head, and every transaction that mints, activates, or clears an
  exclusion rereads the current generation under that head's `FOR UPDATE` lock.
  The first commit mints the generation. A later commit for an exclusion already
  active at the same scope **and of the same origin** records its own
  observation correlation against that existing generation and mints no second
  one, so a repeated trigger is idempotent on the exclusion. An operator clear
  takes the same lock, which is what lets a clear and a concurrent
  re-observation agree on which generation is current. No present schema stores
  a generation, an activation, or a second correlation against an existing
  exclusion: this build appends one independent row per observation under the
  profile lock stated above, which keeps a repeated trigger from blocking its
  terminal observation but records no generation a clear could name.

- **Origin-aware clearing.** Origin is part of the coalescing key because the
  clear protocol decides administrability from it — a policy-origin quarantine
  is clearable by operator command while an OAuth delivery-origin one requires
  re-provisioning
  ([credential-exclusion administration](process-protocol.md#planned)).
  Coalescing across origins would produce one generation with two contradictory
  answers, so a delivery-origin failure against a profile already carrying an
  active policy-origin generation mints its own, and the two are cleared, and
  reported, separately. No present durable row records an origin at all — it
  carries its classified provider cause and nothing more — and both
  credential-home deliveries that could raise a delivery-origin exclusion are
  themselves reserved, so nothing in this build can distinguish the two.

## The static session-template catalog

The file named by `SIGNALBOX_TEMPLATE_CONFIG_FILE` is a separate versioned TOML
document (`config/session-templates.example.toml` is the checked-in example). It
is read at startup, after the model catalog, and re-read on
[configuration reload](#configuration-reload). Its root requires exactly
`version = 1`, an optional array of `[[templates]]` tables, and one optional
`[review_library]` table; a version-only document is a valid empty catalog.
Unknown root and nested fields, a mistyped templates or review-library value,
duplicate names, and every invalid field fail as precise sanitized
`SessionTemplateConfigurationError` variants without including file paths,
prompt content, or document text.

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

**Committed unimplemented functionality — template instruction selectors.** An
ordinary template accepts one optional `instruction_selectors` array containing
at most 256 inline tables. Each table has exactly `root`, `source_path`, `kind`,
and `source_sha256`, plus `configured_root_id` exactly when
`root = "configured"`. `root` is `"workspace"` or `"configured"`; the configured
identity and source hash are 64 lowercase hexadecimal characters encoding 32
bytes; `kind` is `"agent_document"` or `"agent_skill"`; and `source_path` is 1
through 4,096 UTF-8 bytes of nonempty normal components separated by single `/`
characters, with no leading or trailing slash or U+0000. The configured identity
is the path-derived `ConfiguredInstructionRootId` above. An absent or empty
array means no selector and therefore no eligible bundle.

The loader rejects duplicate selectors and canonicalizes them by root
(`workspace` first), configured-root digest bytes when present, raw UTF-8 source
path bytes, kind (`agent_document` first), then source-hash bytes. The immutable
resolved template bundle retains that ordered sequence, and session creation
copies it unchanged as unresolved eligibility input.

Content-digest version three is selected by the template's parsed shape, not by
whether its selector sequence turned out to be nonempty. A template whose
`instruction_selectors` key is present uses version three, including when the
array is explicitly empty, in which case it writes a selector count of zero and
no records. A template with no `instruction_selectors` key keeps version two
unchanged, which is what stops every existing selector-free template from
changing digest. Generated review templates carry no such key and therefore stay
on version two. Absent and explicitly empty are deliberately different digests
for the same effective eligibility, because the digest authenticates what the
template document said rather than what it amounted to.

Version three retains the version-two frames below except that its first frame
is `signalbox/session-template/content-digest/v3`; after the model-settings
digest it writes the selector count as eight unsigned big-endian bytes, then
each canonical selector record. Every variable-length field in a record is
length-framed, and the fixed-width ones are written raw, so the record is
uniquely decodable and two implementations cannot hash one selector differently.
In order: the length-framed root spelling; for `configured` only, the 32 raw
configured-root digest bytes; the length-framed exact source-path bytes; the
length-framed kind spelling; and the 32 raw expected source-hash bytes. A length
frame is eight unsigned big-endian bytes followed by exactly that many bytes,
matching the selector count above and the frames version two already uses; raw
concatenation is not an admissible reading of any of the three variable-length
fields. Thus templates that differ only in selectors have different provenance.
Generated review templates carry the empty resolved sequence and no
`instruction_selectors` key, so they keep the version-two digest. No present
parser admits `instruction_selectors` and no resolved bundle retains it; the
implemented digest is version two, with the stable vector below.

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
request, copied model-settings snapshot, system prompt, and dangerous-tool
blanket. Its content digest is domain-separated SHA-256 over length-framed
canonical values. Each frame is an unsigned 64-bit big-endian byte length
followed by that many exact bytes. The frames, in order, are: ASCII
`signalbox/session-template/content-digest/v2`; the template version as eight
unsigned big-endian bytes; ASCII `direct` or `alias`; the selected UUID as its
16 network-order bytes; ASCII `disabled` or `approve_all`; the exact UTF-8
prompt bytes; and the 32-byte canonical model-settings digest described below.
The name and source form are excluded: an inline and file-backed prompt with the
same version and bundle have the same digest, while changing any copied value or
the template version changes it. The stable vector for version 7, alias
`30000000-0000-4000-8000-000000000003`, `ApproveAll`, and prompt
`Review the change and report concrete findings.`, with provider-default model
settings is hexadecimal
`88de5be79c6130058e68541d508a2ceabe99e25331bd24a9fdc4a9e34d34d8ba`. The daemon
exposes only sorted name/version summaries to clients; clients never receive
prompt text or parse this file.

The canonical model-settings digest uses the same framing. Its first frame is
ASCII `signalbox/model-settings/snapshot-digest/v1`. For each precedence layer
in `per_call`, `session`, `profile`, `global_default` order, it then frames that
layer name followed by its reasoning, fast-mode, and service-tier contribution.
Reasoning uses `inherit`, `provider_default`, or the lowercase domain level;
fast mode uses `inherit`, `disabled`, or `enabled`; and service tier uses
`inherit`, `provider_default`, or its lowercase provider and value separated by
`:` (the Codex CLI provider tag is `codex_cli`). The final frame is
`unbound_provider_defaults`, or `validated_selection` followed by the validating
direct selection's 16 network-order UUID bytes. Resolved values and source
labels are not repeated because the admitted snapshot derives them uniquely from
this complete precedence chain.

Review orchestration retains a second digest for each generated template. It is
domain-separated SHA-256 over the same unsigned-64-bit length framing. Its
frames, in order, are ASCII `signalbox/review-template/orchestration-digest/v2`;
the exact stage or concern key; the source version as eight unsigned big-endian
bytes; ASCII `direct` or `alias`; the selected UUID's 16 network-order bytes;
ASCII `disabled` or `approve_all`; SHA-256 of the exact shared-header bytes;
SHA-256 of the exact body bytes; and the generated template's 32-byte ordinary
content digest. The key frame makes equal prompt bytes used for different stages
or concerns distinct orchestration inputs. The content-digest frame binds the
copied model-settings snapshot as well as the assembled prompt. This
orchestration digest does not replace the ordinary content digest: template
provenance uses the complete assembled bundle digest above, while the immutable
orchestration attempt uses the header/body/key-aware digest.

Creation by template name first consults the user-global durable-command
registry by command identity. An existing create-session claim is reconstituted
and compared using the caller-supplied creation mode and template name before
the current catalog is consulted; an equal replay returns its stored session,
including when that name is absent or changed in the current catalog. Only an
unclaimed command identity resolves against the currently loaded catalog and
copies the complete bundle into the session's immutable defaults version one.
The session separately records the template name and ordinary content digest; it
retains no live catalog reference. Generated review templates follow this same
copy-on-create path: the complete assembled prompt, model selection, approval
blanket, reserved name, and content digest become immutable session evidence. An
edit therefore takes effect at the next restart or reload and affects only
creation commands first handled under the new catalog. Equal replay of an
already handled command and template name returns the original copied session
rather than comparing against the current bundle (INV-047).

Why: a separate file lets operators change the reusable creation surface without
mixing it into immutable model-identity definitions, while one load boundary
keeps validation fail-closed. Config-relative paths make a catalog directory
portable; explicit `$HOME/` supports machine-local long prompts without
committing a machine's absolute home path. Copying preserves ordinary defaults
epoch authority and makes configuration edits forward-only.

## Model-selection validation

Validation happens at two boundaries, on frozen semantic meaning only —
credential presence is never consulted (INV-008):

- **At session creation.** The requested direct model or alias must resolve
  through the static table. Absence is a typed rejection carrying that exact
  `ModelSelectionRequest`; the process protocol projects it to its existing
  `InvalidRequest` result without a protocol change.
- **At acceptance.** `SubmitInput` freezes the requested selection into the
  turn's effective configuration. A direct selection freezes without catalog
  consultation. An alias request consults an acceptance-time definition
  resolver; an unknown alias is a recorded `UnknownModelAlias` rejection, not an
  error. The live process runtime supplies the immutable `HubModelConfiguration`
  alias catalog to the acceptance transaction. These model-selection freeze
  semantics are this page's material; the surrounding input-delivery lifecycle
  is [turn-lifecycle-and-scheduling](turn-lifecycle-and-scheduling.md) scope.
  **Committed unimplemented functionality — retained-region acceptance check.**
  No present surface admits a workspace-instruction region, so no present
  acceptance transaction performs the check described in the rest of this
  bullet. Once workspace-instruction admission exists, every origin-creating
  acceptance transaction resolves the frozen selection against the live
  immutable catalog and rejects the origin before freezing it when the target
  that will actually serve the turn lacks typed-system transport or byte
  capacity for the session's complete retained region. The check belongs to
  origin acceptance as such, not to `SubmitInput`: goal attach, goal resume, and
  scheduler continuation mint accepted origins without a `SubmitInput` call, and
  an origin minted by any of them would otherwise freeze an incapable target and
  fail before provider spawn — precisely the restart-after-retargeting case this
  check exists to prevent. That is also what
  [sessions-and-transcript](sessions-and-transcript.md) promises for every later
  origin. The subject of that check is the effective serving record the frozen
  settings select, not the named direct model: when the frozen overlay enables
  fast mode on a model whose `fast_mode` is `alternate_target`, the check is
  applied to the `fast_target_id` serving record that execution will pin, and to
  its adapter mapping. Each serving target declares transport and capacity
  independently, so validating only the direct target would admit an input that
  fails later, before provider spawn. Where the frozen settings leave the
  effective record undetermined at acceptance, every record the frozen selection
  may still pin must satisfy the check. This check runs even when no defaults
  replacement occurred, so restart or configuration retargeting cannot strand an
  admitted session. A direct selection receives the same check against the
  serving record its own frozen settings select. The typed rejection accepts no
  input, creates no turn, and changes neither defaults nor admissions.
- **At execution.** When the attempt pins its target, the frozen selection is
  resolved against the `ModelTargetCatalog`. An unresolvable selection fails the
  turn as a known failure before any model call exists; a credential or send
  failure occurs only after the call exists. Why: keeping configuration absence
  distinct from provider failure, with no silent model substitution, is what
  INV-018 requires. Lifecycle detail is
  [model-call-execution](model-call-execution.md) material.

Each accepted origin retains the selection frozen from its defaults epoch.
Replacing session defaults imposes no same-adapter restriction: any selection
admitted by the immutable catalog may become the next epoch. The first
subsequent turn resolves its target through the same static table and selects
the latest session credential snapshot entry for that target's family. A
prepared or in-flight predecessor retains its call pin (INV-046).

Dollar cost is derived only while reading a terminal call: the call's pinned
target, the channel pinned on it, and its execution timestamp select the one
configured rate window covering that instant, and the exact credential profile
stored on that call selects `api_metered` or `subscription`. An API-metered
profile produces `real`; a subscription profile produces `metered_equivalent`,
regardless of adapter kind. A timestamp covered by no configured window, missing
historical profile declaration, call with no present usage axis, or historical
call whose input/cache semantics predate the durable pin produces no dollar
figure rather than zero. Codex CLI's reported `input_tokens` includes its
reported cache-creation and cache-read breakdowns. Derivation therefore applies
the ordinary input rate only when both cache breakdown axes are present and can
be subtracted from total input; an omitted breakdown leaves ordinary input
unreported while any independently reported output or cache axis remains
priceable. Each cache rate is applied once. The channel and that inclusive-input
meaning are pinned on the call when it is prepared, and every call this build
prepares pins channel `api`, so a later configuration restart or reload that
reuses the target with another adapter cannot reinterpret historical usage. A
cache breakdown larger than total input yields no figure. A credential update
that advances the session head cannot relabel an earlier call because that call
retains its original profile pin. Deployment keeps one profile name's billing
meaning stable and uses a new name when an authentication update changes that
meaning. The parser cannot detect a same-name semantic rewrite across
configuration restarts; such a rewrite would relabel historical reads and is
invalid deployment evolution.

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
  at the adapter boundary. Why: value rotation preserves the stable name so no
  record or log ever needs the secret (INV-035). The two integration constants
  are `brave-search-primary` and `github-primary`. Every model-provider
  reference is an operator-chosen profile name: because a mapping names a pool
  rather than a profile, no build-provided constant is compared against any
  model-provider name, so an `anthropic` or `openai` profile may be called
  whatever the deployment calls its account. `codex-subscription-primary` and
  `claude-subscription-primary` are the defaults a CLI runtime falls back to
  when its mapping names nothing else, and are not enforced.

- **File-based supply, reread per preparation.** Each `FileCredentialAccess`
  instance binds one consumer-scoped map of references to deployment paths. A
  model adapter receives the complete catalog of that adapter's `file` profiles,
  built from the profile catalog at startup — the direct HTTP adapters at
  `apps/signalboxd/src/main.rs:1070`, and Claude CLI at
  `apps/signalboxd/src/configuration.rs:1454` — while web search and code-host
  operations each receive a singleton map under their fixed integration
  constant.

- **Consumer scoping and reread, both shapes.** A model-profile name equal to an
  integration constant remains a distinct reference in a different consumer's
  map; no lookup or insertion crosses those boundaries. The selected instance
  reads the file for every model call, web search, code-host operation, or
  pull-request tool operation preparation that resolves one; nothing is cached.
  Why: atomic file replacement rotates any credential without restarting
  signalboxd, and an in-flight operation keeps the value it authenticated with.
  Resolution stays reference-scoped: a reference absent from the map fails typed
  `Unmapped`; a missing file is `Unavailable`; an unreadable file is
  `Unreadable` — all reference-only errors, so a failure names an account
  without disclosing which path served it.

- **External CLI logins.** An `ambient` profile leaves login resolution to the
  CLI under the adapter's existing child-environment contract. A Codex
  `codex_home` profile instead names the login directory the selected member's
  child receives as `CODEX_HOME`, and the daemon retains that path as a
  reference only. An `oauth` profile is validated and then rejected before
  anything about it is retained, so what that channel would require of the
  daemon is stated under
  [committed unimplemented functionality](#committed-unimplemented-functionality--credential-lifecycle).
  Whether two profiles denote two independent logins is neither promised nor
  assumed by this inventory: it is
  [one property with a per-delivery disposition](#distinct-members-are-distinct-authorizations),
  established for some deliveries and required of the deployment for others. The
  adapter invents no credential-value shape of its own. The profile's configured
  billing kind labels derived cost; adapter kind and delivery do not.

- **The value is the file's bytes less trailing line termination.** The read
  drops trailing `\n` and `\r` bytes and retains every other byte exactly,
  including leading and interior whitespace. Why: the tools that write a
  credential file — `gh auth token`, `op read`, `pass`, a shell redirect —
  terminate the line they print, so the terminator is how the file ends rather
  than part of the secret; without this, a routine deployment step would produce
  a value no HTTP header could carry. The narrowing happens once, at the file
  channel, so every adapter and the redaction scrub all see the same value. A
  file holding nothing but termination narrows to an empty value, which the
  adapter boundary then refuses exactly as it already refuses an empty file;
  narrowing never invents a credential.

- **No provider or integration startup preflight.** signalboxd never reads a
  provider or integration credential file at boot, so a missing or unsynced one
  cannot block startup or the recovery scan. Why: recovery of acknowledged work
  must not depend on provider or integration authority (INV-034). The explicit
  static credential for a currently routed S3 blob store is the sole exception:
  it is read only after the configuration-independent recovery scan, and its
  authenticated namespace and lifecycle probes gate socket admission and
  scheduling as the blob-storage contract requires.

- **Session credential history.** First handling of every native or imported
  session-creation command appends event ordinal 1 to that session's credential
  history in the same transaction as the session. In this build that event
  carries a complete, nonempty family-to-*reference* snapshot copied from the
  validated mapping table. Record and entry rows are append-only; a guarded head
  names the current event, and model-call preparation reads the latest entry for
  the resolved target's family. Equal command replay returns the recorded
  session without consulting the current table, so a configuration edit never
  silently re-resolves an existing session's credentials.

- **Legacy migration fallback.** Sessions that predate this history carry a
  `migration_backfill` creation event holding the single previously composed
  `anthropic` / `anthropic-primary` pair. While that event remains a session's
  current one, an Anthropic route may resolve through that durable legacy entry
  even when the configured family is named differently; a Codex route never may.
  Preparation falls back to the migrated entry when the resolved family has no
  entry of its own. A later explicit credential event ends the aliasing for that
  session, because resolution then uses only the complete latest snapshot.

- **Resolution timing.** Each direct HTTP adapter resolves the durably pinned
  reference during send preparation — after the durable `Prepared` record,
  before send authorization — and scopes the resulting value to that request
  (INV-002 boundary type). An ambient CLI operation validates its pinned
  external-login reference and prepares the process capability without reading a
  credential value. The shared cancellation contract for preparation and
  execution is owned by [model-call-execution](model-call-execution.md). A
  code-host tool resolves its fixed `github-primary` reference only after the
  durable tool attempt is authorized `InFlight` and immediately before its typed
  transport call; no model argument, client, or runner can select or receive the
  credential. The pull-request suite follows the same timing with its fixed
  GitHub API egress policy.

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

- **Durable references only.** Postgres stores no credential value in this
  build. Every credential is reference-only.

- **Durable credential references.** Each model call durably pins its non-secret
  credential reference at the `Prepared` insert
  (`model_call.credential_reference`), immutable thereafter under the
  authorization-facts trigger; the column is total (`NOT NULL` and non-empty),
  because every insert writes it and no database predates the stack. Resuming a
  stored `Prepared` call re-supplies the stored reference. Tool attempts store
  neither integration references nor values: the immutable compiled code-host
  declaration selects `github-primary` again when execution resumes. Why the
  exception is exactly this shape: a `file` credential has an external source of
  truth, so storing a copy would create a second one and violate the
  one-source-of-truth rule below. A rotating refresh token has no external
  source of truth available — the provider replaces it on every use, and no
  operator edit can produce the current value — so the store that holds it *is*
  the source of truth, and there is nowhere else for it to live.

### Committed unimplemented functionality — credential lifecycle

No present composition supplies any of the topics below.

- **Pool-policy credential history.** No present repository stores a
  family-to-pool-policy snapshot or migrates an existing family-to-reference
  entry; the reference behavior above is what this build does. The pool-policy
  history replaces that snapshot with the complete immutable policy and includes
  a one-time backfill of existing entries. Because a stored policy names members
  by profile reference, the snapshot must also freeze each member's adapter and
  delivery kind and compare them against the current profile registration before
  credential resolution: without it, editing a profile's `adapter` or `delivery`
  would silently re-point a historical session's stored member at a different
  contract, which is exactly what the replay rule above promises cannot happen.
  A disagreeing or absent registration must block scheduling the same way a
  missing historical registration does.

  The migration of the family-to-reference history this build has already
  written is deterministic, and the replay guarantee above fixes what it must
  be: an upgrade may not change which credential an existing session resolves.
  Each existing entry therefore becomes a **singleton policy retaining exactly
  the stored reference** — one member at priority 1, no headroom reserve,
  `first_listed`, [`on_pool_exhausted = "fail"`](credential-availability.md),
  and `stay` for every trigger — which reproduces the one-account, no-failover
  behavior that entry already had. Expanding the entry to whatever pool the
  document now maps that family to is the one thing it must not do: that would
  grant a historical session members it never had, which is precisely the silent
  re-resolution the replay rule forbids, and choosing any other policy would
  discard the reference the session pinned.

  The two profile-owned fields that frozen membership needs — adapter and
  delivery kind — come from the validated registration of the profile the entry
  names, and from nowhere else: not from the document's current family mapping,
  and not from its pool table. A reference naming no current registration cannot
  be migrated, and blocks scheduling rather than being guessed at or dropped —
  the same failure the freeze rule above produces, for the same reason.

- **CLI login channels.** `oauth` is reserved and is rejected as
  `UndeliveredCredentialDelivery`. `codex_home` is not reserved: the child
  process receives a validated path reference through a per-process
  `CODEX_HOME`, and the daemon never reads, copies, or logs the authentication
  material beneath it. Admitting `oauth` must invert the home-owned boundary:
  the daemon must hold the rotating authorization itself and hand each process a
  scratch home carrying everything that home requires except the refresh token,
  which is the one value it must never place there. The complete contents are
  stated by [the `oauth` delivery](#the-oauth-delivery).

- **Codex file resolution.** No present composition or runtime delivers a Codex
  `file` profile; the parser validates its fields and then rejects it at
  startup. Delivery resolves the pinned reference during capability preparation
  and, after the common trailing-termination narrowing, admits exactly a
  nonempty NUL-free UTF-8 value of at most 65,536 bytes. Empty, non-UTF-8,
  NUL-containing, or oversized content must fail preparation as typed
  `CredentialUnusable`; no child may be spawned. Leading and interior whitespace
  remain credential bytes.

- **Stored OAuth material.** The one admitted exception to the rule above is the
  `oauth` delivery in [credential deliveries](#credential-deliveries), which
  stores a credential value precisely where that credential rotates and the
  daemon alone refreshes it, and nowhere else. No present migration or
  repository stores daemon-owned OAuth material, so the reference-only rule
  above is the current at-rest boundary without exception.

### Committed unimplemented functionality — explicit session credential update

No present API, process message, or command updates session credentials. A
future explicit operation may add or change a session credential only by
appending the next complete history event with its own command provenance and
advancing the head by exactly one; it must never rewrite history or
automatically apply a configuration edit. The current append-only record shape
is compatible with that operation.

Each profile is its own credential, so
[credential operations policy](#credential-operations-policy) applies once per
profile: several profiles are several sources of truth, never one secret
delivered under several names. A deployment that points two profiles at one
vault item has not gained an account; it has given one account two names, two
priorities, and two independent availability judgments about the same remaining
quota.

## Runner credential lifecycle

Runner credential profiles are non-secret checked names granted by the daemon
and resolved only by `signalbox-runner`. The daemon, client, database,
transcript, workspace manifest, and runner wire never receive a runner
credential path or value (INV-035, INV-045).

### Committed unimplemented functionality — runner credential execution

No present runner surface admits a lease, provisions a workspace, reads a
credential file for execution, or injects credential bytes. The remaining
paragraphs in this section constrain that future execution surface; they do not
describe behavior available from the registration-only daemon.

A session may hold no credential at all, and no boundary infers one. When the
placement selected no profile the daemon issues no grant, the lease carries no
credential dispatch authorization, and the runner resolves no path and injects
no value for that session's dispatches. A repository entry with no profile then
uses anonymous HTTPS, while an entry that names a profile fails with the typed
`credential_unavailable` class rather than resolving a profile the placement
never selected. Conversely, a named profile is granted to a session with no
repository and no workspace, because the credential is scoped to that session's
dispatches rather than to a clone
([runner protocol and placement](runner-protocol.md)).

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
([runner protocol and placement](runner-protocol.md#planned)). The runner scrubs
the exact value and its JSON-string-escaped form from admitted stdout, stderr,
and result text before forwarding. This reduces accidental echo; it cannot
prevent model-controlled code from transforming or using the value within its
granted repository scope, which is an accepted restricted-profile cost.

Unknown profiles fail before lease claim. A credential failure after a claimed
dispatch is a fixed `ExecutionFailed` observation naming only the profile and
failure class. A transport or supervisor loss remains effect-class ambiguous;
credential failure never authorizes an automatic repeat of side-effecting work.
Model-provider credentials are daemon-only and cannot be granted or injected to
a runner. Explicit `ambient` nevertheless retains same-user filesystem powers
and therefore does not promise those files are unreadable; that access is
outside the credential-grant channel.

## Always-composed session plan family

`plan_write` and `plan_read` have no `[[tool_mappings]]` entry. Both the
compatibility base composition and the complete four-mapping composition
construct them through the injected `SessionPlanPort`; production injects
`SessionPlanRepository`. They require no credential profile, egress policy, or
workspace root, and model arguments cannot select another session or storage
adapter. Their automatic permission defaults and effect classes are owned by
[tool-loop](tool-loop.md#session-plan-tools).

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
  credential value. The separate pull-request adapter does the same and retains
  neither its request-scoped value nor a response body in errors. Access errors
  carry reference and typed failure class only.
- signalboxd logging is a compact INFO tracing subscriber; startup and runtime
  errors log phase, failure class, counts, and aggregate ids only. The
  `crates/application` tracing sites emit the same typed fields, plus the closed
  tool error kind and the daemon-authored catalog tool name at the failed
  tool-attempt site. A runtime-bridge schema rejection additionally emits the
  daemon-authored tool name and the JSON parser's grammar-and-position
  diagnostic, never the rejected schema bytes. No call site in the codebase
  passes accepted-input, assistant content, tool arguments, or tool error detail
  to `tracing`.
- Every provider-controlled text that leaves the Anthropic adapter — stream text
  and thinking deltas, tool-argument JSON, tool proposals, native error bodies,
  provider request ids, reported model identity, stop-sequence and finish
  tokens, transport detail — is scrubbed with the exact preparation-time
  credential value before crossing the boundary. This contract fixes which value
  seeds that scrub; how the adapter applies it — the representations it covers,
  its behaviour across provider chunk boundaries, where it sits relative to
  truncation and parsing, and the limit that bounds the guarantee — is owned by
  [the credential-access boundary](runtime-substrate.md#credential-access-boundary).
  INV-035-tagged tests in `crates/model-runtime/src/credential.rs`,
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

Operational rules the daemon deployment must honor; code cannot enforce them.
Runner credential files instead follow the same-host `0600` contract above and
are outside this cluster-delivery policy:

- **One source of truth per secret.** 1Password owns `file`-delivered runtime
  credentials: the vault item a reference resolves to is the source of truth,
  and rotation is an edit to it. sops-age-in-git owns bootstrap and deployment
  material (including the operator's own credential): the encrypted file in git
  is the source of truth, and rotation history is git history. Maintaining the
  same value in both channels is a defect. A `codex_home` or `oauth` credential
  has no vault item at all, and giving it one would be that same defect in its
  most damaging form: the provider replaces a rotating token on every use, so a
  vault copy is stale the moment it is taken and replaying it invalidates the
  live authorization. For those two deliveries the store the daemon or the CLI
  writes is the source of truth, re-provisioning is the rotation, and the vault
  holds nothing. Kubernetes Secret objects are delivery artifacts of whichever
  channel produced them, never sources of truth; hand-editing one is a defect
  because the next sync overwrites it. This split governs exactly the provider
  and integration runtime credentials plus the bootstrap and deployment material
  the channels themselves depend on, not every cluster-delivered secret:
  user-client authentication, runner enrollment, and the database credential are
  separate open decisions outside it (see Open edges).
- **Acyclic bootstrap chain.** The operator-held age identity (custodied outside
  git and outside automated sync) decrypts the sops channel; the sops channel
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
  active key, rotation has a known-failure window; the mitigation is narrower
  propagation configuration, never silent retry.

## Open edges

- [Graded approval judging](../open-questions.md#graded-approval-judging) owns
  the unresolved actor-audit decision if trusted outcome derivation introduces
  mutable graded thresholds.
- Selection-key retargeting across a restart is not prevented by code:
  reconstitution's `CallTargetMismatch` cross-check fails closed only for a
  session with a live stored call; for everything else, not retargeting a
  `selection_id` is deployment discipline.
- The [credential operations policy](#credential-operations-policy) is
  operational discipline with no code or CI enforcement; violating it cannot be
  caught by any test.
- `DATABASE_URL` via process environment is explicitly provisional; the
  database-credential delivery channel remains an open decision.
- signalboxd erases typed configuration diagnostics before logging:
  catalog-parse and Anthropic-construction variants (and connection errors)
  collapse to a generic `Infrastructure` class plus phase, so startup logs
  cannot distinguish failure causes within the `Configuration` phase. A failed
  migration is the exception and keeps its rejection text.
- [Identity, credentials, and resource governance](../open-questions.md#identity-credentials-and-resource-governance)
  owns the unresolved in-memory credential-hygiene question.
- No adapter in this build reports remaining provider capacity, so
  `headroom_reserve_percent`, `least_used`, and a non-`stay` `on_headroom_low`
  action are rejected at startup rather than silently inert. The observation
  itself is routed through
  [Model fallback and provenance](../open-questions.md#model-fallback-and-provenance).
- **Committed unimplemented functionality — quarantine clearing.** This build
  stores no quarantine, so neither clearing path exists yet. The operator
  command is committed unimplemented functionality; the automatic path
  additionally needs an adapter offering a zero-cost liveness probe, and no
  adapter in this build offers one.

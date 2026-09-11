# Configuration and credentials

This page states how signalboxd and signalbox-runner load their deployment
configuration, what the static catalogs define, and how a credential value
reaches a provider without being stored or logged.

## Overview

Configuration is loaded at startup from the process environment and two
versioned TOML documents: the model catalog and the session-template catalog.
The parser in `apps/signalboxd/src/configuration/mod.rs` and
`apps/signalboxd/src/credential_pools.rs` admits a document fail-closed. The
subsystem also owns the runner's startup configuration, the isolation that keeps
ambient settings away from the production database channel, and the bridge that
carries a credential value from its file or login directory to the adapter that
uses it.

The environment supplies a fixed set of deployment values: the database URL, the
catalog paths, the socket paths, the paths of the two integration credential
files, the optional browser bind address and static-asset root, the log filter,
and the telemetry settings. An absent `SIGNALBOX_RUNNER_SOCKET_PATH` derives the
runner socket by replacing the process socket path's final extension with
`.runner.sock`, and the runner's `daemon_socket_path` dials that path.
`DATABASE_URL` is the whole database channel: a deployment carries every
connection parameter, including an explicit password, in the URL. Before the
asynchronous runtime starts, the daemon replaces itself with the libpq and
certificate-store variables removed; it retains their names only for startup
warnings. The explicit password prevents SQLx from reading a password file.
Whatever TLS mode the URL states, the production connection verifies the server
certificate and hostname in full. Model-provider credential paths come from
`file` profiles in the catalog; `ANTHROPIC_API_KEY_FILE` and
`OPENAI_API_KEY_FILE` are not read. An absent `SIGNALBOX_WEB_BIND` binds a
loopback default, and an explicit socket may use any IP address; the daemon logs
a warning before admitting an explicit non-loopback bind. The daemon's browser
listener serves the `/api` routes on that bind and, when
`SIGNALBOX_WEB_ASSET_ROOT` names a production web build, the static files under
that root; an empty root fails configuration and an absent one answers every
other path 404. The DTOs and schemas under `crates/web-contract` are the
authority for that surface, and the checked-in JavaScript decoders and
TypeScript declarations are generated from them. `RUST_LOG` admits one log level
and nothing else; an empty or whitespace value selects the INFO default
silently, as absence does, and any other value warns and falls back to it.

`SIGNALBOX_OTLP_ENDPOINT`, a base URL to whose path `http/protobuf` export
appends `/v1/traces`, enables span export; its absence disables OTLP and makes
every other OTLP setting inert. Before starting the asynchronous runtime, the
daemon removes general and trace-specific `OTEL_EXPORTER_OTLP_*` endpoint,
headers, timeout, protocol, and compression variables, and warns when any was
present. When export is enabled, both exporters receive the configured headers
explicitly and select gzip compression explicitly, so the exporter library
cannot merge ambient headers or compression. `SIGNALBOX_OTLP_PROTOCOL` selects
`grpc` or `http/protobuf`, `SIGNALBOX_OTLP_HEADERS_FILE` names a
collector-header file read once at startup, `SIGNALBOX_OTLP_SAMPLING_RATIO` sets
the parent-based trace-id sampling ratio from 0 through 1, and
`SIGNALBOX_OTLP_SERVICE_NAME` sets the service name.
`SIGNALBOX_PROMETHEUS_BIND`, an exact IP socket address, enables a separate
Prometheus listener.

`signalbox-runner` takes its configuration path from exactly one source,
`SIGNALBOX_RUNNER_CONFIG_FILE` or `--config PATH`, and rejects both or neither
before opening a socket. It reads that file once at startup as strict versioned
TOML, whose checked-in example is `config/signalbox-runner.example.toml`. Its
`runner_root` must be a real directory owned by the effective user with
owner-only permissions, held under an exclusive lock for the process lifetime,
and its `bubblewrap_path` must resolve to an executable regular file. Its
`allowed_network_hosts` narrows a fixed host list and cannot add a hostname.
`sandboxed_exec` uses an isolated network namespace by default. A dispatched
command that times out under that policy includes the `network_fence_active`
diagnostic; this reports the configured fence without inferring network intent
from the command or its arguments. Sandbox setup failures emit a categorical
warning before command dispatch without logging argv or captured output. Runner
credential profiles are non-secret checked names the daemon grants and only the
runner resolves.

The model catalog declares what the four adapters can serve; the daemon provides
exactly `anthropic`, `openai`, `claude_cli`, and `codex_cli`, no adapter pins a
profile name, and a pool may hold several profiles for one adapter. Each
`[[models]]` entry binds an immutable direct-selection key to one exact provider
target, a model family, a provider-native spelling, token ceilings, and flat USD
rates that are either wholly absent or a complete bundle carrying its own stable
rate version; aliases name a selection. A model whose `fast_mode` is
`alternate_target` names a `fast_target_id` that resolves to a
non-client-selectable `[[serving_targets]]` entry carrying its own target, model
family, provider spelling, and token ceilings. `provider_model` is nonempty and
unpadded, and one spelling routes to exactly one adapter across the document.
`context_window_tokens` is the usable ceiling after any provider or adapter
reservation, not the raw advertised window, and is not smaller than
`max_output_tokens`. The optional `provider_compaction` boolean is an exact
per-target Anthropic capability on `[[models]]` and `[[serving_targets]]`;
omission is false, another adapter cannot declare it true, and the effective
target after fast-target resolution controls both compaction enablement and
opaque-block replay. An OpenAI target may declare a nonempty, unpadded
`reasoning_replay_family` string on `[[models]]` or `[[serving_targets]]`. Two
targets replay each other's reasoning items only when both declare the same
family. A target that declares none does not replay reasoning items. Family
configuration does not suppress durable reasoning retention. Every configured
entry naming one `provider_model` must declare the same value; other adapters
cannot declare this capability. `[model_settings]` is the deployment global
default and each `[[model_settings_profiles]]` entry is a named profile a
model's optional `settings_profile` selects; a selected profile outranks the
global default, and both sit below the session and per-call layers of
[model session settings](model-session-settings.md). An adapter mapping that
names `claude_cli` requires a `[claude_cli]` table carrying that adapter's
`executable`, `mcp_bridge_executable`, and `working_directory`. The required
`[numeric_bounds]` table holds the central numeric-bound inventory and the
members are required except for the defaulted repository-watch poll request
budget, while other tables carry their own configured limits. Numeric-bound
duration policies use Jiff's friendly unsigned-duration syntax.
`max_git_object_bytes` limits decoded Git object content, including packed delta
dependencies; `"none"` leaves blob content unbounded. Commits, trees, and tags
retain the Git family's 1 MiB structural metadata bound.
`max_review_findings_per_run` must be finite and no greater than its domain
bound. A finite `max_blob_replica_count` must admit the durable catalog's full
store bound. Disabling reconciliation requires an unbounded nudge buffer.
Conversation import admits no source-size setting; a `conversation_import` table
is rejected by the closed top-level schema.
`repository_watch_webhook_retention` must be positive and finite and governs
authenticated webhook `expires_at` and merged-pull-request baseline retention as
described in [repository watch](repo-watch.md). `codex_cli_version_probe_bound`
bounds a credential-free startup probe of the configured Codex executable. A
missing or zero bound fails configuration before the socket opens. An
unsuccessful or malformed probe, or an installed version outside the configured
pin, leaves the Codex adapter unavailable while the daemon continues startup.
The same bound limits each read-only Codex capacity probe for
[parked credential waits](credential-availability.md).
One valid document yields correlated immutable in-memory catalogs: the domain
`ModelTargetCatalog` for execution-time target resolution and the
`RuntimeModelCatalog` for the provider bridge. The optional
`repository_watch_poll_request_budget` defaults to 100 and accepts integers from
2 through 1,000, including the quota preflight request in each attempt.

`guard_recovery_initial_delay` and `guard_recovery_maximum_delay` are positive
durations, with the maximum no smaller than the initial delay.
`guard_recovery_elapsed_bound` limits one guard-loss recovery episode, including
pool shutdown and runtime reconstruction; `none` leaves it unbounded.
[Turn lifecycle and scheduling](turn-lifecycle-and-scheduling.md) owns
reacquisition before admission resumes.

The `[[tool_mappings]]` array composes the deployment-mapped tool families and
binds one configured workspace root. Each session's workspace root is derived
from that root by a fixed formula: `<name>.sessions/<session uuid>` beside the
configured root, where `<name>` is the configured root's final path component. A
session names no path and no column supplies one, so the set of roots the daemon
can open is fixed by the configured root alone. `sandboxed_exec` and
`cargo_diagnostics` share one daemon-local bubblewrap profile whose default
launch unshares the user, pid, ipc, uts, and network namespaces and mounts a
fresh `/proc`. A container-process-namespace variant omits the pid unshare and
read-only binds the existing `/proc`; it is admissible only when an outer
container already isolates that namespace. The child inherits none of the
daemon's environment; deployment settings supply additional runtime inputs. The
required `[daemon_tools].sandboxed_exec_timeout_bound` is a friendly duration of
at least one second or `"none"` and bounds the timeout requested from
`sandboxed_exec`. The optional `[daemon_tools]` keys `sandbox_network` (default
`"none"`, or `"host"`), `sandbox_read_only_binds` (default `[]`), and
`sandbox_path_prepend` (default `[]`) select networking, absolute host paths
bound read-only at the same paths, and absolute directories prepended to `PATH`.
Programmatic read-only mounts at explicit destinations overlay the workspace,
worktree, and working-directory binds. Host networking shares the daemon's
network namespace and DNS configuration without destination filtering,
independently of web-egress and tool-mapping policies. Optional
`sandbox_rustup_home` and `sandbox_rustup_toolchain` set `RUSTUP_HOME` and
`RUSTUP_TOOLCHAIN`; automatic toolchain installation is disabled, `CARGO_HOME`
stays private and writable, and `npm_config_cache` is `/workspace/.npm`.

The optional `[tool_proposals]` table sets `max_requests` and
`max_argument_bytes` to nonnegative integers or `"none"`, with defaults of 32
and 1048576. Exceeding either cap produces the per-request errors in
[tool-loop](tool-loop.md).

The optional `[tool_approval_postures]` table decides, per exact composed tool
name, whether a request is approved by policy, judged by the approval judge, or
parked for a person; a tool whose declaration always confirms keeps that
requirement under every posture but delegation. Web tools default to delegation
under either session blanket; explicit `auto` also delegates them and `human`
parks them for a person. Optional `[tool_settings].approval_wait_timeout` is a
positive duration or `"none"`, defaults to `"10m"`, and is retained for each
human wait when that wait first reaches the daemon. The optional
`[approval_judge]` table decides which configured direct selection judges
delegated requests, and when it is absent the judge reuses the request-producing
call's selection. The optional `[workspace_instructions]` table is either absent
or present at version one, and its bounded `registered_roots` array names the
instruction directories registered outside a session's workspace. Its entry,
finding, source-byte, and elapsed discovery limits accept finite values or
`"none"` ([workspace-instructions.md](workspace-instructions.md)).

A credential profile names one account. Its `CredentialReference` is the
non-secret name that appears in configuration, errors, logs, and durable
records; its `CredentialValue` carries the secret bytes and exists only at the
adapter boundary. Every model-provider reference is an operator-chosen profile
name; the two integration constants are `brave-search-primary` and
`github-primary`, and `codex-subscription-primary` and
`claude-subscription-primary` are the defaults a CLI runtime uses when its
mapping names nothing else. A profile's delivery states how its secret reaches
the provider. `file` is the delivery for every credential with an external
source of truth, such as a provider API key or a long-lived token a provider's
tooling mints; a direct-HTTP adapter forms its header from the file value and
rejects `env_key` because it uses no child environment. Each
`FileCredentialAccess` instance binds one consumer-scoped map of references to
deployment paths, and a model adapter receives the complete file-profile catalog
declared for it. `ambient` leaves login resolution to a CLI. `codex_home` names
the Codex login directory; a configured home is an existing readable directory.
An empty home makes that pool member unavailable. Delivery links the selected
profile's `auth.json` into a private per-operation `CODEX_HOME` with an empty
`config.toml`.

GitHub integration profiles declare `adapter = "github"` without model billing
or pool membership. `delivery = "file"` requires `file`;
`delivery = "github_app"` requires positive `app_id` and `installation_id`
integers and an absolute `private_key_file`. Validation names a missing or
invalid App field. The `code_host` and `github` mappings use `github-primary`; a
declared profile supplies that reference, otherwise `GITHUB_TOKEN_FILE` supplies
it. Repository-watch entries select either `credential_file` or
`credential_profile`.

The App key is read at token minting under the credential-file admission rules,
never at boot. The daemon signs an RS256 JWT with issuance sixty seconds in the
past and expiry ten minutes in the future, exchanges it for an installation
access token, and caches only that token and its expiry in memory. Requests
refresh within sixty seconds of expiry; a 401 refreshes the rejected token and
retries the request once. Concurrent requests share a refresh and reuse a token
already refreshed for the same rejection. REST, GraphQL, and HTTPS Git requests
through the profile authenticate as that installation. App key, JWT, and token
material never enter logs or durable records.

A Codex home pool declares one `codex_cli` subscription profile with
`delivery = "codex_home"` and a distinct `codex_home` directory per
independently metered account, then lists those profile names as pool members.
Equal member priorities let `least_used` compare their headroom under the pool's
reserve and headroom action;
[the configuration example](../../config/signalboxd.example.toml) provides a
three-home pool that replaces its ambient profile and pool. Pool selection
rechecks each home and skips a member while its home is empty. If every member
is skipped, the pool applies its configured exhaustion policy before any adapter
preparation; a parked wait retries the live check after provisioning can change.

A credential pool is the set of profiles that may substitute for one another for
one model family. An `[[adapter_mappings]]` entry maps each family to exactly
one pool, and every member of that pool carries the mapping's adapter. A pool's
name is 1 through 256 unpadded NUL-free bytes and it holds 1 through 1,024
members, each with a priority within the pool. Priorities need not be unique or
contiguous: `tie_break` resolves equal values, and gaps let a later profile take
an intermediate rank. Both `tie_break`, which admits `first_listed` and
`least_used`, and `on_pool_exhausted`, which is `park` or `fail`, are required.
The five trigger keys `on_quota_exhausted`, `on_rate_limited`, `on_overloaded`,
`on_credential_rejected`, and `on_headroom_low` each carry one action from the
closed set `stay`, `switch_next_turn`, `switch_now`, `avoid_new_sessions`, and
`quarantine`; an omitted key selects `stay`, and a one-member pool is the
ordinary single-account deployment and needs no trigger keys. Selection happens
at model-call preparation, never at session creation: it prefers the sticky
member while that member remains admissible and otherwise walks members in
priority order, skipping excluded ones and breaking ties by the snapshot's rule.
Trigger actions and the exclusions they create are durable. Every availability
ending is owned by [credential availability](credential-availability.md).

The model-call observation commit retains a reported capacity snapshot against
the call's credential reference, including each window's remaining percentage,
reported duration and reset instant. The latest observation time wins across
calls; an absent snapshot preserves the retained evidence.

A member's headroom is the minimum of its primary and secondary windows. An
expired or missing reset means unknown capacity. Within an equal priority,
`least_used` samples members with unknown capacity in configured order before
preferring greater known headroom. A member's `headroom_reserve_percent`
overrides the pool reserve; a known headroom at or below the reserve excludes
that member from selection. A retained observation at or below the reserve fires
`on_headroom_low` under the call's frozen policy; with no reserve, the trigger
threshold is zero. Unknown capacity neither excludes a member nor fires a
headroom action. A classified provider failure's configured action, including
`stay`, takes precedence over `on_headroom_low` from the same observation; the
capacity snapshot is retained.

The session-template catalog is read after the model catalog. Each template
binds a name and version to a model or alias, a system prompt, and a
dangerous-tool blanket. A prompt is inline or a file reference, either relative
to the document's parent directory or `$HOME/` plus a relative suffix resolved
from the process `HOME` at load; the file must be a readable regular UTF-8 file
and yields the same bounded `SessionSystemPrompt` as an inline value, with no
trimming or interpolation. One valid table becomes an immutable resolved bundle
whose content digest, `SessionTemplateContentDigest`, is domain-separated
SHA-256 over length-framed canonical values in a fixed frame order. Unknown
fields, mistyped values, duplicate names, and every invalid field fail as
sanitized `SessionTemplateConfigurationError` variants without file paths,
prompt content, or document text.

Every session carries an append-only credential history. First handling of a
native or imported session-creation command appends event ordinal 1 in the same
transaction as the session. That event carries a complete nonempty
family-to-reference snapshot copied from the validated mapping table, and
preparation reads the latest entry for the resolved target's family. Sessions
predating the history carry a `migration_backfill` creation event; while it is
current, an Anthropic route may resolve through it and a Codex route never may.

`plan_write` and `plan_read` have no `[[tool_mappings]]` entry. Every
composition constructs them through the injected `SessionPlanPort`, and
production injects `SessionPlanRepository`.

## Design decisions

Model-provider credential paths live in catalog profiles rather than environment
variables, because one variable cannot name several accounts.

The production connection path requires the host, user, and password in the URL.
It removes libpq connection variables, `SSL_CERT_FILE`, and `SSL_CERT_DIR`
before starting the asynchronous runtime, logs each removed variable, and logs
the presence of the default password file. The URL's explicit password keeps
that file unread.

The local test connection path keeps SQLx's behavior and no check confines its
URL to a local cluster.

A failed migration is the one startup failure that records the database's own
rejection text in a structured field, because the phase alone cannot separate a
rejected constraint from an unreachable database.

The browser listener emits no permissive CORS headers and adds no account,
login, bearer-token, TLS, proxy, VPN, or ingress machinery.

The daemon supplies no Anthropic or OpenAI endpoint or per-adapter timeout
setting and constructs each adapter with its defaults. The whole-exchange
timeout is the required `numeric_bounds.model_exchange_timeout` policy, and the
exact value `"none"` makes the exchange unbounded.

`signalbox-debug` composes no daemon tool catalog and reads no
`GITHUB_TOKEN_FILE`; it is a development driver, not the client protocol.

A nonempty OTLP header file requires an HTTPS endpoint, so collector credentials
never cross the network without transport protection.

Span-queue insertion is nonblocking, and a full queue drops the newest span
rather than evicting older work or waiting on the daemon.

The daemon contains no backend-specific tracing protocol or attribute.

The runner's credential parser and resolver are name-generic, so another
credential shape is a configuration entry rather than a runner code branch.

The runner rejects reserved model-provider profile and environment names and has
no model-provider configuration field, because secret bytes have no
self-describing type that could classify a file as a provider key.

The model catalog has no version 2 and no in-place upgrade path, because a
version discriminator is needed only when two shapes must be accepted at once; a
document in any other shape is rejected, not migrated.

`billing_kind` is required and never inferred, because terminal cost derivation
trusts it to choose between a real charge and a metered equivalent.

Unknown fields are rejected at the root and inside every table, because a
silently ignored key would let a typo change model meaning invisibly.

Only a regular file the daemon's own effective credentials may execute satisfies
the `PATH` search for a bare `mcp_bridge_executable` name, so a file another
user may run does not shadow one it can.

Keeping a selection key immutable is deployment discipline: nothing prevents an
edited document from pointing an existing `selection_id` at a new `target_id`
across a restart.

The startup-scan restart path rebuilds its target catalog from the stored calls
rather than from configuration, and reads no quarantine record, so recovery of
acknowledged work depends on neither configuration nor provider state.

Paths and queries remain unrestricted request data at an admitted `web_fetch`
origin.

The derived session root's parent is a sibling of the configured root rather
than a child, because a per-session root inside the configured root would be
readable and writable by every session still bound to it.

The workspace table keys a workspace by an identity and the canonical root it
was minted for, not by a path, because authority grants must be scoped to
something stabler than a path.

A derived parent that is itself one of the configured composition's directories
is refused, because ancestry is not equality and the bound pair cannot show the
nesting. A parent that is a real directory whose contents are a bind mount of a
tree inside the configured root is an accepted residual.

The configured root must have a lexical parent and final component; a root such
as `/srv/workspace/child/..` is rejected at composition rather than treated as
unprovisioned, which would silently return every session to the shared root.

A configured pathname whose directory pair cannot be captured fails the request
closed rather than falling back to the pair pinned at startup, because comparing
against the startup pair alone would admit the sharing the comparison exists to
refuse.

A filesystem may reuse a device and inode pair, so a derived directory removed
and recreated while unretained can present the identities the record names; this
is an accepted residual.

The bubblewrap profile applies no resource limit, uid or gid drop, seccomp
policy, or landlock policy, so it does not contain a deliberately hostile
program. An `AF_UNIX` pathname socket inside the workspace root remains
connectable and `AF_VSOCK` remains available, so the network namespace does not
close every transport. A credential inside the workspace root is readable,
because credential settings are admitted on presence alone and never checked
against that root. `/proc` and `/dev` carry kernel- and host-derived data no
workspace bind governs, and everything under the workspace root is writable,
including the repository's `.git`. `cargo_diagnostics` defaults to automatic
approval yet compiles and runs the workspace's own build scripts, procedural
macros, and test binaries. This profile is the daemon's own; the runner's
sandbox profile is owned by [runner protocol](runner-protocol.md).

An adapter-and-delivery pair is admitted exactly when that adapter's own
delivery contract defines how the secret reaches its provider, and every other
pair is rejected. Why: a permission needs one edit when an adapter gains a
delivery, where a matrix needs two.

`ambient` has no account identity at admission and never acquires one, because
the daemon never reads the ambient login store; so a document holds at most one
`ambient` profile per CLI adapter and never combines a Codex `ambient` profile
with a `codex_home` profile.

Member independence is stated as one property with a per-delivery disposition
rather than a list of rejected alias spellings, because a rejection list covers
only shapes already imagined.

`file` independence is required of the deployment: the daemon rejects only equal
lexically normalized paths, and an ordinary copy is indistinguishable from a
second credential. Two distinct paths that a symlink, hard link, or copy
resolves to one secret remain two members, and the cost is bounded to one extra
successor attempt that fails as its predecessor did.

Settings whose effect the daemon cannot supply are typed startup failures rather
than retained and inert: `round_robin` is rejected. Codex pools admit
`least_used`, headroom reserves and non-`stay` `on_headroom_low`; the other
adapters reject them. The Codex adapter reads capacity with
`account/rateLimits/read` after initialization and merges primary and secondary
windows from `account/rateLimits/updated` into the current call's evidence.
Remaining capacity rounds down to whole percentage points. A null or absent
window preserves its previous value within that call; a notification with
neither window emits no capacity evidence. Thread startup and turn execution do
not wait for the capacity read; a rejected or unanswered read supplies no new
evidence. A notification carrying a window supersedes an outstanding read, whose
reply is consumed without emitting evidence. A read reply received after turn
completion still supplies capacity evidence unless superseded. Members without
retained evidence have unknown capacity: `least_used` samples them in configured
order before members with retained evidence, reserves do not exclude them, and
headroom actions do not fire.

The pool name and member bounds keep the duplicated exhaustion evidence and the
authoritative policy read below the process protocol's frame limit under
worst-case JSON escaping.

Priority is a property of the membership rather than the profile, because one
account holds different ranks in different pools.

`stay` creates no durable state and writes no action row; the observation
commits alone, so the default configuration can still commit a terminal
observation.

`switch_now` on `on_credential_rejected` rotates to the next admitted member
when no stop was requested; `CredentialRejected` admits that successor without
non-acceptance proof. The rejected profile and `CredentialRejected` cause remain
recorded durably on the failed attempt. `switch_now` remains refused on
`on_headroom_low`, because low headroom is not a failure.

A `codex_home` refresh race gets no delivery-layer bypass, because the Codex CLI
reports one undifferentiated authentication failure the adapter cannot split;
every `codex_home` credential rejection follows the pool's configured
`on_credential_rejected` action.

All adapters admit `switch_now` for quota, rate-limit and overload failures. The
classified kind and pinned policy determine successor admission; a pre-stream
rejection and a definitive mid-stream error use the same policy. Non-acceptance
proof is not required for these known failures.

An `avoid_new_sessions` exclusion is durable and scoped to the membership that
observed it until an operator clears its exact generation. It applies to every
session except one that has already completed a call through that member on the
same pool.

An operator clear removes a pending `switch_next_turn` displacement or an
`avoid_new_sessions` exclusion exactly as it clears a quarantine; the request is
owned by [process protocol](../spec/process-protocol.md).

A session's credential history stores the preferred reference rather than the
pool policy, so a fresh availability chain resolves the pool from the current
document and a pool edited across a restart can change which members an existing
session admits.

The template name and prompt source form are excluded from the content digest,
so inline and file-backed prompts with the same version and bundle share a
digest.

Session templates live in a separate file so operators change the reusable
creation surface without editing model-identity definitions, while one load
boundary keeps validation fail-closed.

A deployment keeps one profile name's billing meaning stable and uses a new name
when authentication changes it; the parser cannot detect a same-name semantic
rewrite.

A credential is named by a stable reference and its value rotates behind it, so
no record or log ever needs the secret.

The credential value is the file's bytes less trailing line termination, because
the tools that write a credential file terminate the line they print.

A `file` credential has an external source of truth, so the daemon stores no
copy of it; a stored copy would be a second source.

Each profile is its own credential; pointing two profiles at one vault item
gives one account two names and two availability judgments, not a second
account.

Model-provider credentials are daemon-only and cannot be granted or injected to
a runner. An explicit `ambient` login nevertheless retains same-user filesystem
powers outside the grant channel.

## Boundary contracts

The optional `[codex_cli].model_context_window_overrides` map is an inline TOML
table; a nested `[codex_cli.model_context_window_overrides]` table is invalid.
Every key exactly matches the `provider_model` of a configured model routed
through the `codex_cli` adapter, every value is a positive raw Codex
`model_context_window` token count, and any unmatched or differently routed key
fails startup. Codex applies its own reservation to that raw count;
`context_window_tokens` remains the independently configured usable
post-reservation ceiling enforced by the daemon, and the loader does not derive
either value from the other.

The daemon refers to a credential by its non-secret name everywhere except at
the point of use. OAuth authorization tables hold the refresh and identity
tokens the delivery needs; no credential value appears in a log, an error, or
any other durable record. No credential file path or database URL appears in a
log, an error, or a durable record. Provider output follows the
[runtime substrate](runtime-substrate.md). A credential for one repository never
authorizes a request to another. That isolation comes from how a credential is
provisioned or from the repository entry a runner selects; the daemon's
code-host tools use one fixed credential reference.

Errors, logs, and diagnostic evidence contain classes, counts, and canonical
identifiers. They never contain source bytes, host or credential paths, raw or
unsanitized provider payloads, SQL, or user content other than a bounded,
credential-redacted provider error body, except the rejection text a failed
migration records; a tool failure may name a bounded workspace-relative path.
Retained source content, such as an imported transcript entry, is not diagnostic
evidence.

`HOME` locates the default PostgreSQL password file and must be a nonempty
absolute path when a template uses a `$HOME/` prompt reference. The
database-channel warning names the ignored channel, never its contents, and is
logged before database contact.

Missing required values, unreadable model catalogs, and invalid startup-only
sections fail startup in the Configuration phase before database contact.
Reloadable catalogs and prompt files are validated after the pending reload
intent is read; failed provider transport construction also fails in
Configuration. After the database connects, an invalid configured workspace root
or a failed tool-suite construction fails at the same phase. A derived
per-session root is composed on first use, so its failures are per-session tool
failures. Startup and shutdown logs carry the phase, an operator failure class,
and small typed fields. Every tool dependency is supplied by parsed
configuration, the database pool, or explicit credential and transport values;
no tool family discovers ambient authority.

The deployment paths are accepted without I/O at environment parsing; the
selected catalogs and template prompt contents are validated during startup. At
startup, reload, and each resolution, model-provider and integration credential
files, including repository polling, push, and webhook secrets, must resolve to
regular files owned by the daemon's effective user and at most 64 KiB. Group or
other permission bits produce a warning and the file is read. Other failures use
a typed error naming the credential reference and failed check without secret
contents. The credential of a currently routed S3 blob store is read after the
recovery scan and before socket admission, as [blob storage](blob-storage.md)
requires.

Unauthenticated session, search, usage, attention, and blob reads require an IP
or `localhost` `Host` authority; another authority receives a 403
`unadmitted_host_rejected` before data is read. No process-protocol frame is a
browser DTO. Application errors are a separate error kind and are never inferred
from HTTP status alone.

Every `/api` route rejects `Sec-Fetch-Site: cross-site` with a 403
`cross_site_api_request_rejected` before the admitted-authority check or storage
access, while other or absent fetch-site values pass this gate.

Browser mutation routes use POST, require `application/json`, and when `Origin`
is supplied require its host and effective port to equal the request `Host`
authority. A `Host` without an explicit port has effective port 80 because the
listener is plain HTTP, and the daemon never derives that port from the origin.
A missing origin is admitted for non-browser and same-origin clients; an
invalid, opaque, missing-authority, or cross-origin pair receives a structured
transport error.

The OTLP HTTP client ignores ambient proxy configuration, refuses redirects, and
has the derived traces endpoint as its only route. Collector header values are
sent only as transport metadata and never become a span, event, resource
attribute, metric, or log field. `SIGNALBOX_OTLP_SERVICE_NAME` admits exactly
`signalboxd`, `signalboxd.development`, `signalboxd.staging`, and
`signalboxd.production`; `service.name` is the sole resource attribute and the
resource starts empty, so no host, process, environment, or SDK attribute is
added.

Exported spans and events carry daemon-minted UUIDs, closed tokens, and bounded
unsigned counts only. Source location, thread fields, busy and idle time, links,
and error conversion are disabled, and the fixed scope name is `signalboxd` with
no version. `terminal_outcome` and `cause_code` are enum projections, never
error messages, and a new cause requires a compiler-checked
`ModelCallCauseToken`. Any other event name, module target, field set, malformed
UUID, token value, or `error` field is rejected before the OpenTelemetry layer.
Collector and transport errors emit one static content-free warning, drop the
batch, and return success to the SDK.

`GET /metrics` returns the registry, other paths return 404, and there is no
authentication or TLS, so deployment network policy owns reachability. Counter
labels are allocated from closed enums at registry construction, and the only
free-form label is the scheduler gauge's daemon-minted session id. The durable
counters consume committed typed outbox transitions and ignore content-bearing
input events; the dispatcher retains only the last durable sequence.

A runner repository entry with no credential profile admits anonymous HTTPS only
and never asks the runner or daemon to select a credential. Absolute runner
paths are canonicalized without following a final credential symlink; duplicate,
nested, or runner-root-overlapping allowlist paths fail closed. Runner startup
never reads credential bytes and never logs configuration paths, repository
URLs, or values. The daemon, client, database, transcript, workspace manifest,
and runner wire never receive a runner credential path or value.

A catalog parse error is a typed sanitized value and no file content appears in
its text. An unknown or invalid field is rejected without its name, so
`config/signalboxd.example.toml` is the operator's guide. A profile name is
opaque to code: no build-provided constant is compared against it. Catalogs are
read at startup. `reload_configuration` validates the complete replacement and
atomically replaces the model and alias catalog, session-template catalog, and
repository-watch configuration; every other section is startup-only. A
replacement whose startup-only sections differ leaves the running configuration
in place. Reload never rewrites evidence already recorded. File watching and
polling are external callers of the verb.

Every serving record states its family, and the adapter mapping rather than the
selectable record pointing at it supplies its adapter and credential pool. Input
guarding, output reservation, and post-response usage enforcement use the
effective serving record's limits for the enabled call, not the selectable
source record's.

An absent `[web_fetch]` table admits every origin permitted by the transport. A
present table requires each request to match `allowed_origins`; an empty array
admits no origin. Each configured entry is a bare HTTP(S) origin canonicalized
to its scheme, host, and effective port before duplicates are rejected. The
GitHub egress policy admits exactly `https://api.github.com:443` for
authenticated requests, and model arguments cannot widen either admission rule.

Admission is not delivery: the daemon supplies a surface only for `anthropic`
and `openai` `file`, `claude_cli` `ambient` and `file`, and `codex_cli`
`ambient`, `codex_home`, and `oauth`. The `codex_cli` spelling of `file` is
validated and then rejected as `UndeliveredCredentialDelivery`.

Two model entries naming one target must agree on their complete rates or on
their complete absence of rates. Rates are never written to a model-call row;
the billing registries are consulted only when a read surface derives dollar
cost. Ordinary-path reconstitution cross-checks every stored call's target
against the configured `ModelTargetCatalog` and fails closed as
`CallTargetMismatch` corruption when they differ.

The configured root is opened once during tool construction and its pinned
authority is cloned into the workspace and execution suites. Repository roots,
including empty repositories and linked worktrees, also compose the Git suite; a
plain directory has no Git executor but registers Git declarations for
repository-backed derived sessions. A nonexistent, non-directory, final-symlink,
or malformed repository root fails startup.

Repository-watch pull-request dispatch provisions its derived directory; other
derived directories are provisioned by deployment. Only a reported absence at
the derived path is unprovisioned, and such a session binds the configured root;
a present non-directory, a symlink, or a path the daemon cannot classify is
misprovisioned and fails closed. Which root a session bound is recorded on its
first workspace-root-bound tool invocation and does not change for the process's
lifetime; the first record written wins, so two concurrent first requests
converge on one root. Isolation is checked against directory identities rather
than pathnames: a composed root sharing its worktree, worktree administration,
or common administration directory with the configured root or with another
bound session is refused. Linked worktrees sharing common references and objects
cannot bind separate session serialization domains. Administration directories
nested under another bound workspace or its Git administration are refused by
comparing captured ancestry with all three bound directory identities. Ancestry
capture needs only search permission on ancestor directories and refuses device
boundaries or Linux mount-ID boundaries. Failure to compose or bind a derived
root closes that tool request as a known failure whose sanitized detail names
the closed reason, and it never falls back to another root.

The secret reaches the provider through the profile's delivery, never through a
process environment variable of the daemon. Two families of one adapter may
prefer different profiles wherever the adapter resolves from an adapter-scoped
catalog, which the direct HTTP adapters and `claude_cli` do; a profile declared
for another adapter is unmapped in every case.

Every two members of one pool that a successor may substitute between denote
authorizations the provider meters, throttles, and rejects independently. The
daemon establishes this where it can and requires it of the deployment where it
cannot. `codex_home` independence is admitted by normalized path: two profiles
may not name one normalized directory, and independence of the token families
inside distinct directories remains a deployment assertion. Quarantine is
durable and scoped to the profile rather than the pool, so a profile ranked in
two pools is excluded from both and a shared authorization is removed everywhere
at once.

The Claude CLI credential value reaches the CLI through a private request-scoped
settings store, never the child environment, and the store is removed when the
capability drops; how the store is built is owned by
[runtime substrate](runtime-substrate.md). The daemon treats a credential home
only as a path reference and never opens, copies, parses, serializes, or logs
authentication material inside it. An operation whose reference names neither
the runtime's ambient profile nor a configured home is a typed
unavailable-credential preparation failure and starts no child.

Selection happens for each model-call availability chain: preparation loads the
target's admitted pool and skips chain exclusions, pending displacements,
membership exclusions, and quarantines. Preparation persists the selected member
and the call-pinned pool policy, and observation commit translates
classifications through that frozen policy, never the session's later state.
Stickiness needs no separate durable state: preparation prefers the member the
session's most recent `Prepared` call on that pool pinned, including one that
later failed under `stay`. `switch_next_turn` creates a durable pending
displacement scoped to the session, policy snapshot, member, and source turn; it
is ignored inside that source turn and consumed by the transaction that prepares
a later turn through another member.

Every transaction writing an exclusion first takes the affected profile's
action-head lock, keyed by profile reference alone and in byte order when it
touches more than one. Preparation locks the action head of every member it may
select before reading any exclusion state and holds those locks through the
`Prepared` insert. The ordering position of these locks is owned by
[persistence protocol](persistence-protocol.md).

A historical read resolves a call's billing kind and rates from the reference
the call pinned, so a pool edited across a restart cannot relabel a stored call.

Loading a review library generates four stage templates and one template for
each configured concern. The concern inventory is a nonempty subset of the five
closed concern keys and retains their closed order. All nine possible names are
reserved even when no library is configured, so an ordinary entry cannot shadow
one. Creation by template name first consults the durable command registry, and
an equal replay returns its stored session even when the name is absent or
changed in the current catalog; the claim protocol is owned by
[identity and commands](identity-and-commands.md). Only an unclaimed command
identity resolves against the loaded catalog and copies the complete bundle into
the session's immutable defaults version one. The session records the template
name and content digest and retains no live catalog reference, so an edit
affects only creations first handled under the new catalog. The daemon exposes
only sorted name and version summaries to clients; clients never receive prompt
text or parse the file.

Model-selection validation happens at two boundaries on frozen semantic meaning
only; credential presence is never consulted. At session creation the requested
direct model or alias must resolve through the static table, and absence is a
typed rejection carrying the exact `ModelSelectionRequest`. At acceptance
`SubmitInput` freezes the requested selection: a direct selection freezes
without catalog consultation, and an alias resolves through a definition lookup,
so an unknown alias is a recorded `UnknownModelAlias` rejection rather than an
accepted identity. At execution the frozen selection is resolved against the
`ModelTargetCatalog`, and an unresolvable selection fails the turn as a known
failure before any model call exists. Replacing session defaults imposes no
same-adapter restriction, and a prepared or in-flight predecessor retains its
call pin; the binding rule is owned by
[sessions and the transcript](sessions-and-transcript.md). In the provider
bridge a durably resolved target with no `RuntimeModelCatalog` mapping is a
typed adapter defect, `UnconfiguredTarget`, never provider evidence.

Dollar cost is derived only while reading a terminal call. An API-metered
profile produces a real figure and a subscription profile a metered equivalent,
regardless of adapter kind. A model with no configured rates, a missing profile
declaration, no reported usage axis, or pre-pin input semantics produces no
dollar figure rather than zero, and an axis absent from an otherwise reported
set is skipped rather than suppressing the figure. Codex CLI and OpenAI report
cache-inclusive input, so ordinary input is priced only when both cache axes are
present and subtractable, and each cache rate is applied once. The
inclusive-input meaning is pinned on the call when prepared, so a later
configuration change reusing the target with another adapter cannot reinterpret
historical usage.

The adapter invents no credential-value shape of its own; the profile's
configured billing kind labels derived cost, and adapter kind and delivery do
not. Each direct HTTP adapter resolves the durably pinned reference during send
preparation, after the durable `Prepared` record and before send authorization,
and scopes the value to that request. A failed resolution, or a value that
cannot form an HTTP header, is a typed known preparation failure: the call ends
`KnownFailed` with no automatic retry and no fallback. A provider rejecting the
credential after send is ordinary outcome evidence, not a preparation failure;
[model-call execution](model-call-execution.md) owns that outcome.

A code-host tool resolves its fixed `github-primary` reference only after the
durable tool attempt is authorized `InFlight` and immediately before its
transport call, and no model argument, client, or runner can select or receive
that credential. Tool attempts store neither integration references nor values;
the immutable compiled code-host declaration selects `github-primary` again when
execution resumes. The web-search and pull-request tools resolve their fixed
integration credential on each request and convert a missing or unusable value
into known-failure evidence. The plan tools require no credential profile,
egress policy, or workspace root, and model arguments cannot select another
session or storage adapter.

Exact-value redaction is seeded with the credential a direct HTTP adapter
resolves at preparation and retained in the request's one-shot capability, so it
exists only for a profile whose value the daemon reads; a code-host tool instead
resolves its fixed reference and builds its scrubber inside execution. Every
provider-controlled text leaving such an adapter, and every checked string in a
successful code-host result, is scrubbed of that value and its JSON-escaped form
before it crosses into evidence. CI job-log downloads also retain the scrubber
for the App token used by the redirect response. Truncated logs and repository
file selections scrub trailing prefixes of either response-credential spelling
before applying the final text bound. An `ambient` or `codex_home` profile gives
the daemon no value; its output follows the
[runtime substrate](runtime-substrate.md).

The GitHub and code-host adapters share `github-primary`, which needs API access
to read pull requests, publish reviews and comments, reply to and resolve review
threads, read repository files and directories, read checks and CI job logs, and
rerun failed jobs. Neither adapter pushes Git changes. The repository-watch
credential needs read access for polling and checkout provisioning. Classic
`repo` is broader than read-only access, so a fine-grained read credential
limits that role to the watched repositories. An optional absolute
`push_credential_file` on `[[repository_watch.repositories]]` supplies a
deployment-owned HTTPS token or SSH private key. Optional `push_remote_url`
selects an exact HTTPS, `ssh://`, or `git@host:` destination; its default is the
watched repository's GitHub HTTPS URL. HTTPS uses the push token, or the
installation token for an App-backed repository targeting GitHub when no
separate push file is configured. SSH uses the configured key when present, or,
on Linux, the host SSH agent exposed through `SSH_AUTH_SOCK` in the sandbox.
Agent-backed push authority is derived only when that socket accepts a
connection. Each configured credential file is reread through
`FileCredentialAccess` on every push; HTTPS passes an authenticated URL rewrite
in the child environment, and SSH retains a private temporary key file through
push and remote confirmation. GitHub HTTPS destination matching uses the parsed,
normalized hostname.

Push credential files participate in the repository-watch credential isolation
checks, including symlink and hard-link aliases of polling, push, and webhook
credentials. The push family is registered from the configuration installed by
durable reload recovery at startup; a repository-watch reload that changes push
registration takes effect at the next boot.

The optional `[repository_watch]` section composes the
[repository-watch module](repo-watch.md). Its `enabled` boolean defaults to
true; false disables module polling, webhook listening, and command dispatch,
including convergence-sweep target enrollment and session commissioning. The
`workflows_enabled` boolean defaults to false and selects workflow-driven
observations when true. Repository-watch duration fields accept integer seconds
or Jiff's friendly unsigned-duration strings; rule cooldowns retain whole-second
precision.

The optional `[convergence]` table deserializes the
[shared convergence policy](../../crates/convergence/README.md), including its
validation of nonempty reviewer identities after bot-suffix normalization.
Convergence reads and the sweep require this policy; other code-host operations
do not use it.

Each request and execution pass uses one immutable catalog snapshot.

A reload that adds, edits, or removes `repository_watch.rules` commits
activations and deactivations in the [reconciliation transaction](repo-watch.md)
that records each activation's repository event tail, inside the reload
boundary. A reload pauses sweep admission and stops and joins active sweep
attempts before re-running convergence configured-target reconciliation after
rule activation and event-tail capture commit, inside the reload boundary, using
an empty effective target set when repository watch is disabled; sweep admission
resumes under the replacement snapshot on success or the prior snapshot on
rule-revision rejection only if that snapshot validates against the current
startup-only sections, as [reload recovery](process-protocol.md) requires.
Enabling convergence while repository watch is enabled composes the sweep task;
disabling either terminates the task. A running sweep reads the new targets,
template, interval, and credential path at its next attempt.

Startup installs the OAuth registration catalog after database migrations. OAuth
provisioning runs the configured device exchange, retains refresh and identity
tokens with the canonical configuration tuple and generation, and stores no
authorization when the response lacks an identity token. Authorization commits
and pool-policy membership insertion serialize account-independence checks
against every retained co-membership.

`oauth` is spelled `delivery = "oauth"` with exactly five required fields:
`client_id`, `token_url`, `refresh_token_url`, `device_authorization_url`, and
the string array `scopes`. These are configuration, never build-provided
constants. `client_id` is 1 through 1,024 NUL-free UTF-8 bytes preserved
exactly. `scopes` holds 1 through 64 strings of 1 through 256 bytes, each byte
an RFC 6749 scope-token character, declared order is request order, exact
duplicates are rejected, and no normalization occurs. All endpoints are absolute
`https` URLs with no fragment and no user information; every other scheme is
rejected with no plaintext or local-host exception. The tuple is compared by
parsed canonical components, scheme, lowercased host, effective port, path, and
query, never by configured bytes. The delivery admits only
`billing_kind = "subscription"`.

Refresh sends JSON `{ client_id, grant_type: "refresh_token", refresh_token }`
to `refresh_token_url`; device polling uses `token_url`.

Refresh and dispatch compare the stored tuple with the current registration
under the profile row lock; mismatch quarantines the generation before any token
is sent. One process-shared refresh per profile and generation marks refresh in
progress before one POST, with redirects and automatic retries disabled.
Cancellation while waiting for initial profile locks or before refresh is sent
cancels preparation; an unsent refresh clears its marker. Definitive
non-rotation clears the marker. Connection loss after possible request bytes,
cancellation after send, redirects, indeterminate responses, and refresh tokens
rejected as expired, reused, or revoked quarantine the generation. Refresh
replaces returned refresh and identity tokens in one commit, retaining either
token when omitted; differing account identity quarantines. An ambiguous
replacement commit is reread: a committed replacement is adopted and an
uncleared marker quarantines. Access tokens live only in memory and refresh
lazily after restart. Cache expiry comes from the access-token JWT `exp` claim.
Cached tokens with at most five minutes remaining refresh before installation to
stay outside Codex's proactive-refresh window.

Dispatch holds the profile row lock through copying access and identity tokens
and account metadata into a scratch home, withholding the refresh token. The
adapter seeds exact-value redaction before writing either token. Homes use
`0700` directories and `0600` regular files beneath one daemon-owned `0700`
root, with descriptor-relative creation and removal rejecting symlinks. The root
appends `.oauth` to the process socket path and cannot overlap runner socket
artifacts. Completion removes the home; startup validates all retained homes
before scavenging and fails without removal on ownership, type, or containment
mismatch. The Codex child uses the home's file backend and token-only
authentication with ambient credentials, keyrings, helpers, and external stores
disabled; inability to deliver the home is a typed pre-send failure. An
access-token rejection during an invocation refreshes the token and retries the
same profile in a new durable call. A rejection after that successful refresh,
or a failed refresh, rotates to the next profile. A successful invocation clears
the token's rejection-recovery state. An expired access token does not
quarantine the profile or apply its pool rejection action. Delivery failure
evidence and quarantine commit atomically and bypass pool trigger policy. OAuth
quarantine reads lock only currently registered OAuth pool members. Successful
re-provisioning clears OAuth delivery-origin quarantine and cached access;
failure preserves both. Deletion holds the dispatch profile lock while removing
authorization and cached access, advances the retained generation, and preserves
registration and history.

A `codex_home` profile accepts `max_concurrent_invocations` from 1 through
1,024. The startup registration bounds per-member invocation reservations; an
omitted bound leaves the profile unbounded. Saturation participates in
credential pool admission as [contention](credential-availability.md).

## Planned

- Input-modality declarations on model and serving-target records, and the blob
  catalog they feed: [design](../design/configuration-and-credentials.md).
- Dated rate windows on a model entry; the present grammar admits one flat rate,
  which is one window across all time:
  [design](../design/configuration-and-credentials.md).
- Operator-assigned provider-safe identities for configured instruction roots:
  [design](../design/configuration-and-credentials.md).
- Workspace-instruction transport and capacity declarations on model,
  serving-target, and adapter-mapping records, and the origin-acceptance check
  that enforces them: [design](../design/configuration-and-credentials.md).
- Durable workspace records written from the per-session derivation:
  [design](../design/configuration-and-credentials.md).
- Pre-activation workspace binding and template instruction selectors:
  [design](../design/configuration-and-credentials.md).
- Codex CLI `file` delivery and OAuth database-restore rules:
  [design](../design/configuration-and-credentials.md).
- Round-robin selection (`round_robin`):
  [design](../design/configuration-and-credentials.md).
- Credential-exclusion lifecycle: reset-aware expiry, probe recovery, and
  coalescing action-head generations:
  [design](../design/configuration-and-credentials.md).
- Pool-policy credential history and the migration of family-to-reference
  entries: [design](../design/configuration-and-credentials.md).
- Explicit session credential update:
  [design](../design/configuration-and-credentials.md).
- Runner credential use during provisioning and execution: lease admission, file
  read, injection, the Git credential helper, and output scrubbing:
  [design](../design/configuration-and-credentials.md).

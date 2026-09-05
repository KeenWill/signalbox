# Configuration and credentials design

This design is not built; it extends
[configuration and credentials](../spec/configuration-and-credentials.md) and is
deleted when the work lands.

## Goal

The daemon reloads its catalogs without a restart, prices a call against dated
rate windows, declares each model's input modalities and workspace-instruction
capacity, records the workspace roots it derives, binds a session to its
workspace before its first turn when a template asks for it, and holds a
daemon-owned OAuth authorization for a Codex CLI child without handing the child
the refresh token. Credential exclusions expire, coalesce, and clear; sessions
carry the complete pool policy they were created under; and a runner reads,
injects, and scrubs a granted credential for the work it dispatches.

## Design

Reload is one admin verb, `reload_configuration`, owned by
[process protocol](../spec/process-protocol.md). It re-reads the configured
paths, validates the complete replacement exactly as startup does, and swaps the
in-memory catalogs atomically on success. The reloadable sections are the model
and alias catalog with its rate windows, the session-template catalog, and the
repository-watch configuration, whose reload transaction is owed to
[repository watch](../spec/repo-watch.md); every other section is startup-only.
Any failure, and any replacement whose startup-only sections differ, leaves the
running configuration in place. File watching and polling are external tooling
that calls the verb.

A model entry carries zero or more `[[models.rate_windows]]` entries, each one
dated price window over that entry's own `provider_model`. A window names the
commercial provider that published the rates, a channel of `api` or `batch_api`,
an `effective_from` date, an optional `effective_until` date, the all-or-none
four USD-per-million-token rates as nonnegative decimal strings, and a
provenance pair of source URL and retrieval date. Both bounds are canonical
`YYYY-MM-DD` strings; a window covers a call timestamp from `effective_from` at
00:00:00 UTC inclusive to `effective_until` at 00:00:00 UTC exclusive, and
windows for one target and channel do not overlap. A window's identity is its
provider, provider model, channel, and `effective_from`, and that identity is
what a derived cost names. A published window's rates and bounds do not change;
the one admitted edit closes an open window by setting its `effective_until` to
the `effective_from` of the successor installed with it. Declaring part of a
rate set is a configuration error, and declaring no window yields no dollar
figure. Two model entries naming one target agree on their complete window sets
or complete absence. Cost derivation selects the window covering the call's
execution timestamp on the channel pinned on the call. The present flat rate
migrates into one `api`-channel window that keeps its four rates, starts before
every stored call, never closes, and takes its provider and provenance pair from
the operator, so it is one window across all time. The document root may carry
an optional `[verified_through]` table mapping a provider name to a date; it is
provenance metadata, never a resolution gate.

Each `[[models]]` and `[[serving_targets]]` record admits `input_modalities`: a
nonempty array from the closed set `text`, `image`, and `document` that rejects
duplicates, must contain `text`, and defaults to exactly `["text"]`. The
model-capability process projection materializes the selectable record's
modalities in the closed order `text`, `image`, `document`, and call preparation
uses the effective serving record's set. A typed media result whose target lacks
the modality fails preparation before durable authorization, as
[blob storage](../spec/blob-storage.md) requires.

A configured instruction root carries two identities.
`ConfiguredInstructionRootId` is the template-selector identity: SHA-256 over
the literal UTF-8 `signalbox-configured-instruction-root-v1`, then the canonical
path as an unsigned 64-bit big-endian byte length and that many bytes, displayed
as 64 lowercase hexadecimal characters. It is daemon- and template-side only and
never reaches a model or provider. The provider-safe reference is the identity a
model may see and is therefore operator-assigned rather than path-derived,
because a public unkeyed path hash lets a reader hash guessed directories and
recover the layout the reference withholds. An entry of `registered_roots` may
be written as a table with exactly `path`, validated as the string form is, and
`provider_reference`, exactly 64 lowercase hexadecimal characters naming 32
opaque bytes the operator generates once and keeps stable. Startup rejects a
missing reference, a duplicate across roots, and one equal to any root's
`ConfiguredInstructionRootId`. The daemon persists the association from each
root's `ConfiguredInstructionRootId` to its reference and rejects a
configuration presenting a known root with a different reference before
discovery or registration reuse runs. The association is a reservation in both
directions for as long as any stored evidence names it: a reference retired with
its root is refused for any other root while retained aliases or eligibility
entries still carry it, and the owning root re-presenting its own reference is
the ordinary restart. A root without a reference cannot become provider-visible.

Every `[[models]]`, `[[serving_targets]]`, and `[[adapter_mappings]]` record
admits an all-or-none pair: `workspace_instruction_transport = "typed_system"`
and `workspace_instruction_capacity_bytes`, a positive `u32` measured over the
exact serialized `WorkspaceInstructionRegion` bytes. Omitting both means
unsupported; supplying one, another transport spelling, or a capacity below the
fixed 65,536-byte version-one region ceiling is a typed startup failure. The
effective serving record, including an alternate fast target, is authoritative
for a call, and its adapter mapping must declare the same transport and at least
that capacity or startup rejects the configuration. A mapping's capacity is the
adapter implementation's maximum for that family, at least 65,536 and at least
every model or serving target in the family; a mapping omitting the pair maps
only targets that also omit it. Context-window tokens are never converted into
this byte value. Every origin-creating acceptance transaction resolves the
frozen selection against the live immutable catalog and rejects the origin,
before freezing it, when the effective serving record lacks typed-system
transport or byte capacity for the session's complete retained region. The check
belongs to origin acceptance, not to `SubmitInput`: goal attach, goal resume,
and scheduler continuation mint accepted origins too. When the frozen settings
enable fast mode on a model whose `fast_mode` is `alternate_target`, the check
applies to the `fast_target_id` serving record and its mapping; where the
effective record is undetermined at acceptance, every record the selection may
still pin must satisfy it. The typed rejection accepts no input, creates no
turn, and changes neither defaults nor admissions.

The workspace table keys a workspace by an identity and the canonical root it
was minted for. The record is written from the per-session derivation and never
read by it: nothing consults the table to decide which root to open. The root is
canonicalized once, when the record is minted, and stored in canonical form; no
later comparison normalizes anything. `WorkspaceId` generation follows
[identity and commands](../spec/identity-and-commands.md), and the grants keyed
by the record are owned by
[git authority threat model](../spec/git-authority-threat-model.md).

An ordinary template admits one optional `instruction_selectors` array of at
most 256 inline tables. Each table has exactly `root`, `source_path`, `kind`,
and `source_sha256`, plus `configured_root_id` exactly when
`root = "configured"`. `root` is `"workspace"` or `"configured"`; the configured
identity and source hash are 64 lowercase hexadecimal characters encoding 32
bytes; `kind` is `"agent_document"` or `"agent_skill"`; and `source_path` is 1
through 4,096 UTF-8 bytes of nonempty normal components separated by single `/`
characters with no leading or trailing slash and no U+0000. The loader rejects
duplicates and canonicalizes selectors by root with `workspace` first,
configured-root digest bytes when present, raw source-path bytes, kind with
`agent_document` first, then source-hash bytes. The resolved bundle retains that
ordered sequence, and session creation copies it unchanged as unresolved
eligibility input. Content-digest version three is selected by the presence of
the `instruction_selectors` key, including when the array is empty: its first
frame is `signalbox/session-template/content-digest/v3`, and after the
model-settings digest it writes the selector count as eight unsigned big-endian
bytes, then each canonical record as the length-framed root spelling, the 32 raw
configured-root digest bytes for `configured` only, the length-framed source
path, the length-framed kind, and the 32 raw expected source-hash bytes. A
template without the key keeps version two, so every existing template and every
generated review template keeps its digest.

A session whose template carries a `workspace` selector binds its workspace
before its first turn activates, through the same configured-versus-derived
resolution, misprovisioning refusal, identity checks, and sticky
process-lifetime binding the spec page states; discovery and selector resolution
run against the bound root before activation freezes eligibility, and no
candidate pathname is probed while the binding is open. Instruction-eligibility
initialization records the selector-set hash, the complete discovery identity,
the resolved root path, and the exact worktree and `.git` filesystem identities.
One session-scheduler transaction revalidates the live process binding against
that evidence, installs the initial allow-list, copies it into the first turn's
eligibility snapshot, and activates the turn; it commits all three or none.
After a restart, an already-active first turn proceeds only after the binding
resolver reconstructs its process record from the durable correlation; missing
or different filesystem identities fail closed, and recovery neither rescans
selectors nor substitutes newly registered bundle identities.
Configured-root-only selectors carry no workspace correlation but use the same
atomic install-and-activate transition.

Codex `file` delivery resolves the pinned reference during capability
preparation and, after the common trailing-termination narrowing, admits exactly
a nonempty NUL-free UTF-8 value of at most 65,536 bytes; empty, non-UTF-8,
NUL-containing, or oversized content fails preparation as typed
`CredentialUnusable` and spawns no child.

`oauth` is spelled `delivery = "oauth"` with exactly four required fields:
`client_id`, `token_url`, `device_authorization_url`, and the string array
`scopes`. These are configuration, never build-provided constants. `client_id`
is 1 through 1,024 NUL-free UTF-8 bytes preserved exactly. `scopes` holds 1
through 64 strings of 1 through 256 bytes, each byte an RFC 6749 scope-token
character, declared order is request order, exact duplicates are rejected, and
no normalization occurs. Both endpoints are absolute `https` URLs with no
fragment and no user information; every other scheme is rejected with no
plaintext or local-host exception. The tuple is compared by parsed canonical
components, scheme, lowercased host, effective port, path, and query, never by
configured bytes. The delivery admits only `billing_kind = "subscription"`.

Provisioning is explicit and operator-invoked; the daemon performs the
device-authorization exchange itself against the profile's configured endpoints
and never drives the CLI's own login, because the CLI would mint a tuple baked
into its binary rather than the one the profile declares. The command requests a
device authorization, relays the user code and verification URI to the operator,
polls the token endpoint under the one-POST-per-attempt and no-redirect rules,
and on success harvests the refresh token, the identity token, and non-secret
account metadata into one transaction. Provisioning that returns no identity
token fails typed and stores nothing. That transaction decides account-level
independence: it consults every profile sharing a pool-policy revision with this
one and fails, storing nothing, when a different co-member already stores the
harvested account identity. Interning a pool-policy revision applies the same
rule to the membership it freezes, under the same locks; between them the two
moments are exhaustive. The lock span is owned by
[persistence protocol](../spec/persistence-protocol.md). Whether provisioning
disturbs an operator's existing login is the authorization server's decision and
not a property this delivery provides.

A stored authorization is bound to the exact `client_id`, `token_url`,
`device_authorization_url`, and ordered `scopes` it was minted under, persisted
in the same transaction as the token generation. Every refresh and every
dispatch compares that stored tuple with the current registration by canonical
components, under the profile row lock and before any request is formed; a
mismatch never sends the stored token, the generation quarantines, and
re-provisioning is the only recovery.

The daemon is the sole refresher. It locks the profile row, reads the stored
token, and transactionally marks that generation's refresh in progress; the
winner owns one process-shared single-flight keyed by profile and generation,
and a concurrent preparation observing the marker joins it rather than starting
another exchange. The limit is one POST per attempt, to the configured
`token_url`'s exact canonical target, with redirects and automatic retries
disabled at every layer. A failure that definitively did not rotate the token
clears the marker and leaves the generation available to a later attempt. Replay
after an ambiguous exchange is forbidden: once request bytes may have been
written, a connection loss, redirect, or indeterminate response leaves the
outcome unknown, the daemon never presents that token again, and the generation
quarantines. A second transaction re-locks and matches the generation, compares
the account identity the response carries with the stored one, persists the
returned token, and clears the marker before the new access token is used; a
differing identity quarantines instead. A committed replacement overwrites the
previous refresh token. Every refresh that returns a new identity token replaces
the stored one in the same commit; a refresh that returns none leaves it in
place. When a replacement commit is ambiguous the daemon rereads the durable
generation, whether it restarts or stays alive with the marker still present: a
committed replacement is adopted and published to the joined single-flight, and
an uncleared marker quarantines. Cancellation is definitely non-rotating before
possible request bytes and ambiguous afterward. Access tokens are held in memory
only; a clean restart discards them without contacting a provider, and the first
later preparation that needs the profile refreshes lazily, so recovery stays
configuration-independent. A refresh rejected as expired, reused, or revoked is
permanent: the profile quarantines and re-provisioning is the only recovery.
Delivery-layer quarantine that occurs before a provider request names its own
typed refresh or credential-home failure, commits that evidence atomically with
the quarantine, and bypasses pool trigger policy.

Dispatch supplies each invocation a scratch credential home carrying the
complete authentication state the CLI needs to form a request minus the refresh
token: the daemon-minted access token, the identity token, and the harvested
account metadata. Withholding the refresh token permits concurrency: processes
holding none share no mutable authorization state, and the daemon refreshes once
on behalf of all of them. Every token written into a scratch home seeds the
adapter's exact-value redactor before it is written, and a path that cannot
install the redactor fails preparation before writing the home or spawning the
CLI; how the adapter applies the scrub is owned by
[runtime substrate](../spec/runtime-substrate.md). Scratch homes live beneath a
single daemon-owned `0700` root, are themselves `0700`, contain only `0600`
regular files, are created and removed through descriptor-relative operations
that reject symlinks, and are removed on normal completion. Before accepting
work, startup scavenges every entry it can prove is an owned scratch home
beneath that root; an ownership, type, or containment mismatch fails startup and
removes nothing. Dispatch forces the CLI's file or ephemeral backend to that
home and disables ambient, keyring, helper, and external stores; failure to
enforce that selection is a typed pre-send delivery failure. An access token
that expires while a long invocation is running is not an authorization failure:
the profile stays eligible for a later call and the failed call is not retried
automatically.

Database restore transactionally quarantines every restored `oauth` profile
before signalboxd may start against the restored state; an ordinary restart does
not. No present process message provisions, re-provisions, deletes, or clears
quarantine for an `oauth` profile; the delivery needs an operator-authorized
administrative boundary with an idempotency and response contract, owned by
[process protocol](../spec/process-protocol.md), before an `oauth` profile is
usable.

`max_concurrent_invocations` on a `codex_home` profile is a reserved field with
the range 1 through 1,024. Capacity reservations, contention waits, and
refresh-race coordination become admissible together; no accepted bound is
inert. `round_robin` owns one durable global cursor per interned pool-policy
revision and priority value. The repository interns the policy's complete
canonical structural value, pool name, ordered members, each member's expected
adapter and delivery kind, membership settings, tie-break, exhaustion rule, and
trigger actions, under a uniqueness constraint on that value, so an unchanged
document reuses one revision across restarts and an exact reversion reuses the
old one; hashes accelerate lookup but never establish equality. The cursor names
one member ordinal in that priority's declaration order. An admissible sticky
member is still preferred; otherwise selection starts at the cursor and walks
cyclically, skipping inadmissible members, and the transaction that commits that
`Prepared` record advances the cursor to the next declared member even when that
member is excluded. A sticky selection advances nothing. Preparation locks the
cursor row `FOR UPDATE` after its session scheduler, the candidate action heads,
and any candidate capacity rows, then rereads the facts those locks protect; no
path acquires a capacity row while holding a cursor row. A failed preparation
advances nothing. `least_used` and a headroom reserve are admitted once an
adapter reports remaining capacity; that adapter defines the normalized
quantity, the observation lifetime, and a deterministic secondary tie-break. The
admission gate is the capacity report alone.

A membership exclusion is reset-aware: a reported reset time clears it when that
time passes, and only an exclusion carrying no reported reset is indefinite. The
generation's effective reset is the latest reset any attached correlation
reported, and an observation reporting no reset makes the generation indefinite;
indefinite is absorbing. Only an operator clear, an availability probe that
costs nothing and calls no model, or another durable availability update ends an
indefinite policy-origin generation. An operator clear removes a pending
`switch_next_turn` displacement or an `avoid_new_sessions` exclusion exactly as
it clears a quarantine; the request is owned by
[process protocol](../spec/process-protocol.md). Each profile carries a durable
action head, and every transaction that mints, activates, or clears an exclusion
rereads the current generation under that head's `FOR UPDATE` lock. The first
commit mints the generation; a later commit for an exclusion already active at
the same scope and of the same origin records its correlation against that
generation and mints no second one. Origin is part of the coalescing key because
a policy-origin quarantine is clearable by operator command while a
delivery-origin one requires re-provisioning, except a `codex_home` quarantine,
which an operator clears once the store is repaired; a delivery-origin failure
against a profile carrying an active policy-origin generation mints its own, and
the two are cleared and reported separately.

The session credential history event carries a complete family-to-pool-policy
snapshot rather than a family-to-reference one. Each immutable policy includes
the pool name, ordered members, every member's frozen adapter and delivery kind,
membership settings, tie-break and exhaustion rules, and all trigger actions;
preparation never resolves it through the current document's pool table. Before
credential resolution, preparation requires the selected member's frozen adapter
to equal the resolved target's adapter and requires the current registration to
retain both that adapter and delivery kind; absence or mismatch is a typed
pre-send credential-configuration failure that blocks scheduling. Each call pins
the interned `pool_policy_id` at the `Prepared` insert beside its credential
reference, and observation commit reloads that pinned policy. The one-time
migration of existing family-to-reference entries is deterministic: each entry
becomes a singleton policy retaining exactly the stored reference, one member at
priority 1, no headroom reserve, `first_listed`, `on_pool_exhausted = "fail"`,
and `stay` for every trigger. The member's adapter and delivery kind come from
the validated registration of the profile the entry names and from nowhere else;
a reference naming no current registration is not migrated and blocks
scheduling.

An explicit session credential update appends the next complete history event
with its own command provenance and advances the head by exactly one; it never
rewrites history and never applies a configuration edit automatically.

A session may hold no credential, and no boundary infers one: with no profile
selected the daemon issues no grant, the lease carries no credential
authorization, and the runner injects nothing; a repository entry naming a
profile then fails `credential_unavailable`, while a named profile is granted to
a session with no repository because the credential is scoped to the session's
dispatches. At lease admission the runner requires the exact granted name in its
startup configuration, and absence rejects the claim before any executable
capability is issued. Immediately before each dispatch, and again when
provisioning a repository worktree whose clone is authenticated, the runner
opens the configured path without following symlinks, requires a `0600` regular
file owned by the effective user, reads at most 65,536 bytes, and drops trailing
`\n` and `\r` bytes; empty, NUL-containing, unreadable, oversized, wrong-owner,
wrong-mode, or non-regular files are typed unavailable failures. The value is
scoped to that dispatch or provisioning and never cached. It is supplied only
under the configured environment name inside the bubblewrap namespace and never
in arguments, remote URLs, Git configuration, the inherited environment, errors,
or logs. Git tools use a fixed runner-owned credential helper bound to the
repository entry the dispatch resolved, the manifest's repository key for an
existing worktree or the checked `repository` argument for `git_clone`; the
helper returns the value only when the query's protocol, exact `github.com`
host, and owner/repository path match that entry's validated canonical URL and
the entry names exactly the granted profile. Every guarded Git command that
installs the helper also forces `credential.useHttpPath=true` on the same
command line, because Git otherwise strips the path before calling the helper.
The runner scrubs the exact value and its JSON-escaped form from admitted
stdout, stderr, and result text; it cannot prevent model-controlled code from
using the value within its granted scope. A credential failure after a claimed
dispatch is a fixed `ExecutionFailed` observation naming only the profile and
failure class, and it never authorizes an automatic repeat of side-effecting
work.

## Compatibility constraints

The configuration grammar already admits the `oauth` spelling, the `codex_cli`
`file` spelling, `max_concurrent_invocations`, `headroom_reserve_percent`,
`round_robin`, `least_used`, and a non-`stay` `on_headroom_low`, and rejects
each at startup as undelivered or unobservable. Supplying a surface for any of
them changes no grammar.

The session credential record and entry rows are append-only behind a guarded
head. Any update appends one complete event and advances the head by one.

The workspace table and its constraints exist and nothing writes them; the
per-session derivation must never start reading them.

Every durable action row is appended per observation under the profile's
action-head lock and never updated, except the `switch_next_turn` displacement a
later preparation consumes. Generations and correlations are added beside those
rows, not by rewriting them.

The one flat rate per model is one window across all time. Cost derivation for
every call already stored produces the same figure when windows arrive.

Content digest version two is unchanged for every template without an
`instruction_selectors` key, so no existing template or generated review
template changes digest.

The acceptance check for retained-region capacity belongs to origin acceptance,
which every origin-minting path shares; it is not a `SubmitInput` feature.

The runner has no model-provider configuration field and rejects reserved
model-provider names; runner credential execution adds no such field.

Every catalog reader takes one immutable snapshot, so reload can swap the
snapshot atomically without a reader observing two documents.

## Acceptance criteria

- `reload_configuration` re-reads and validates the complete document as startup
  does, swaps atomically on success, and leaves the running configuration in
  place on any failure or on a startup-only difference.
- A model entry admits non-overlapping dated windows per channel, a cost read
  names the window covering the call's timestamp, and every stored call prices
  as it did under the flat rate.
- `input_modalities` is admitted, defaulted, projected in closed order, and used
  by preparation from the effective serving record.
- A configured root's provider-safe reference is operator-assigned, persisted
  against its path-derived identity, and refused for any other root while stored
  evidence names it.
- Transport and capacity declarations are validated across model, serving
  target, and mapping, and an incapable origin is rejected before it freezes on
  every origin-minting path.
- The derived root's workspace record is written once, in canonical form, and
  never consulted to choose a root.
- A template selector binds the workspace before first activation, and the
  install-and-activate transition commits atomically and fails closed after
  restart on a changed root.
- An `oauth` profile is provisioned, refreshed under a single-flight with one
  POST per attempt, never replayed after an ambiguous exchange, quarantined on
  tuple mismatch or rejected refresh, and its scratch home never holds a refresh
  token.
- `max_concurrent_invocations` is admitted together with the reservation that
  gives it effect, `round_robin` selects through its durable cursor, and
  `least_used` and headroom reserves are admitted only once an adapter reports
  remaining capacity.
- An exclusion with a reported reset clears when it passes, an operator clear or
  zero-cost probe ends an indefinite policy-origin generation while a
  delivery-origin one ends only by re-provisioning except a `codex_home`
  quarantine, which an operator clears once the store is repaired, and repeated
  triggers of one origin coalesce onto one generation.
- Session credential history carries the complete pool policy, each call pins
  its policy id, every existing entry whose profile is still registered migrates
  to a singleton policy that resolves the same credential it did before, and an
  entry naming no current registration blocks scheduling.
- An explicit credential update appends one event and advances the head by one.
- A runner injects a granted credential only under the configured environment
  name inside the sandbox, the Git helper answers only the matching canonical
  repository with `credential.useHttpPath=true` forced, and admitted output is
  scrubbed of the value and its JSON-escaped form.

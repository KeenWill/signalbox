# Configuration and credentials design

This design is not built; it extends
[configuration and credentials](../spec/configuration-and-credentials.md) and is
deleted when the work lands.

## Goal

The daemon prices a call against dated rate windows, declares each model's input
modalities and workspace-instruction capacity, records the workspace roots it
derives, binds a session to its workspace before its first turn when a template
asks for it. Credential exclusions expire, coalesce, and clear; sessions carry
the complete pool policy they were created under; and a runner reads, injects,
and scrubs a granted credential for the work it dispatches.

## Design

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

Database restore transactionally quarantines every restored `oauth` profile
before signalboxd may start against the restored state; an ordinary restart does
not.

`round_robin` owns one durable global cursor per interned pool-policy revision
and priority value. The repository interns the policy's complete canonical
structural value, pool name, ordered members, each member's expected adapter and
delivery kind, membership settings, tie-break, exhaustion rule, and trigger
actions, under a uniqueness constraint on that value, so an unchanged document
reuses one revision across restarts and an exact reversion reuses the old one;
hashes accelerate lookup but never establish equality. The cursor names one
member ordinal in that priority's declaration order. An admissible sticky member
is still preferred; otherwise selection starts at the cursor and walks
cyclically, skipping inadmissible members, and the transaction that commits that
`Prepared` record advances the cursor to the next declared member even when that
member is excluded. A sticky selection advances nothing. Preparation locks the
cursor row `FOR UPDATE` after its session scheduler, the candidate action heads,
and any candidate capacity rows, then rereads the facts those locks protect; no
path acquires a capacity row while holding a cursor row. A failed preparation
advances nothing. Capacity-dependent selection follows
[configuration and credentials](../spec/configuration-and-credentials.md).

A membership exclusion is reset-aware: a reported reset time clears it when that
time passes, and only an exclusion carrying no reported reset is indefinite. The
generation's effective reset is the latest reset any attached correlation
reported, and an observation reporting no reset makes the generation indefinite;
indefinite is absorbing. Only an operator clear, an availability probe that
costs nothing and calls no model, or another durable availability update ends an
indefinite policy-origin generation. Each profile carries a durable action head,
and every transaction that mints, activates, or clears an exclusion rereads the
current generation under that head's `FOR UPDATE` lock. The first commit mints
the generation; a later commit for an exclusion already active at the same scope
and of the same origin records its correlation against that generation and mints
no second one. Origin is part of the coalescing key because a policy-origin
quarantine is clearable by operator command while a delivery-origin one requires
re-provisioning or deletion, except a `codex_home` quarantine, which an operator
clears once the store is repaired; a delivery-origin failure against a profile
carrying an active policy-origin generation mints its own, and the two are
cleared and reported separately.

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

The configuration grammar already admits the `codex_cli` `file` spelling and
`round_robin`, and rejects each at startup as unsupported. Supplying a surface
for any of them changes no grammar.

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

## Acceptance criteria

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
- `round_robin` selects through its durable cursor.
- An exclusion with a reported reset clears when it passes, an operator clear or
  zero-cost probe ends an indefinite policy-origin generation while a
  delivery-origin one ends by re-provisioning or deletion except a `codex_home`
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

# Work backlog

> **Non-authoritative planning scratchpad — do not review for consistency.**
> This file decides nothing and is not a statement of record. It is the owner's
> working map of what work exists and what can run in parallel; entries are
> orientation, not design. Every design choice, accepted cost, blocker, and open
> question named here is settled elsewhere and that record governs, never this
> file: design in the owning `docs/spec/` page's diff at pickup, decisions in
> pull-request descriptions and git history, open questions in
> `docs/open-questions.md`. Do not hold entries to cross-document consistency or
> treat their prose as normative — it is deliberately loose and is superseded by
> the real record when an item is picked up. The owner revises this file freely;
> agents never reorder it.

The owner-curated menu of pullable work for goal runs, a granular companion to
the target model's [priority order](../target-model.md#priority-order). Entries
state what they touch so parallel launches are mechanical: any set of items with
disjoint `Owns`/`Collides-with` groups may run concurrently. This is a
parallelism-and-collision map, not a design document — designs happen as
specification diffs when an item is picked up.

Entry order is curated through owner sessions and does not override milestone
priority (the target model's priority order plus explicit owner flags — the
owner-flagged next major milestone today is the tool-loop foundation). How a
milestone-less run selects from this file is defined once, in
[goal-mode.md](goal-mode.md); this file does not restate it. The owner reorders,
adds, and retires entries; agents never reorder.

Entry format: status is `ready`, `in-flight`, or `blocked-on: <what>`; size is
S/M/L/XL. Standing engineering cautions for every entry: hold typed identities
at every boundary including future SDKs; drive client state from acknowledged
facts, never optimistically; design runner authentication in from day one; never
ship an endpoint or state ahead of its semantics.

## Terminal stop and steer verbs [blocked-on: client stack merge] [size: S]

Owns: `apps/client`, `crates/process-protocol` (additive request kinds),
signalboxd server handlers. Collides-with: the client stack files. Steering and
proof-bearing stops are landed daemon-side with no client verb; this is the
cheapest capability on the board.

## Frontier scaling fix [ready] [size: M]

Owns: persistence read paths, domain frontier materialization. Collides-with:
turn machinery. The recorded post-model-call obligation: remove the quadratic
frontier/projection loads.

## OpenAI composition wiring [delivered by the adapter-wiring pair and the catalog rows] [size: S]

Owns: signalboxd configuration/composition, the model catalog example.
Collides-with: `apps/signalboxd`. The daemon composition wired the OpenAI and
Claude Code CLI adapters on 2026-08-06 (`agent/wire-openai-adapter`,
`agent/wire-claude-cli-adapter`), so no merged adapter is unreachable. The
catalog example then gained `[[models]]` rows for the openai, codex, and
claude_code families, so it no longer admits one provider only; those rows ship
commented, because each of those families needs process configuration or a key
file the checked-in example cannot carry.

## Model catalog automation [blocked-on: the wrapped-CLI drift-defense scaffolding] [size: S-M]

Owns: a scheduled provider-listing watcher and the catalog-diff step that rides
the wrapped-CLI pin-bump smoke, plus the PRs either one opens. Collides-with:
the drift-defense CI wiring the subscription-runtime tracks introduce, and
whichever entry holds the catalog file when a generated PR lands. Keeping the
catalog current is an operations concern the composition-wiring entry above does
not own; that entry makes the catalog multi-provider once, this one keeps it
from going stale.

Owner direction, 2026-07-25 (orientation only; per-model defaults, alias policy,
and identity-generation rules are settled in the owning spec diff at pickup, not
here): keeping the model catalog current should be automated, with the owner as
the merge gate. Two mechanisms, both composing with the drift-defense pattern
already recorded in the subscription-runtime entry's pin/Renovate/gated-smoke
addendum below rather than restating it. (1) A provider-API watcher — a
scheduled, main-only job under the same environment-protected-secrets rules as
the gated smokes — queries each configured provider's model-listing endpoint,
diffs the result against the catalog file (the `[[models]]` records in the
example daemon configuration, or wherever the catalog lives at pickup), and
opens a PR drafting entries for the new models with generated identities and
conservative defaults for owner review; it never merges anything itself. (2)
CLI-bump piggyback — for the wrapped-CLI runtimes, the Renovate pin-bump PR's
compatibility smoke also enumerates the CLI's advertised models and surfaces
catalog diffs in that same PR, so a CLI update shipping new models forces the
catalog question at the moment of change.

## De-hub naming pass [server rename executed; remainder blocked-on: in-flight stacks landing] [size: S]

The server rename executed on 2026-07-25 (`agent/signalboxd-rename`): crate,
binary, and directory are `signalboxd`, and "hub" vocabulary left the living
docs and config. Owns what remains: hub-named code identifiers and comment
vocabulary — `single_hub`/`SingleHubGuard`, `FencedHubDatabase`, `run_hub`,
`hub.sock` test fixtures, the persistence `hub_fence` module, and
"hub-minted"/"hub-resolved" comment vocabulary across crates; whether the
migration-committed `hub_fence_state` SQL names follow is the owner's call
(needs a new migration). Collides-with: broad — that vocabulary reaches
`crates/process-protocol`, persistence internals, and `apps/signalboxd` — so it
runs effectively solo and waits for the current stacks. "Hub" survives only as
occasional prose metaphor, never as the name of a binary, crate, module, or
protocol concept. The Swift client's naming is not this entry's job — it was
renamed to Signalbox-prefixed names in its own pass, and the native client
rewire owns any protocol-driven renames that remain.

Owner direction, 2026-07-25: the point previously deferred here is settled — the
server renames to `signalboxd`, and future runner processes are a separate
`signalbox-runner` binary (thin binaries over shared workspace crates), not the
same binary in a different role. This pass renames the server and carries no
open questions; the runner-protocol entry below records the runner texture.

## Owner-to-user rename [blocked-on: in-flight stacks landing; inventory step] [size: M]

Owns: naming across domain types, spec prose, and client-facing surfaces.
Collides-with: a broad prose surface, so it pairs with the de-hub pass after the
board quiets. Rename the platform actor "owner" to "user" everywhere except
three carve-outs the owner set: (1) process documents where "the owner" means
the repository owner personally (`AGENTS.md`, `goal-mode.md`, and this backlog);
(2) uses meaning ownership in the computer-science sense — a row, aggregate, or
state machine owning data; (3) historical commits and pull-request descriptions,
which remain unchanged. Starts with an inventory pass classifying every
occurrence before any mechanical rename.

## Conversation import [in-flight] [size: L]

Owns: new converter crate, session creation/ancestry, imported-conversation
store (new migration). Collides-with: session-creation surfaces only. Running as
a goal session with owner addenda (maximum-fidelity conversion, raw
preservation, adoption as a standing client capability rather than an
import-time mode). Owner addendum: an importer conformance corpus — synthetic
fixture conversations only, never the owner's real archives, covering each
source-format era, with golden/expect assertions on the imported result — is
part of the entry's scope.

Owner direction, 2026-07-26 (orientation only; the provenance marker's
representation and the lineage mechanism are settled in the owning spec diff at
pickup, not here): agent-to-agent subagent session transcripts import as
first-class conversations — the same imported-conversation store, the same
importer identity model, nothing second-class about how they are held. Two
additions come with them. First, linkage to the parent session's imported
conversation, recorded as durable evidence in the same spirit as the
source-session lineage evidence the header already carries. Second, a typed
provenance marker distinguishing an agent-driven session from a human-driven
one; it composes with the session satellites the session-metadata entry below
owns rather than standing up a parallel mechanism, and whether it lands as a
metadata tag or a dedicated typed field is the design pass's call.

One tension is flagged for pickup rather than settled here. The import spec's
recorded law forbids deriving identity from a filename or source path and keeps
converters bytes-only, yet for some source formats the parent linkage lives only
in the archive's directory layout — a per-session subagent directory whose
placement, not whose bytes, names the parent. The likely resolution is declared
lineage: the parent arrives as explicit caller-supplied evidence on the import
request, the operator asserting the relationship, which keeps the converter
pure. The same request also carries the source filename, captured as recorded
provenance evidence and never as identity input, which leaves the byte-digest
identity law untouched. It is cheap, auditable corroboration: some source
formats name their files by session identifier, and that is worth the most
exactly where in-band metadata is thin, as in the subagent case. The owning spec
diff decides.

Counting follows from the first-class stance: subagent conversations are
conversations and count in any conversation inventory, with the provenance
marker enabling filtered views rather than exclusion from the count.

## Migration baseline reset [blocked-on: schema-audit verdict; owner checkpoint call] [size: S-M]

Owns: `crates/persistence/migrations` (rewrite to a clean baseline), the
persistence spec's migration-inventory prose, history-sensitive tests that pin
migration versions, and dev-database recreation notes. Collides-with: any
in-flight work carrying a new migration or leaning on migration history. Squash
the migration set to a from-scratch baseline embodying the correct-choice
schema, per the pre-production schema discipline decision; the pending schema
audit decides scope, and each squash happens only at an owner-declared
checkpoint.

## Provider transport security [in-flight] [size: M]

Owns: the runtime adapter crates only. Collides-with: nothing on the board. The
transport/TLS/reqwest-upgrade work is landing; the parser piece is the remaining
open PR. No longer an unstarted item exposed to selection.

## Subscription-backed provider runtimes (three tracks)

New `ModelRuntime` adapters that spend subscription capacity instead of API
billing. A pure adapter crate that adds no signalboxd wiring collides with
runtime crates only — parallel-safe against everything else and against each
other. The exception is provider dispatch: whichever runtime track wires it
first (see below) also touches signalboxd composition, and therefore collides
with the OpenAI composition wiring entry and any other signalboxd-composition
work. One caveat: every runtime-track crate edits the root `Cargo.toml`
workspace-member list and `Cargo.lock`, and the provider-security track also
touches `Cargo.lock` (reqwest upgrade). That is a light merge-coordination point
(lockfile conflicts), not a semantic collision — land them in sequence or expect
trivial lockfile rebases. The runtime trait is rated stable (two-method
signature byte-stable since early on; evidence vocabulary grows additively), so
adapters written now are unlikely to reshape. Prior art exists in the owner's
own prior subprocess-based provider work and is supplied per session at launch,
not pointed at here; whatever CLI argv, JSON-event parsing, and
process-supervision it carries, its turn-shaped semantics must be tightened to
Signalbox's evidence-shaped contract (exit-0-without-a-terminal-marker is
BoundaryLoss, not success). Open design tensions the track's spec-diff must
resolve, not decide here: (1) a subprocess is one physical request the adapter
cannot prove is retry-free internally, so the spec-diff has to reconcile that
boundary with the one-physical-request invariants (INV-025/026); (2) for the
wrapped-CLI tracks below, auth rides the CLI's ambient subscription login, so
the spec-diff has to reconcile that with the credential-reference boundary and
per-request value durability the `ModelRuntime` contract pins (recovered calls,
logged-in-account changes).

The FIRST of these to wire also introduces the provider-dispatch mechanism
signalboxd lacks today (selection is currently two hardcoded "anthropic"
points); an adapter-only PR does not touch signalboxd, but the first
second-provider wiring PR must add the enum/factory. The adapter-author
conformance checklist and the loopback test pattern from the runtime-adapter
study are the reusable body of each goal prompt.

Further prior art, for the design rather than the code: an earlier unmerged
prototype from the owner holds a working dual-runtime reference —
runtime-backend plus capability-snapshot routing with fail-explicit rejection (a
session requiring tools cannot land on a backend lacking them), a
provider-neutral agent event vocabulary spanning both CLIs, and
none/import_only/adopt_resume/adopt_fork adoption modes for provider-owned
external sessions held as durable pointers.

Owner addendum on drift defense: the wrapped CLIs are pinned to exact versions
and bumped frequently via Renovate; a compatibility smoke — cheapest available
model, real credentials from environment-protected secrets never exposed to fork
PRs, main-only or manually dispatched — verifies each bump before it lands under
us.

Owner direction, 2026-07-25 (orientation only; the invariant amendment lands
with the pickup spec-diff, not here): the CLI-wrap path is the supported
subscription integration, and the direct-transport reimplementation below is
parked by owner call — a cost/priority judgment, revisitable later. The
rationale is that the choice is reversible by construction: the wrapped CLI is
an intended external-control surface, and the runtime trait seam keeps a future
direct adapter a drop-in replacement behind the same two-method contract.

On open tension (1) above, the owner set the dispatch-boundary direction — it
governs any subprocess adapter, both wrap tracks included: INV-025/026 are
reconciled by re-anchoring the invariant's subject to the adapter's unit of
irrevocable dispatch — one HTTPS request for the direct adapters, one process
spawn for a subprocess adapter. At that boundary the invariants hold at full
strength: at most one spawn per prepared call, never a respawn on ambiguity,
process death without a terminal marker is BoundaryLoss evidence, and the
platform owns every retry decision. The CLI's internal requests are
provider-internal — the same epistemic position the direct adapters already hold
toward a provider's server side. Accepted costs, stated: evidence granularity
coarsens to the handed inputs plus the emitted event stream; the CLI's internal
retries burn subscription capacity the platform cannot itemize (mitigated by
capturing emitted rate-limit events); and a platform retry after BoundaryLoss
can duplicate provider-side effects, which is cost-only for chat calls.
Cancellation prefers the CLI's protocol-level interrupt with process kill as the
fallback, and the evidence distinguishes the two. The formal invariant-text
amendment lands with the pickup spec-diff, not now.

On statelessness: stateless-exact integration is the aim — each prepared call
spawns a fresh invocation with the full context rendered in, so every existing
frontier-exactness law holds unmodified. A stateful facade is admitted only
where genuinely needed, through provider-owned external sessions held as durable
pointers with explicit adoption modes (the
none/import_only/adopt_resume/adopt_fork vocabulary above); choosing where that
line falls is the pickup spec-diff's job.

### Codex CLI wrap [blocked-on: owner commission call] [size: S-M]

The lead track of the three — the wrap path the owner chose on 2026-07-25.
`codex exec --json`; the thread/turn/item event taxonomy is cleanly namespaced
with an unambiguous turn.completed/turn.failed terminal (the demanding part of
the evidence model). CLI owns subscription auth — zero credential handling. An
intended external-control surface; only real event risk is schema drift between
CLI versions (pin a version, snapshot-test). Boundary direction is set (the
owner direction above); the formal invariant amendment lands with the pickup
spec-diff. What remains is the owner's commission call on pickup timing — not
selectable by goal mode until then.

### Claude Code CLI wrap [blocked-on: owner commission call] [size: S-M]

`claude -p --output-format stream-json --verbose` (+
`--include-partial-messages` for deltas). Clean result terminal message; CLI
owns subscription auth. Fragility: the full stream-json event set is
undocumented and version-fragile — snapshot-test. Do not use `--bare` for
subscription runs (it forces an API key). Same position as the Codex track: the
owner's dispatch-boundary direction above applies here too (one process spawn is
the unit of irrevocable dispatch), the formal amendment lands with the pickup
spec-diff, and pickup waits on the owner's commission call.

### Codex-subscription Rust reimplementation [blocked-on: owner revisit — wrap path chosen] [size: L-XL]

Reimplements the open-source Codex CLI's direct subscription transport
(chatgpt.com backend Responses endpoint, OAuth/PKCE token lifecycle, SSE
Responses events) in Rust — no subprocess. Wire types + SSE are mechanical (M,
done twice already); the token-refresh lifecycle, credential store, identity
headers, and error taxonomy are the L-XL part. HIGH fragility: an undocumented
internal endpoint that can change silently. Parked by owner call on 2026-07-25
(the owner direction above) as a cost/priority judgment: the wrap path de-risks
the same wire behavior at a fraction of the build-and-carry cost, and the
runtime trait seam keeps this a drop-in replacement, so take it up again only if
subprocess overhead proves unacceptable — and record the accepted cost before
starting. Codex source is Apache-2.0 (attribution/patent terms).

## Provider account pools and limit-aware dispatch [blocked-on: provider dispatch mechanism (first subscription-runtime wiring); owner design pass] [size: M-L]

Owns: an account concept and account-aware dispatch/selection policy, account
cooldown state, and additive evidence enrichment (capturing provider retry-after
material). Collides-with: signalboxd composition/dispatch — the same surface as
the first subscription wiring and the OpenAI composition wiring — and scheduler
policy surfaces. Generalizes the dispatch concern of the subscription-runtimes
entry above: spread sessions and calls across multiple provider accounts —
several API accounts per provider and several subscription accounts (for the
CLI-wrapped runtimes an account is effectively the subprocess's profile/home).
The foundation is already in place: the evidence taxonomy distinguishes
RateLimited from QuotaExhausted (billing, never retry-later) from Overloaded,
and every model call durably pins its credential reference at Prepared, so
per-account attribution of past calls already exists in storage. Direction the
owner set: account affinity is per-session by default — a session sticks to one
account, preserving provider-side prompt-cache locality; switching accounts or
even providers *within* a session is considered only in the failure case (rate
limit, quota exhaustion, overload), where cache locality is already forfeit
anyway. Open tensions the spec-diff at pickup must resolve, not this entry: (1)
reaction policy — today any ProviderError, rate limits included, is KnownFailed
and the turn simply fails; cooldown-and-route-subsequent-work-elsewhere fits the
no-automatic-retry doctrine, while re-dispatching the failed turn on another
account is automatic retry in effect and needs an explicit owner decision; (2)
retry/backoff conditions per error kind (retry-after-honoring cooldowns for
RateLimited, account-dead for QuotaExhausted, spread for Overloaded); (3) for
subscription accounts, the cost texture of multi-account spreading — an owner
call, the same bucket as the owner-revisit parking on the reimplementation
track.

Owner direction, 2026-07-25 (orientation only; the pickup spec-diff still
carries the real design): the owner set the reaction doctrine per evidence kind,
answering open tensions (1) and (2) above. RateLimited and Overloaded get
platform-owned deferred retry — a durable cooldown window, bounded re-attempts,
and each re-attempt is a new prepared call, so the
one-dispatch-per-prepared-call evidence law holds unmodified. QuotaExhausted
never auto-retries on the same credential — billing state, not weather — and
instead marks the credential failover-eligible. CredentialRejected and
PermissionDenied fail closed with no failover: silently rotating past a
misconfigured credential would hide the problem from the owner. All other kinds
keep today's behavior.

On failover granularity, refining the per-session affinity direction above: the
session pins a credential at creation to preserve provider-side prompt-cache
prefixes — the stated point of pinning — and automatic failover is permitted
only at attempt boundaries, and only when the pinned credential is cooling down
or quota-exhausted, the case where the cache is going cold anyway. Every call
still pins the credential it actually used, keeping history truthful. Automatic
pools are same-provider only.

On limit-state durability: per-credential cooldown/limit state lives in durable
rows fed by the rate-limit evidence already captured, consulted at dispatch time
— restart-safe, and displayable by future monitor surfaces.

One boundary clarification the owner recorded alongside: user-driven
model/provider switching mid-session — selecting a different model between
turns, or ending a turn, switching, and continuing — is a wanted first-class
capability and deliberately separate from automatic failover. This entry does
not own it; see the mid-session model selection entry below.

## Native client rewire, macOS first [blocked-on: client stack + snapshot import merges] [size: L]

Owns: `clients/native`, possibly additive process-protocol frames.
Collides-with: client stack files. Rewire the imported SwiftUI app's protocol
layer to the local socket; first task is restoring the test-target wiring lost
with the build-system exclusion (see the import's known-issues list). The
mock-fixture screenshot harness ports first — it is how the app iterates. iOS
waits for remote transport.

## Swift client CI [blocked-on: native client rewire] [size: M]

Owns: GitHub Actions macOS workflows for the native client — unit tests and
screenshot-golden comparisons against the capture manifest. Collides-with:
`clients/native` and CI config. Blocked because the imported snapshot's test
targets are unreachable until the rewire re-wires them (the first item of the
rewire's inventory). The repo is public, so macOS runners are free.

## Swift-to-server E2E CI [blocked-on: native client rewire; process protocol] [size: M]

Owns: an E2E workflow booting the server with Postgres and driving the rewired
native client against it (scripted provider, no real credentials).
Collides-with: CI config and `apps/signalboxd` composition. Exercises the
client-server protocol as CI evidence rather than manual smoke.

## Tool loop foundation [in-flight] [size: XL]

Owner-flagged: the next major milestone. The owner design pass completed on
2026-07-23; implementation is running as a solo turn-side goal session.

Owns: domain turn machinery, tool entries (the storage-blocked assistant
tool-use variant), ToolRequest/ToolAttempt lifecycle, approval algebra
(AwaitingApproval storage and flow), persistence slice, first daemon-local tool.
Collides-with: everything turn-side — runs solo. The gate for the entire tool
economy (catalog, permissions, confirm/deny, shared tools, delegation). This
foundation is the daemon-side approval algebra plus the first daemon-local tool;
the client approval surface is a separate later milestone whose UX is settled
then.

Cross-reference: the tool registry this foundation establishes is where two
later per-tool declarations land — admissible execution loci and effect class
(pure/idempotent/side-effecting) — recorded as owner direction in the runner
protocol and placement entry below.

## Durable approval waits [blocked-on: tool loop design pass] [size: M]

Owns: a waiting-for-confirmation turn state, dedupe-keyed resume commands in the
outbox, replay eligibility on the executor path. Collides-with: turn machinery —
these are the wait mechanics the tool loop's approval flow will need, so it
lands with or just behind that foundation. Closes the spec's open edge for
tool/approval waits. The reference design is an earlier unmerged prototype from
the owner: resume commands keyed `resume_turn:{turn}:{invocation}` in the
outbox, claimed with `FOR UPDATE SKIP LOCKED` and replayed to reconnecting
executors, with replay eligibility conditioned on turn state.

## Tool catalog buildout [ready] [size: L, spread over batches]

Owns: catalog declarations and their executors — one new tool module per entry
with its argument schema, permission default, and effect class, plus the
integration clients the credentialed tools need. Collides-with: the tool-loop
registry surface, since every batch touches the compiled catalog wiring;
individual tools are otherwise disjoint from each other, so batches parallelize
well among themselves and poorly against the foundation itself. Best pulled as
small batches of related tools rather than one pass.

Owner direction, 2026-07-25 (orientation only; per-tool argument shapes, result
shapes, and failure taxonomies are settled in the owning specification diff at
pickup): the inventory below distills the owner's prior work into the tools the
platform wants, tiered by what gates them. It is a menu and a sequencing map,
not a design.

Tier 0 — daemon-side, no credentials, unblocked now. An echo tool (a conformance
and test fixture more than a capability), a bounded single-URL web fetch, and a
session status update tool that writes through the session-metadata satellites
owned by the entry below.

Tier 1 — daemon-side behind held credentials, also unblocked now. Daemon-held
credentials ride the existing configuration channel (the same file-path pattern
as the provider keys); the runner entry's credential-profile machinery is not a
prerequisite here — its (tool, profile) approval pairing governs this tier only
once profiles exist, and until then the registry's own approval defaults apply.
A change-request review suite against the code host: summary, changed files,
per-file patch, checks status, comment, review threads, thread reply, thread
resolve, CI job log, and rerun failed jobs. Its first consumer is the platform's
own review workflows (the review-workflow tier below), which is why it leads the
tier. Alongside it, a library-documentation lookup service.

Tier 2 — runner-side workspace tools [blocked-on: runner protocol design pass].
File operations (read, multi-read, contextual read, write, text replace, patch
apply, copy, move, delete, directory create, list, glob, content search, stat);
version-control inspection (status, diff, log, show); shell execution and
interactive process management (start, read, write, stop, list); and project dev
loops (build, lint, test). This tier is the concrete catalog input to that
design pass — the placement, dispatch, and advertised-catalog machinery in the
runner protocol and placement entry below exists to carry exactly this list, so
the two are read together.

Tier 3 — feature-coupled, each item blocked on the feature it needs rather than
on any tool machinery. Task management tools [blocked-on: a task aggregate] —
the domain feature of the durable session tasks entry below, not a tool;
delegation tools, meaning delegate to a sub-session plus list, read, and
summarize delegated sessions \[blocked-on: a child-session delegation substrate
— goal mode alone owns goal-session semantics, not child sessions\]; skills,
guidance, and profile tools [blocked-on: those concepts existing at all]; report
and artifact persistence [blocked-on: the artifact-boundary open question];
image inspection [blocked-on: multimodal input]; and MCP-bridged tools
[blocked-on: the deferred MCP pass].

Every tool added under the current seam follows the compiled-registry pattern
the first compiled tool `current_time` established — a process-lifetime
immutable catalog value carrying each tool's permission default and effect
class. The per-tool placement and effect-class declarations recorded as owner
direction in the runner protocol and placement entry below govern this catalog
once that design lands; this entry does not restate them.

## Session metadata, tags, and visibility [in-flight] [size: M-L]

Owns: session satellite tables, list projection, additive protocol frames.
Collides-with: the session-creation command surface (the creation-time
visibility override lands there, where Context assembly's session-defaults stage
also composes); otherwise little — parallel-safe against turn machinery. Titles,
tags, archive/restore, filtered and paginated listing — plus visibility control
for the automation era: sessions spawned by automations and background work must
not crowd the interactive default view, while monitor surfaces see everything
and can hop into any session. Owner-flagged high priority — the daily-driver
item.

The owner commissioned the pickup on 2026-07-25. The bottom specification diff
governs the implementation:
[sessions-and-transcript](../spec/sessions-and-transcript.md) owns the metadata
and listing contract, [process-protocol](../spec/process-protocol.md) owns the
additive wire surface, and
[open-questions.md](../open-questions.md#session-organization-visibility-and-retention)
owns deferred visibility and filter design.

## Mid-session model selection [blocked-on: owner commission call] [size: S-M]

Owns: an additive protocol affordance and client UI surface. Collides-with:
little. Owner-wanted capability: change the session's model target between turns
from the client — pick a different model for the next turn, or end a turn,
switch, and continue in place. The server-side mechanism already exists: session
defaults carry the model-selection request, and the existing
`ReplaceSessionDefaults` command installs a new defaults version affecting only
origin input accepted afterward — this entry exposes that machinery to clients
rather than inventing a new one. Distinct from automatic failover by owner call:
this is the user choosing, not the platform routing around a limit; the
account-pools entry above records that boundary and does not own this.

## Monitor stream [blocked-on: client stack merge] [size: M]

Owns: outbox dispatcher consumers, additive monitor protocol surface.
Collides-with: dispatcher wiring. Daemon-wide fleet view fed by the outbox:
session summaries, needs-attention triage, the operator escape hatch. The future
web surface's backbone.

## Channel integrations [blocked-on: client stack merge; actor-admissibility decision (inbound path)] [size: M]

Owns: new channel-adapter crate(s), channel-binding satellite, outbox consumer
registration. Collides-with: dispatcher wiring only. Slack/email/SMS as outbound
notification surfaces and inbound input paths; a session synchronized with a
Slack channel. Likely seams (to be decided in the entry's spec-diff, not here):
outbound over the dispatcher feed, inbound through SubmitInput with actor
attribution — the latter pending the actor-admissibility question.

## Token-level streaming to clients [blocked-on: streaming-checkpoint decision] [size: L]

Owns: model-call observation path, follow protocol, persistence checkpoints.
Collides-with: turn machinery. Deltas are collected today but not delivered; the
deferred draft-streaming policy decides what is durable versus transient.

## Compaction [in-flight] [size: L]

Owns: frontier machinery, compaction entries, new spec section. Collides-with:
turn machinery. Owner-commissioned implementation started 2026-07-28.

## Smarter compaction timing [blocked-on: compaction] [size: M]

Owner direction, 2026-07-28. Compact at coherent task and turn boundaries before
the context limit becomes the immediate constraint. Owns: compaction trigger
policy and timing evidence. Collides-with: turn machinery and goal mode.

## Agent-controlled self-compaction [blocked-on: compaction; tool policy] [size: M]

Owner direction, 2026-07-28. Give an agent an explicit tool for compacting its
current session when its own task state indicates a useful boundary. Owns: tool
contract and compaction authorization. Collides-with: tool registry and context
assembly.

## Cross-session read tools for agents [blocked-on: read-tool policy] [size: M]

Owner direction, 2026-07-28. Add bounded tools through which an agent can
inspect other durable sessions without flattening their transcripts into the
current session. Owns: read contracts and actor authorization. Collides-with:
tool registry, conversation listing, and context assembly.

## Templates [delivered by session-templates stack] [size: M]

Owns: static template configuration and session-creation additions.
Collides-with: session-creation surfaces. Version one is the owner-commissioned
named, versioned, copy-on-create bundle of model selection, system prompt, and
dangerous-tool blanket.

Follow-up owner direction, 2026-07-28 (deferred under
[template storage and authoring surfaces](../open-questions.md#template-storage-and-authoring)):

- durable database template objects with protocol CRUD;
- agent tools for reading and editing templates so agents can help the owner
  author them.

## Goal mode in platform [blocked-on: tool loop; owner design pass] [size: L-XL]

Owns: goal-session semantics (an outcome, constraints, and a verifiable stop),
scheduling/lifecycle hooks, and the prompt-composition hooks it needs.
Collides-with: tool loop surfaces and context assembly. Runs goal-directed
sessions natively in the platform — the workflow the owner currently drives
through external CLI agents — as a first-class session kind. A destination-tier
feature from the target model that previously had no entry.

## Context assembly pipeline [blocked-on: in-flight stacks landing; pickup spec-diff] [size: XL]

Owns: the model-facing prompt/context composition seam — default and
per-use-case system prompts, instruction files, skill/tool description
injection, and lifecycle transformation points. Collides-with: frontier
materialization, Compaction, Templates, and the session-creation command surface
— the session-defaults stage composes at creation, shared with Session
metadata's creation-time visibility override. The organizing idea
(owner-stated): everything composed into the model call — system prompt,
instruction files, tool metadata, compaction — is frontier composition, so the
plugin interface is typed transformations over the structured operation at
defined lifecycle points, not string-to-string middleware. This is the owning
entry that Templates' pending "system-prompt configuration category,"
Compaction's prompts, and goal-mode-as-plugin hang off. Vendoring stance: openly
licensed CLI implementations may be vendored with attribution; closed-source
clients are reference-only — observe, do not copy.

Owner direction, 2026-07-25 (orientation only; the pickup spec-diff still
carries the real design): composed context follows a
derivation-with-pinning-plus-observation model, with two stage kinds. Pure
transforms are typed frontier-to-frontier, or frontier-to-provider-messages at
render, and deterministic — the call records the stage identity/version vector
it was rendered under, so attribution is durable and core stages are
byte-reconstructible. Observer stages may read the world — clock, files,
external sources — but whatever they contribute is durably recorded before the
call consumes it. The bright line the owner adopted: if the repo can't re-derive
it, the store remembers it — core built-in pure stages are pin-only, while
observer stages and swap-outable third-party plugin stages record their output
verbatim, so every byte the model saw is either reconstructible or stored.
Plugin composability is configuration: the pipeline is an ordered list of stage
bindings, changing config changes future calls only, and recorded history names
the composition it used. Accepted costs, stated: observer outputs consume
storage; observers run in the turn's critical path; observation granularity is a
deliberate policy knob, since per-render observation (e.g. seconds-precision
time) breaks provider prompt-cache prefixes; and the pure/observer boundary is
enforced by seam types and review for now, with process isolation for plugins
later. Initial stage vocabulary — explicitly non-exhaustive and additive:
session-defaults (system-prompt composition at session creation), turn-open
(per-turn injection), render (frontier to provider messages); compaction joins
later as the first rewriting stage, with its own design session. First
deliverable: system-prompt composition only — base, per-use-case, and
instruction-file contributions with a defined merge order, declarative config,
no plugin runtime yet; the typed seam is the deliverable. Later layers, in no
committed order: compaction as a rewriting stage; observer stages with
freshness/TTL policies; the plugin isolation runtime; goal-mode-as-plugin (the
goal-mode entry above stays the owning entry for that); per-session pipeline
overrides; cache-aware stage placement.

Cross-reference: runner loss or replacement extends the session frontier with an
injected message naming the new machine, working directory, and tool list — an
injection this pipeline composes, recorded as owner direction in the runner
protocol and placement entry below.

## Durable session tasks [blocked-on: owner design pass] [size: M]

Owns: task satellite store, protocol additions, later model-callable task tools.
Collides-with: little. Per-session task rows with status/priority hierarchy.

## Artifacts [blocked-on: artifact-identity decision] [size: L]

Owns: artifact store, entry linkage, protocol frames. Collides-with: tool loop
(artifacts largely arrive from tools). Prompt-context artifacts — "what did the
model actually see" — are the observability target worth matching.

## Restricted executor [blocked-on: sandbox-minimum decision; execution-identity decision] [size: L]

Owns: execution placement and sandboxing for tool execution. Collides-with: tool
loop and runner-protocol machinery. A first restricted placement for tool
execution per the target model's execution-isolation target.

## Runner protocol and placement [blocked-on: owner commission call (capability, placement, and auth kernel decided 2026-07-25; design pass unblocked)] [size: XL]

Owns: runner registry, outbound runner connection protocol, dispatch fencing
completion, placement. Collides-with: tool loop machinery. Carries the remote
tool catalog; runner auth (separate credentials, allowlists, no
permission-downgrade on re-registration) is designed in from day one.

Owner direction, 2026-07-25 (orientation only; the design pass still carries the
real design): runners are the processes that host goal runs and automation
sessions, and they ship as a separate `signalbox-runner` binary — a thin binary
over shared workspace crates, distinct from `signalboxd` — so this entry also
owns that binary when it is built. The lifetime spectrum is a design input: some
deployments run persistent daemon runners on owner machines, others run
short-lived dynamically-registered runners (ephemeral cloud sandboxes) that
register with the server, work, and disconnect. Consequences the design pass
takes as given: registration and deregistration are first-class protocol flows,
runner identity is not machine-pinned, and authentication must work for a runner
that did not exist minutes earlier — which sharpens the standing
design-runner-authentication-in-from-day-one caution. Everything else stays with
the design pass.

Owner direction, 2026-07-25 (second pass — placement, dispatch, effect classes;
orientation only, same standing caveat): placement is a registry property. Each
tool declares a non-empty set of admissible loci — Daemon (signalboxd-local; the
locus name deliberately avoids legacy hub naming), Runner (with a selector), or
both — and where both are admissible the session's attached runner is preferred,
falling back to the daemon. Declarations are static per tool; per-call dynamic
placement is a later upgrade. An MCP-bridged locus is reserved vocabulary for a
future pass, not designed here. Dispatch topology: a runner initiates one held
outbound connection (a WebSocket-shaped streaming channel) over which the daemon
streams leased work, and runners never accept inbound connections. That channel
is transport, never truth — lease and claim state is durable in the store, and a
reconnecting runner re-syncs from durable state.

Effect class is a required declaration on every tool, with no default: pure,
idempotent, or side-effecting (pure implies idempotent; idempotent means
state-changing but safely retryable). The retry law follows from it — pure and
idempotent tools may be re-leased after a lost lease, while a side-effecting
tool's lost attempt is crash-classified into typed evidence through the existing
physical-attempt machinery and is never silently re-dispatched. A
runner-advertised tool carrying no daemon-side effect declaration is treated as
side-effecting.

Runner tool catalogs are advertised, never trusted. Approval defaults, effect
class, and placement admissibility for runner tools come from a daemon-side
owner-editable catalog — configuration validated into typed domain at load,
following the model-catalog TOML precedent — and the advertisement is compared
against that catalog; an advertised tool with no daemon-side declaration is
excluded, or fails closed to confirmation. A runner never widens its own
approval surface, and the no-permission-downgrade-on-re-registration point above
stands. Credential doctrine for the first slice: a tool declaring credential
access is Daemon-only, and signalboxd hands no credentials over the runner
protocol — INV-035 read as placement law. Runners may hold their own ambient
machine or environment credentials, which sit outside this model;
credential-scoped runner classes are a recorded deferred extension.

Runner identity and session placement, kernel only (the design pass owns the
rest): runner identity is logical — enrollment-based, not hardware-fingerprinted
— yet a session may target either a capability class or a specific runner
identity, both first-class at session creation (the owner's new-session flow:
pick a machine or a class, optionally a working directory, with a default
workdir for ephemeral runners). Once a session executes on a runner it is pinned
there, because workspace state makes silent cross-runner rescheduling incorrect;
there is no automatic migration. Runner loss or replacement is an explicit event
that extends the session frontier — the model is told the new machine,
directory, and tool list through an injected message, composing with the
context-assembly direction above. Recovery flows, lease-affinity interaction,
and workspace lifecycle all need the design pass. MCP — a daemon-side client for
centralized servers, runner-side hosting for sandboxed execution — stays
deferred to its own pass, flagged soon by owner priority.

Owner direction, 2026-07-25 (third pass — credential profiles; orientation only,
same standing caveat): the kernel is a split between credential values and
credential selection, and the design pass owns everything the split does not
settle. Values are runner-resident. A runner holds named credential profiles
locally — scoped read-only and admin variants of an infrastructure credential, a
dedicated agent VCS identity — provisioned by the owner on that machine
out-of-band, and no value ever transits the runner protocol. That preserves the
credential doctrine above rather than weakening it: the daemon still hands
nothing down the channel.

Selection, audit, and policy are daemon-resident. Runners advertise profile
names, never values, at enrollment, over the same advertised-never-trusted
channel as the tool catalog and validated the same way against the daemon-side
owner-editable catalog. Session creation then selects a profile as a third
placement axis alongside machine and working directory, and the picker can only
offer profiles the targeted runner actually advertised — credential availability
composes with placement, so a runner that never held a profile can never be
granted it. The daemon records every grant durably, which makes it auditable
which sessions ran under which profile.

Approval posture resolves on (tool, credential profile), not on tool alone.
Properly scoped credentials substitute for approval judging: a session on a
read-only profile runs the matching tools under automatic approval with no
judging spend, while the admin-profile variant of the same tools falls back to
confirmation or the session's blanket posture. This is a new input to the
existing approval-resolution chain, not new machinery, and it operationalizes
the credential-ops policy already distilled in the spec (least-privilege,
optional-mount, channel-scope) as session-selectable mounts.

Lifecycle is snapshot-then-replace. The profile grant and the tool set are
snapshotted into session state at creation; a mid-session change is an explicit
owner command replacing defaults forward-only — the same shape as the direction
recorded in the mid-session model selection entry above — and each change
extends the frontier with an injected message so the model is informed of its
changed tools and credentials. Revoking a profile gates future tool dispatches;
an in-flight leased call completes or crash-classifies normally, and nothing is
yanked mid-execution.

Workspace provisioning is a runner capability in the same advertisement model: a
session declares that it needs a repo workspace, and the runner provisions
worktree-per-session and owns cleanup. The first consumer is the goal of running
this repo's own review workflow inside the platform (the review-workflow tier
entry below). Details go to the design pass.

## Delegation and child sessions [blocked-on: delegation cause decision; tool loop; selectable transcript-frontier decision (fork selection)] [size: L]

Owns: delegated creation cause (typed, rejected today), child-result delivery,
delegation tools. Collides-with: session creation + tool loop. The orchestrator
tier: sessions spawning linked sessions. Includes the owner's "tangent" move:
fork from any frontier point of any session — including an automation-spawned
one — into a new session with different runner and tool capabilities; the same
seed-from-frontier machinery the import milestone builds, with retargeting.

## Remote transport and real auth [blocked-on: owner design pass] [size: L]

Owns: network transport beside the local socket, authentication. Collides-with:
process protocol surfaces. Gates iOS, the web surface, and any off-machine
client. Bolted-on shared-key auth is the anti-pattern to avoid.

Owner direction, 2026-07-28 (orientation only): prioritize a near-local
Tailscale path for a real macOS client before broad internet exposure. The
design pass must cover authenticated identity, authorization, revocation, and
transport binding together; it must not present a raw local process socket on
the tailnet or treat tailnet membership alone as application authorization.
iPhone and iPad remain deferred behind this slice.

## Native imported-conversation inspection [size: S]

Owns: native SwiftUI imported transcript projection and continuation creation.
Collides-with: process-protocol imported inspection and native conversation
detail. The native v18 client identifies imported conversations through the
unified list, and process-protocol version seventeen now provides their entry
inventory. Add the read-only transcript and continue-from-frontier action
without a client-owned transcript interpretation.

## Web surface [blocked-on: monitor stream; remote transport] [size: L]

Owns: new web client. Collides-with: nothing daemon-side once its feeds exist.
Owns the operator/monitor role; needs-attention triage first.

## OpenAI-compatible facade [blocked-on: remote transport] [size: M]

Owns: compat endpoint surface. Collides-with: transport surfaces. One endpoint
makes every OpenAI-speaking tool a Signalbox client; also a conversation-import
seam.

## Automation triggers [blocked-on: tool loop; channel integrations; owner design pass] [size: XL]

Owns: trigger/condition machinery, automation session provenance. Collides-with:
broad — late-stage item. Standing automations that create and drive sessions
from input conditions (mail arriving, schedules, watched states). The owner's
private integrations stay outside the repo as plugins; Signalbox owns the
trigger seam, session provenance, and the visibility classification they rely
on.

## Review-workflow tier [blocked-on: tool loop (fix workflows)] [size: XL]

Owns: a new workflow bounded context above sessions —
Target/Run/Pass/Finding/ExternalLink aggregates, their store, and a
workflow-facing protocol surface. Collides-with: nothing current; it sits above
the existing spec surface. A destination-tier item: standing review workflows
with sessions as the execution substrate — workflow passes traced as session
transcripts, workflow conflicts escalating into first-class interactive
sessions. The reference design carries a nine-state finding machine;
reservation-row idempotent external posting (pending ledger row before the API
call, mapping onto the outbox/durable-command idempotency doctrine); judge and
dedupe confidence policy versioned as data (accept ≥0.70, publish ≥0.80 in the
reference); model and workspace providers behind protocol seams; and merge-based
stack propagation. Port the design, not the code. Prior art: an earlier unmerged
prototype from the owner — implemented and unit-tested, never production-smoked.

## Client SDK [blocked-on: protocol stabilization] [size: M]

Owns: new SDK crate/package. Collides-with: nothing. Typed identities held at
the SDK boundary — untyped-identity erosion characteristically starts exactly
there.

## Remote runner credential separation [blocked-on: remote runner transport] [size: M]

Owns: remote runner authentication configuration and verification.
Collides-with: runner transport and enrollment. Give runners a dedicated secret
type and configuration channel, compare it in constant time, and reject fallback
to client or provider credentials.

Provenance: distilled from the predecessor system’s follow-ups ledger.

## Remote runner admission policy [blocked-on: remote runner transport] [size: S-M]

Owns: enrollment issuance and remote admission policy. Collides-with: runner
authentication. Require explicit daemon-side authority for each admitted runner
identity or capability class; possession of another valid platform credential
must never permit runner self-enrollment.

Provenance: distilled from the predecessor system’s follow-ups ledger.

## Runner selection client flow [blocked-on: executable runner stack] [size: M]

Owns: runner summaries, session-creation placement fields, and terminal-client
selection. Collides-with: process-protocol and client session creation. Expose
only live eligible identities and capability classes, then submit the selected
typed placement with session creation.

Provenance: distilled from the predecessor system’s follow-ups ledger.

## Runner fleet projection [blocked-on: multi-runner enrollment; monitor stream] [size: M]

Owns: runner health/read projections and monitor presentation. Collides-with:
runner connection state and monitor protocol. Project connection recency,
availability, advertised tools, and effective permission posture from durable
daemon facts.

Provenance: distilled from the predecessor system’s follow-ups ledger.

## Runner capability-class visibility [blocked-on: runner fleet projection] [size: S]

Owns: picker, fleet, and pinned-session presentation of capability classes.
Collides-with: runner client projections. Preserve the typed
`RunnerCapabilityClass` through every read model and render it wherever a user
chooses or diagnoses placement.

Provenance: distilled from the predecessor system’s follow-ups ledger.

## Role-specific process identities [ready] [size: M]

Owns: process-protocol identity wrappers and terminal-client state.
Collides-with: process-protocol and client stacks. Replace role-erasing
`CanonicalUuid` fields and client state with nominal session, turn, entry, and
tool-request wire types while preserving their canonical UUID encoding.

Provenance: distilled from the predecessor system’s follow-ups ledger.

## External integration process locus [blocked-on: integration-host design pass] [size: L]

Owns: an out-of-process integration host and its tool locus. Collides-with: tool
placement and the deferred MCP pass. Define the isolation, lifecycle, and
failure contract before browser automation or service integrations can be loaded
into either `signalboxd` or a workspace runner.

Provenance: distilled from the predecessor system’s follow-ups ledger.

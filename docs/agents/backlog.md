# Work backlog

> **Non-authoritative planning scratchpad — do not review for consistency.**
> This file decides nothing and is not a statement of record. It is the owner's
> working map of what work exists and what can run in parallel; entries are
> orientation, not design. Every design choice, accepted cost, blocker, and open
> question named here is settled elsewhere and that record governs, never this
> file: design in the owning `docs/spec/` page's diff at pickup, decisions in
> `docs/decisions.md`, open questions in `docs/open-questions.md`. Do not hold
> entries to cross-document consistency or treat their prose as normative — it
> is deliberately loose and is superseded by the real record when an item is
> picked up. The owner revises this file freely; agents never reorder it.

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

Owns: `apps/client`, `crates/process-protocol` (additive request kinds), hubd
server handlers. Collides-with: the client stack files. Steering and
proof-bearing stops are landed hub-side with no client verb; this is the
cheapest capability on the board.

## Frontier scaling fix [ready] [size: M]

Owns: persistence read paths, domain frontier materialization. Collides-with:
turn machinery. The recorded post-model-call obligation: remove the quadratic
frontier/projection loads.

## OpenAI composition wiring [blocked-on: client stack merge] [size: S]

Owns: hubd configuration/composition, the model catalog example. Collides-with:
`apps/hubd`. The merged OpenAI adapter is unreachable; the catalog admits only
one provider.

## De-hub naming pass [blocked-on: in-flight stacks landing] [size: S-M]

Owns: the `apps/hubd` rename (binary and directory to `signalboxd`) and "hub"
vocabulary across code, spec prose, and config. Collides-with: broad — hub
vocabulary reaches `crates/process-protocol`, persistence internals, and most
`docs/spec/` pages, not just `apps/hubd` — so it runs effectively solo and waits
for the current stacks. "Hub" survives only as occasional prose metaphor, never
as the name of a binary, crate, module, or protocol concept. The Swift client's
naming is not this entry's job — it was renamed to Signalbox-prefixed names in
its own pass, and the native client rewire owns any protocol-driven renames that
remain.

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
the repository owner personally (`AGENTS.md`, `goal-mode.md`, this backlog, the
decision-log voice); (2) uses meaning ownership in the computer-science sense —
a row, aggregate, or state machine owning data; (3) historical
`docs/decisions.md` entries — the log is append-only, so past entries keep their
original actor spellings and the rename is recorded there as a new terminology
entry, never as edits. Starts with an inventory pass classifying every
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
billing. A pure adapter crate that adds no hubd wiring collides with runtime
crates only — parallel-safe against everything else and against each other. The
exception is provider dispatch: whichever runtime track wires it first (see
below) also touches hubd composition, and therefore collides with the OpenAI
composition wiring entry and any other hubd-composition work. One caveat: every
runtime-track crate edits the root `Cargo.toml` workspace-member list and
`Cargo.lock`, and the provider-security track also touches `Cargo.lock` (reqwest
upgrade). That is a light merge-coordination point (lockfile conflicts), not a
semantic collision — land them in sequence or expect trivial lockfile rebases.
The runtime trait is rated stable (two-method signature byte-stable since early
on; evidence vocabulary grows additively), so adapters written now are unlikely
to reshape. Prior art exists in the owner's own prior subprocess-based provider
work and is supplied per session at launch, not pointed at here; whatever CLI
argv, JSON-event parsing, and process-supervision it carries, its turn-shaped
semantics must be tightened to Signalbox's evidence-shaped contract
(exit-0-without-a-terminal-marker is BoundaryLoss, not success). Open design
tensions the track's spec-diff must resolve, not decide here: (1) a subprocess
is one physical request the adapter cannot prove is retry-free internally, so
the spec-diff has to reconcile that boundary with the one-physical-request
invariants (INV-025/026); (2) for the wrapped-CLI tracks below, auth rides the
CLI's ambient subscription login, so the spec-diff has to reconcile that with
the credential-reference boundary and per-request value durability the
`ModelRuntime` contract pins (recovered calls, logged-in-account changes).

The FIRST of these to wire also introduces the provider-dispatch mechanism hubd
lacks today (selection is currently two hardcoded "anthropic" points); an
adapter-only PR does not touch hubd, but the first second-provider wiring PR
must add the enum/factory. The adapter-author conformance checklist and the
loopback test pattern from the runtime-adapter study are the reusable body of
each goal prompt.

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

Owner direction, 2026-07-25 (orientation only; the invariant amendment and its
decision-log entries land with the pickup spec-diff, not here): the CLI-wrap
path is the supported subscription integration, and the direct-transport
reimplementation below is parked by owner call — a cost/priority judgment,
revisitable later. The rationale is that the choice is reversible by
construction: the wrapped CLI is an intended external-control surface, and the
runtime trait seam keeps a future direct adapter a drop-in replacement behind
the same two-method contract.

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
amendment and its decision entry land with the pickup spec-diff, not now.

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
material). Collides-with: hubd composition/dispatch — the same surface as the
first subscription wiring and the OpenAI composition wiring — and scheduler
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
carries the real design and its decision-log entries): the owner set the
reaction doctrine per evidence kind, answering open tensions (1) and (2) above.
RateLimited and Overloaded get platform-owned deferred retry — a durable
cooldown window, bounded re-attempts, and each re-attempt is a new prepared
call, so the one-dispatch-per-prepared-call evidence law holds unmodified.
QuotaExhausted never auto-retries on the same credential — billing state, not
weather — and instead marks the credential failover-eligible. CredentialRejected
and PermissionDenied fail closed with no failover: silently rotating past a
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
Collides-with: CI config and `apps/hubd` composition. Exercises the
client-server protocol as CI evidence rather than manual smoke.

## Tool loop foundation [in-flight] [size: XL]

Owner-flagged: the next major milestone. The owner design pass completed on
2026-07-23; implementation is running as a solo turn-side goal session.

Owns: domain turn machinery, tool entries (the storage-blocked assistant
tool-use variant), ToolRequest/ToolAttempt lifecycle, approval algebra
(AwaitingApproval storage and flow), persistence slice, first hub-local tool.
Collides-with: everything turn-side — runs solo. The gate for the entire tool
economy (catalog, permissions, confirm/deny, shared tools, delegation). This
foundation is the hub-side approval algebra plus the first hub-local tool; the
client approval surface is a separate later milestone whose UX is settled then.

## Durable approval waits [blocked-on: tool loop design pass] [size: M]

Owns: a waiting-for-confirmation turn state, dedupe-keyed resume commands in the
outbox, replay eligibility on the executor path. Collides-with: turn machinery —
these are the wait mechanics the tool loop's approval flow will need, so it
lands with or just behind that foundation. Closes the spec's open edge for
tool/approval waits. The reference design is an earlier unmerged prototype from
the owner: resume commands keyed `resume_turn:{turn}:{invocation}` in the
outbox, claimed with `FOR UPDATE SKIP LOCKED` and replayed to reconnecting
executors, with replay eligibility conditioned on turn state.

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
and its decision-log entries govern the implementation:
[sessions-and-transcript](../spec/sessions-and-transcript.md#session-metadata-and-list-projection)
owns the metadata and listing contract,
[process-protocol](../spec/process-protocol.md) owns the additive wire surface,
and
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
Collides-with: dispatcher wiring. Hub-wide fleet view fed by the outbox: session
summaries, needs-attention triage, the operator escape hatch. The future web
surface's backbone.

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

## Compaction [blocked-on: frontier-policy decision] [size: L]

Owns: frontier machinery, compaction entries, new spec section. Collides-with:
turn machinery. The frontier-snapshot substrate is ready. Never expose the state
before the semantics.

## Templates [blocked-on: system-prompt configuration category] [size: M]

Owns: template store, session-creation additions. Collides-with:
session-creation surfaces. Versioned, derivable prompt/tool/model presets; the
versioned-defaults machinery is the in-repo analog.

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
carries the real design and its decision-log entries): composed context follows
a derivation-with-pinning-plus-observation model, with two stage kinds. Pure
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

## Runner protocol and placement [blocked-on: runner capability/auth decisions] [size: XL]

Owns: runner registry, outbound runner connection protocol, dispatch fencing
completion, placement. Collides-with: tool loop machinery. Carries the remote
tool catalog; runner auth (separate credentials, allowlists, no
permission-downgrade on re-registration) is designed in from day one.

Owner direction, 2026-07-25 (orientation only; the design pass still carries the
real design and its decision-log entries): runners are the processes that host
goal runs and automation sessions, and they ship as a separate
`signalbox-runner` binary — a thin binary over shared workspace crates, distinct
from `signalboxd` — so this entry also owns that binary when it is built. The
lifetime spectrum is a design input: some deployments run persistent daemon
runners on owner machines, others run short-lived dynamically-registered runners
(ephemeral cloud sandboxes) that register with the server, work, and disconnect.
Consequences the design pass takes as given: registration and deregistration are
first-class protocol flows, runner identity is not machine-pinned, and
authentication must work for a runner that did not exist minutes earlier — which
sharpens the standing design-runner-authentication-in-from-day-one caution.
Everything else stays with the design pass.

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

## Web surface [blocked-on: monitor stream; remote transport] [size: L]

Owns: new web client. Collides-with: nothing hub-side once its feeds exist. Owns
the operator/monitor role; needs-attention triage first.

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

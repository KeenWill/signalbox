# Work backlog

The granular, owner-curated expansion of the target model's
[priority order](../target-model.md#priority-order). Each entry is a pullable
unit of work for a goal run. Entries state what they touch so parallel launches
are mechanical: any set of items with disjoint `Owns`/`Collides-with` groups may
run concurrently. This file is an ordering and parallelism artifact, not a
design document — designs happen as specification diffs when an item is picked
up. The owner reorders, adds, and retires entries; agents never reorder.

Entry format: status is `ready`, `in-flight`, or `blocked-on: <what>`; size is
S/M/L/XL. Standing cautions from the predecessor system's recorded regrets: hold
typed identities at every boundary including future SDKs; ack-driven client
state only; runner auth designed in from day one; never ship an endpoint or
state ahead of its semantics.

## Terminal stop and steer verbs [blocked-on: client stack merge] [size: S]

Owns: `apps/client`, `crates/process-protocol` (additive request kinds), hubd
server handlers. Collides-with: the client stack files. Steering and
proof-bearing stops are landed hub-side with no client verb; this is the
cheapest capability on the board.

## Frontier scaling fix [blocked-on: stop-requests merge] [size: M]

Owns: persistence read paths, domain frontier materialization. Collides-with:
turn machinery. The recorded post-model-call obligation: remove the quadratic
frontier/projection loads.

## OpenAI composition wiring [blocked-on: client stack merge] [size: S]

Owns: hubd configuration/composition, the model catalog example. Collides-with:
`apps/hubd`. The merged OpenAI adapter is unreachable; the catalog admits only
one provider.

## Conversation import [in-flight] [size: L]

Owns: new converter crate, session creation/ancestry, imported-conversation
store (new migration). Collides-with: session-creation surfaces only. Running as
a goal session with owner addenda (maximum-fidelity conversion, raw
preservation, adoption as a standing client capability rather than an
import-time mode).

## Provider transport security [ready — prompt in hand] [size: M]

Owns: the runtime adapter crates only. Collides-with: nothing on the board.
Closes the provider-call-security open question and takes the deliberate reqwest
upgrade with loopback re-verification.

## Subscription-backed provider runtimes (three tracks)

New `ModelRuntime` adapters that spend subscription capacity instead of API
billing. Each is a self-contained crate colliding with runtime crates only —
parallel-safe against everything else and against each other. The runtime trait
is rated stable (two-method signature byte-stable since early on; evidence
vocabulary grows additively), so adapters written now are unlikely to reshape.
Prior art for all three: the owner's own native-provider subprocess handlers on
the private-mono importer branch — its exact CLI argv, JSON-event parsing, and
process-supervision lessons transfer; its lossy turn-shaped semantics must be
tightened to Signalbox's evidence-shaped contract (a subprocess is one physical
request the adapter cannot prove is retry-free internally — an explicitly
accepted cost; exit-0-without-a-terminal-marker is BoundaryLoss, not success).

The FIRST of these to wire also introduces the provider-dispatch mechanism hubd
lacks today (selection is currently two hardcoded "anthropic" points); an
adapter-only PR does not touch hubd, but the first second-provider wiring PR
must add the enum/factory. The adapter-author conformance checklist and the
loopback test pattern from the runtime-adapter study are the reusable body of
each goal prompt.

### Codex CLI wrap [ready — lead track] [size: S-M]

`codex exec --json`; the thread/turn/item event taxonomy is cleanly namespaced
with an unambiguous turn.completed/turn.failed terminal (the demanding part of
the evidence model). CLI owns subscription auth — zero credential handling.
Officially sanctioned automation path; only real risk is event-schema drift
between CLI versions (pin a version, snapshot-test).

### Claude Code CLI wrap [ready] [size: S-M]

`claude -p --output-format stream-json --verbose` (+
`--include-partial-messages` for deltas). Clean result terminal message; CLI
owns subscription auth. Fragility: the full stream-json event set is
undocumented and version-fragile — snapshot-test. Do not use `--bare` for
subscription runs (it forces an API key).

### Codex-subscription Rust reimplementation [blocked-on: owner ToS-cost decision] [size: L-XL]

Reimplements the open-source Codex CLI's direct subscription transport
(chatgpt.com backend Responses endpoint, OAuth/PKCE token lifecycle, SSE
Responses events) in Rust — no subprocess. Wire types + SSE are mechanical (M,
done twice already); the token-refresh lifecycle, credential store, anti-abuse
identity headers, and error taxonomy are the L-XL part. HIGH fragility
(undocumented internal endpoint that can change silently) and a real
ToS/account-standing risk: it calls an internal endpoint impersonating the
official client identity, and subscription terms generally restrict programmatic
access — personal-use-on-own-account can still draw rate-limits or flags.
Deferred deliberately: a wrapped CLI de-risks the same wire behavior first; take
this only if subprocess overhead proves unacceptable, and record the accepted
cost before starting. Codex source is Apache-2.0 (attribution/patent terms).

## Native client rewire, macOS first [blocked-on: client stack + snapshot import merges] [size: L]

Owns: `clients/native`, possibly additive process-protocol frames.
Collides-with: client stack files. Rewire the imported SwiftUI app's protocol
layer to the local socket; first task is restoring the test-target wiring lost
with the build-system exclusion (see the import's known-issues list). The
mock-fixture screenshot harness ports first — it is how the app iterates. iOS
waits for remote transport.

## Tool loop foundation [blocked-on: owner design pass] [size: XL]

Owner-flagged: the next major milestone — schedule the design pass first.

Owns: domain turn machinery, tool entries (the storage-blocked assistant
tool-use variant), ToolRequest/ToolAttempt lifecycle, approval algebra
(AwaitingApproval storage and flow), persistence slice, first hub-local tool.
Collides-with: everything turn-side — runs solo. The gate for the entire tool
economy (catalog, permissions, confirm/deny, shared tools, delegation). The
predecessor's approval UX policy (oldest-first queue, approve-fast
deny-deliberate, error-aware requeue, durable decision audit) is the reference
for the client half that follows.

## Session metadata, tags, and visibility [blocked-on: owner design pass] [size: M-L]

Owns: session satellite tables, list projection, additive protocol frames.
Collides-with: little — parallel-safe against turn machinery. Titles, tags,
archive/restore, filtered and paginated listing — plus visibility control for
the automation era: sessions spawned by automations and background work must not
crowd the interactive default view, while monitor surfaces see everything and
can hop into any session. Start simple: creation cause and actor attribution
already distinguish owner-initiated from automation-spawned sessions for free,
so the default view is "sessions with recent owner interaction" plus manual tags
as the override; expressive filter rules stay an open edge. Owner-flagged high
priority — the daily-driver item.

## Monitor stream [blocked-on: client stack merge] [size: M]

Owns: outbox dispatcher consumers, additive monitor protocol surface.
Collides-with: dispatcher wiring. Hub-wide fleet view fed by the outbox: session
summaries, needs-attention triage, the operator escape hatch. The future web
surface's backbone.

## Channel integrations [blocked-on: client stack merge] [size: M]

Owns: new channel-adapter crate(s), channel-binding satellite, outbox consumer
registration. Collides-with: dispatcher wiring only. Slack/email/SMS as outbound
notification surfaces and inbound input paths; a session synchronized with a
Slack channel. Outbound rides the dispatcher feed; inbound rides SubmitInput
with actor attribution.

## Token-level streaming to clients [blocked-on: streaming-checkpoint decision] [size: L]

Owns: model-call observation path, follow protocol, persistence checkpoints.
Collides-with: turn machinery. Deltas are collected today but not delivered; the
deferred draft-streaming policy decides what is durable versus transient.

## Compaction [blocked-on: frontier-policy decision] [size: L]

Owns: frontier machinery, compaction entries, new spec section. Collides-with:
turn machinery. The frontier-snapshot substrate is ready; the predecessor
shipped only a stub endpoint — never expose the state before the semantics.

## Templates [blocked-on: system-prompt configuration category] [size: M]

Owns: template store, session-creation additions. Collides-with:
session-creation surfaces. Versioned, derivable prompt/tool/model presets; the
versioned-defaults machinery is the in-repo analog.

## Durable session tasks [blocked-on: owner design pass] [size: M]

Owns: task satellite store, protocol additions, later model-callable task tools.
Collides-with: little. Per-session task rows with status/priority hierarchy.

## Artifacts [blocked-on: artifact-identity decision] [size: L]

Owns: artifact store, entry linkage, protocol frames. Collides-with: tool loop
(artifacts largely arrive from tools). Prompt-context artifacts — "what did the
model actually see" — were the predecessor's best observability feature and the
reference target.

## Runner protocol and placement [blocked-on: runner capability/auth decisions] [size: XL]

Owns: runner registry, outbound runner connection protocol, dispatch fencing
completion, placement. Collides-with: tool loop machinery. Carries the remote
tool catalog; runner auth (separate credentials, allowlists, no
permission-downgrade on re-registration) is designed in from day one.

## Delegation and child sessions [blocked-on: delegation cause decision; tool loop] [size: L]

Owns: delegated creation cause (typed, rejected today), child-result delivery,
delegation tools. Collides-with: session creation + tool loop. The orchestrator
tier: sessions spawning linked sessions. Includes the owner's "tangent" move:
fork from any frontier point of any session — including an automation-spawned
one — into a new session with different runner and tool capabilities; the same
seed-from-frontier machinery the import milestone builds, with retargeting.

## Remote transport and real auth [blocked-on: owner design pass] [size: L]

Owns: network transport beside the local socket, authentication. Collides-with:
process protocol surfaces. Gates iOS, the web surface, and any off-machine
client. The predecessor's bolted-on shared-key auth is the recorded
anti-pattern.

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
private adapters (for example email infrastructure) stay outside the repo as
plugins; Signalbox owns the trigger seam, session provenance, and the visibility
classification they rely on.

## Client SDK [blocked-on: protocol stabilization] [size: M]

Owns: new SDK crate/package. Collides-with: nothing. Typed identities held at
the SDK boundary — the predecessor's recorded newtype erosion started exactly
there.

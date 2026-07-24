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

## Conversation import [in-flight] [size: L]

Owns: new converter crate, session creation/ancestry, imported-conversation
store (new migration). Collides-with: session-creation surfaces only. Running as
a goal session with owner addenda (maximum-fidelity conversion, raw
preservation, adoption as a standing client capability rather than an
import-time mode).

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
prototype of the owner's holds a working dual-runtime reference —
runtime-backend plus capability-snapshot routing with fail-explicit rejection (a
session requiring tools cannot land on a backend lacking them), a
provider-neutral agent event vocabulary spanning both CLIs, and
none/import_only/adopt_resume/adopt_fork adoption modes for provider-owned
external sessions held as durable pointers.

### Codex CLI wrap [blocked-on: INV-025/026 request-boundary reconciliation] [size: S-M]

The lead track of the three once unblocked. `codex exec --json`; the
thread/turn/item event taxonomy is cleanly namespaced with an unambiguous
turn.completed/turn.failed terminal (the demanding part of the evidence model).
CLI owns subscription auth — zero credential handling. Officially sanctioned
automation path; only real event risk is schema drift between CLI versions (pin
a version, snapshot-test). The subprocess request-boundary question above
(INV-025/026) is a foundation decision the spec-diff must settle and the owner
must accept before implementation starts.

### Claude Code CLI wrap [blocked-on: INV-025/026 request-boundary reconciliation] [size: S-M]

`claude -p --output-format stream-json --verbose` (+
`--include-partial-messages` for deltas). Clean result terminal message; CLI
owns subscription auth. Fragility: the full stream-json event set is
undocumented and version-fragile — snapshot-test. Do not use `--bare` for
subscription runs (it forces an API key). Same blocker as the Codex track: the
subprocess request-boundary reconciliation (INV-025/026) is a foundation
decision the spec-diff must settle and the owner must accept first.

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
economy (catalog, permissions, confirm/deny, shared tools, delegation). This
foundation is the hub-side approval algebra plus the first hub-local tool; the
client approval surface is a separate later milestone whose UX is settled then.

## Durable approval waits [blocked-on: tool loop design pass] [size: M]

Owns: a waiting-for-confirmation turn state, dedupe-keyed resume commands in the
outbox, replay eligibility on the executor path. Collides-with: turn machinery —
these are the wait mechanics the tool loop's approval flow will need, so it
lands with or just behind that foundation. Closes the spec's open edge for
tool/approval waits. The reference design is an earlier unmerged prototype of
the owner's: resume commands keyed `resume_turn:{turn}:{invocation}` in the
outbox, claimed with `FOR UPDATE SKIP LOCKED` and replayed to reconnecting
executors, with replay eligibility conditioned on turn state.

## Session metadata, tags, and visibility [blocked-on: owner design pass] [size: M-L]

Owns: session satellite tables, list projection, additive protocol frames.
Collides-with: little — parallel-safe against turn machinery. Titles, tags,
archive/restore, filtered and paginated listing — plus visibility control for
the automation era: sessions spawned by automations and background work must not
crowd the interactive default view, while monitor surfaces see everything and
can hop into any session. A likely simple starting point, to be settled in the
entry's spec-diff and not here: creation cause and actor attribution already
distinguish owner-initiated from automation-spawned sessions for free, so the
interactive default could derive from that attribution with manual tags as an
override; expressive filter rules stay an open edge. Owner-flagged high priority
— the daily-driver item.

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
prototype of the owner's — implemented and unit-tested, never production-smoked.

## Client SDK [blocked-on: protocol stabilization] [size: M]

Owns: new SDK crate/package. Collides-with: nothing. Typed identities held at
the SDK boundary — untyped-identity erosion characteristically starts exactly
there.

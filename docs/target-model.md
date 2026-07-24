# Target model

This document records the owner's directional product and domain target: the
destination Signalbox is being steered toward, written down so milestone
selection aims at a destination instead of drifting.

## Purpose and authority

This document is **directional destination only, never authority**. Implemented
behavior is owned by the [living specification](spec/README.md) under
`docs/spec/`, together with the [invariant catalog](invariants.md) and the
[domain spine](domain-spine.md); where this document and those pages disagree
about what the system does, they win and this document describes only where the
system is headed. The [glossary](glossary.md) is a terminology index, not part
of that normative surface. Concepts described here that the spec does not yet
describe are **targets awaiting decisions, not decisions**: naming a concept
here authorizes neither its implementation nor a silent closure of an open
question. The [one-place rule](../AGENTS.md) applies throughout — implemented
semantics are linked, never restated, and only target-only concepts are owned by
this document.

Milestone selection rules for autonomous runs live in
[goal-mode.md](goal-mode.md); this document owns only the
[priority order](#priority-order) those rules reference.

## Product vision

Signalbox's standing purpose, deployment shape, and first-version non-goals are
owned by the [vision](vision.md): a personal, single-owner, always-on platform
for durable LLM-assisted work — one central hub with Postgres as the canonical
store, outbound-connected runners for tool execution, and terminal, web, macOS,
and iOS clients.

The product target sharpens that into a destination: a session platform that
absorbs the best interaction features of contemporary agent products — durable
multi-device conversations, mid-turn steering, tool use gated by inspectable
approvals, delegated sub-agent sessions, forking from any earlier point,
artifacts with provenance, and live reconnect that never lies about what is
final — while keeping the properties those products rarely guarantee: sessions
and accepted work survive process restarts and client disconnects
([recovery posture](architecture.md#recovery-posture), INV-007, INV-010), every
external effect is honestly classified including ambiguity (INV-025), and
provenance for who or what caused each change is reconstructible after the fact.

## Concept catalog

The complete target noun set, each with a one-line responsibility. Names follow
the [glossary](glossary.md). Concepts the [living spec](spec/README.md) already
describes appear here only as destination responsibilities; concepts marked
*(target)* have no implemented behavior and are detailed only here.

| Concept                                       | Responsibility                                                                                                                                                                                                                                     |
| --------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Session**                                   | One durable, independently browsable conversation ([glossary](glossary.md#session)); the minimal long-lived aggregate boundary.                                                                                                                    |
| **SessionCreationCause / TranscriptAncestry** | Two independent immutable creation facts — why the session exists and where its initial semantic context came from.                                                                                                                                |
| **AcceptedInput**                             | One admitted submission made durable with its explicit delivery treatment and recoverable disposition; the implemented baseline admits only owner-submitted user content ([glossary](glossary.md#accepted-input)).                                 |
| **DurableCommandId**                          | The owner-global idempotency identity for durably handled caller commands ([glossary](glossary.md#durable-command-identity)).                                                                                                                      |
| **SemanticTranscriptEntry**                   | One immutable identified semantic-history fact, distinct from operational, streaming, and presentation state; target entry types below.                                                                                                            |
| **Turn**                                      | One durable logical request for a conversational outcome under one frozen effective configuration ([glossary](glossary.md#turn)).                                                                                                                  |
| **TurnAttempt**                               | One exclusive physical orchestration tenure advancing an active turn ([glossary](glossary.md#turn-attempt)).                                                                                                                                       |
| **ModelCall**                                 | One durable authorization to attempt one provider interaction, carrying its exact pinned target and context frontier ([glossary](glossary.md#model-call)).                                                                                         |
| **ContextFrontier / TranscriptFrontier**      | The immutable identified snapshot of the exact ordered semantic context consumed by one call or fixed at a turn start, and the distinct purpose-specific source boundary used by ancestry.                                                         |
| **ToolRequest**                               | One logical request for a named tool operation with normalized arguments, policy state, and eventual logical outcome ([glossary](glossary.md#tool-request); target lifecycle below).                                                               |
| **ToolAttempt**                               | One physical effort to execute one tool request at one placement, fenced by its dispatch generation ([glossary](glossary.md#tool-attempt)).                                                                                                        |
| **ApprovalDecision** *(target record)*        | An immutable recorded human decision permitting or denying one exact tool request; the binding rule is fixed ([glossary](glossary.md#approval), INV-019), while the durable record shape, consumption, and expiry await the future tool decisions. |
| **Tool-risk metadata** *(target)*             | Trusted, registry-owned classification of a tool operation — side-effect scope, reversibility, idempotency, data egress — supplied by the tool registry and never by model-provided arguments; awaiting the future tool-safety decisions.          |
| **Effective configuration**                   | The complete immutable configuration governing one turn ([glossary](glossary.md#effective-configuration)); target contents below.                                                                                                                  |
| **Resolved model plan** *(target)*            | A nonempty ordered set of exact provider/model candidates plus an explicit frozen fallback policy, generalizing today's single pinned target only if fallback is ever accepted.                                                                    |
| **Streaming response snapshot** *(target)*    | An optional, noncanonical, replaceable checkpoint of partial provider output used only for reconnect continuity; never transcript truth.                                                                                                           |
| **Artifact** *(target)*                       | An independently retrievable output with its own identity, content digest, and producer provenance, linked into sessions by reference.                                                                                                             |
| **Goal** *(target)*                           | One durable persistent objective with explicit pursue, pause, resume, and revise transitions; identity and lifecycle require a future foundation decision.                                                                                         |
| **UpdateSubscription** *(target)*             | One durable standing registration that converts later updates into explicitly delivered session input; identity, lifetime, delivery, and cancellation require a future foundation decision.                                                        |
| **Actor**                                     | Typed provenance attribution recorded with commands and transitions — provenance, not a multi-user authorization model; the variant set is closed, never open-ended.                                                                               |

### Target semantic-entry types

The implemented entry payloads are owned by
[sessions-and-transcript](spec/sessions-and-transcript.md). The target entry set
additionally includes, each awaiting its owning decision:

- **Remaining content and outcome markers** — refusal, reconciliation,
  accepted-risk, mismatch, tool-result, approval, and other semantic facts whose
  exact entry boundaries remain open.
- **Supersession** — editing is append-only: a replacement entry plus a typed
  supersession relation, never in-place mutation of committed history.
- **Compaction summaries** — an explicit semantic marker standing for summarized
  history; any non-prefix-preserving context policy needs its own foundation
  decision.
- **Visibility annotations** — hiding content from default projections without
  erasing durable history; destructive purge stays a separate retention-policy
  decision.
- **Delegated-session references** — a typed reference to a child session and
  its delivered result, arriving with delegation.

### Effective configuration: target contents

The implemented baseline is deliberately model-selection-only
([configuration-and-credentials](spec/configuration-and-credentials.md)), and
any new semantic category must extend the request, default, override, and
effective-value algebras together. The target grows the frozen value to cover:

- the model plan (single pinned target today; a resolved model plan if fallback
  is ever accepted);
- tool policy together with the pinned tool revisions the turn may use;
- the prompt-renderer version, so the entry-to-provider-prompt projection that a
  retry uses is the one the turn froze; and
- execution limits (token, tool, output, recursion, and wall-time budgets —
  resource governance remains an open question).

Informally, this complete frozen value is the turn's *execution fingerprint*:
recovery may continue the same turn only while it is unchanged, and any semantic
difference is new logical work. The authority is structural typed equality of
the frozen value — a digest or request hash never defines identity.

## Target lifecycles

Implemented lifecycles are owned by the living spec and linked at the end of
this section, not restated. The sketches below are targets: they show the
destination the future tool decisions should reach, and they are constrained by
the accepted invariants cited inline. Conflating session identity with retry
identity, or a logical request with its physical attempts, weakens recovery —
the identity boundaries exist to prevent exactly that, and every sketch below
preserves them.

### Logical tool requests (target)

```text
Proposed
  -> RejectedByPolicy                       (terminal: policy refuses without consulting the owner)
  -> AwaitingApproval                       (policy requires a human decision)
  -> Ready                                  (policy allows, or a matching approval is consumed)
AwaitingApproval -> Ready | Denied
Ready -> Dispatching                        (a fenced tool attempt is created)
Dispatching -> Succeeded | KnownFailed | Ambiguous
```

Accepted rules that already shape this design: a denied request can never create
an authorized physical attempt (INV-027); terminal outcomes never reopen
(INV-006); turn terminalization must close every authorized-but-undispatched
request so nothing can dispatch it after the slot is released, and an interrupt
that closes an approval wait terminally cancels the owned request. The request
belongs to its turn, not to one attempt, so it survives approval pauses, client
reconnects, hub restarts, and replacement turn attempts. Changed arguments or a
changed tool revision are a new request, never a mutation. Request-level
`Ambiguous` derives from attempt evidence: physical ambiguity is recorded on the
attempt (INV-025) and effect policy decides whether another attempt is ever
permitted (INV-026).

### Physical tool attempts (target)

```text
Prepared -> Executing -> Succeeded | KnownFailed | Ambiguous
Prepared -> CancelledBeforeExecution
```

Each attempt records its executor placement, dispatch identity, timing, output,
and outcome classification. Dispatch and result acceptance are fenced by the
attempt-plus-generation pair, so a stale attempt or superseded dispatch cannot
advance current state (INV-011, INV-021). An external write whose
acknowledgement is lost ends `Ambiguous` and is never blindly repeated (INV-025,
INV-026); a second attempt for the same request exists only where effect policy
proves repetition safe.

### Approval algebra (target)

An `ApprovalDecision` is an immutable fact, not mutable request state: it
records approve or deny for one exact `ToolRequestId`, the normalized arguments
presented to the owner (a digest may index them, but structural equality remains
the authority), the pinned tool revision, and the material execution constraints
— the accepted binding rule (INV-019; [glossary](glossary.md#approval)).
Consumption is transactional: `AwaitingApproval` plus a matching unexpired
approval becomes `Ready` in the same transaction that closes the turn's approval
wait and creates its continuing attempt. A stale, expired, or mismatched
decision has no effect on current state and is retained as history. A model or
automated-judge recommendation is a distinct actor and never masquerades as
human approval (INV-020). Expiry, revocation, and scoped standing grants remain
open.

### Cancellation of tool work (target)

Baseline cancellation authority is the applied interrupt; there is no standalone
cancel command. For an individual effect the target keeps three honest timings:

1. **Before an attempt is prepared** — the request is closed non-dispatchable;
   no external effect exists.
2. **Prepared but not sent** — the attempt ends cancelled only with evidence it
   never crossed the external boundary.
3. **After send** — best-effort cancellation is requested and durably recorded,
   but the external system may continue; classification may honestly be
   completion, known failure, or ambiguity, and completion may win the race.
   Cancellation never claims rollback.

A stale attempt cannot commit results after supersession or terminalization, and
local process cancellation must terminate the entire process group (see
[execution isolation](#execution-isolation-target)).

### Delegation (target)

A delegated child is a real session: distinct identity, independently browsable,
with an explicit typed relationship to the exact parent work (INV-031). The
delegated creation cause carries the exact durable parent-work identity, and
cause remains independent of transcript ancestry. Result delivery targets the
exact parent `ToolRequest`, so a late or duplicate child result cannot attach to
the wrong work. Parent cancellation may request child cancellation under
explicit policy but never deletes the child session or its history. Child waits,
result representation, detached work, and cancellation propagation all await
delegation's owning foundation decision.

### Implemented lifecycles

The turn and attempt lifecycle — waits, stop causes, terminal guards, the
session slot, and the startup recovery scan — is owned by
[turn-lifecycle-and-scheduling](spec/turn-lifecycle-and-scheduling.md);
model-call semantics, retry and continuation identity, and provider failure
classification by [model-call-execution](spec/model-call-execution.md). Two
consequences worth repeating only as orientation: retry intent is always
expressed as a new `TurnAttempt` — a terminal state never reopens (INV-006) —
and unresolved ambiguity holds the turn in its recovery-decision wait until
evidence or an explicit owner decision resolves it, rather than being coerced
into failure or silently retried.

## Execution isolation target

Tool execution should not inherit the owner's ambient authority. The target
restricted-executor profile, for both hub-local executors and restricted
runners:

- a dedicated restricted execution identity, never the owner's account;
- no ambient owner credentials — no SSH agent, browser profile, credential-store
  socket, cloud metadata, or provider key is inherited; any credential a tool
  needs is injected per attempt under the hub-controlled boundary (INV-035;
  [configuration-and-credentials](spec/configuration-and-credentials.md));
- explicit mounts — read-only workspace by default, allowlisted writable paths,
  symlink and mount escapes rejected;
- network disabled or allowlisted per tool policy;
- process-group cleanup — cancelling an attempt terminates the whole process
  tree; and
- enforced resource limits — CPU, memory, process count, disk, output size, wall
  time.

Claimed isolation never exceeds evidence: declared, configured, and verified
runner properties stay distinct, an ambient-user runner is an explicit visible
choice never labeled sandboxed, and the
[execution boundary](glossary.md#execution-boundary) shown for a dispatch
reflects only what the evidence supports (INV-022, INV-023). The isolation
strength required for hostile or arbitrary generated code is deliberately still
open; the target treats stronger sandboxes as an extension point, not a baseline
assumption.

## Live updates target

The target reconnect semantics: a client reconstructs authoritative durable
state from a snapshot with an observation cursor, resumes strictly ordered
durable-transition events after that cursor, and treats streamed drafts as
replaceable transient content (INV-032). The publication mechanism inside the
hub is the transactional outbox; its implemented storage foundation and
same-transaction appends are owned by
[persistence-protocol](spec/persistence-protocol.md). The local version-one
publisher and client boundary are owned by
[process-protocol](spec/process-protocol.md); transient-draft relay and remote
transports remain future work.

## Destination features

The owner's feature arc beyond the landed model-call substrate. Everything here
is directional under [Purpose and authority](#purpose-and-authority): each
feature reaches code only through its own future owning decisions, none is
authorized by appearing here, and the [priority order](#priority-order) still
governs sequencing. The inspirations are the same contemporary agent products
the [product vision](#product-vision) absorbs; the features below are where a
durable-session substrate should let Signalbox go further than they do.

### Context management and compaction (target)

Compaction as a first-class product surface, not a hidden token-budget
mitigation: owner-supplied compaction prompts, multiple named compaction
strategies — long-running work warrants a different treatment than a short
exchange — and whole compaction workflows that select, apply, and review a
strategy. The owning seats are identified: the deferred
[compaction-summary entry](#target-semantic-entry-types) is the explicit
semantic marker, and any non-prefix-preserving context policy needs an
additional foundation decision. That future decision should preserve cheap
strategy experimentation: competing compactions should be able to coexist as
distinct snapshots over retained history. It must also define when trying one
may discard anything and how compaction interacts with the still-open retention
policy.

### Inter-session messaging (target)

Sessions send messages to sessions: an accepted input whose actor is another
session's agency rather than the owner. This requires a foundation extension to
the closed actor algebra, an explicit decision admitting that actor for
`SubmitInput`, and the still-open authentication and authorization decisions.
Delivery and queueing reuse the implemented treatments
([turn-lifecycle-and-scheduling](spec/turn-lifecycle-and-scheduling.md)): a
session-sent message arrives under the same explicit delivery requests and
durable queue order as owner input, never through a parallel channel. The
baseline `Interrupt` treatment remains owner-only (INV-029); session-issued
interruption requires a foundation decision extending that cancellation
authority. The client-facing surface is a planned
[session-management tool family](#the-tool-system-as-the-load-bearing-layer):
list sessions and send a message. Receive-update callbacks additionally require
a decision defining their standing subscription and delivery lifecycle.

### Orchestrator sessions and linking (target)

An orchestrator session coordinates linked sub-sessions — created by it, or
created separately and linked on request — with results and messages flowing
between orchestrator and sub-sessions. Sessions and their durable messages
remain hub-owned when work spans worktrees, pull requests, or machines; runners
only execute dispatched tools, and the hub applies every resulting message
through the same durable messaging surface. The grounding is future delegation
and the rule it builds on — a child is a real session, independently browsable,
with an explicit typed relationship to the exact parent work (INV-031), per the
[delegation sketch](#delegation-target) above. Linking a session that delegation
did not create requires its own foundation decision for the typed
related-session relationship. Cross-machine tool placement remains owned by the
future runner-protocol decision.

### Session linking and visibility authority (target)

Which sessions may create sub-sessions, which may link other sessions without an
approval pause, and which may see other sessions at all is per-session
configurable authority. An attended watch mode can set less restrictive
authority for subsequently accepted origin turns while the owner is watching,
and background work can set stricter authority for later turns; an accepted
turn's frozen effective configuration does not change. Affecting an in-flight
tool request would instead require an explicit policy-reevaluation and
approval-invalidation decision. No configuration grants unlimited permission.
Visibility and approval authority is owned by the future tool-policy and
approval decisions, constrained by the accepted binding and honesty rules
(INV-019, INV-020, INV-023); per-session configurability lands through new
configuration categories, extending the request, default, override, and
effective-value algebras together. Independent-session linking remains blocked
on the separate foundation decision identified above.

### Goal mode as a platform feature (target)

A persistent objective a session works toward across turns — pursued, paused,
resumed, revised — as a product capability, not only this repository's own
[operating rules](goal-mode.md). A foundation decision must define durable goal
identity and the pursue, pause, resume, and revise transitions. That lifecycle
then composes with long-running turns, scheduled creation causes at the explicit
extension point session creation leaves open, and delegation.

### The tool system as the load-bearing layer

Most of the features above surface as tools, so the future tool-policy,
approval, and runner-protocol decisions are the enabling decisions for this
whole section, not merely for step five of the priority order.
Session-management tools to list sessions and send a message are a planned tool
family under those decisions, entering turns like any other tool: normalized
logical requests, policy, approvals, and fenced attempts per the
[target lifecycles](#target-lifecycles) above. A link-session tool additionally
requires the independent-link foundation decision identified above. A standing
update subscription is not an ordinary tool attempt: it needs its own decision
for registration identity, lifetime, callback delivery, and conversion of later
events into session input.

### An app-facing SDK (target)

The app-facing SDK remains a directional target only: it queues behind the
destination features above, and the model-runtime substrate it would build on is
owned by [runtime-substrate](spec/runtime-substrate.md).

## Deferred and explicit non-goals

The [vision's first-version non-goals](vision.md#first-version-non-goals) stand.
In addition, the following are absent from the target itself — agents should not
select milestones toward them, and a future change of direction would revise
this document first:

- **Multi-user ACLs, teams, and shared quotas.** Single-owner scope is fixed; a
  future multi-owner model is a foundation decision.
- **Distributed schedulers and cross-host workers.** The single-hub,
  Postgres-coordinated baseline
  ([turn-lifecycle-and-scheduling](spec/turn-lifecycle-and-scheduling.md))
  stands with explicit adapter seams; no broker or worker fleet is in the
  target.
- **Full event sourcing.** Rejected as the primary representation
  ([persistence-protocol](spec/persistence-protocol.md)); append-only tables
  exist only where facts are immutable.
- **Session merging and multi-source ancestry.** An explicit extension boundary,
  not a destination.
- **Deterministic model replay.** Rerunning pinned inputs is comparative new
  work, never a claim of reproducing provider behavior.
- **Persisting every streaming token.** Drafts remain transient; the only target
  checkpoint is the noncanonical streaming snapshot above.

## Priority order

The near-term arc, in order. Landed steps stay listed so the arc keeps its
shape; their implemented behavior is owned by the [living spec](spec/README.md).

1. **Durable input acceptance** — landed
   ([sessions-and-transcript](spec/sessions-and-transcript.md),
   [identity-and-commands](spec/identity-and-commands.md)).
2. **Turn creation and the session slot** — landed
   ([turn-lifecycle-and-scheduling](spec/turn-lifecycle-and-scheduling.md)).
3. **A model call against a scripted provider** — landed
   ([model-call-execution](spec/model-call-execution.md)).
4. **A smoke against a real provider** — landed for the owner-run smoke
   ([model-call-execution](spec/model-call-execution.md),
   [runtime-substrate](spec/runtime-substrate.md)); the production security
   posture for outbound provider calls remains open per
   [provider-call-security](open-questions.md#provider-call-security). This was
   the gate before destination-feature milestones.
5. **The tool loop with approvals.** ToolRequest and ToolAttempt lifecycles, the
   trusted risk registry, approval consumption, and a first harmless hub-local
   tool. Blocked on the tool-policy and approval decisions, which do not yet
   exist.
6. **The restricted executor.** The
   [execution isolation target](#execution-isolation-target) applied to a first
   restricted placement. Blocked on the sandbox-minimum and execution-identity
   decisions.
7. **Delegation and forking.** Fork frontier selection (the still-open
   selectable-boundary question), then delegation under its own foundation
   decision: delegated cause, child waits, targeted result delivery,
   cancellation propagation.
8. **Artifacts.** Identity, digest, producer provenance, controlled byte
   storage, and transcript links.
9. **Destination features.** Reach the
   [destination-feature arc](#destination-features) through its owning
   decisions: context management and compaction; inter-session messaging;
   orchestrator sessions, linking, and visibility authority; and persistent goal
   mode. Their order within this step remains owner-directed milestone
   selection.

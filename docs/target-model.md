# Target model

This document records the owner's directional product and domain target: the
destination Signalbox is being steered toward, written down so milestone
selection aims at a destination instead of drifting.

## Purpose and authority

This document is **directional, not normative**. Accepted records under
[docs/decisions/](decisions/README.md), the [invariant catalog](invariants.md),
and the [decision log](decisions.md) always override it; where this document and
an accepted record disagree, the record wins and this document is out of date.
Concepts described here that lack an accepted ADR are **targets awaiting
decisions, not decisions**: naming a concept here authorizes neither its
implementation nor a silent closure of an open question. The
[one-place rule](../AGENTS.md) applies throughout — decided semantics are
linked, never restated, and only target-only concepts are owned by this
document.

Milestone selection rules for autonomous runs live in
[goal-mode.md](goal-mode.md); this document owns only the
[priority order](#priority-order) and [concept status map](#concept-status-map)
those rules reference. A row marked *Target* identifies work that needs a
decision before code; a row marked *Reserved* names the ADR number that decision
must use. Cross-check the [open questions inventory](open-questions.md) before
treating anything here as settled.

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
the accepted [glossary](glossary.md); concepts without an accepted decision are
marked as targets and detailed only here.

| Concept                                       | Responsibility                                                                                                                                                                                                                                                                                                            |
| --------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Session**                                   | One durable, independently browsable conversation ([glossary](glossary.md#session)); minimal long-lived aggregate boundary per [ADR-0038](decisions/0038-session-aggregate-boundary.md).                                                                                                                                  |
| **SessionCreationCause / TranscriptAncestry** | Two independent immutable creation facts — why the session exists and where its initial semantic context came from ([ADR-0003](decisions/0003-session-creation-and-transcript-ancestry.md)).                                                                                                                              |
| **AcceptedInput**                             | One user submission made durable with its explicit delivery treatment and recoverable disposition ([glossary](glossary.md#accepted-input); [ADR-0027](decisions/0027-input-delivery-lifecycle.md), content [ADR-0037](decisions/0037-baseline-user-content.md)).                                                          |
| **DurableCommandId**                          | The owner-global idempotency identity for durably handled caller commands ([glossary](glossary.md#durable-command-identity); [ADR-0001](decisions/0001-domain-terminology-and-identity.md), storage [ADR-0034](decisions/0034-durable-command-storage-and-equality.md)).                                                  |
| **SemanticTranscriptEntry**                   | One immutable identified semantic-history fact, distinct from operational, streaming, and presentation state ([ADR-0036](decisions/0036-initial-semantic-transcript-entries.md)); target entry types below.                                                                                                               |
| **Turn**                                      | One durable logical request for a conversational outcome under one frozen effective configuration ([glossary](glossary.md#turn); [ADR-0004](decisions/0004-turn-and-attempt-lifecycle.md)).                                                                                                                               |
| **TurnAttempt**                               | One exclusive physical orchestration tenure advancing an active turn ([glossary](glossary.md#turn-attempt); [ADR-0004](decisions/0004-turn-and-attempt-lifecycle.md)).                                                                                                                                                    |
| **ModelCall**                                 | One durable authorization to attempt one provider interaction, carrying its exact pinned target and context frontier ([glossary](glossary.md#model-call); [ADR-0005](decisions/0005-model-call-retry-semantics.md)).                                                                                                      |
| **ContextFrontier / TranscriptFrontier**      | The immutable identified snapshot of the exact ordered semantic context consumed by one call or fixed at a turn start, and the distinct purpose-specific source boundary used by ancestry ([ADR-0030](decisions/0030-context-frontier-snapshots.md)).                                                                     |
| **ToolRequest**                               | One logical request for a named tool operation with normalized arguments, policy state, and eventual logical outcome ([glossary](glossary.md#tool-request); identity and ownership [ADR-0001](decisions/0001-domain-terminology-and-identity.md); target lifecycle below).                                                |
| **ToolAttempt**                               | One physical effort to execute one tool request at one placement, fenced by its dispatch generation ([glossary](glossary.md#tool-attempt); fencing [ADR-0009](decisions/0009-dispatch-fencing.md)).                                                                                                                       |
| **ApprovalDecision** *(target record)*        | An immutable recorded human decision permitting or denying one exact tool request; the binding rule is accepted ([glossary](glossary.md#approval), INV-019), while the durable record shape, consumption, and expiry await the reserved tool decisions.                                                                   |
| **Tool-risk metadata** *(target)*             | Trusted, registry-owned classification of a tool operation — side-effect scope, reversibility, idempotency, data egress — supplied by the tool registry and never by model-provided arguments; awaiting the reserved [tool-safety decisions](open-questions.md#tool-safety-reserved-adr-0011-adr-0012-adr-0013-adr-0014). |
| **Effective configuration**                   | The complete immutable configuration governing one turn ([glossary](glossary.md#effective-configuration); baseline [ADR-0027](decisions/0027-input-delivery-lifecycle.md)); target contents below.                                                                                                                        |
| **Resolved model plan** *(target)*            | A nonempty ordered set of exact provider/model candidates plus an explicit frozen fallback policy, generalizing today's single pinned target only if fallback is accepted (reserved [ADR-0006/ADR-0007](open-questions.md#model-fallback-and-provenance-reserved-adr-0006-adr-0007)).                                     |
| **Streaming response snapshot** *(target)*    | An optional, noncanonical, replaceable checkpoint of partial provider output used only for reconnect continuity; never transcript truth (the open streaming-checkpoint question under [ADR-0022](decisions/0022-persistence-representation.md)).                                                                          |
| **Artifact** *(target)*                       | An independently retrievable output with its own identity, content digest, and producer provenance, linked into sessions by reference.                                                                                                                                                                                    |
| **Actor**                                     | Typed provenance attribution recorded with commands and transitions — provenance, not a multi-user authorization model; the closed variant set is normative in [ADR-0039](decisions/0039-actor-attribution.md).                                                                                                           |

### Target semantic-entry types

[ADR-0036](decisions/0036-initial-semantic-transcript-entries.md) fixes the
first two entry payloads; the
[remaining variants are open](open-questions.md#identity-representation). The
target entry set additionally includes, each awaiting its owning decision:

- **Assistant content and outcome markers** — committed assistant output plus
  the completion, refusal, cancellation, reconciliation, accepted-risk, and
  mismatch markers whose required presence accepted records already fix.
- **Supersession** — editing is append-only: a replacement entry plus a typed
  supersession relation, never in-place mutation of committed history.
- **Compaction summaries** — an explicit semantic marker standing for summarized
  history; any non-prefix-preserving context policy needs the foundation
  decision [ADR-0030](decisions/0030-context-frontier-snapshots.md) reserves for
  it.
- **Visibility annotations** — hiding content from default projections without
  erasing durable history; destructive purge stays a separate
  [retention policy](open-questions.md#archival-and-retention-reserved-adr-0028-adr-0029).
- **Delegated-session references** — a typed reference to a child session and
  its delivered result, arriving with delegation (reserved
  [ADR-0002](open-questions.md#delegation-reserved-adr-0002)).

### Effective configuration: target contents

The accepted baseline is deliberately model-selection-only;
[ADR-0027](decisions/0027-input-delivery-lifecycle.md) requires any new semantic
category to extend the request, default, override, and effective-value algebras
together. The target grows the frozen value to cover:

- the model plan (single pinned target today; a resolved model plan if fallback
  is accepted);
- tool policy together with the pinned tool revisions the turn may use;
- the prompt-renderer version, so the entry-to-provider-prompt projection that a
  retry uses is the one the turn froze; and
- execution limits (token, tool, output, recursion, and wall-time budgets —
  [resource governance is open](open-questions.md#identity-credentials-and-resource-governance-reserved-adr-0015-through-adr-0018)).

Informally, this complete frozen value is the turn's *execution fingerprint*:
recovery may continue the same turn only while it is unchanged, and any semantic
difference is new logical work. The authority is structural typed equality of
the frozen value — a digest or request hash never defines identity
([ADR-0005](decisions/0005-model-call-retry-semantics.md)).

## Target lifecycles

Lifecycles already decided are linked at the end of this section and not
restated. The sketches below are targets: they show the destination the reserved
tool decisions (ADR-0011 through ADR-0014) should reach, and they are
constrained by the accepted invariants cited inline. Conflating session identity
with retry identity, or a logical request with its physical attempts, weakens
recovery — the accepted identity boundaries
([ADR-0001](decisions/0001-domain-terminology-and-identity.md)) exist to prevent
exactly that, and every sketch below preserves them.

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
that closes an approval wait terminally cancels the owned request
([ADR-0004](decisions/0004-turn-and-attempt-lifecycle.md)). The request belongs
to its turn, not to one attempt, so it survives approval pauses, client
reconnects, hub restarts, and replacement turn attempts
([ADR-0001](decisions/0001-domain-terminology-and-identity.md)). Changed
arguments or a changed tool revision are a new request, never a mutation.
Request-level `Ambiguous` derives from attempt evidence: physical ambiguity is
recorded on the attempt (INV-025) and effect policy decides whether another
attempt is ever permitted (INV-026).

### Physical tool attempts (target)

```text
Prepared -> Executing -> Succeeded | KnownFailed | Ambiguous
Prepared -> CancelledBeforeExecution
```

Each attempt records its executor placement, dispatch identity, timing, output,
and outcome classification. Dispatch and result acceptance are fenced by the
attempt-plus-generation pair, so a stale attempt or superseded dispatch cannot
advance current state — normative in
[ADR-0009](decisions/0009-dispatch-fencing.md) (INV-011, INV-021). An external
write whose acknowledgement is lost ends `Ambiguous` and is never blindly
repeated (INV-025, INV-026); a second attempt for the same request exists only
where effect policy proves repetition safe.

### Approval algebra (target)

An `ApprovalDecision` is an immutable fact, not mutable request state: it
records approve or deny for one exact `ToolRequestId`, the normalized arguments
presented to the owner (a digest may index them, but structural equality remains
the authority), the pinned tool revision, and the material execution constraints
— the accepted binding rule (INV-019; [glossary](glossary.md#approval)).
Consumption is transactional: `AwaitingApproval` plus a matching unexpired
approval becomes `Ready` in the same transaction that closes the turn's approval
wait and creates its continuing attempt (the turn-side transition is normative
in [ADR-0004](decisions/0004-turn-and-attempt-lifecycle.md)). A stale, expired,
or mismatched decision has no effect on current state and is retained as
history. A model or automated-judge recommendation is a distinct actor and never
masquerades as human approval (INV-020). Expiry, revocation, and scoped standing
grants remain
[open](open-questions.md#tool-safety-reserved-adr-0011-adr-0012-adr-0013-adr-0014).

### Cancellation of tool work (target)

Baseline cancellation authority is the applied interrupt; there is no standalone
cancel command ([ADR-0004](decisions/0004-turn-and-attempt-lifecycle.md),
[ADR-0027](decisions/0027-input-delivery-lifecycle.md)). For an individual
effect the target keeps three honest timings:

1. **Before an attempt is prepared** — the request is closed non-dispatchable;
   no external effect exists.
2. **Prepared but not sent** — the attempt ends cancelled only with evidence it
   never crossed the external boundary.
3. **After send** — best-effort cancellation is requested and durably recorded,
   but the external system may continue; classification may honestly be
   completion, known failure, or ambiguity, and completion may win the race.
   Cancellation never claims rollback.

A stale attempt cannot commit results after supersession or terminalization
([ADR-0009](decisions/0009-dispatch-fencing.md)), and local process cancellation
must terminate the entire process group (see
[execution isolation](#execution-isolation-target)).

### Delegation (target; reserved ADR-0002)

A delegated child is a real session: distinct identity, independently browsable,
with an explicit typed relationship to the exact parent work (INV-031). The
delegated creation cause carries the exact durable parent-work identity, and
cause remains independent of transcript ancestry
([ADR-0003](decisions/0003-session-creation-and-transcript-ancestry.md)). Result
delivery targets the exact parent `ToolRequest`, so a late or duplicate child
result cannot attach to the wrong work. Parent cancellation may request child
cancellation under explicit policy but never deletes the child session or its
history. Child waits, result representation, detached work, and cancellation
propagation are all
[reserved for ADR-0002](open-questions.md#delegation-reserved-adr-0002).

### Decided lifecycles

The turn and attempt lifecycle, including waits, stop causes, terminal guards,
and the startup recovery scan, is normative in
[ADR-0004](decisions/0004-turn-and-attempt-lifecycle.md) with the
closed-boundary refinement in
[ADR-0031](decisions/0031-direct-fatal-terminalization.md). Model-call
semantics, retry and continuation identity, and provider-target mismatch
handling are normative in
[ADR-0005](decisions/0005-model-call-retry-semantics.md). Two consequences worth
repeating only as orientation: retry intent is always expressed as a new
`TurnAttempt` — a terminal state never reopens (INV-006) — and unresolved
ambiguity holds the turn in its recovery-decision wait until evidence or an
explicit owner decision resolves it, rather than being coerced into failure or
silently retried.

## Execution isolation target

Tool execution should not inherit the owner's ambient authority. The target
restricted-executor profile, for both hub-local executors and restricted
runners:

- a dedicated restricted execution identity, never the owner's account;
- no ambient owner credentials — no SSH agent, browser profile, credential-store
  socket, cloud metadata, or provider key is inherited; any credential a tool
  needs is injected per attempt under the hub-controlled boundary (INV-035,
  [ADR-0017](decisions/0017-credential-lifecycle.md));
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
strength required for hostile or arbitrary generated code is deliberately an
[open question](open-questions.md#tool-safety-reserved-adr-0011-adr-0012-adr-0013-adr-0014);
the target treats stronger sandboxes as an extension point, not a baseline
assumption.

## Live updates target

Reconnect semantics are decided: a client reconstructs authoritative durable
state from a snapshot with an observation cursor, resumes strictly ordered
durable-transition events after that cursor, and treats streamed drafts as
replaceable transient content (INV-032; normative in
[ADR-0019](decisions/0019-process-protocol.md) and
[ADR-0021](decisions/0021-compatibility-and-negotiation.md)).

The publication mechanism inside the hub is decided: the transactional outbox is
normative in [ADR-0040](decisions/0040-transactional-outbox.md). No code
implements it yet.

## Concept status map

Statuses: **Implemented** (accepted decision plus code in tree), **Accepted**
(decision on `main`; code partial or absent), **Reserved** (awaiting the named
reserved ADR number), **Proposed** (an ADR proposal exists but is not accepted),
**Target** (no decision yet; owned by this document).

| Target concept                                                             | Status                                                                                                                                                                                                                                                                |
| -------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Session identity, creation, and long-lived aggregate                       | Implemented — [ADR-0001](decisions/0001-domain-terminology-and-identity.md), [ADR-0003](decisions/0003-session-creation-and-transcript-ancestry.md), [ADR-0038](decisions/0038-session-aggregate-boundary.md); domain and Postgres `CreateSession`/load slices exist  |
| Durable command identity, storage, and replay                              | Implemented — [ADR-0001](decisions/0001-domain-terminology-and-identity.md), [ADR-0033](decisions/0033-identity-generation-supply-and-encoding.md), [ADR-0034](decisions/0034-durable-command-storage-and-equality.md); registry and first typed command family exist |
| Accepted-input delivery lifecycle                                          | Accepted — [ADR-0027](decisions/0027-input-delivery-lifecycle.md), [ADR-0037](decisions/0037-baseline-user-content.md); acceptance transaction and occupied-slot storage implemented; steering consumption and matching interrupt pending (owner-ratified deferral)   |
| Turn / TurnAttempt lifecycle                                               | Accepted — [ADR-0004](decisions/0004-turn-and-attempt-lifecycle.md), [ADR-0031](decisions/0031-direct-fatal-terminalization.md); eligible-turn activation orchestration and Postgres slot storage implemented; attempt progression, stop, and recovery pending        |
| ModelCall lifecycle and provider evidence                                  | Accepted — [ADR-0005](decisions/0005-model-call-retry-semantics.md); domain values and transitions implemented, no provider adapter                                                                                                                                   |
| ContextFrontier snapshots                                                  | Implemented — [ADR-0030](decisions/0030-context-frontier-snapshots.md); domain values and Postgres materialized membership                                                                                                                                            |
| SemanticTranscriptEntry (origin and failed-turn variants)                  | Implemented for origin entries (committed at activation) — [ADR-0036](decisions/0036-initial-semantic-transcript-entries.md); the TurnFailed producer is pending                                                                                                      |
| Assistant-content, steering, tool, and outcome entry variants              | Target — [open](open-questions.md#identity-representation)                                                                                                                                                                                                            |
| Supersession (edit-as-append)                                              | Target                                                                                                                                                                                                                                                                |
| Compaction summaries                                                       | Target — requires the foundation decision reserved by [ADR-0030](decisions/0030-context-frontier-snapshots.md)                                                                                                                                                        |
| Visibility annotations                                                     | Target — destructive purge separately reserved ([ADR-0029](open-questions.md#archival-and-retention-reserved-adr-0028-adr-0029))                                                                                                                                      |
| Effective configuration (model-selection baseline)                         | Implemented — [ADR-0027](decisions/0027-input-delivery-lifecycle.md); `OriginConfiguration::freeze` and persisted frozen provenance; an alias-definition source is pending                                                                                            |
| Effective-configuration target categories (tools, prompt renderer, limits) | Target — [open](open-questions.md#configuration-categories)                                                                                                                                                                                                           |
| Resolved model plan and fallback policy                                    | Reserved — ADR-0006/ADR-0007 ([open](open-questions.md#model-fallback-and-provenance-reserved-adr-0006-adr-0007))                                                                                                                                                     |
| ToolRequest / ToolAttempt identities and ownership                         | Accepted — [ADR-0001](decisions/0001-domain-terminology-and-identity.md); identities only, implemented in domain code                                                                                                                                                 |
| ToolRequest / ToolAttempt lifecycles, risk taxonomy, retry policy          | Reserved — ADR-0011 through ADR-0014 ([open](open-questions.md#tool-safety-reserved-adr-0011-adr-0012-adr-0013-adr-0014))                                                                                                                                             |
| ApprovalDecision record, consumption, expiry                               | Reserved — ADR-0011 through ADR-0014; binding rule accepted (INV-019, INV-020, INV-027)                                                                                                                                                                               |
| Tool-risk metadata registry                                                | Reserved — ADR-0011                                                                                                                                                                                                                                                   |
| Dispatch fencing and initial scheduler                                     | Accepted — [ADR-0009](decisions/0009-dispatch-fencing.md), [ADR-0010](decisions/0010-initial-scheduler-mechanics.md)                                                                                                                                                  |
| Runner protocol, capabilities, placement                                   | Reserved — ADR-0008 ([open](open-questions.md#scheduling-and-runners-reserved-adr-0008))                                                                                                                                                                              |
| Execution isolation profiles                                               | Reserved — sandbox minimums with ADR-0011 through ADR-0014; execution identity, enrollment, and credentials with ADR-0015 through ADR-0018 ([ADR-0017](decisions/0017-credential-lifecycle.md) accepted)                                                              |
| Delegation and child sessions                                              | Reserved — ADR-0002 ([open](open-questions.md#delegation-reserved-adr-0002))                                                                                                                                                                                          |
| Forking from a transcript frontier                                         | Accepted — [ADR-0003](decisions/0003-session-creation-and-transcript-ancestry.md), [ADR-0030](decisions/0030-context-frontier-snapshots.md); selectable frontier boundaries [open](open-questions.md#identity-representation)                                         |
| Archive / restore                                                          | Reserved — ADR-0028; destructive retention ADR-0029                                                                                                                                                                                                                   |
| Live-update protocol semantics                                             | Accepted — [ADR-0019](decisions/0019-process-protocol.md), [ADR-0021](decisions/0021-compatibility-and-negotiation.md)                                                                                                                                                |
| Transactional outbox publication                                           | Accepted — [ADR-0040](decisions/0040-transactional-outbox.md); no code                                                                                                                                                                                                |
| Actor attribution                                                          | Implemented — [ADR-0039](decisions/0039-actor-attribution.md); SubmitInput attribution storage and stored-actor validation                                                                                                                                            |
| Streaming response snapshots                                               | Target — streaming-checkpoint question open under [ADR-0022](decisions/0022-persistence-representation.md)                                                                                                                                                            |
| Artifacts                                                                  | Target                                                                                                                                                                                                                                                                |

## Deferred and explicit non-goals

The [vision's first-version non-goals](vision.md#first-version-non-goals) stand.
In addition, the following are absent from the target itself — agents should not
select milestones toward them, and a future change of direction would revise
this document first:

- **Multi-user ACLs, teams, and shared quotas.** Single-owner scope is fixed; a
  future multi-owner model is a foundation decision (the owner namespace note in
  [ADR-0034](decisions/0034-durable-command-storage-and-equality.md)).
- **Distributed schedulers and cross-host workers.** The single-hub,
  Postgres-coordinated baseline is accepted
  ([ADR-0010](decisions/0010-initial-scheduler-mechanics.md)) with explicit
  adapter seams; no broker or worker fleet is in the target.
- **Full event sourcing.** Rejected as the primary representation by
  [ADR-0022](decisions/0022-persistence-representation.md); append-only tables
  exist only where facts are immutable.
- **Session merging and multi-source ancestry.** An explicit extension boundary,
  not a destination
  ([ADR-0003](decisions/0003-session-creation-and-transcript-ancestry.md)).
- **Deterministic model replay.** Rerunning pinned inputs is comparative new
  work, never a claim of reproducing provider behavior.
- **Persisting every streaming token.** Drafts remain transient; the only target
  checkpoint is the noncanonical streaming snapshot above.

## Priority order

The near-term arc, in order. Each step should land through the decision-first
process in [AGENTS.md](../AGENTS.md); a step whose blocking decision is reserved
or open is reached by proposing that decision, not by implementing around it.

1. **Durable input acceptance.** The complete `SubmitInput` slice: canonical
   construction, owner-global deduplication, and one atomic acceptance
   transaction persisting content, delivery treatment, order facts, disposition,
   and configuration provenance before acknowledgement. Decisions accepted
   ([ADR-0027](decisions/0027-input-delivery-lifecycle.md),
   [ADR-0034](decisions/0034-durable-command-storage-and-equality.md),
   [ADR-0037](decisions/0037-baseline-user-content.md)); domain values largely
   exist, so the milestone is the transaction and application boundary.
2. **Turn creation and the session slot.** Eligibility derivation, origin
   semantic entry, starting frontier, activation with the initial prepared
   attempt, and database slot enforcement
   ([ADR-0004](decisions/0004-turn-and-attempt-lifecycle.md),
   [ADR-0010](decisions/0010-initial-scheduler-mechanics.md),
   [ADR-0022](decisions/0022-persistence-representation.md),
   [ADR-0030](decisions/0030-context-frontier-snapshots.md),
   [ADR-0036](decisions/0036-initial-semantic-transcript-entries.md)).
3. **A model call against a scripted provider.** Target resolution and pinning,
   prepared-call creation, an in-repo scripted provider adapter, transient draft
   streaming, assistant-content commit, and the idempotent startup scan
   (INV-034). Blocked in part by the open assistant-content entry variant.
4. **The tool loop with approvals.** ToolRequest and ToolAttempt lifecycles, the
   trusted risk registry, approval consumption, and a first harmless hub-local
   tool. Blocked by reserved ADR-0011 through ADR-0014.
5. **The restricted executor.** The
   [execution isolation target](#execution-isolation-target) applied to a first
   restricted placement. Blocked by the sandbox-minimum and execution-identity
   decisions above.
6. **Delegation and forking.** Fork frontier selection (the open
   selectable-boundary question), then delegation under reserved ADR-0002:
   delegated cause, child waits, targeted result delivery, cancellation
   propagation.
7. **Artifacts.** Identity, digest, producer provenance, controlled byte
   storage, and transcript links.

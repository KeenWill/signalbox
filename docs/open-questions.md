# Open questions

This is the inventory of unresolved foundational questions. A "leaning" guides
exploration but is not a decision. Closing a question requires an entry in the
[decision log](decisions.md) or, at foundation weight, a foundation-level
accepted record. Accepted decisions are specified in the
[living specification](spec/README.md) and the decision log; scenario
identifiers refer to [scenarios.md](scenarios.md).

## Identity representation

- **Public URL identity representation.**
  [identity-and-commands](spec/identity-and-commands.md) closes generation,
  supply, minting authority, and baseline PostgreSQL encoding; the local
  [process protocol](spec/process-protocol.md) closes its version-one wire
  fields. Browser and other public URL forms remain open. (S01, S02, S04, S08,
  S10, S12, S24)
- **Semantic transcript-entry extensions and rendering.**
  [sessions-and-transcript](spec/sessions-and-transcript.md) fixes
  origin-accepted-input and failed-turn payloads plus their eligibility and
  terminal-failure commit boundaries, together with assistant text, logical
  tool-use and tool-result references, completed-turn markers, and their commit
  boundaries. Refusal, reconciliation, mismatch, accepted-risk, approval-event,
  and delegation variants remain open together with rich assistant content and
  provider/client rendering. The tool-result content extension is tracked under
  [Tool safety](#tool-safety). The steering payload and stop marker are fixed by
  the
  [steering and stop decision](decisions.md#2026-07-23--atomic-steering-consumption-and-proof-bearing-stop-requests).
  Imported semantic history is owned separately by
  [conversation-import](spec/conversation-import.md). Blocks only those later
  native semantic-history slices. (S02–S04, S08, S09, S17)
- **Selectable native transcript-frontier boundaries.** Which terminal native
  semantic boundaries a client may select as a `TranscriptFrontier` remains
  open; imported-frontier selection is already owned by
  [conversation-import](spec/conversation-import.md#imported-frontier-points).
  Blocks native fork selection. (S17)

## Accepted-input content

- **Content extensions and rendering.**
  [sessions-and-transcript](spec/sessions-and-transcript.md) fixes the initial
  text-only `UserContent` value, exact equality, and PostgreSQL mapping. Rich
  content, attachments, other non-text variants, resource governance, and
  provider/client rendering remain open. Blocks those extensions, not the first
  `SubmitInput` slice. (S01, S03, S08)

## Model-input projection

- **Projection and summarization beyond the implemented role mappings.** The
  [M3 rendering decision](decisions.md#2026-07-22--render-the-initial-model-frontier-by-semantic-entry-role)
  and [model-call execution](spec/model-call-execution.md) own the implemented
  model-input projections; [conversation-import](spec/conversation-import.md)
  owns only normalized imported source content. Rich imported tool/result/media
  projection, semantic compaction, selective omission, summarization, rebasing,
  and context-window policy remain routed through the accepted frontier
  extension gate owned by
  [turn-lifecycle-and-scheduling](spec/turn-lifecycle-and-scheduling.md) and
  [sessions-and-transcript](spec/sessions-and-transcript.md). Blocks those
  extensions. (S02, S17, S28)

## Conversation import

- **Exact mappings for additional source formats.** Older backup formats have no
  converter. A later slice must select each source format's exact mapping and
  converter version, with synthetic fixtures and persistence round-trip
  coverage. The accepted format-versioned converter seam remains fixed. (S28)
- **Import discovery and operational surfaces beyond one file.** The owner
  terminal's explicit-format, one-file operation and its distinct inserted or
  already-imported receipt are implemented in
  [conversation-import](spec/conversation-import.md#operational-surface).
  Directory traversal, file watching, bulk-import policy, source-size admission
  beyond the inherited process-frame bound, raw-record access, and any
  authorization beyond the owner-private local socket remain undecided. (S28)

## Delegation

- **Parent cancellation propagation to active delegated children.** Leaning:
  explicit relationship policy with visible child outcomes. Blocks delegation.
  (S18, S19)
- **Detached delegated work in version one.** Leaning: exclude unless a core
  scenario proves need. Blocks delegation scope. (S18, S19)
- **Representation of child results in the parent conversation.** Leaning:
  structured durable reference plus explicit delivered content. Blocks
  delegation. (S18, S19)
- **Waits on delegated children and the progressing-turn slot.** The accepted
  turn lifecycle defers child waits to the delegation decision. Blocks
  delegation. (S18, S19)
- **Multi-source or merged transcript ancestry.** Accepted baseline is none or
  one immutable source frontier with an explicit extension boundary. Deferrable.
  (S17)

## Queue management

- **Editing, canceling, reordering, or changing delivery policy of queued
  input.** Excluded from the accepted input-delivery baseline; any addition
  needs explicit dispositions. Later scope. (S09)

## Turn lifecycle

- **Standalone active-turn cancellation.** Not a baseline feature: the accepted
  turn lifecycle defines cancellation authority only through applied interrupts,
  and adding a standalone command requires a future foundation decision with its
  own proof and disposition rules. Later scope. (S07)
- **Ambiguous provider-call recovery.** A restart-recovered unstopped in-flight
  call parks its turn in the awaiting-recovery wait
  ([model-call-execution](spec/model-call-execution.md)) with no resolving
  writer yet. The retired design analysis identified adopting a provider
  request-status API — with its polling posture and evidence classes — as the
  resolution path; the full analysis is in git history. Later scope. (S02)
- **Direct interrupt-only reconciliation from a running attempt.**
  [turn-lifecycle-and-scheduling](spec/turn-lifecycle-and-scheduling.md) adds
  direct reconciliation only for fatal mismatch at a closed aggregate boundary;
  whether an interrupt-only path may bypass `StopRequested` remains undecided.
  Later scope. (S07)

## Session organization, visibility, and retention

- **Creation-attributed default visibility.** The implemented visibility and
  attribution limits are owned by
  [sessions-and-transcript](spec/sessions-and-transcript.md#session-metadata-and-list-projection).
  Decide derivation, override shape and authority, and monitor inclusion
  together with the attributed-creation implementation.
- **Expressive metadata filters.** The implemented filter grammar is owned by
  [sessions-and-transcript](spec/sessions-and-transcript.md#session-metadata-and-list-projection).
  Whether to add OR, negation, attribute predicates, case folding, or a general
  query language remains open.
- **Imported-conversation archive semantics.** Ordinary session archive and
  immutable imported-source behavior are owned by
  [sessions-and-transcript](spec/sessions-and-transcript.md#session-metadata-and-list-projection)
  and [conversation-import](spec/conversation-import.md). Whether imported
  conversation records have a distinct non-destructive archive state, and how
  that state affects discovery, remains undecided.
- **Destructive retention or purge beyond ordinary archive.** Kept separate from
  ordinary archive; exact policy undefined. Later scope. (S17, S25)

## Regeneration

- **Regeneration command acceptance, queue placement, source frontier, and
  relation representation.** The identity rule is accepted (always new logical
  work; never reopen the original); the rest blocks the regeneration feature.
  (S26)

## Configuration categories

- **Additional effective-configuration categories.** System prompts, prompt
  templates, custom parameters, instructions, tool enablement/configuration,
  placement constraints, per-turn resources, and interpreting-policy selections
  are unavailable baseline capabilities; a future subsystem decision must extend
  the request, session-default, override, and effective-value algebras together
  ([configuration-and-credentials](spec/configuration-and-credentials.md)).
  Blocks those capabilities. (S02, S05, S13–S16)

## Model fallback and provenance

- **Whether version one supports automatic fallback.** Leaning: none until an
  explicit policy is justified. Deferrable for the first provider slice. (S22,
  S23)
- **Which failure classes permit fallback, if it exists.** Leaning: narrow
  allowlist of classified availability failures; refusal alone never qualifies.
  Blocks fallback. (S22, S23)
- **Fallback configuration and visibility.** Requires explicit session/turn
  policy, per-call provenance, and clear UI; no constructible fallback
  configuration exists in the baseline. Blocks fallback. (S20, S22)
- **Model identifier normalization and detailed provenance representation.** The
  mismatch disposition itself is accepted
  ([model-call-execution](spec/model-call-execution.md)). Blocks the provider
  provenance schema. (S20–S23)
- **Future known-provider-failure retry.** Version one never automatically
  retries a known or ambiguous provider failure; any later retry command or
  policy, including backoff and resource limits, is a separate decision the
  accepted no-retry policy leaves open. Blocks retry features. (S02, S04, S22)

## Scheduling and runners

Dispatch fencing and initial scheduler mechanics are decided, specified in
[turn-lifecycle-and-scheduling](spec/turn-lifecycle-and-scheduling.md); the
runner identity, catalog, lease, placement, credential-grant, and workspace
domain laws are specified in
[runner protocol and placement](spec/runner-protocol.md). The questions below
remain open.

- **Runner loss recovery beyond explicit replacement.** The domain foundation
  makes loss observable, forbids automatic migration, and emits complete
  replacement-change facts. The owner command, application transaction, injected
  semantic-message shape, client presentation, and recovery options when no
  eligible replacement exists remain undecided. Blocks executable replacement
  and frontier extension. (S30, S32)
- **Lease and affinity orchestration.** One session is pinned to one runner
  after first execution, while daemon-local tools remain separately admissible.
  Scheduler ordering between attachment, credential authorization, workspace
  provisioning, lease creation, runner loss, and explicit replacement remains
  undecided. Blocks runner dispatch. (S16, S30–S32)
- **MCP placement.** A future daemon-side MCP client may centralize shared
  servers and a future runner-side host may execute sandbox-local servers. Exact
  catalog declaration, lifecycle, credential, and compatibility semantics are
  deferred to the MCP pass; no MCP locus exists today. Blocks MCP tools.
- **Runner reconnect and result orchestration.** The domain fence binds lease,
  runner, physical tool attempt, and dispatch generation. Exact reconnect
  inventory, duplicate/stale acknowledgement handling, subscriber observation,
  and retention of rejected evidence remain undecided with the wire and store
  stack. Blocks runner result delivery. (S05, S06, S12, S16, S31)

## Tool safety

- **Future tool-attempt retry.** Version one never automatically creates another
  physical attempt after a prepared, known-failed, crash-lost, or ambiguous tool
  attempt. The runner foundation separately emits typed retry authority after
  lease loss: loss before claim retains the never-executed attempt, while
  claimed pure or idempotent loss requires a fresh physical attempt identity.
  Application creation and durable linkage of that successor attempt still need
  effect/evidence eligibility, duplicate-risk, idempotency-key, resource-limit,
  and audit decisions. Blocks executable runner retry. (S05, S06, S31)
- **Ambiguous tool-wait resolution.** Version one can preserve the exact
  external-effect attempt and terminalize through a proof-bearing interrupt. Who
  may record resolving evidence, how an exact accepted-risk continuation is
  represented, and which effects permit it remain undecided. Blocks
  reconciliation and continuation from `AwaitingToolRecovery`. (S06)
- **Durable tool-definition revisioning.** The implemented compiled catalog is
  immutable for one process lifetime. A dynamic catalog or a deployment that
  changes a definition while requests are outstanding must first decide how the
  advertised schema, permission default, effect class, validator, and executor
  revision are pinned and compared. Blocks runtime catalog mutation and safe
  rebinding across outstanding requests.
- **Runner-catalog file and reload lifecycle.** The runner domain validates an
  owner catalog independently of representation. TOML schema, configuration
  path, startup/reload behavior, revision identity, change audit, and safe
  rebinding of active registrations remain undecided. Blocks configured runner
  catalogs.
- **Execution-strategy configuration placement.** Version one serializes tool
  attempts without exposing a knob. Whether a later serial/concurrent choice is
  a deployment, session-default, per-turn, or executor-selection value remains
  undecided. Blocks configurable/concurrent execution, not the serial loop.
- **Model-declared approval expiry.** Pending owner approval currently waits
  indefinitely. Whether a model may request an expiry, how it is frozen, and
  what durable resolution expiry creates remain undecided.
- **LLM-judge approval mechanics.** `JudgeRecommendation` is typed but has no
  producer or storage. Prompt storage, provenance/session tagging, and the
  boundary between recommendation and policy remain undecided; a judge can never
  claim owner agency (INV-020).
- **Per-tool session overrides and high-risk guardrails.** The accepted policy
  ladder reserves exact per-tool overrides between the dangerous blanket and
  registry defaults, but override storage, replacement/equality semantics, and
  the list of operations the blanket must never bypass remain undecided.
- **Rich result-content variants.** Attempt content is text-only. Image and
  file/artifact arms, their resource governance, and provider/client rendering
  remain undecided.
- **Initial sandboxing requirements.** Leaning: explicit ambient and restricted
  profiles only to the strength justified by effective evidence. Blocks runner
  release. (S13, S14)
- **Ambient-user runner behavior.** Leaning: explicit selection and visible
  boundary, likely stricter policy for material effects. Blocks the ambient
  runner. (S13)
- **Workspace provisioning and cleanup recovery.** The domain fixes
  worktree-per-session capability, runner ownership, and cross-runner
  noninheritance. Repository acquisition, worktree naming, filesystem
  containment, cleanup timing, crash recovery, and leaked-workspace reporting
  remain undecided. Blocks executable workspace provisioning. (S32)

## Identity, credentials, and resource governance

Provider and integration credential lifecycle (storage, delivery, and rotation)
is decided, specified in
[configuration-and-credentials](spec/configuration-and-credentials.md); the
questions below remain open.

- **Owner client authentication and revocation.** Keep the hub's authorization
  model single-owner while choosing a remotely safe authentication boundary.
  Blocks any remote client. (S01, S10, S24, S25)
- **Runner authentication exchange, rotation, and recovery.** Enrollment,
  runner, and authentication-reference identities plus terminal enrollment
  revocation are fixed by
  [runner protocol and placement](spec/runner-protocol.md). Credential format,
  bootstrap delivery, proof exchange, rotation overlap, compromise recovery,
  channel binding, and authentication failure audit remain undecided. Blocks
  remote runners. (S05, S06, S12–S16, S30–S32)
- **Credential-scoped runner classes.** Credential profiles are selected only
  after targeting a runner that advertised them. Whether a capability-class
  selector may itself require a profile, and how availability changes affect
  class membership, remains undecided. Blocks profile-aware dynamic runner
  pools. (S30, S32)
- **Runner result credential egress.** Runner credential control fields carry
  profile names and policy, never values. Arbitrary runner tool result and error
  text can nevertheless echo machine-local data. Whether the runner, daemon, or
  tool contract applies redaction or stronger egress controls remains undecided.
  Blocks a general no-credential-disclosure claim for runner output.
- **In-memory credential hygiene.** Zeroization or equivalent handling for the
  request-scoped value read by `FileCredentialAccess` remains undecided, with no
  implementation. This question is separate from the accepted storage and
  delivery semantics but applies to the current file-backed credential path.
- **Controlled provider proxy and private trust roots.** Whether and how a
  deployment may select an explicit outbound provider proxy or private
  certificate authority remains undecided. The implemented adapters expose
  neither capability and disable ambient proxy discovery. Blocks only
  deployments requiring that transport extension.
- **First-release resource limits.** Leaning: explicit bounded concurrency and
  configurable usage limits at effect boundaries. Blocks public release.
  (S02–S06, S13–S18)

## Actor attribution

- **Actor-admissibility follow-ups.** See the authoritative routing and open
  edges in [identity-and-commands](spec/identity-and-commands.md).

## Telemetry correlation

- **Durable-command telemetry token.** Telemetry deliberately omits
  caller-supplied `DurableCommandId` values today
  ([identity-and-commands](spec/identity-and-commands.md)). The retired `dc1`
  design — a versioned, domain-separated, truncated HMAC-SHA-256 token under a
  deployment-owned key epoch, so caller-chosen identifiers stay non-enumerable
  while correlation survives restart and rotation is an explicit epoch change —
  is unimplemented and carries no current authority; git history holds the full
  retired record, and recommissioning it is a fresh foundation decision. Blocks
  per-command telemetry correlation.

## Protocols and persistence

- **Authenticated transports and remote clients.** The local baseline is owned
  by [process-protocol](spec/process-protocol.md). Remote access still requires
  decisions for client identity, authentication, authorization, revocation, and
  credential delivery. (S01, S24)
- **Browser transport.** Technology remains open and blocks the web client;
  snapshot and durable-update semantics are defined by
  [process-protocol](spec/process-protocol.md), while transient model-update
  streaming remains open below. (S02, S24)
- **Runner transport and durable store.** The accepted boundary requires one
  runner-initiated held outbound streaming connection, no runner inbound
  listener, and durable registration, lease, claim, and result authority
  independent of the channel. Exact transport technology, framing,
  authentication binding, compatibility version, backpressure, heartbeat, store
  schema, transactions, reconnect snapshot, and stale-evidence retention remain
  undecided. Blocks the runner binary and remote dispatch. (S12, S16, S30–S32)
- **Compatibility beyond the retained process-protocol versions.** Versions one
  through four have their owning [specification](spec/process-protocol.md). A
  future compatibility window, negotiation scheme, and generated-client policy
  remain undecided. (S01, S24)
- **Transient model-update relay.** Whether provider token deltas cross the
  process boundary, and the required draft identity, sequencing, replacement,
  backpressure, and redaction rules, remain undecided. The implemented durable
  transition relay is owned by [process-protocol](spec/process-protocol.md).
  Blocks live-token display. (S02, S24)
- **Process-protocol operation expansion.** Defaults replacement, delivery
  treatments other than `StartWhenNoActiveTurn`, cancellation, approval, tools,
  and administrative operations need their owning product slices and exact wire
  projections. Blocks only those operations. (S01–S10)
- **Persistence implementation within the accepted relational baseline.**
  [persistence-protocol](spec/persistence-protocol.md) closes the broad
  stable-storage question, selects the driver, pool, migration, runtime, and
  ephemeral-test stack, fixes the domain-owned complete-projection boundary for
  reconstructing opaque values, and closes atomic client-visible update-event
  append with commit-ordered cursors;
  [identity-and-commands](spec/identity-and-commands.md) closes canonical
  command payload/result storage and equality;
  [sessions-and-transcript](spec/sessions-and-transcript.md) fixes the complete
  current-session projection and load-by-identity semantics; and
  [turn-lifecycle-and-scheduling](spec/turn-lifecycle-and-scheduling.md) fixes
  evidence-bearing active-turn reconstitution with session-scoped acceptance
  tails. Streaming checkpoints, dispatch-generation placement, archival form,
  and exact cancellation-delivery records remain open. Those remaining questions
  block only their corresponding adapter slices; the generic scaffold and first
  typed command family are not blocked. (S03, S04, S17, S25, S27)
- **Update-event retention, pruning, and multiple hub processes.** Version one
  is owned by [process-protocol](spec/process-protocol.md). A pruning watermark,
  follower retention guarantees, and any later multiple-hub shared-fan-out
  mechanism remain undecided. Blocks pruning and multi-hub deployment. (S24)
- **Swift client type generation.** Leaning: generated boundary types mapped to
  hand-written client domain types. Deferrable until the Swift client. (S01,
  S24)

## Client scope

- **Client forms after the terminal baseline.** The selected baseline is owned
  by [process-protocol](spec/process-protocol.md). Whether a later daily client
  is a TUI, web app, or native app remains unselected. (S01, S02, S10, S24)
- **Apple client code organization.** Defer until the protocol and the first
  native slice are known. (S01, S24)
- **Web client technology (Rust/Wasm or TypeScript).** No leaning until the
  browser protocol and product slice are measured. (S01, S02, S24)
- **Client approval presentation.** How pending tool-approval prompts are
  surfaced and owner decisions are collected across the terminal baseline and
  later client forms remains undesigned. (S10, S11, S24)

## General-purpose artifacts

Artifact identity, ownership, lifecycle, content addressing, and retention have
no accepted aggregate boundary. The reference-not-copy posture review workflows
take today is owned by [review-workflows](spec/review-workflows.md). A future
foundation decision must define the artifact aggregate and its authority before
a workflow can attach one. This blocks general-purpose workflow artifacts, not
the implemented session and external-link evidence.

## Destination features (target model)

These unresolved foundation requirements are authoritative here. The
[target model](target-model.md) is non-normative direction for their destination
and ordering.

- **Goal identity and lifecycle.** Durable persistent-objective identity and
  lifecycle require a future foundation decision. Blocks platform goal mode.
- **Standing update-subscription lifecycle.** Identity, lifetime, delivery, and
  cancellation for standing update subscriptions require a future foundation
  decision. Blocks the planned callback surface.
- **Review-workflow orchestration.** The
  [review-workflow foundation](spec/review-workflows.md) fixes the target, run,
  pass, finding, external-link, and store contracts. Exact application commands,
  durable hold or atomic accepted-input admission and run/pass projection, pass
  scheduling, code-host/model/workspace adapter seams, workflow-facing protocol,
  prompts, automatic publication, repair, conflict escalation, and merge-based
  stack propagation remain to be designed and implemented above that foundation.
  Blocks only executable review workflows.
- **Independent session-link relationship.** Links between sessions that
  delegation did not create require their own foundation decision. Blocks
  session linking and visibility authority. (S18, S19)
- **Inter-session messaging actor extension.** Session-actor accepted input
  requires an actor-algebra extension
  ([identity-and-commands](spec/identity-and-commands.md)), explicit
  `SubmitInput` admissibility, and the open
  [identity, credentials, and resource governance](#identity-credentials-and-resource-governance)
  decisions. Blocks inter-session messaging. (S18, S19)

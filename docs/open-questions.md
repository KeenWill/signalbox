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
  tool-use references, completed-turn markers, and their final response commit
  boundary. Refusal, cancellation, reconciliation, mismatch, accepted-risk,
  steering, tool-result, approval, and delegation variants remain open together
  with rich assistant content and provider/client rendering. The M3 boundary and
  its reopening obligation are recorded in the
  [pending-steering fail-closed decision](decisions.md#2026-07-22--m3-pending-steering-fail-closed-boundary).
  Blocks only those later semantic-history slices. (S02–S04, S08, S09, S17)
- **Selectable transcript-frontier boundaries.** Which terminal semantic
  boundaries a client may select as a `TranscriptFrontier` remains open; the
  accepted frontier semantics decide only how a validated selection resolves
  into a new session's context. Blocks fork selection. (S17)

## Accepted-input content

- **Content extensions and rendering.**
  [sessions-and-transcript](spec/sessions-and-transcript.md) fixes the initial
  text-only `UserContent` value, exact equality, and PostgreSQL mapping. Rich
  content, attachments, other non-text variants, resource governance, and
  provider/client rendering remain open. Blocks those extensions, not the first
  `SubmitInput` slice. (S01, S03, S08)

## Model-input projection

- **Projection and summarization beyond the M3 role mapping.** The
  [M3 rendering decision](decisions.md#2026-07-22--render-the-initial-model-frontier-by-semantic-entry-role)
  fixes only the admitted text-entry role mapping and exact frontier order.
  Semantic compaction, selective omission, summarization, rebasing, and
  context-window policy remain routed through the accepted frontier extension
  gate and semantic-entry open-question routing — owned today by
  [turn-lifecycle-and-scheduling](spec/turn-lifecycle-and-scheduling.md) and
  [sessions-and-transcript](spec/sessions-and-transcript.md) — including their
  foundation-decision requirements. Blocks those extensions, not the admitted
  text-only M3 rendering. (S02, S17)

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
- **Ambiguous provider-call recovery.** A restart-recovered in-flight call parks
  its turn in the awaiting-recovery wait
  ([model-call-execution](spec/model-call-execution.md)) with no resolving
  writer yet. The retired design analysis identified adopting a provider
  request-status API — with its polling posture and evidence classes — as the
  resolution path; the full analysis is in git history. Later scope. (S02)
- **Direct interrupt-only reconciliation from a running attempt.**
  [turn-lifecycle-and-scheduling](spec/turn-lifecycle-and-scheduling.md) adds
  direct reconciliation only for fatal mismatch at a closed aggregate boundary;
  whether an interrupt-only path may bypass `StopRequested` remains undecided.
  Later scope. (S07)

## Archival and retention

- **Archive eligibility, nonterminal work handling, and restore target.**
  Leaning: preserve identity and history; never silently abandon work. Blocks
  archive/restore. (S25)
- **Archive effect on delegated children and related sessions.** Leaning:
  explicit typed policy with visible outcomes; no implicit cascade or
  independence rule selected. Blocks archiving related sessions. (S18, S19, S25)
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

## Provider call security

- **Outbound TLS posture for provider calls.** Certificate-verification
  requirements, trust roots, TLS-version floor, and any proxy allowance for the
  hub's outbound provider connections are undecided. Blocks the first outbound
  provider adapter. (S02, S04, S20–S23)
- **Provider response-size limits.** Whether and where the hub bounds provider
  response bodies and streamed deltas before they reach parsing and storage.
  Blocks the first outbound provider adapter. (S02, S04, S24)
- **Provider call timeout budgets.** See the authoritative open edge in
  [model-call-execution](spec/model-call-execution.md).
- **Provider-response parsing hardening.** Parsing limits and rejection behavior
  for provider responses under the malicious-model-output threat model. Blocks
  the first outbound provider adapter. (S02, S04, S23)

## Scheduling and runners

Dispatch fencing and initial scheduler mechanics are decided, specified in
[turn-lifecycle-and-scheduling](spec/turn-lifecycle-and-scheduling.md); the
questions below remain open.

- **Runner capability, evidence, and placement model.** Leaning: typed core
  properties with explicit evidence levels; effective guarantees never stronger
  than supporting evidence. Blocks the runner protocol. (S05–S16)
- **Runner pinning and workspace affinity.** Leaning: explicit session/turn
  pinning where locality matters, with observable failure. Blocks workspace
  tools. (S05–S16)
- **Multiple runners in one turn.** Leaning: at most one selected runner
  initially, counting hub-local tools separately. Constrains version one.
  (S13–S16)

## Tool safety

- **Tool-risk classification.** Needs an argument-aware effect taxonomy with a
  conservative unknown class before tool execution. Blocks tool execution. (S05,
  S06, S10, S11, S15, S16)
- **Which operations require confirmation.** Leaning: hub risk policy
  considering arguments, placement, and prior scoped grants. Blocks tool
  execution. (S10, S11, S13–S16)
- **LLM-judge influence on approval policy.** Leaning: advisory or bounded
  policy signal only, never human approval identity. Deferrable. (S10, S11)
- **Retry policy for side-effecting commands and tools.** Leaning: classify
  effect and evidence; never auto-retry ambiguous writes. Blocks tool retry.
  (S05, S06, S12)
- **Initial sandboxing requirements.** Leaning: explicit ambient and restricted
  profiles only to the strength justified by effective evidence. Blocks runner
  release. (S13, S14)
- **Ambient-user runner behavior.** Leaning: explicit selection and visible
  boundary, likely stricter policy for material effects. Blocks the ambient
  runner. (S13)

## Identity, credentials, and resource governance

Provider and integration credential lifecycle (storage, delivery, and rotation)
is decided, specified in
[configuration-and-credentials](spec/configuration-and-credentials.md); the
questions below remain open.

- **Owner client authentication and revocation.** Keep the hub's authorization
  model single-owner while choosing a remotely safe authentication boundary.
  Blocks any remote client. (S01, S10, S24, S25)
- **Runner enrollment, authentication, and revocation.** Strong runner identity
  distinct from capability claims, with rotation. Blocks remote runners. (S05,
  S06, S12–S16)
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
  snapshot and transient-stream semantics are defined by
  [process-protocol](spec/process-protocol.md). (S02, S24)
- **Compatibility after exact process-protocol version one.** Version one has
  its owning [specification](spec/process-protocol.md). A future compatibility
  window, negotiation scheme, and generated-client policy remain undecided.
  Blocks a version-two protocol. (S01, S24)
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
  and exact cancellation-delivery records remain open. The
  [first physical frontier-layout choice](decisions.md#2026-07-17--materialize-complete-membership-for-first-context-frontier-storage)
  materializes complete ordered membership while preserving the accepted
  frontier semantics' freedom for a later semantics-preserving migration. Those
  remaining questions block only their corresponding adapter slices; the generic
  scaffold and first typed command family are not blocked. (S03, S04, S17, S25,
  S27)
- **Submit-path scaling: scheduling projection and frontier storage.** The
  [first frontier layout](decisions.md#2026-07-17--materialize-complete-membership-for-first-context-frontier-storage)
  materializes complete membership per snapshot and the submit path loads the
  complete scheduling projection, content included per submission, inside the
  session lock, degrading at hundreds of turns per session. A completeness
  representation that bounds scheduling reads, plus a prefix-sharing or delta
  layout the accepted frontier semantics permit, remains concretely undesigned.
  The
  [decision log](decisions.md#2026-07-20--adversarial-audit-corrective-package)
  owns its accepted scheduling disposition. (S03, S04, S17)
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

## Destination features (target model)

These unresolved foundation requirements are authoritative here. The
[target model](target-model.md) is non-normative direction for their destination
and ordering.

- **Goal identity and lifecycle.** Durable persistent-objective identity and
  lifecycle require a future foundation decision. Blocks platform goal mode.
- **Standing update-subscription lifecycle.** Identity, lifetime, delivery, and
  cancellation for standing update subscriptions require a future foundation
  decision. Blocks the planned callback surface.
- **Independent session-link relationship.** Links between sessions that
  delegation did not create require their own foundation decision. Blocks
  session linking and visibility authority. (S18, S19)
- **Inter-session messaging actor extension.** Session-actor accepted input
  requires an actor-algebra extension
  ([identity-and-commands](spec/identity-and-commands.md)), explicit
  `SubmitInput` admissibility, and the open
  [identity, credentials, and resource governance](#identity-credentials-and-resource-governance)
  decisions. Blocks inter-session messaging. (S18, S19)

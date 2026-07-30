# Open questions

This is the inventory of unresolved foundational questions. A "leaning" guides
exploration but is not a decision. Closing a question requires an owner-accepted
pull request or, at foundation weight, a foundation specification diff. Accepted
cross-component and wire contracts live in the
[living specification](spec/README.md); scenario identifiers refer to
[scenarios.md](scenarios.md).

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
  the steering and stop decision. Imported semantic history is owned separately
  by [conversation-import](spec/conversation-import.md). Blocks only those later
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

- **Projection and summarization beyond the implemented role mappings.**
  [Model-call execution](spec/model-call-execution.md) owns the implemented
  model-input projections; [conversation-import](spec/conversation-import.md)
  owns only normalized imported source content. Rich imported tool/result/media
  projection, selective omission beyond the fixed compaction projection,
  alternative summaries, and rebasing remain routed through the accepted
  frontier extension gate owned by
  [turn-lifecycle-and-scheduling](spec/turn-lifecycle-and-scheduling.md) and
  [sessions-and-transcript](spec/sessions-and-transcript.md). Blocks those
  extensions. (S02, S17, S28)

## Conversation import

- **Exact mappings for additional source formats.** Older backup formats have no
  converter. A later slice must select each source format's exact mapping and
  converter version, with synthetic fixtures and persistence round-trip
  coverage. The accepted format-versioned converter seam remains fixed. (S28)
- **Import operational surfaces beyond explicit file and directory scans.** The
  owner terminal's explicit-format, one-file and recursive directory-scan
  operations are implemented in
  [conversation-import](spec/conversation-import.md#operational-surface), and
  the single-conversation inspection read is implemented in
  [conversation-import](spec/conversation-import.md#imported-conversation-inspection).
  File watching, source-size admission beyond the inherited process-frame bound,
  raw-record access, and any authorization beyond the owner-private local socket
  remain undecided. Listing across imported conversations is implemented by the
  unified conversation listing in
  [process protocol](spec/process-protocol.md#client-requests); filesystem
  discovery of unimported sources beyond the explicit directory scan is not.
  (S28)

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
  ([model-call-execution](spec/model-call-execution.md)). An owner decision now
  releases the slot by terminalizing the turn over that exact ambiguity
  ([process-protocol](spec/process-protocol.md)), but nothing resolves what the
  provider actually did. The retired design analysis identified adopting a
  provider request-status API — with its polling posture and evidence classes —
  as the resolution path; the full analysis is in git history. Later scope.
  (S02)
- **Direct interrupt-only reconciliation from a running attempt.**
  [turn-lifecycle-and-scheduling](spec/turn-lifecycle-and-scheduling.md) adds
  direct reconciliation only for fatal mismatch at a closed aggregate boundary;
  whether an interrupt-only path may bypass `StopRequested` remains undecided.
  Later scope. (S07)

### Automatic context compaction

This is a blocking condition rather than an open design question. Automatic
context compaction ships with a known defect on its primary path, accepted on
the grounds that the code sits unused until something depends on it. That ground
disappears the moment anything relies on it, so the condition is recorded here
rather than only in the review thread that raised it.

**The defect.** The compaction request wraps accumulated plain-text history in
JSON with provenance metadata and reserves the same `max_output_tokens` as the
ordinary call, and is never counted against `context_window_tokens`. It can
therefore be *larger* than the input that already overflowed the window. The
provider may reject the summary call for context overflow; that call is then
terminalized, and the per-turn automatic marker prevents a second attempt.

**The consequence.** A session that crosses its context window has its queued
turn stalled with its single automatic attempt consumed and no path forward
inside the running daemon — which is the exact situation automatic compaction
exists to rescue. Nothing durable is corrupted, no summary boundary is written
wrong, and no transcript entries are lost: the failed call is recorded as
legitimate terminal non-Completed evidence. The session is stalled, not damaged.

**The trigger is the common case, not an edge of it.** Compaction is invoked
precisely when history is large. History large enough that wrapping it in JSON
with metadata overflows the window is the middle of that condition rather than
its boundary.

**The condition.** Automatic context compaction must not be relied on until the
summary call is guaranteed to fit. Anything built on top of it, and any workflow
that assumes a long-running session will rescue itself, is blocked on that fix
rather than merely improved by it. Explicit compaction is unaffected by this
particular defect.

**Shape of the fix.** Count the summary request against `context_window_tokens`
before triggering it, or select a compaction strategy guaranteed to fit — for
example bounding the history actually wrapped rather than reserving the full
`max_output_tokens` on top of unbounded input. Scheduled as a follow-up pull
request against a quiet `main` rather than inside the compaction stack.

Raised as a review finding and dispositioned with this condition attached:
https://github.com/KeenWill/signalbox/pull/314#discussion_r3670652441

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

- **Additional effective-configuration categories.** Prompt composition and
  custom parameters, instructions, tool enablement/configuration, placement
  constraints, per-turn resources, and interpreting-policy selections remain
  unavailable; a future subsystem decision must extend the request,
  session-default, override, and effective-value algebras together
  ([configuration-and-credentials](spec/configuration-and-credentials.md)).
  Static copy-on-create session templates compose only the already-implemented
  model selection, bounded system prompt, and dangerous-tool blanket; every
  richer composition or configuration category stays blocked here. (S02, S05,
  S13–S16, S34, S35)

## Template storage and authoring

- **Durable objects, protocol CRUD, and agent authoring tools.** Static
  startup-file loading, daemon-side create-by-name resolution, and read-only
  name/version listing are fixed by
  [configuration and credentials](spec/configuration-and-credentials.md) and
  [process protocol](spec/process-protocol.md). Whether templates become durable
  database objects, the exact protocol CRUD and concurrency contract, and agent
  tools that read or edit templates remain undecided. Blocks only those storage
  and authoring surfaces. (S35)

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
- **Detailed provider provenance representation.** Model identifier
  normalization is decided: the
  [provider-target identity rule](spec/model-call-execution.md#provider-target-identity)
  accepts an alias resolved to its own dated snapshot as the same target and
  keeps a different lineage as a distinct substitution outcome. The mismatch
  disposition itself is likewise accepted
  ([model-call-execution](spec/model-call-execution.md)). What remains open is
  the durable per-call provenance schema that would record the concrete served
  identity and a substitution as evidence rather than as operator diagnostics
  and a fail-closed error. Blocks the provider provenance schema. (S20–S23)
- **Future known-provider-failure retry.** Version one never automatically
  retries a known or ambiguous provider failure; any later retry command or
  policy, including backoff and resource limits, is a separate decision the
  accepted no-retry policy leaves open. Blocks retry features. (S02, S04, S22)

## Scheduling and runners

Dispatch fencing, initial scheduler mechanics, and the complete version-one
local runner orchestration are specified in
[turn-lifecycle-and-scheduling](spec/turn-lifecycle-and-scheduling.md) and
[runner protocol and placement](spec/runner-protocol.md). The loss, replacement,
cleanup, contract-gap, and session-composition questions this section previously
carried are decided, and each decision is stated by the contract page that owns
it: staged replacement ordering and the runner-recovery turn phase in
[turn-lifecycle-and-scheduling](spec/turn-lifecycle-and-scheduling.md#runner-loss-session-recovery);
same-runner recovery after a registration-triggered loss, deployment-scoped
successor promotion, non-transferable workspace cleanup, pinned canonical digest
bytes, runner-to-daemon failure frames, workspace-release acknowledgement,
forced Git transport configuration, and the independent
[session-composition axes](spec/runner-protocol.md#session-composition) in
[runner protocol and placement](spec/runner-protocol.md); the runner-recovery
phase, the placement transcript payload, creation-record placement, and the
runner event family in
[persistence-protocol](spec/persistence-protocol.md#relational-representation);
the closed runner execution object, creation-request placement, and template
creation carrying placement in
[process-protocol](spec/process-protocol.md#client-requests); the relocation
transcript boundary in
[sessions-and-transcript](spec/sessions-and-transcript.md#semantic-transcript-entries);
capability-derived tool advertisement in
[model-call-execution](spec/model-call-execution.md#frontier-rendering); and Git
subprocess deadlines, full-ref branch validation, unborn-HEAD clones, and honest
output truncation in
[tool-loop](spec/tool-loop.md#version-one-workstation-tool-contracts). Why: a
decided question is a contract, and a contract binds only where the implementer
of that contract reads it; a decision restated on this page would be a second
authority over prose that already owns it, free to drift from the page it
paraphrases. Multiple simultaneously enrolled runners and owner-directed
relocation of a healthy session are committed functionality that version one
defers rather than open questions
([runner protocol and placement](spec/runner-protocol.md#the-singleton-runner-rule-is-temporary)).
The questions below remain open.

- **Workspace portability between runners.** Moving a session that owns a
  workspace to another runner requires that workspace to exist, or to be
  reconstructible, on the destination. Version one never carries a workspace
  across a placement change: a replaced runner's clone is leaked and the
  successor provisions its own. That is sufficient for a repository-backed
  session whose work is already pushed and insufficient for uncommitted state,
  and it is the missing prerequisite for interchangeable runners on a hosted
  backend rather than persistent workstations. Decide what a portable workspace
  is — reprovisioning from durable facts, an explicit transfer, or a shared
  volume the destination binds — before any automated placement across a runner
  family exists. Not a blocker: owner-directed moves of a workspace-free
  session, and of a session whose work is pushed, require none of it. (S16,
  S30–S32)
- **Automatic scheduling, load balancing, and MCP placement.** Placement selects
  a runner by exact identity or capability class and is never rescheduled; no
  policy chooses among several satisfying runners, balances load, or admits an
  MCP locus. Deciding those requires multiple simultaneously enrolled runners
  plus a stated selection policy and its observability, and it composes with the
  workspace portability question above. Blocks automatic placement, not manual
  placement. (S16, S30–S32)

## Tool safety

### Review-slog toolkit adoption

This is a blocking condition rather than an open design question. The
review-slog toolkit ships with a known race in its merge gate, accepted on the
grounds that the toolkit is not yet load-bearing. That ground disappears the
moment it is adopted, so the condition is recorded here rather than only in the
review thread that raised it.

**The window.** `review_gate_transaction` reads stack state, thread inventory,
convergence state, stack state again, and convergence state again, then requires
the two stack reads to be equal and the two convergence reads to be equal before
composing the gate. The stack pair brackets the interval between the first and
second stack reads; the convergence pair brackets the interval between the first
and second convergence reads. Neither pair brackets the interval between the
final stack read and the final convergence read. A stack-only change inside that
interval — the immediate base advancing, or a child change request being opened
or force-pushed — leaves both stack reads equal, because both were taken before
it, and leaves both convergence reads equal, because convergence evidence
carries no ancestry facts. The equality check passes and the gate composes its
verdict from a stack snapshot that is already stale.

**What becomes silently missable.** Every stack-derived blocker:
`parent_needs_merge_forward`, `base_chain_missing_main`,
`child_needs_merge_forward`, and `evidence_truncated` where it derives from a
truncated child page. The gate reports `ready: true` with no blocker recorded
and nothing in the result marking the stack evidence as stale, so a reader of
the output cannot detect the condition. Convergence-derived blockers —
unresolved, undispositioned and buried threads, continuous-integration state,
mergeability, and reviewer verdict status — are not affected, because the gate
is composed from the final convergence read, which is the freshest read in the
transaction.

**This is a sequencing argument, not a severity one.** A base advancing
concurrently with a gate check is normal in a merge train, not exotic; the race
is not rare. What makes it acceptable to ship is that merges are gated by the
standalone convergence checker, not by this tool, so a stale verdict cannot
currently affect a real merge decision.

**The condition.** The review gate must not be used to gate any merge decision
until the stale-stack-read window is closed. Adoption is blocked on the fix; the
fix does not follow adoption.

**Shape of the fix.** Minimally, a third stack read after the final convergence
read, folded into the equality check: this closes the window and leaves only the
post-transaction interval, which no read ordering can close, since the base may
always advance after the last read. Preferably, a stable read loop that repeats
the stack and convergence reads until two consecutive complete snapshots agree.
Both are small changes and either is cheap relative to trusting the tool with a
merge decision.

Raised as a review finding and dispositioned with this condition attached:
https://github.com/KeenWill/signalbox/pull/306#discussion_r3669682038

- **Future tool-attempt retry.** General automatic retry, accepted-risk retry
  after ambiguity, idempotency-key policy, duplicate-risk controls, and retry
  resource limits beyond the sealed
  [runner lease-loss transitions](spec/runner-protocol.md#effect-classes-and-runner-leases)
  remain undecided. (S05, S06, S31)
- **Ambiguous tool-wait resolution.** Who may record resolving evidence, how an
  exact accepted-risk continuation is represented, and which effects permit it
  beyond the
  [proof-bearing terminal paths](spec/turn-lifecycle-and-scheduling.md#runner-loss-session-recovery)
  remain undecided. Blocks reconciliation and continuation from
  `AwaitingToolRecovery`. (S06)
- **Durable tool-definition revisioning.** The implemented compiled catalog is
  immutable for one process lifetime. A dynamic catalog or a deployment that
  changes a definition while requests are outstanding must first decide how the
  advertised schema, permission default, effect class, validator, and executor
  revision are pinned and compared. Blocks runtime catalog mutation and safe
  rebinding across outstanding requests.
- **Dynamic runner-catalog lifecycle.** Mutable behavior beyond the
  [compiled version-one catalog](spec/runner-protocol.md#advertised-catalogs-and-daemon-authority)
  requires representation, revision identity, change audit, compatibility, and
  safe rebinding decisions.
- **Execution-strategy configuration placement.** Whether a future
  serial/concurrent choice beyond the
  [fixed serial loop](spec/tool-loop.md#serialized-staged-execution) is a
  deployment, session-default, per-turn, or executor-selection value remains
  undecided. Blocks configurable/concurrent execution.
- **Model-declared approval expiry.** Pending owner approval currently waits
  indefinitely. Whether a model may request an expiry, how it is frozen, and
  what durable resolution expiry creates remain undecided.
- **LLM-judge approval mechanics.** `JudgeRecommendation` is typed but has no
  producer or storage. Prompt storage, provenance/session tagging, and the
  boundary between recommendation and policy remain undecided; a judge can never
  claim owner agency (INV-020).
- **Additional high-risk guardrails.** Operations that a future policy must
  never make automatic, richer values beyond the
  [fixed profile/override ladder](spec/runner-protocol.md#sandbox-profiles-and-approval),
  and dynamic replacement/equality semantics remain undecided.
- **Rich result-content variants.** Attempt content is text-only. Image and
  file/artifact arms, their resource governance, and provider/client rendering
  remain undecided.
- **Large durable payload architecture.** Tool evidence is bounded by storage
  policy rather than by physics: 1 MiB of result text, 1 MiB of arguments, 4,096
  bytes of error detail, and 4,096 bytes of exact runner value, all held in
  PostgreSQL `text` columns with no physical ceiling near those values. Version
  one derives its
  [process output caps](spec/tool-loop.md#version-one-workstation-tool-contracts)
  from exactly those bounds, so oversized executor output is truncated honestly
  before result admission rather than turning a working command into a failure;
  `ResultTooLarge` remains the admission classification for an admitted result
  that still exceeds the durable bound. Deliberately delivering larger payloads
  — files well past 1 MiB — needs its own design: where the bytes live, how a
  result references rather than embeds them, what the model and each client see,
  and the abuse and denial-of-service controls a larger bound requires. Recorded
  as a design question rather than a blocker; the truncating caps remain correct
  until it is answered.

## Identity, credentials, and resource governance

Provider and integration credential lifecycle (storage, delivery, and rotation)
is decided, specified in
[configuration-and-credentials](spec/configuration-and-credentials.md); the
questions below remain open.

- **Owner client authentication and revocation.** Keep the daemon's
  authorization model single-owner while choosing a remotely safe authentication
  boundary. Blocks any remote client. (S01, S10, S24, S25)
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
- **Runner result credential egress beyond exact-value redaction.** Whether
  stronger taint, isolation, or egress controls beyond the
  [runner credential boundary](spec/configuration-and-credentials.md#runner-credential-lifecycle)
  apply remains undecided. Blocks a general no-credential-disclosure claim for
  runner output.
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
- **Remote runner transport and reconnect.** The dedicated local socket,
  framing, heartbeat, reconnect inventory, and transaction orchestration are
  owned by [runner protocol and placement](spec/runner-protocol.md). Remote
  transport, authentication binding, compatibility negotiation, internet
  backpressure, and cross-host stale-evidence retention remain undecided. Blocks
  remote dispatch, not the local runner. (S12, S16, S30–S32)
- **Compatibility after the process-protocol freeze.** The single pre-deployment
  version and its freeze condition are owned by
  [process-protocol](spec/process-protocol.md). A future compatibility window,
  negotiation scheme, and generated-client policy remain undecided. (S01, S24)
- **Transient model-update relay.** Whether provider token deltas cross the
  process boundary, and the required draft identity, sequencing, replacement,
  backpressure, and redaction rules, remain undecided. The implemented durable
  transition relay is owned by [process-protocol](spec/process-protocol.md).
  Blocks live-token display. (S02, S24)
- **Process-protocol operation expansion.** The interrupt, canonical tool
  decision, next-safe-point steering, and after-current queue treatments now
  cross the wire ([process-protocol](spec/process-protocol.md)); administrative
  operations still need their owning product slices and exact wire projections.
  Blocks only those operations. (S01–S10)
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
- **Update-event retention, pruning, and multiple daemon processes.** Version
  one is owned by [process-protocol](spec/process-protocol.md). A pruning
  watermark, follower retention guarantees, and any later multiple-daemon
  shared-fan-out mechanism remain undecided. Blocks pruning and multi-daemon
  deployment. (S24)
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
- **Client approval presentation.** The terminal baseline now surfaces the
  pending request through the transcript's awaiting-turn and tool-use lines and
  collects decisions through `approve`/`deny`
  ([process-protocol](spec/process-protocol.md#terminal-client)); interactive
  prompting and later client forms remain undesigned. (S10, S11, S24)

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
  pass, finding, external-link, and store contracts. The caller-driven
  application commands, durable retry receipts, run/pass projection, and
  workflow-facing local process protocol are implemented. Automatic pass
  scheduling, durable hold or atomic accepted-input creation,
  code-host/model/workspace adapter seams, prompts, automatic publication,
  repair, conflict escalation, and merge-based stack propagation remain to be
  designed and implemented above that surface. Blocks automatic end-to-end
  review workflows.
- **Independent session-link relationship.** Links between sessions that
  delegation did not create require their own foundation decision. Blocks
  session linking and visibility authority. (S18, S19)
- **Inter-session messaging actor extension.** Session-actor accepted input
  requires an actor-algebra extension
  ([identity-and-commands](spec/identity-and-commands.md)), explicit
  `SubmitInput` admissibility, and the open
  [identity, credentials, and resource governance](#identity-credentials-and-resource-governance)
  decisions. Blocks inter-session messaging. (S18, S19)

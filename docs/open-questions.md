# Open questions

This is the inventory of unresolved foundational questions. A "leaning" guides
exploration but is not a decision. Closing a question requires an
maintainer-accepted pull request or, at foundation weight, a foundation
specification diff. Accepted cross-component and wire contracts live in the
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
  boundaries. Refusal, reconciliation, mismatch, accepted-risk, and
  approval-event variants remain open together with rich assistant content and
  provider/client rendering. The tool-result content extension is tracked under
  [Tool safety](#tool-safety). The steering payload and stop marker are fixed by
  the steering and stop decision. Imported semantic history is owned separately
  by [conversation-import](spec/conversation-import.md). Blocks only those later
  native semantic-history slices. (S02–S04, S08, S09, S17)
- **Selectable native transcript-frontier boundaries.** Which terminal native
  semantic boundaries a client may select as a `TranscriptFrontier` remains
  open; imported-frontier selection is already owned by
  [conversation-import](spec/conversation-import.md). Blocks native fork
  selection. (S17)

## Accepted-input content

- **Further content variants and rendering.** Ordered multipart content with
  content-addressed attachment parts, its replay equality, persistence, terminal
  rendering, and model-visible stubs are decided and specified by
  [blob storage](spec/blob-storage.md). Any non-text content variant beyond
  attachment parts and provider-native media rendering remain open. Blocks only
  those further extensions. (S01, S03, S08)

## Model-input projection

### Graded approval judging

Whether execution-approval judging should replace its direct recommendation with
separate risk and brief-alignment grades remains undecided. Any such change must
define the grade contract, trusted outcome derivation, remaining input evidence,
durable audit shape, graded wire and projection data, evaluation method, and
shadow-to-live promotion path in an owner-accepted specification and
implementing stack. Safety ceilings remain owned by
[Additional high-risk guardrails](#tool-safety), while parent-supplied task and
authority evidence remains owned by
[Turn-origin instructions in the approval-judge request](#tool-safety). The
interactive prompting and later client-form choices remain owned by
[Client approval presentation](#client-scope). The following related questions
also require owner rulings:

- **Corpus governance.** Approval corpora follow the digest contract owned by
  [evaluation system](spec/eval-system.md); their identity and admitted storage
  forms are defined by `CorpusManifest` in the approval-judge evaluation crate.
  Which admitted storage form this corpus uses remains undecided, together with
  access, redaction, retention, and deletion rules.

- **Promotion bounds.** The maximum false-allow rate, minimum acceptable
  improvement, minimum labeled case count, required slices, and statistical
  treatment for promotion from shadow to live graded authority remain open. A
  promotion comparison must define each metric's denominator and its treatment
  of parks, failed calls, and repeated trials.

- **Label semantics.** Whether an ordinary user allow or deny is the final
  quality label, or evaluation needs a separate “judge correct” ruling and an
  approval rationale, remains open. Execution rulings are observations rather
  than correctness labels until this is decided.

- **Unparked sampling.** Whether and how operators may provide post-hoc labels
  for automatically allowed or denied requests remains open. Without it, the
  recorded corpus is selected toward parked requests and cannot support
  whole-population promotion claims.

- **Shadow budget.** The graded shadow sampling fraction for production shadow
  traffic remains open. Provider-cost ceilings and concurrency remain owned by
  [First-release resource limits](#identity-credentials-and-resource-governance).
  Retention and deletion of observations admitted to the approval corpus remain
  owned by Corpus governance above.

- **Configuration actor audit.** If trusted outcome derivation introduces
  mutable threshold configuration, whether source-control and deployment audit
  are sufficient provenance for changes to it, or Signalbox needs an
  authenticated configuration-change command, remains open.

### Further projection and summarization

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

### Workspace instructions and skills

The accepted baseline in
[workspace instructions and skills](spec/workspace-instructions.md) owns greedy
candidate discovery, typed registration, session/template eligibility,
deliberate admission, projection rather than transcript append, and exact
per-turn provenance. The following extensions remain undecided:

- **Discovery invalidation and expansion.** Whether the daemon watches or
  explicitly rescans roots, which ignore language or depth bound applies,
  whether and how symbolic links may be followed, and which additional vendor
  instruction formats become candidates. See the owning
  [discovery contract](spec/workspace-instructions.md).
- **Runner-workspace discovery.** The accepted daemon-local refusal needs a
  placement-revision-correlated runner operation for greedy discovery, typed
  findings, and exact source reads before runner-provisioned workspaces can
  contribute candidates. Blocks workspace discovery for runner-backed sessions;
  configured daemon roots remain available.
- **Retrieval and automatic activation.** Search or deterministic ranking over
  eligible metadata, path-triggered admission, and any template-eager tier need
  exact recorded trigger and budget contracts. The baseline remains deliberate
  identity-addressed admission.
- **Skill resources and rendered-byte externalization.** Addressing and hashing
  files below a skill bundle, export policy for retained rendered plaintext, and
  whether a later migration moves version-one admission-row wrapper bytes to
  content-addressed blob storage remain open.
- **Whole-bundle unload.** Projection reserves removal at a later turn boundary,
  but unload authority, tombstone rendering, admitted-set history, and the
  model-facing operation remain foundation work. See the owning
  [projection contract](spec/workspace-instructions.md#planned).

## Conversation import

- **Exact mappings for additional source formats.** Older backup formats have no
  converter. A later slice must select each source format's exact mapping and
  converter version, with synthetic fixtures and persistence round-trip
  coverage. The accepted format-versioned converter seam remains fixed. (S28)
- **Import operational surfaces beyond explicit file and directory scans.** The
  user terminal's explicit-format, one-file and recursive directory-scan
  operations are implemented in
  [conversation-import](spec/conversation-import.md), and the
  single-conversation inspection read is implemented in
  [conversation-import](spec/conversation-import.md). File watching, source-size
  admission beyond the inherited process-frame bound, raw-record access, and any
  authorization beyond the owner-private local socket remain undecided. Listing
  across imported conversations is implemented by the unified conversation
  listing in [process protocol](spec/process-protocol.md); filesystem discovery
  of unimported sources beyond the explicit directory scan is not. (S28)

## Transcript ancestry

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
  ([model-call-execution](spec/model-call-execution.md)). The daemon now spends
  a bounded durable reconciliation budget and automatically releases the slot by
  terminalizing over that exact ambiguity; an operator decision may win the same
  race and becomes required only when the budget exhausts
  ([process-protocol](spec/process-protocol.md)). Neither treatment resolves
  what the provider actually did. Whether a provider request-status API can
  replace the conservative ambiguous outcome with trustworthy evidence,
  including its polling posture and evidence classes, remains undecided. Later
  scope. (S02)
- **Per-session scheduler scan gating and fairness.** Deployment configuration
  now owns the scheduler sweep and turn-liveness cadences
  ([turn-lifecycle-and-scheduling](spec/turn-lifecycle-and-scheduling.md)). What
  remains undecided is whether one session may tune its own scan gate and how
  contending sessions share a deployment-wide pass budget. Later scope. (S01,
  S02)
- **Direct interrupt-only reconciliation from a running attempt.**
  [turn-lifecycle-and-scheduling](spec/turn-lifecycle-and-scheduling.md) adds
  direct reconciliation only for fatal mismatch at a closed aggregate boundary;
  whether an interrupt-only path may bypass `StopRequested` remains undecided.
  Later scope. (S07)

## Session organization, visibility, and retention

- **Creation-attributed default visibility.** The implemented visibility and
  attribution limits are owned by
  [sessions-and-transcript](spec/sessions-and-transcript.md). Decide derivation,
  override shape and authority, and monitor inclusion together with the
  attributed-creation implementation.
- **Expressive metadata filters.** The implemented filter grammar is owned by
  [sessions-and-transcript](spec/sessions-and-transcript.md). Whether to add OR,
  negation, attribute predicates, case folding, or a general query language
  remains open.
- **Imported-conversation archive semantics.** Ordinary session archive and
  immutable imported-source behavior are owned by
  [sessions-and-transcript](spec/sessions-and-transcript.md) and
  [conversation-import](spec/conversation-import.md). Whether imported
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

- **Additional effective-configuration categories.** Prompt composition,
  sampling and output-shape parameters beyond the implemented model/session
  settings contract, tool enablement/configuration, placement constraints,
  per-turn resources, and interpreting-policy selections remain unavailable; a
  future subsystem decision must extend the request, session-default, override,
  and effective-value algebras together
  ([configuration-and-credentials](spec/configuration-and-credentials.md)).
  Workspace-instruction eligibility is the separate typed selector and
  allow-list algebra owned by
  [workspace instructions and skills](spec/workspace-instructions.md), so it
  neither waits on nor satisfies this general configuration question. Reasoning
  level, fast mode, and provider-tagged service tier are owned by
  [model and session settings](spec/model-session-settings.md). Compaction
  threshold, target size, and never-compact/full-context controls remain
  deferred here for a separate follow-on slice. Static copy-on-create session
  templates compose model selection, bounded system prompt, dangerous-tool
  blanket, and the model-settings layer owned by that contract; every other
  richer composition or configuration category stays blocked here. (S02, S05,
  S13–S16, S34, S35, S37)

## Template storage and authoring

- **Durable objects, protocol CRUD, and agent authoring tools.** Static
  startup-file loading, daemon-side create-by-name resolution, and read-only
  name/version listing are fixed by
  [configuration and credentials](spec/configuration-and-credentials.md) and
  [process protocol](spec/process-protocol.md). Whether templates become durable
  database objects, the exact protocol CRUD and concurrency contract, and agent
  tools that read or edit templates remain undecided. Blocks only those storage
  and authoring surfaces. (S35)

## Codex CLI fixture validation

- **Validation of recorded event-shape fixtures against the pinned CLI.** The
  adapter build mechanically derives its supported version from the exact npm
  pin, and the automatic pull-request smoke checks the installed executable's
  version, feature inventory, ambient-skill controls, and one live exchange.
  Neither establishes that the offline event fixtures still represent all
  current CLI event shapes. Decide whether a pin bump regenerates those fixtures
  from the installed CLI or validates the existing corpus against it, including
  how the resulting artifact is reviewed. Blocks claiming fixture-corpus review
  as an enforced pin-bump gate; it does not block the existing mechanical pin or
  live compatibility gates.

## Codex CLI image capability features

- **Whether the pinned CLI's image features return once provider input carries
  image bytes.** The adapter hard-disables `image_generation` and `view_image`.
  Each adds a model-visible tool that the adapter's structured-output envelope
  does not carry, and `view_image` — which loads a local image file into the
  conversation context — is enabled by default in the pinned inventory, so
  classifying it as non-capability would leave it live rather than merely
  acknowledged. Accepted input can carry blob-backed attachment parts, but the
  model sees only their text stubs: no present provider input carries attachment
  bytes as image or file media. If provider-native image or file delivery is
  added, decide whether either CLI feature is re-enabled and how the bytes reach
  the spawned CLI. Blocks re-enabling either name; it does not block attachment
  parts or the present disables, which stand on the capability rule alone.

## Model fallback and provenance

- **Automatic fallback.** Decided and specified: what a selection attempt can
  end as, and every projection of each ending, by
  [the credential-availability machine](spec/credential-availability.md); the
  qualifying causes and the successor-call shape by
  [availability successor calls](spec/model-call-execution.md); the pool
  grammar, per-membership ranking, and closed action vocabulary by
  [credential pools and selection](spec/configuration-and-credentials.md#credential-pools-and-selection).
  What remains open is the client projection: snapshots expose each call's usage
  and the final turn state, while the predecessor, cause, and successor relation
  is committed future storage that no present migration or repository operation
  supplies. Blocks fallback UI, not fallback. (S22)
- **Whether an automatic successor may cross adapter kinds.** Decided for the
  first slice: no. A pool's members share one adapter, so cross-kind
  substitution is inexpressible rather than merely disabled, and moving a
  session between adapter kinds stays an explicit defaults replacement. Whether
  mixed pools are ever admitted, and what would reconcile two adapters'
  authentication shapes if they were, remains open. (S22)
- **Provider headroom observation.** Selecting a profile by remaining capacity
  requires an observation surface no adapter currently captures: the Anthropic
  HTTP adapter reads only a request identifier from response headers, and the
  Codex CLI adapter's documented percentage headers are not established as
  reachable through its process boundary. What a deployment may configure where
  no adapter supplies headroom is decided and no longer open: startup rejects
  `headroom_reserve_percent`, `tie_break = "least_used"`, and any
  `on_headroom_low` action other than `stay`, under the fail-closed admission
  rule in
  [credential pools and selection](spec/configuration-and-credentials.md#credential-pools-and-selection),
  because a protection that silently never fires reads as one the deployment
  has. What remains undecided is which adapters can supply headroom at all and
  the normalized quantity, observation lifetime, and deterministic secondary
  tie-break a later contract must define before `least_used` is admitted, and
  whether a free probe exists that does not consume the quota it reports. Blocks
  capacity-aware selection, not availability failover. (S22)
- **Zero-cost liveness probes.** Quarantine semantics are decided and owned by
  [credential pools and selection](spec/configuration-and-credentials.md#credential-pools-and-selection):
  durable, profile-scoped, cleared by an operator command or by a probe that
  calls no model. What remains open is whether any adapter can offer such a
  probe. Absent one, an operator command is the only clearing path. Blocks
  automatic recovery from a rejected credential, not recovery itself. (S22)
- **Access-token-only Codex CLI conformance evidence.** The committed `oauth`
  delivery contract is owned by
  [credential deliveries](spec/configuration-and-credentials.md#credential-deliveries).
  What remains open is the minimum supported CLI version and exact live
  conformance check that establish this behavior. The implementing slice cannot
  land until that evidence exists; a current CLI version declining the store
  blocks that slice rather than making the committed delivery optional. (S22)
- **Reuse-detection blast radius.** Whether a provider rejecting a reused
  refresh token invalidates only that token or the whole authorization family is
  not determinable from either CLI's source. It does not affect the `oauth`
  delivery, which has exactly one refresher, but it bounds how bad a
  `codex_home` concurrency violation is: single-token rejection is recoverable,
  family revocation is account loss. (S22)
- **Detailed provider provenance representation.** Model identifier
  normalization is decided: the
  [provider-target identity rule](spec/model-call-execution.md) accepts an alias
  resolved to its own dated snapshot as the same target and keeps a different
  lineage as a distinct substitution outcome. The mismatch disposition itself is
  likewise accepted ([model-call-execution](spec/model-call-execution.md)). What
  remains open is the durable per-call provenance schema that would record the
  concrete served identity and a substitution as evidence rather than as
  operator diagnostics and a fail-closed error. Blocks the provider provenance
  schema. (S20–S23)
- **Future same-profile retry.** Repeating a known provider failure or ambiguous
  outcome against the target and credential profile that produced it remains
  outside every accepted policy; the successor-call decision above authorizes
  same-target failover through another eligible profile, never a repeat of the
  same profile. Any later same-profile retry command or policy, including
  backoff and resource limits, is a separate decision the accepted no-retry
  policy leaves open. Blocks retry features. (S02, S04, S22)

## Scheduling and runners

Dispatch fencing, initial scheduler mechanics, and the complete version-one
local runner orchestration are specified in
[turn-lifecycle-and-scheduling](spec/turn-lifecycle-and-scheduling.md) and
[runner protocol and placement](spec/runner-protocol.md). The loss, replacement,
cleanup, contract-gap, and session-composition questions this section previously
carried are decided, and each decision is stated by the contract page that owns
it: staged replacement ordering and the runner-recovery turn phase in
[turn-lifecycle-and-scheduling](spec/turn-lifecycle-and-scheduling.md);
same-runner recovery after a registration-triggered loss, deployment-scoped
successor promotion, non-transferable workspace cleanup, pinned canonical digest
bytes, runner-to-daemon failure frames, workspace-release acknowledgement,
forced Git transport configuration, and the independent
[session-composition axes](spec/runner-protocol.md) in
[runner protocol and placement](spec/runner-protocol.md); the runner-recovery
phase, creation-record placement, and the runner event family in
[persistence-protocol](spec/persistence-protocol.md), and the placement
transcript entry in the
[persistence-protocol design](design/persistence-protocol.md); the closed runner
execution object, creation-request placement, and template creation carrying
placement in [process-protocol](spec/process-protocol.md); the relocation
transcript boundary in
[sessions-and-transcript](spec/sessions-and-transcript.md); capability-derived
tool advertisement in [model-call-execution](spec/model-call-execution.md). Why:
a decided question is a contract, and a contract binds only where the
implementer of that contract reads it; a decision restated on this page would be
a second authority over prose that already owns it, free to drift from the page
it paraphrases. Multiple simultaneously enrolled runners and user-directed
relocation of a healthy session are committed functionality that version one
defers rather than open questions
([runner protocol and placement](spec/runner-protocol.md#planned)). The
questions below remain open.

- **Runner workstation tool execution.** No present runner surface executes a
  workstation tool. Registry choices not already constrained by committed
  functionality — including its remaining inventory, any additional names, and
  per-tool deadlines — remain undecided. Existing per-tool compatibility
  constraints remain binding. The committed unimplemented runner protocol
  remains the owner of placement, sandbox, approval, workspace, credential, and
  generic dispatch behavior; this question cannot redefine those constraints.
  Blocks runner-side tool registry and executor implementation.
- **Daemon Git push transport.** `git_push_configured` is implemented as a
  declaration and executor over an injected transport, but no production
  `GitPushTransport` exists. Remote authority and destination policy are decided
  and stated under
  [remote destination authority](spec/git-authority-threat-model.md):
  destinations are durable records an operator mints, scoped by workspace
  identity, and `https` only. The credential policy for a push and the
  production transport itself remain undecided; until they are decided the tool
  stays absent from the daemon registry. Blocks daemon-side Git push.
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
  family exists. Not a blocker: user-directed moves of a workspace-free session,
  and of a session whose work is pushed, require none of it. (S16, S30–S32)
- **Automatic scheduling, load balancing, and MCP placement.** Placement selects
  a runner by exact identity or capability class and is never rescheduled; no
  policy chooses among several satisfying runners, balances load, or admits an
  MCP locus. Deciding those requires multiple simultaneously enrolled runners
  plus a stated selection policy and its observability, and it composes with the
  workspace portability question above. Blocks automatic placement, not manual
  placement. (S16, S30–S32)

## Goal mode

Statement lineage, transition authority, scheduler continuation, and the bounded
automatic resumption of an execution-failure block are specified in
[goal mode](spec/goal-mode.md). The questions below remain open.

- **Separating consecutive execution failures from distant ones.** The run an
  attempt budget is derived from ends only at a goal event, so consecutive
  execution failures separated by successful turns count together: a pursuit
  that fails transiently five times, however far apart and however much work
  succeeded between them, exhausts its budget and parks for an operator. The
  conservative direction is deliberate, because the alternative reads turn
  dispositions the goal event stream does not carry. Blocks nothing committed.

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
  [runner lease-loss transitions](spec/runner-protocol.md) remain undecided.
  (S05, S06, S31)
- **Ambiguous tool-wait resolution.** Who may record resolving evidence, how an
  exact accepted-risk continuation is represented, and which effects permit it
  beyond the
  [proof-bearing terminal paths](spec/turn-lifecycle-and-scheduling.md) remain
  undecided. Blocks reconciliation and continuation from `AwaitingToolRecovery`.
  (S06)
- **Durable tool-definition revisioning.** The implemented compiled catalog is
  immutable for one process lifetime. A dynamic catalog or a deployment that
  changes a definition while requests are outstanding must first decide how the
  advertised schema, permission default, effect class, validator, and executor
  revision are pinned and compared. Blocks runtime catalog mutation and safe
  rebinding across outstanding requests.
- **Dynamic runner-catalog lifecycle.** Mutable behavior beyond the
  [compiled version-one catalog](spec/runner-protocol.md) requires
  representation, revision identity, change audit, compatibility, and safe
  rebinding decisions.
- **Execution-strategy configuration placement.** Whether a future
  serial/concurrent choice beyond the [fixed serial loop](spec/tool-loop.md) is
  a deployment, session-default, per-turn, or executor-selection value remains
  undecided. Blocks configurable/concurrent execution.
- **Model-declared approval expiry.** Pending user approval currently waits
  indefinitely. Whether a model may request an expiry, how it is frozen, and
  what durable resolution expiry creates remain undecided.
- **Additional high-risk guardrails.** Operations that a future policy must
  never make automatic, richer values beyond the
  [fixed profile/override ladder](spec/runner-protocol.md), and dynamic
  replacement/equality semantics remain undecided.
- **External approval-judge corpus adaptation.** Whether and how to adapt public
  agent-safety datasets such as R-Judge, AgentHarm, and ToolEmu into the
  approval-judge case schema remains undecided. A future mapping must select and
  pin source revisions, establish each dataset's label mapping and trajectory
  treatment, and verify license terms before redistribution; source content is
  not vendored. Blocks only external-corpus evaluation, not the synthetic corpus
  or eval harness.
- **Turn-origin instructions in the approval-judge request.** The delegated
  request context carries session-scoped authority — the goal generation the
  judged turn is bound to, the template name, and the system prompt frozen for
  that turn — but no turn-origin content. A delegation-origin child turn's exact
  parent-supplied task is therefore not shown, so a child created from a broad
  template may ask for an effect its delegated task never covered while the
  judge sees only the wider session authority. Freezing that task alongside the
  session-level fields is undecided, because each added field is further
  attacker-influenced text placed inside the judge's own prompt, and the
  injection posture is what makes any session-derived context admissible at all.
  Recorded as a design question rather than a blocker; authority the context
  does not settle escalates rather than approves.
- **Per-template thread-resolution policy.** Whether a session template may
  choose its own posture toward
  [`change_request_thread_resolve`](spec/tool-loop.md) — so that one template
  resolves the reviewer threads it has answered while another may only reply and
  leave resolution to the reviewer — is undecided. Deciding it requires the
  template configuration surface to carry per-template tool posture at all,
  which is itself open under
  [Template storage and authoring](#template-storage-and-authoring). Recorded as
  a design question rather than a blocker; it blocks only a per-template choice,
  never the posture the daemon composition already applies.
- **Rich result-content variants.** Attempt content is text-only. Image and
  file/artifact arms, their resource governance, and provider/client rendering
  remain undecided. The byte substrate such arms would reference is owned by
  [blob storage](spec/blob-storage.md).
- **Large durable payload architecture.** Tool evidence is bounded by storage
  policy rather than by physics: 1 MiB of result text, 1 MiB of arguments, 4,096
  bytes of error detail, and 4,096 bytes of exact runner value, all held in
  PostgreSQL `text` columns with no physical ceiling near those values. Under
  [tool-loop result authority](spec/tool-loop.md), every admitted result fits
  those bounds. A family may compact output with its crate-owned truncation and
  completeness evidence, or its bounded transport may reject an oversized
  response before result admission; the family contract owns that choice.
  `ResultTooLarge` remains the admission classification for an admitted result
  that still exceeds the durable bound. Blob storage decides only where
  deliberately larger byte payloads live: content-addressed blobs with
  model-visible attachment stubs and bounded explicit reads. The tool-result
  side remains open — whether and how a family's durable admitted result
  references a blob rather than embedding bytes, its truncation and completeness
  evidence, and per-family adoption. The existing family caps remain correct
  until that lands.
- **Repository configuration outside the model's writable root.** A session's
  `.git` sits inside its writable root, so repository-local Git configuration is
  model-writable, and version one answers that key by key: a forced transport
  allowlist, an emptied credential-helper list, disabled repository hooks, and
  an effective-URL check that binds every remote-reaching operation to its
  canonical repository after Git's own rewrite expansion
  ([runner protocol and placement](spec/runner-protocol.md#planned)). That
  posture is not a closed set: configuration that changes what Git runs rather
  than where it connects is neutralized only where a command-line setting names
  it, so each new key is found rather than excluded. Putting the administrative
  directory and its configuration outside the model's reach would retire the
  whole class instead of enumerating it, and needs its own design — where that
  directory lives, how every invocation names it so the worktree pointer cannot
  be repointed, what the sandbox binds, and what a session's own `git` usage
  sees. Recorded as a design question rather than a blocker; the forced
  configuration and the effective-URL check remain the version-one boundary.
- **Several bound workspaces per session, and explicit session relocation.** A
  session binds one workspace root, derived from the configured root by the
  fixed session-UUID formula owned by
  [configuration and credentials](spec/configuration-and-credentials.md#derived-session-workspace-roots),
  which is what keeps the set of roots the daemon can open a property of
  deployment configuration alone. Two operations are anticipated on that
  mechanism and are inexpressible today: a session bound to several workspaces
  at once, and an operator moving or pinning a session's workspace deliberately.
  Both are explicit rebinds of the per-session instance rather than anything
  derived from placement — they compose with runner placement without being
  selected by it, and the workspace portability question under
  [scheduling and runners](#scheduling-and-runners) owns carrying a workspace
  between runners rather than rebinding one inside a daemon. Deciding them needs
  what names a further root without letting a session name a path, how the
  isolation comparisons that today refuse a directory shared between two
  sessions read across several roots one session holds, and what a rebind owes
  executors already retained against the previous root. Recorded as a design
  question rather than a blocker; the one-root-per-session derivation remains
  correct until it is answered. (S15)

## Identity, credentials, and resource governance

Provider and integration credential lifecycle (storage, delivery, and rotation)
is decided, specified in
[configuration-and-credentials](spec/configuration-and-credentials.md); the
questions below remain open.

- **User client authentication and revocation.** Keep the daemon's authorization
  model single-user while choosing a remotely safe authentication boundary.
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
- **Browser transport.** Settled for the web client: the same-origin browser
  transport merged in PR #1000 and is owned by
  [configuration-and-credentials](spec/configuration-and-credentials.md). It no
  longer blocks the web client; transient model-update streaming remains open
  below. (S02, S24)
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
- **Web client technology.** Settled: the web campaign uses React and TypeScript
  with TanStack, Redux Toolkit, and Radix. This owner-approved platform choice
  is no longer open. (S01, S02, S24)
- **Client approval presentation.** The terminal baseline now surfaces the
  pending request through the transcript's awaiting-turn and tool-use lines and
  collects decisions through `approve`/`deny`
  ([process-protocol](spec/process-protocol.md)); interactive prompting and
  later client forms remain undesigned. (S10, S11, S24)

## General-purpose artifacts

Artifact content addressing, byte storage, and the reference-not-embed posture
are decided and specified by [blob storage](spec/blob-storage.md): immutable
SHA-256-addressed blobs, a durable replica catalog, class-routed named stores,
and an append-only version one. The reference-not-copy posture review workflows
take today is owned by [review-workflows](spec/review-workflows.md). The
questions below remain open; they block general-purpose workflow artifacts, not
the implemented session and external-link evidence.

- **Artifact aggregate and authority.** What a named artifact is above a blob —
  mutable aliases over changing digests, producer provenance, ownership, and
  workflow attachment — needs its own foundation decision before a workflow can
  attach one.
- **Non-socket ingest paths.** Daemon-local file adoption and runner-produced
  artifact ingest — moving multi-gigabyte content into the catalog without
  base64 chunking over the local socket — remain undecided.
- **Store lifecycle beyond append-only.** A native network-filesystem store
  kind, replica-set routes, replica retirement, a marked-deleted state, and
  garbage collection remain undecided. The append-only catalog is their fixed
  constraint; mark/sweep rather than reference counting is nonbinding
  exploration guidance only.

### File and media interpretation

The architecture is specified in
[file and media interpretation](spec/file-and-media.md). These choices remain
open and bind no implementation:

- **Parser dependency budget.** Decide whether isolated native decoders are
  admissible. Leaning: pure Rust first, with native libraries approved per
  adapter only when coverage requires them and executable isolation exists.
- **OCR and transcription.** Choose explicit inference providers, local readers,
  or absence. Leaning: exclude both because they add selection, credentials,
  cost, privacy, and nondeterministic replay beyond file reading.
- **Provider-native general files.** Decide which model adapters may receive
  them. Leaning: require an exact per-adapter type inventory and never treat a
  generic provider file surface as accepting unknown bytes.
- **File-media turn budgets.** Set cumulative typed-read request and source-work
  ceilings after first-adapter benchmarks while preserving every per-request and
  per-call hard ceiling. Blocks production enablement, not interface work.
- **File classification cache.** Decide whether validated classifications need a
  cache beyond immutable tool results. Leaning: omit it until measurement proves
  a need because it adds invalidation and reader-retirement law without
  improving correctness.

## Program substrate and evaluations

The substrate and evaluation contracts are owned by
[program-substrate](spec/program-substrate.md) and
[eval-system](spec/eval-system.md). Two edges remain deferred:

- **Remote and out-of-process program hosts.** The frame protocol is the seam;
  only the in-daemon host is committed. Hosting programs in a separate
  supervised process or on remote execution infrastructure requires a future
  transport decision. Blocks nothing committed.
- **Evaluation exporters.** Run scalars and per-case tables toward external
  experiment trackers are deferred; the SQL surface is the contract until a
  concrete tracker need appears. Blocks nothing committed.

## Destination features (target model)

These unresolved foundation requirements are authoritative here. The
[target model](target-model.md) is non-normative direction for their destination
and ordering.

- **Goal identity and lifecycle.** Durable persistent-objective identity and
  lifecycle require a future foundation decision. Blocks platform goal mode.
- **Standing update-subscription lifecycle.** Identity, lifetime, delivery, and
  cancellation for client-facing standing update subscriptions require a future
  foundation decision. Blocks the planned callback surface. Narrowed: durable
  program event subscriptions — identity, wake delivery, and cancellation for
  registered programs — are decided and owned by
  [program-substrate](spec/program-substrate.md), and are no longer part of this
  question.
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
  decisions. Blocks general inter-session messaging routed through
  `SubmitInput`; it does not block the typed, relationship-bound delegation
  message records committed by S18 and S19.

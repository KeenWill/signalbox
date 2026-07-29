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
[runner protocol and placement](spec/runner-protocol.md). The questions below
remain open.

The first four are one family. They are the loss, replacement, and cleanup
deadlocks of the version-one singleton-runner model: each one is a state the
specification can reach and cannot leave, and the runner implementation slice —
daemon listener, placement and loss handling — hits all four in the same pass.
They were raised as review findings on the specification pull request and are
recorded here rather than closed, because the specification admits the states
today and the implementer needs the decisions before writing the transaction
set. Deciding them together is cheaper than deciding them one at a time: the
same replacement transaction is the answer surface for three of them.

- **Replacement ordering against an in-flight model call.** When heartbeat loss
  occurs during an already-authorized daemon-local model call, the loss rules
  let that call keep "its ordinary completion or ambiguity law" while
  `ReplaceLostRunner` immediately appends the reference-only
  `RunnerPlacementChanged` semantic entry and extends the next context frontier.
  The later model observation must then append its assistant and tool entries
  from the older frozen source frontier, so it either violates the prefix-only
  frontier law or orders the placement event ahead of output that never saw it.
  The two rules are jointly underspecified and cannot both hold. Decide whether
  replacement is *rejected* while a call is in flight or *staged* until the call
  reaches its observation boundary, with the placement event appended after that
  output; the prefix-only law is enforced by persistence triggers that reject at
  commit, so an implementation that guesses wrong fails loudly in its own tests
  rather than writing a mis-ordered transcript. Blocks the replacement
  transaction and any loss handling that can race a live call. (S16, S30–S32)
  [Thread](https://github.com/KeenWill/signalbox/pull/307#discussion_r3662727053)
- **Recovery after registration-triggered loss.** `RunnerLost` can arise when a
  live runner re-registers without a pinned capability, without its connection
  or enrollment being marked lost. In that state the singleton enrollment rule
  admits no pending successor, while the replacement rule rejects the same
  runner and requires a *different* currently-registered live runner — which
  version one cannot supply. A session reaching this loss source can therefore
  never call `replace_lost_runner`, the only transaction that would recover it.
  Decide between a checked same-runner/current-enrollment recovery and admitting
  a provisioning-only successor for this loss source specifically. Blocks
  recovery from capability-losing re-registration. (S16, S30–S32)
  [Thread](https://github.com/KeenWill/signalbox/pull/307#discussion_r3662727058)
- **Cleanup ownership for a retired runner.** After a repository-owning runner
  is durably lost and replaced, the predecessor enrollment is revoked and its
  identity cannot resume, yet the specification states that "its cleanup owner
  is structurally the runner that provisioned it; no daemon-cleanup alternative
  is constructible" and the workspace cannot transfer to the successor. The
  resulting `workspace_release` has no connection to deliver on, so every
  loss-based replacement permanently strands the old clone. The consequence is
  leaked sandbox disk rather than lost session data, but the design has no exit.
  Decide between a cleanup-only fenced resume for the retired identity and a
  checked transfer mechanism that moves cleanup ownership without granting lease
  authority. Blocks a replacement path that does not leak clones. (S16, S30–S32)
  [Thread](https://github.com/KeenWill/signalbox/pull/307#discussion_r3662727060)
- **Promotion without a lost session placement.** `replace_lost_runner` is the
  only transaction allowed to promote a pending successor, and it targets a lost
  *session placement*. If the singleton runner is lost before any session is
  pinned — a fresh deployment with no sessions, or one where every placement is
  an unpinned capability-class request, which loss explicitly does not affect —
  there is no placement for the transaction to target and the pending runner
  stays provisioning-only forever. Decide on an owner promotion path independent
  of any session placement. Blocks recovery of a deployment that loses its
  runner before pinning a session. (S16, S30–S32)
  [Thread](https://github.com/KeenWill/signalbox/pull/307#discussion_r3662727064)

The remaining questions are independent of that family.

- **Durable acknowledgement for workspace release.** `workspace_release` /
  `workspace_released` is the only two-frame exchange in the frame table with no
  daemon acknowledgement, unlike `workspace_leak_recorded`,
  `workspace_recorded`, and `result_recorded`. A runner that deletes the
  manifest and then crashes has no acknowledged boundary telling it when the
  release record may be discarded, and a retained operation can never reach one;
  a daemon restart can therefore resend a release whose manifest the runner no
  longer holds, or the runner can retain the sole workspace-operation slot
  indefinitely. Add a recorded acknowledgement and replay journal analogous to
  provisioning and results — the pattern to copy already exists in both. Blocks
  crash-safe workspace release. (S16, S30–S32)
  [Thread](https://github.com/KeenWill/signalbox/pull/307#discussion_r3662727046)
- **Credential profile requirement for repository placements.** Session creation
  makes `--credential-profile` optional ("either selector requires
  `--repository`; the credential profile is optional"), while the credential
  lifecycle requires the exact granted profile name before a repository can be
  provisioned. The two contracts cannot both be implemented: a null
  authorization either fails every such creation or forces the runner to infer a
  credential the owner never selected. Decide between requiring the
  repository-bound profile explicitly at session creation and defining a
  credential-free repository mode across both owning contracts, and make the two
  sections agree. Blocks the first repository-bound session placement. (S13,
  S16, S30–S32)
  [Thread](https://github.com/KeenWill/signalbox/pull/307#discussion_r3662727040)
- **Git subprocess deadlines.** Only `shell_exec` and `build_test` accept
  `timeout_seconds`; `git_clone` and the other Git tool schemas specify neither
  a timeout argument nor a fixed deadline. As written, a stalled clone, fetch,
  push, or hook holds the runner's single global execution permit indefinitely
  while heartbeats stay healthy, blocking every runner session and producing no
  terminal evidence. Decide a bounded deadline and a timeout classification for
  each Git operation — whether the deadline is fixed per operation or a caller
  argument, and which terminal evidence a timeout produces. Blocks the
  workstation-tools slice that writes these schemas. (S05, S06, S16)
  [Thread](https://github.com/KeenWill/signalbox/pull/307#discussion_r3662727037)
- **Branch-name validation admits Git shorthand.** Validation defined in terms
  of `git check-ref-format --branch` inherits that command's operand, which its
  own help text describes as `<branchname-shorthand>`; it accepts `@{-1}`, so a
  token that is not a literal branch name passes. Subsequent create or push
  operations can then target the previous branch, fail because the expanded
  branch already exists, or construct the invalid literal `refs/heads/@{-1}`.
  Decide the exact check: validate the complete `refs/heads/<input>` form, or
  require the command's normalized output to equal the input exactly. Blocks
  branch-argument validation in the workstation-tools slice. (S05, S06)
  [Thread](https://github.com/KeenWill/signalbox/pull/307#discussion_r3662727050)

A later review round raised a further set, recorded below. They divide into
contract gaps — places where the runner specification requires something the
owning persistence, protocol, or transcript contract does not yet admit — and
executable-behaviour gaps, where the specified behaviour cannot be produced by
the tools as described. The first group is what an implementer hits on day one,
because each one is a write with nowhere to land.

- **Persisting the runner-recovery phase.** Runner loss parking an active turn
  requires `awaiting_runner_recovery`, but the closed persistence lifecycle
  inventory admits only running, model and tool recovery, and approval. The loss
  transaction cannot store the phase without violating the lifecycle
  discriminator checks, and restart cannot reconstitute it. Add the storage
  discriminator, payload correlations, transition checks, and migration to the
  persistence contract. Blocks runner-loss handling end to end. (S16, S30–S32)
  [Thread](https://github.com/KeenWill/signalbox/pull/307#discussion_r3670266219)
- **Storage for runner-placement transcript entries.** The
  `RunnerPlacementChanged` entry is mandatory on replacement, but the relational
  specification defines no corresponding `semantic_transcript_entry` kind,
  placement-revision payload, correlation constraint, migration, or
  reconstitution arm. The replacement transaction must therefore either violate
  the closed semantic-entry schema or invent an undocumented representation.
  Specify the typed persistence contract. Pairs directly with the
  replacement-ordering question above, which decides *when* this entry is
  appended; this one decides whether it can be stored at all. Blocks
  replacement. (S16, S30–S32)
  [Thread](https://github.com/KeenWill/signalbox/pull/307#discussion_r3670266222)
- **Runner transitions in the transactional outbox.** The closed outbox
  inventory has no `runner_state_transition` event kind, typed record table, or
  append rule, while runner-loss handling requires one such event appended per
  affected session. Every loss, suspect, recovery, pin, replacement, or
  abandonment event must therefore either violate the exactly-one-typed-record
  trigger or disappear from followers. Add the runner event family and its
  atomic append producers. Blocks followers observing any runner state change.
  (S16, S30–S32)
  [Thread](https://github.com/KeenWill/signalbox/pull/307#discussion_r3670201474)
- **Placement through session creation.** A non-null `runner_placement` has
  nowhere to land: both creation command families — ordinary and
  imported-frontier — carry only provenance, defaults, and creation mode, their
  storage versions have no placement payload, and the durable-command contract
  requires every caller-supplied semantic field in its typed record. A valid
  request therefore cannot atomically establish or truthfully replay its
  placement. Extend both creation command families, storage versions, equality,
  and commit transactions with the exact placement. Blocks creating a
  runner-backed session at all. (S16, S30–S32)
  [Thread](https://github.com/KeenWill/signalbox/pull/307#discussion_r3670201470)
- **Runner execution metadata schema.** A runner-produced
  `tool_execution_result` carries a required closed `execution` object, but the
  runner arm defines no discriminator, no member holding the outcome, and no
  allowed outcome tokens or associated fields. The daemon arm is
  `{"type":"daemon"}`, which gives clients nothing to infer the runner shape
  from, so the object cannot be encoded or validated interoperably. Enumerate
  the exact runner object and its outcome variants. Blocks any client rendering
  a runner-executed tool result. (S05, S06, S16)
  [Thread](https://github.com/KeenWill/signalbox/pull/307#discussion_r3670201459)
- **Canonical bytes hashed by runner digests.** Advertisement, leak-report,
  page-chain, and workspace digests are defined over a "canonical checked
  representation" that fixes no byte serialization, field framing or ordering,
  Unicode treatment, or domain separation. Equal typed facts can therefore hash
  differently across the daemon and runner implementations and fail enrollment,
  acknowledgement, or reconnect replay, while ambiguous concatenations can hash
  identically. Define the exact canonical byte encoding for every digest input,
  including domain separation between digest kinds. Blocks enrollment and
  reconnect replay, and is cheapest to decide before either side is written.
  (S16, S30–S32)
  [Thread](https://github.com/KeenWill/signalbox/pull/307#discussion_r3670201465)
- **Runner-to-daemon operation failures.** A runner that cannot provision a
  workspace — credential unavailable, repository unclonable, sandbox refusing
  startup — has no frame with which to report it: `workspace_ready` carries only
  a successful manifest, and the generic `rejected` frame is daemon-to-runner
  only. The same gap applies when local lease admission refuses an offer before
  `lease_claim`. The typed provisioning rejection the protocol requires, and
  exposes as `replacement_provisioning_failed`, therefore cannot be recorded,
  leaving a claimed replacement or prepared attempt waiting on an operation the
  healthy runner has already refused. Add correlated, replayable
  runner-to-daemon failure frames with their durable acknowledgement boundaries.
  Blocks every unhappy path in provisioning and replacement. (S16, S30–S32)
  [Thread](https://github.com/KeenWill/signalbox/pull/307#discussion_r3670266230)
- **Neutralizing repository Git transport rewrites.** Validating the canonical
  `remote.<name>.url` is not sufficient, because model-writable `.git/config`
  can retain that URL while adding `url.<base>.insteadOf` together with
  `protocol.ext.allow=always`, so Git executes a substituted external helper.
  Because `git_fetch` is classified `Idempotent`, a claimed-loss retry can then
  repeat arbitrary non-idempotent code rather than a fetch. Require a sanitized
  Git configuration and an HTTPS-only *effective* transport, not validation of
  the pre-rewrite remote URL. This is the highest-consequence item in this set:
  it is the one whose wrong answer executes attacker-chosen code inside the
  sandbox rather than merely failing. Blocks the repository tool surface. (S05,
  S06, S16)
  [Thread](https://github.com/KeenWill/signalbox/pull/307#discussion_r3670266227)
- **Fitting process results into durable tool evidence.** The per-stream output
  bounds admit results that cannot commit: roughly 800 KiB of stdout, once
  base64-padded and wrapped in the result object, exceeds the 1 MiB
  `ToolResultContent::Text` limit, converting a contract-valid success into
  `ResultTooLarge`. A nonzero exit is worse, since the contract permits two 1
  MiB streams while durable error detail is capped at 4,096 bytes. Set aggregate
  encoded output bounds that fit the durable evidence algebra, or add a
  structured bounded result representation, so the promised executor outcomes
  can actually commit. Blocks `shell_exec` and `build_test` on realistic output.
  (S05, S06)
  [Thread](https://github.com/KeenWill/signalbox/pull/307#discussion_r3670266228)
- **Successful clones of empty repositories.** `git clone` of an empty
  repository exits successfully while reporting that fact, and
  `git rev-parse HEAD` then fails because no commit exists, so the mandatory
  `head` result cannot represent a valid successful clone. Recording it as a
  known failure is both inaccurate and leaves the newly created destination
  behind. Admit an explicit unborn or nullable HEAD result, or reject and clean
  up empty repositories under a stated contract. Blocks first-clone provisioning
  against a fresh repository, which is a normal starting state rather than an
  exotic one. (S05, S06, S16)
  [Thread](https://github.com/KeenWill/signalbox/pull/307#discussion_r3670201478)
- **Runner placement flags against template creation.** The terminal syntax
  admits `create --template NAME` together with `--runner` or `--runner-class`,
  because the runner options sit outside the model/template alternative — but
  the closed `create_session_from_template` request carries only `command_id`
  and `template_name`. The selected placement must therefore be silently
  discarded or encoded as an illegal extra member. Decide between making runner
  flags conflict with `--template` and deliberately extending the template
  request. Blocks template creation on a runner-backed session. (S16, S30–S32)
  [Thread](https://github.com/KeenWill/signalbox/pull/307#discussion_r3670201444)
- **Workspace-free restricted placements.** A placement selecting
  `workspace: none` with `workspace-restricted` is admitted, yet the compiled
  registry advertises all ten workstation tools, whose paths and working
  directories are defined relative to the exact session repository and whose
  restricted supervisor requires that repository as its sole writable bind. With
  no provisioned repository those advertised tools cannot be admitted at lease
  claim. Reject the combination in version one, or add an authoritative
  workspace requirement that filters the executable snapshot. Blocks the
  workspace-free placement mode. (S16, S30–S32)
  [Thread](https://github.com/KeenWill/signalbox/pull/307#discussion_r3670201450)

### Specification corrections

Not open questions — the intent is settled and the prose is wrong. Recorded here
because they would otherwise survive into an implementation.

- **Placement reconstitution rejection conditions are inverted.** The conditions
  that should *cause* placement reconstitution to reject a record are attached
  to an "unless" exception clause, so a mismatched pinned state or a too-new
  credential record reads as something that prevents rejection rather than
  something that triggers it. An implementer following the sentence literally
  would build a fail-open reconstitution that accepts exactly the corrupt states
  the rule exists to reject. Intended meaning: a mismatched pinned state or a
  credential record newer than the reconstituted placement **must cause
  rejection**. Rewrite the sentence so the conditions read as triggers.
  [Thread](https://github.com/KeenWill/signalbox/pull/307#discussion_r3670385088)

## Tool safety

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
- **Compatibility beyond the retained process-protocol versions.** Versions one
  through four have their owning [specification](spec/process-protocol.md). A
  future compatibility window, negotiation scheme, and generated-client policy
  remain undecided. (S01, S24)
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

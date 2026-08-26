# Model-call execution

The daemon-owned terminal treatment of a restart-ambiguous model call is
verified against this PR (`agent/turn-lifecycle-hardening`).

The user-vocabulary surface on this page was re-verified through PR #378
(`agent/user-vocabulary`).

The durable usage-provenance column and read projection are verified against PR
`#389` (`agent/cost-accounting`).

Multipart attachment rendering remains part of the foundation proposal from PR
#553 (`agent/blob-storage-foundation`). Distinct attachment sizing, streamed
replica verification, typed pre-authorization failure, and retryable
unavailability are verified against this implementing change
(`agent/blob-storage-attachment-preparation`).

This page describes the implemented model-call orchestration chain as verified
against the implementing stack through PR #201 (`agent/tool-loop-proof`):
rendering a context frontier into provider messages, the staged prepare /
authorize-send / commit-observation effects, assistant content, intra-turn tool
rounds and final turn completion, provider failure classification into physical
dispositions, and the retry prohibition. What a credential-pool selection
attempt can end as, and every projection of each ending, is owned by
[credential availability](credential-availability.md); this page owns the
terminal-evidence-and-cause column of that table and the successor call's own
mechanics. Tool requests, approvals, attempts, and continuation are owned by
[tool-loop](tool-loop.md). Turn and attempt lifecycle law lives in
[turn-lifecycle-and-scheduling](turn-lifecycle-and-scheduling.md); semantic
entries and frontiers in [sessions-and-transcript](sessions-and-transcript.md);
storage protocol and the outbox in
[persistence-protocol](persistence-protocol.md); the typed model-runtime layer
in [runtime-substrate](runtime-substrate.md); daemon startup, scheduling, and
shutdown composition in
[turn-lifecycle-and-scheduling](turn-lifecycle-and-scheduling.md); and model
configuration and credentials in
[configuration-and-credentials](configuration-and-credentials.md). The
`apps/signalboxd` supervision and `signalbox-debug` code homes this page names
were verified through PR #258 (`agent/signalboxd-rename`); the
[provider-target identity](#provider-target-identity) rule and the sanitized
model-call cause codes were verified through PR #280
(`agent/provider-identity-normalization`). The complete frontier-payload
projection and identity-before-terminal-evidence precedence were verified
through PR #288 (`agent/audit-fix-docs-coherence`); durable closed
provider-failure causes were verified through PR #330
(`agent/audit-verified-fixes`); the session system prompt on the prepared
operation was verified through PR #286 (`agent/session-system-prompt`).
Provider-reported token evidence retention and exact commit-ambiguity comparison
were verified through PR #301 (`agent/token-usage`); the empty-thinking
completion rule was verified through PR #305 (`agent/sonnet-streamed-tool-use`).
Configured token-limit enforcement and the routed Anthropic/Codex production
composition are verified through PR #373 (`agent/adapter-wiring`). The
crate-shared commit-ambiguity helper home was verified against this PR
(`agent/domain-cleanup`). The context-summary projection and dedicated
compaction-call evidence were verified through PR #312
(`agent/context-compaction-core`); the explicit trigger, dormant automatic
preparation machinery, configured prompt, and provider-native input-counting
implementation were verified through PR #314
(`agent/context-compaction-protocol`). The daemon does not schedule that
automatic machinery. Session-delegation semantic rendering and its
provider-neutral bridge were verified against this PR (`agent/delegation`). The
runner-placement rendering and executable session-tool snapshot paragraphs are
the foundation proposal at the bottom of their implementing stack and become
verified only with those child pull requests. Availability successor calls and
their durable provider-directed backoff are verified against this PR
(`agent/multi-account-pools`). Invariant tags cite
[docs/invariants.md](../invariants.md).

## Call records and lifecycle

A model call is one durable daemon authorization to attempt a provider
interaction (INV-014). Ordinary and dedicated compaction calls reserve their
`ModelCallId` from one append-only global call-identity registry, so the same
physical identity cannot name both call kinds (INV-001). Its record
(`crates/domain/src/model_call.rs`) fixes at creation: `ModelCallId`, owning
turn and attempt, the exact frozen model selection, the turn-pinned resolved
target, and the exact ordered context frontier it consumes (INV-015).
Nonterminal states are `Prepared`, `InFlight`, and `CancellationRequested`;
terminal history is a separate `EndedModelCall` carrying one of five physical
dispositions — `Completed`, `KnownFailed`, `Refused`, `Cancelled`, `Ambiguous` —
and exposes no transition back (INV-006).

The predecessor matrix:

- `Prepared -> InFlight` is the only send authorization.
- `Prepared` classifies terminally only as `KnownFailed`; ending an unsent call
  as `Cancelled` requires the exact applied-interrupt proof for the call's own
  turn (INV-029). An unsent call cannot complete, refuse, or become ambiguous.
- `InFlight` and `CancellationRequested` accept every disposition.
- Terminal state never reopens. Why: terminal physical history is the record of
  what was externally done, and rewriting it would let later facts silently
  change that record.

The same terminal transition stores four token-usage fields — input, output,
cache-creation input, and cache-read input — on the `model_call` row. Each field
is independently nullable: null means that axis was not supplied, while a
present zero remains zero. Every call also carries the closed
`usage_provenance_kind` discriminator, exactly `reported` or `estimated`. The
prepared checkpoint also pins `usage_input_includes_cache_tokens`, which
preserves whether input is inclusive of separately reported cache axes even if a
later daemon configuration routes the target through another adapter. Calls
prepared before that pin's migration retain null as an unknown historical
meaning, so a read never derives cost from possibly cache-inclusive input.
Current execution paths produce only `reported`; `estimated` is reserved for a
later explicit estimator and no present writer selects it. Calls closed from
`ProvenUnsent`, `CancellationConfirmed`, capability failure, or restart recovery
have all four fields unreported because no provider usage evidence exists.
Historical rows retain any reported axes exactly; the absent semantic pin, not
rewriting those axes, prevents an invented dollar derivation. The terminal-row
immutability rule makes this evidence write-once; no later path normalizes or
corrects it.

Storage enforces the matrix durably
(`crates/persistence/migrations/202607220001_model_call_execution.sql`): the
`model_call_changes_are_guarded` trigger rejects any insert whose state is not
`Prepared`, any mutation of the eleven authorization-fact columns (the pinned
credential reference joined them in
`202607220002_model_call_credential_reference.sql`), any non-monotonic
transition, any rewrite of a terminal row, any unsent-terminal disposition other
than `KnownFailed`/`Cancelled`, and any delete; `model_call_pinned_target_fk`
forces every call row's resolved target to equal the turn's pinned target. Why:
the schema backstops the aggregate against any buggy or racing writer, not just
the audited one. Migration `202607290301_model_call_token_usage.sql` adds the
terminal-only usage-field constraints and rejects reported usage on every direct
`Prepared -> Terminal` transition, because that transition proves no send was
authorized. Migration `202608020014_model_call_usage_provenance.sql` adds the
non-null closed provenance column with `reported` as the existing-row and
current-writer value. It rejects an unknown spelling and prevents provenance
rewrites except when a nonterminal call and its usage become terminal in the
same update. The same migration leaves the input semantic null on existing rows,
establishes cache-exclusive as the default for later inserts, and rejects every
rewrite after insertion.

The provider target is pinned as a turn-level fact before any call exists: the
turn's frozen selection resolves through an immutable configured
`ModelTargetCatalog` to one exact `ResolvedProviderTarget`, and every call in
the turn must use it (the pin FK above is the durable form of "must").
Resolution failure pins nothing, creates no call, and atomically fails the
attempt and turn. Why: pinning before the first `ModelCallId` prevents a mutable
alias or deployment change from being smuggled into a turn as recovery.

## Aggregate and reconstitution

`ModelCallExecution` (`crates/domain/src/model_execution.rs`) is the
purpose-specific aggregate: one active accepted-input turn in its `Running`
phase plus the one call owned by its current turn attempt. Earlier rounds remain
durable transcript/request/result history. Reconstitution is fail-closed: it
rejects a non-running phase, session or snapshot mismatches, frontier entries
that do not exactly back ordered membership, missing or unreferenced origin
content, a call whose turn/attempt/frontier/selection/target contradict the
checked turn facts, more than one call, and any attempt/call state pair outside
`(Prepared, none)`, `(Prepared, Prepared)`, `(Running, Prepared)`,
`(Running, InFlight)`, or the proof-bearing
`(StopRequested, CancellationRequested)` pair. A tool continuation may use
`(Prepared, Prepared)` when no request was physically authorized, or
`(Running, Prepared)` after execution began; either continuation must prove that
the exact stored call frontier includes the current tool round's complete result
evidence. The stopped pair must reconstruct the exact applied-interrupt proof
retained by the attempt before it can authorize cancellation observation or
restart recovery. Why: acting on a partially consistent projection could
authorize a second provider effect against stale authority, so every invalid
shape refuses rather than repairs. Sealed constructors (compile-fail-tested)
prevent forging call records or terminal history outside the aggregate
(INV-002).

Reconstituting a checkpointed `Prepared` call also reloads the call's exact
stored snapshot, not only the turn's starting snapshot. When steering extended
that frontier, the complete acceptance tail must reconstruct every consumed
input in acceptance order; the corresponding suffix must contain exactly one
`SteeringAcceptedInput` entry per receipt, correlated to the input, source turn,
and current call, and every referenced input's checked content must be present.
The call becomes resumable only when that extended snapshot is a strict
prefix-preserving extension of the starting snapshot and its complete ordered
membership equals those checked semantic entries. Why: checkpointing cannot
erase steering that the durable call was prepared to observe.

Scheduling projection reconstitution independently reloads every consumed
input's stored session, lifecycle, acceptance position, source turn, and
consuming call. Each fact must have exactly one matching
`SteeringAcceptedInput`; the call must belong to that source turn and lifecycle,
and its snapshot must equal the turn's starting snapshot plus the complete
acceptance-ordered steering suffix. Terminal response-frontier validation uses
that checked call snapshot as its prefix. Why: every adapter reaching the domain
seam must reject cross-wired steering history, even when its storage schema has
already performed the same correlation.

## Frontier rendering

`PreparedModelOperation::render` (`crates/application/src/model_execution.rs`)
first applies the context-compaction projection to the exact complete frontier.
If summaries exist, the latest summary is first and every entry after its exact
through-boundary follows; the selected summary is omitted from its later
physical position. Otherwise the complete order is unchanged. Malformed range or
append provenance fails closed. The resulting order becomes provider-neutral
messages:

- `OriginAcceptedInput` renders as a user-role message with its checked accepted
  input parts in order; text remains exact text and each attachment becomes the
  exact bounded textual stub owned by
  [blob storage](blob-storage.md#attachment-visibility-and-model-reads);
- `SteeringAcceptedInput` renders the referenced accepted input through that
  same ordered part projection;
- `ModelIdentityChanged` renders as the structured provider-neutral identity
  change retaining the exact selected-model UUID and bound session-defaults
  epoch; the provider bridge later projects it as an injected user-role message
  with the fixed `Signalbox session event: your model identity is now` prefix
  (INV-046);
- `ContextSummary` renders as a distinct user-role prior-conversation summary,
  retaining the producing compaction call and exact summarized range in the
  provider-neutral value;
- `RunnerPlacementChanged` resolves its complete same-session successor
  placement record and renders as a structured provider-neutral placement change
  retaining the positive placement revision and selected sandbox profile. The
  provider bridge projects one of two exact injected user-role messages. For
  `workspace-restricted` it emits
  `Signalbox session event: runner placement changed to revision {revision} with profile workspace-restricted; the prior placement can no longer execute. The successor writable root and working directory are now active. Relocation did not delete prior files; they may still exist, but only paths exposed inside the successor restricted workspace are reachable.`
  For `ambient` it emits
  `Signalbox session event: runner placement changed to revision {revision} with profile ambient; the prior placement can no longer execute. The successor working directory is now active. Relocation did not delete prior files, and they may remain reachable at their previous paths through the invoking user's filesystem; check before recreating or overwriting them.`
  The braces are replaced by the canonical decimal revision. Missing, stale,
  cross-session, or non-successor placement authority fails rendering instead of
  inventing text. The same profile-specific text renders every relocation,
  including a working-directory move on the same runner and a later
  user-directed move of a healthy session
  ([runner protocol and placement](runner-protocol.md#committed-functionality-beyond-version-one)).
  What is genuinely unavailable is authority to execute through the retired
  placement; the old path is no longer the active working directory or writable
  root. Physical files are not reported lost: a restricted successor exposes
  only its own namespace, while an ambient successor may still expose an old
  path, particularly after a same-runner move. Why: reporting deletion or
  inaccessibility the relocation did not enforce can cause the model to recreate
  or overwrite work that still exists;
- `AssistantText` renders as an assistant message retaining its producing-call
  provenance;
- imported `Text` with an attested value renders with its imported user or
  assistant speaker and exact decoded text, retaining the imported-entry
  reference rather than a native accepted-input or producing-call identity; a
  text block with typed value absence is skipped;
- imported `SourceEvent`, `SourceMessageBlock`, `MessageContentAbsent`,
  `ToolCall`, `ToolResult`, `Thinking`, `RedactedThinking`, and `Document`
  entries are skipped by the first conservative renderer while remaining in the
  exact context frontier;
- `TurnFailed`, `TurnCompleted`, and `TurnCancelled` markers are skipped — they
  delimit history and carry no model-visible content;
- `AssistantToolUse` and its proposal-ordered result entries render as paired
  assistant tool calls and user tool results after resolving their referenced
  request, attempt, and decision records through [tool-loop](tool-loop.md).

The renderer admits the delegation semantic variants with these mappings:

- `DelegatedTask` renders as a structured provider-neutral delegated-task
  message retaining the child, parent session and turn, and exact spawning
  request. The provider bridge emits one injected user-role message with exact
  prefix `Signalbox delegated task:\n` followed by the checked task bytes. The
  transport role does not create an accepted input or `Actor::User`; the
  structured value retains model/tool-authored spawn provenance;
- `DelegationMessage` renders as a structured provider-neutral session event
  retaining the relationship, message, sender, recipient, and recipient-wide
  delivery sequence. The provider bridge emits one injected user-role message
  with exact prefix
  `Signalbox delegation message from session {sender_session_id}:\n` followed by
  the immutable message content, replacing the braces with the canonical UUID.
  The transport role does not reclassify the sender as the user;
- a foreground `DelegationResult` resolves its exact `await_session` request and
  renders through the ordinary paired tool-result path. A returned result uses
  the delivered bytes; another outcome uses the compact closed
  outcome/reason/provenance JSON defined by the delegation process contract: one
  object whose members appear in the exact order `outcome`, `reason`,
  `provenance`, encoded without insignificant whitespace. A background result
  instead renders as a structured provider-neutral session event retaining its
  awaiting request and recipient-wide delivery sequence. Its injected user-role
  form is
  `Signalbox background child result from session {child_session_id}:\n{content}`
  for returned content, or
  `Signalbox background child outcome from session {child_session_id}: {compact_json}`
  for another outcome. Braces are replaced by the canonical child UUID, exact
  returned bytes, or that same exact compact JSON respectively. These transport
  messages are neither accepted input nor child transcript access.

The prepared model operation carries one immutable `ExecutableToolSnapshot`, not
the unfiltered process registry. Preparation includes every daemon-only tool;
includes a combined-locus tool whenever its daemon executor is available; and
includes a runner-only tool only when the session placement can bind that exact
declaration to current execution authority. A pinned placement uses its frozen
tool inventory and current matching registration. An ordinary unpinned request
includes a runner-only definition only when a currently live registration
satisfies its selector, sandbox, workspace, repository, and credential
availability.

The snapshot is a function of the session's actual composition, not of the
compiled registry. A declaration whose arguments, paths, or working directory
are defined relative to a session repository is included only for a session that
has a repository worktree. A declaration whose session capability itself
requires a credential profile is included only for a session that was granted
one. When a remote declaration's credential requirement comes from a repository
entry instead, preparation compares that entry's advertised optional profile to
the session's optional selection: absence equals absence and means anonymous
access, while a present name must equal the exact grant. For a declaration that
operates on the session's repository worktree, that entry is the exact
repository key recorded by the workspace manifest; no other matching key can
satisfy it. For `git_clone`, which introduces a repository into a writable root
rather than operating on an existing worktree, any advertised entry with the
matching optional profile satisfies preparation. No absent/present pair matches,
and absence never selects a credential. A session composed without a workspace
therefore advertises exactly the tools that can execute in it and no placement
combination is rejected merely for being workspace-free
([runner protocol and placement](runner-protocol.md#session-composition) owns
the composition axes, and
[tool-loop](tool-loop.md#registry-placement-and-effect-metadata) owns which
declarations carry a workspace requirement). Why: advertising a tool that cannot
be admitted at lease claim spends a model round to learn what preparation
already knew, and honest advertisement is cheaper than a late refusal. An
exact-identity selector binds that runner and registration revision for a
possible first dispatch, so its loss produces `RunnerLostBeforePin`. A
capability-class selector freezes the class and required availability, not a
runner identity; the eventual first dispatch may select only a then-current
satisfying registration. If none remains, the proposal closes known-failed as
`ToolUnavailableBeforePin` without creating an attempt or placement, because no
runner execution was authorized. `RunnerAbandoned` exposes daemon-executable
tools only. `RunnerLost` and `RunnerLostBeforePin` cannot prepare a new model
operation while the turn awaits user recovery. An operation prepared before loss
retains its frozen snapshot and physical-call disposition, but a runner-only
proposal from it cannot authorize against the lost locus. A combined-locus
definition remains executable through its daemon locus when runner availability
disappears; an already frozen runner selection never silently falls back after
the provider returns.

Each snapshot entry binds the exact model definition, permission/effect policy,
and selected executable locus used to validate and authorize a returned
proposal. The runtime bridge maps only these entries to provider tool
definitions and accepts `ToolCall` completion parts only with a matching
`ToolUse` finish reason. A tool absent from the snapshot is an unknown proposal
for that operation even if another session or a later registration can execute
it. Provider-native tool types remain inside the bridge.

Every message keeps its source-qualified semantic-entry reference and its
content-authority provenance. Why: inherited entries need not come from a native
turn in the current session, so role and provenance derive from the entry
itself, never from turn grouping; imported record is never flattened into native
execution evidence (INV-038). The runtime bridge then maps these to provider
wire messages; provider types never cross the application boundary (INV-002;
layering rules in [runtime-substrate](runtime-substrate.md)).

Why imported non-text is initially skipped: source administrative events and
content absence are not conversational messages; emitting an imported tool call
or result through the native tool-message vocabulary would imply Signalbox-owned
tool identities and execution evidence that do not exist; exposing imported
thinking would reveal source-private reasoning; and the provider-neutral request
has no admitted media projection. Skipping affects only model visibility. It
does not remove, rewrite, summarize, or reorder the semantic entries or their
addressable imported frontier. A richer projection requires a later foundation
decision.

## Compaction calls and triggers

Summary production uses a dedicated physical model call with its own durable
`Prepared`, `InFlight`, and terminal lifecycle. The record pins the session's
current direct selection, resolved provider target, complete source frontier,
non-secret credential reference, terminal disposition, and the provider's
independently optional input, output, cache-creation-input, and cache-read-input
token fields. Only a completed call may produce a `ContextSummary` and
compaction result. Its completion content folds into the summary under the same
content rule the bridge applies to ordinary assistant content
([provider observation classification](#provider-observation-classification)):
text parts concatenate in order and an empty thinking block is dropped, while
thinking with actual text, redacted thinking, and tool calls fail the summary
closed. The compaction request configures no thinking display, so the empty
thinking block is a default-path shape rather than an exceptional one. The
compaction prompt is a required bounded deployment value in the model-catalog
configuration, not a source-code literal; the ordinary session system prompt is
not substituted for it.

Authorization, failure, and completion take the same per-session row lock used
by guarded session mutation. Each transition first rereads its exact call and
command lifecycle: an equal `InFlight`, failed terminal disposition, or complete
summary/result is a successful replay, while a different terminal fact fails
closed. The daemon retries database and ambiguous-commit outcomes at this seam;
it does not start provider interaction until authorization is resolved. The
dormant automatic preparation path retries transient database failures while
loading its selected transcript range, retaining the live `Prepared` call as
provably unsent rather than consuming that queued turn's sole automatic attempt.
The daemon does not invoke that path. An integrity failure still terminalizes
the unsent call if a future scheduler admits it. After a successful provider
result, the daemon retains the summary and its usage in memory until the exact
completion is durably applied or replayed.

The explicit `compact_session` request names a session and an optional semantic
transcript position. Absence selects the latest safe terminal or pre-call
boundary. The command records no projection preference: once its summary result
commits, later model inputs in that session follow the projection rule. Command
replay returns the same compaction identity, call, exact through position,
summary entry, and result frontier.

Preparation also rejects a freshly minted summary-entry, result-frontier, or
compaction identity that already names a durable record, so the daemon remints
and retries before any provider interaction exactly as it does for a colliding
call identity; the rejected claim rolls back, leaving the user-global command
reusable. A uniqueness violation observed later, while applying the completion,
is a decided fact rather than a retryable database failure: the completion fails
closed and its in-flight call is left to startup recovery, because the prepared
identities are pinned by then and every identical retry fails the same way.

Every catalog model selection declares required positive `max_output_tokens` and
`context_window_tokens`; configuration is invalid when the output ceiling
exceeds the context ceiling. Both are operator-declared per selection and never
inferred from provider or model names. Adapters with a provider setting surface,
including Anthropic, send the configured output ceiling in the provider request.
Codex CLI instead renders the ceiling as model-visible advisory context because
the CLI exposes no provider-side control. After a nominal completion, the daemon
retains adapter-reported usage and changes the observation to `KnownFailed` when
reported output exceeds `max_output_tokens`, or when the reported
input-plus-output lower bound exceeds `context_window_tokens`. Missing usage
fields remain missing and are never invented. Adapters need no separate counting
operation, and the daemon performs no automatic pre-activation compaction;
explicit compaction remains available.

The explicit trigger uses the same compaction transaction and provider-call
lifecycle. An explicit command first resolves its user-global replay state; an
equal applied command returns its original receipt even when the current
deployment no longer resolves the original selection or compaction credential.
Configuration and credential resolution occur only for an unseen command.

Provider interaction remains outside database transactions, and the summary
range loader selects only the exact source-qualified range fixed by the Prepared
call rather than materializing the complete physical transcript. Restart treats
a completed summary boundary as ordinary validated frontier evidence. Startup
atomically terminalizes a standalone Prepared compaction call as `KnownFailed`
and an InFlight call as `Ambiguous`, marks its exactly correlated pending
command failed, and produces no summary or result frontier. Any missing,
duplicate, or mismatched command/call correlation fails closed.

## Staged execution

`ModelCallExecutionService::execute` runs one linear invocation over five
composed roles (prepare, capability, authorize-send, provider,
commit-observation) plus an id generator and a dispatch gate. No database
transaction is ever open across credential I/O or provider work.

The two off-transaction provider roles share one call-scoped
`CancellationSignal`. It resolves when an authoritative reload finds the exact
call `CancellationRequested` or terminal: direct cancellation of a prepared call
therefore releases blocked capability preparation, while issued-call
cancellation reaches provider invocation. Capability preparation reports this
signal as `Cancelled`, and the application returns `NoWork`; it never converts
authoritative cancellation into the guarded known-failure closure for a call
that may already be terminal (INV-037).

**SPEC PROPOSAL — parent-cascade cancellation.** The same poll treats an exact
delegation logical-terminal proof as authoritative cancellation even though the
retained model-call row may still say `Prepared` or `InFlight`. Capability work
then returns `NoWork`; invocation cancellation reaches the provider. If a
provider response wins physically but its observation transaction reloads after
the parent cascade committed, the transaction discards that response and the
application returns `NoWork`. It never derives a second turn outcome, overwrites
the delivered child result, or substitutes provider provenance for the parent
command. This proposal is accepted with the implementing stack's merge.

1. **Prepare transaction.** Locks the session, reconstitutes the aggregate, and
   either: reports no runnable work; creates and commits the exact `Prepared`
   call with its pinned non-secret credential reference
   ([configuration-and-credentials](configuration-and-credentials.md)), the
   turn-target pin, and a `ModelCallTransition` (`Prepared`) outbox event. If
   pending steering exists, the same transaction first consumes every eligible
   input in ascending acceptance position, appends its correlated semantic
   entry, and derives the exact extended frontier supplied to the call. It then
   stops the invocation (`Checkpointed`); reloads an already-committed
   `Prepared` call read-only and returns its request material (`Ready`); closes
   target-resolution failure as an atomic no-call attempt-and-turn failure. A
   new `Prepared` call is never advanced to `InFlight` in its creating
   transaction. Why: committing durable call identity before any external step
   means a crash can never produce a provider effect with nothing durable to
   classify.
2. **Capability preparation (no transaction).** The provider adapter resolves
   its credential internally from the call's durably pinned reference (reloading
   a `Prepared` call without one fails closed) and builds an opaque, one-shot,
   call-bound send capability; application and domain code only move the value
   and cannot inspect, persist, or log it (INV-035;
   [configuration-and-credentials](configuration-and-credentials.md)). Why: a
   nonserializable one-shot value makes credential escape and capability reuse
   structurally impossible rather than a review convention. Preparation races
   the shared cancellation signal above. A trustworthy ordinary failure here
   commits the accepted `Prepared -> KnownFailed` closure with attempt and turn
   failure in a separate guarded transaction. A deterministic adapter defect is
   not provider evidence, but before raising its fatal operator signal the
   application commits the same guarded unsent known-failure closure as an
   infrastructure preparation failure. Only failure or ambiguity of that closure
   leaves `Prepared` for startup to validate and retry; a successfully recorded
   defect cannot terminate every later incarnation on the same call.
3. **Attachment preparation (no transaction).** Before send authorization, the
   application checked-sums the catalogued lengths of every distinct attachment
   represented across the complete rendered request. A sum above
   `blob_storage.max_blob_bytes` returns
   `AttachmentPreparationFailure::TooLarge { maximum_bytes }` and closes the
   unsent call, attempt, and turn as known failure before store I/O or
   authorization. Otherwise it streams and verifies at least one recorded
   replica for each digest. It retains no blob bytes and holds no database
   transaction during store I/O. No matching recorded replica returns
   `AttachmentPreparationFailure::Missing`; every readable candidate failing
   verification returns `AttachmentPreparationFailure::Corrupt`. Either guarded
   failure transaction closes the still-unsent call, attempt, and turn as known
   failure without provider cause. When no candidate verifies and at least one
   remains temporarily unavailable, `AttachmentPreparationFailure::Unavailable`
   is a sanitized operator failure: it releases all store and preparation
   resources, leaves the call `Prepared`, commits no turn outcome, and permits a
   later execution pass to retry the same unsent call. It is an expected
   nonfatal deferred execution result and is never routed to
   `FatalExecutionSupervisor`. Authoritative cancellation aborts store I/O,
   returns `NoWork`, and never substitutes an attachment failure for the
   cancellation closure. Reusing a successful check through the bounded
   turn-scoped verification inventory is committed unimplemented functionality
   until a blob-store adapter supplies the immutable-generation token required
   by [blob storage](blob-storage.md#wire-vocabulary); current later ranges
   therefore reverify.
4. **Authorize-send transaction.** After acquiring the process-shared
   per-attempt dispatch gate, a distinct transaction reloads authority and
   commits `Prepared -> InFlight`. A `Prepared` owning attempt moves
   `Prepared -> Running`, whether it is the turn's initial attempt or a
   denial-only tool continuation. A tool-continuation attempt that already
   entered `Running` while executing its batch remains there. The same
   transaction appends a `ModelCallTransition` (`InFlight`) outbox event — every
   durable physical transition, not just the terminal one, is externally
   observable atomically with its commit. The gate permit is retained into the
   send and released at the runtime's first report that provider acceptance is
   possible (`SendCommenced`); if no acceptance report ever arrives, it is
   released when the provider interaction returns, and the
   ambiguous-authorization reread paths drop it before returning. Why: holding
   the gate across the authorize commit and send start serializes
   execution-service passes for that attempt across the acceptance-capable
   boundary; it does not serialize interrupt application.
5. **Provider interaction (no transaction).** The provider port is invoked at
   most once per invocation, and exactly once only after the `InFlight` commit
   is known. It consumes the capability exactly once and returns one
   provider-neutral terminal observation bound to the sealed issued correlation
   (session, turn, attempt, call, target, frontier). Its runtime
   `CancellationSignal` is the shared durable signal defined above.
6. **Commit-observation transaction.** A fresh transaction reloads and
   revalidates complete authority — it never trusts the pre-send projection —
   checks the observation's correlation against fresh state, and atomically
   commits the call disposition, attempt and turn transitions, semantic entries,
   terminal frontier, and outbox rows. If the frozen credential-pool policy
   derives any durable effect from that observation — a profile quarantine, a
   pending session displacement, a membership exclusion, or the chain exclusion
   that removes the failed member from this turn's availability-successor chain
   — the transaction reloads the immutable policy identity pinned by that
   `Prepared` call rather than the session's current credential-history head. It
   commits every derived record with the observation's exact correlation, in the
   same all-or-nothing transaction as the terminal evidence and the successor or
   wait disposition; no side may commit alone. The chain exclusion is included
   for the same reason as the rest: without it a crash between the observation
   and a later release could readmit the profile whose failure parked the turn.

Failure keeps its stage: `ModelCallExecutionError` names which of prepare,
render, capability, attachment preparation, preparation-failure commit,
preparation-failure reread, authorization, authorization reread, authorization
reconciliation, provider, or observation commit failed.

### Identity minting and commit ambiguity

The application owns all candidate identity minting (UUIDv7); persistence uses
or discards candidates but never mints its own. Fixed-count call, entry, and
frontier candidates are minted immediately before each port call. Inventories
knowable only under an authoritative lock use application-owned generator
closures: initial preparation draws one steering semantic-entry candidate and
one fallback reclassified-successor candidate per pending input, while terminal
closure and startup recovery draw one reclassified-successor candidate per
pending input. Persistence invokes those closures inside the transaction but
never owns minting. Why: the locked pending count moves into the transaction
without moving identity authority into persistence. A proven daemon-minted
identity collision (unique-violation rollback on the call, entry, frontier, or
reclassified-turn key) is the only same-invocation transaction retry, with fresh
candidates and no repeated credential or provider work. Why: a proven
unique-violation rollback is the one failure that guarantees the transaction had
no effect, so retrying it cannot duplicate anything.

Commit ambiguity has an explicit detection rule (`commit_failure_is_ambiguous`,
`crates/persistence/src/lib.rs`): a database error with SQLSTATE 08007 or 40003,
or any non-database error while awaiting `COMMIT`, is ambiguous; a
server-rejected commit is a plain non-ambiguous failure. The identity
constraints are immediate, so their unique violations surface during statement
execution. `ModelCallRepositoryError::from_database` checks those named
constraint violations before generic database or commit-ambiguity
classification, preserving identity collision as the only retryable failure.

Ambiguous commits are never resolved by replay:

- An ambiguous prepare-stage commit fails the invocation; authoritative state
  must be reread before any later action.
- An ambiguous authorize-send commit triggers a read-only reread: if the call is
  still `Prepared`, the capability and permit are discarded and the error
  returned; if `InFlight` committed, the unconsumed capability is proof of
  non-send, and the service commits a `KnownFailed` observation for the issued
  call without ever sending; if an interrupt concurrently committed
  `CancellationRequested`, the same unconsumed capability proves no send, the
  stop remains authoritative, and the service commits the correlated `Cancelled`
  observation instead; if the interrupt already terminalized the unsent call as
  `Cancelled`, the complete proof-bearing closure is authoritative and the
  service returns `NoWork`.
- A failed terminal-observation commit retains the unchanged observation in
  memory. A later pass rereads durable state first: `Pending` recommits the
  identical observation; `AlreadyCommitted` (same disposition and content)
  discards it. Any drift in correlation or content is rejected.

### One call, one physical interaction

Per durable authorization, at most one physical interaction may reach the
provider-acceptance boundary. Storage backstops single-call-ness independently
of the aggregate: `model_call_attempt_once UNIQUE (turn_attempt_id)` admits at
most one call row per attempt against any buggy or racing writer. There is no
automatic retry after a known failure and no automatic retry of an ambiguous
outcome (INV-025, INV-026); a known failure fails the attempt and turn unless
its pool authorizes an availability successor against a *different* eligible
profile ([availability successor calls](#availability-successor-calls)) — the
`terminal` and `successor` rows of
[the credential-availability machine](credential-availability.md#the-credential-availability-machine),
which states the four ordered gates that send a failure to the first rather than
the second, the first gate it fails deciding — and ambiguity parks the turn for
recovery. That exception is substitution, never repetition: no path re-issues a
call against the profile that failed. A later scheduler pass never treats an
issued unclassified call as fresh authorization. Why: a lost acknowledgement
cannot prove the provider did not act, so repetition risks undisclosed duplicate
provider effects and spend; honest ambiguity is preferred to an invented
exactly-once claim.

### Availability successor calls

The rule above governs repetition: one durable authorization never reaches the
provider twice. It does not govern substitution of the credential that failed,
which is the `successor` ending of
[the credential-availability machine](credential-availability.md#the-credential-availability-machine).
A `KnownFailed` call whose cause is one of the three availability causes —
`provider_quota_exhausted`, `provider_rate_limited`, or `provider_overloaded` —
and whose pool configures `switch_now` for that cause may be followed by a
*successor call*: a distinct model call, on a successor turn attempt, against
the next admitted member of the same credential pool
([configuration-and-credentials](configuration-and-credentials.md#credential-pools-and-selection)).
This is the `successor` row of
[the credential-availability machine](credential-availability.md#the-credential-availability-machine),
which owns every other projection of it; this section owns the call's own
mechanics.

That framing is what makes this compatible with the accepted rules rather than
an exception to them. The predecessor stays terminal and stays `KnownFailed`;
nothing reclassifies it, and its pinned target, pinned credential reference, and
reported usage remain exactly what it recorded. The successor pins the same
resolved target and a different credential reference, so no call changes
identity mid-flight and INV-018 is untouched. Because the successor belongs to
its own attempt, `model_call_attempt_once UNIQUE (turn_attempt_id)` still admits
exactly one call row per attempt and needs no relaxation; the attempt chain is
the one intra-turn tool rounds already create.

An observation that admits this path ends the predecessor attempt as
`KnownFailure` but does not terminalize the turn. Under the session-scheduler
lock, its atomic observation transaction applies the frozen `switch_now` action
and either prepares the successor attempt and call or enters the pool's
exhausted or contended disposition. It appends no `TurnFailed`, creates no
terminal frontier, and does not reclassify pending steering while a successor or
wait retains the active turn. A concurrently accepted stop is serialized by that
same lock: when its applied-interrupt proof already exists, the known failure
follows the ordinary stop-requested terminal path and no successor is created;
when the observation wins first, the later stop targets the newly active
successor. One commit can therefore never both terminalize the turn and
authorize a successor.

Three causes qualify and no others. Refusal never qualifies: it is provider
judgment about the request, so another account would refuse the same content and
substituting one would be shopping for a different answer. Ambiguity never
qualifies (INV-025): a lost acknowledgement cannot prove the provider did not
act, so a successor could duplicate both an effect and its spend. Credential
resolution failure and `provider_credential_rejected` never qualify: both are
deployment misconfiguration, and moving to another account hides the account
that is broken. For each admitted availability cause, the adapter supplies
distinct typed evidence that the request was not accepted; classification as
quota exhaustion, rate limiting, or overload alone is insufficient. Every other
known failure keeps the behavior above, failing its attempt and turn.

The chain is bounded by the pool. A member that produced a qualifying failure is
excluded from the current availability-successor chain, so at most one call per
member exists per chain and the longest possible chain is the pool's member
count. A successful call ends that chain before any tool-round continuation is
prepared. A parked wait does **not** end it: the wait is part of the chain that
entered it, and releasing the wait resumes that same chain and recomputes
admission. That is forced by the release contract rather than chosen here — the
wait-release origin carries the predecessor call and its non-acceptance proof
exactly where this chain had already observed a qualifying failure, so a
successor prepared at release is that failure's authorized successor, and the
`wait-transition fail (after call)` ending exists for the release that finds the
pool exhausted instead. A release that began a fresh chain could carry neither.
The chain bound survives this because a qualifying failure excludes its member
for the rest of the turn, not merely for the chain, so no wait can readmit one.
Every durable membership exclusion and profile quarantine is retained as well.

A member that produced a qualifying failure stays excluded for the rest of the
turn, not merely for the chain that observed it. A release therefore never
re-admits a profile whose own failure parked the turn, so this turn issues at
most one availability-successor call per pool member however many times it parks
and resumes. That bound is on availability-successor calls, not on the turn's
provider calls in total: a tool loop still creates its own continuation attempts
and calls, which this rule neither counts nor limits.

Why the exclusion outlives its chain: a one-member pool configured `switch_now`
with `park` would otherwise park on a reset-bearing failure, wake at the
deadline, drop the sole member's exclusion, call the same profile again, and
repeat without bound — an automatic same-profile retry loop, which INV-014 and
INV-018 forbid and which
[model fallback and provenance](../open-questions.md#model-fallback-and-provenance)
explicitly leaves outside accepted policy under its future same-profile retry
question. Nothing readmits such a member within the same turn. A reset passing
releases the wait without readmitting the member that failed, and an operator
clear of the exact predecessor correlation does not readmit it either: a clear
is administrative repair that takes effect from the next turn, deliberately not
a retry command. Making it readmit here would turn `clear_credential_exclusion`
into precisely the same-profile retry that question reserves for a separate
decision, and repeated clears would defeat this bound. When no member remains,
the turn takes the exhaustion path below rather than calling a failed member
again.

When no member remains admissible, which ending the attempt reaches is decided
by whether the exhaustion selects a wait — which `fail` never does and `park`
does only while some exclusion a wake can clear remains — together with whether
this **availability chain** has already issued a call, the chain and not the
turn, since a later tool round opens a fresh chain against a turn that has
already issued calls. Every such ending, and every projection of each, is stated
once by
[the credential-availability machine](credential-availability.md#the-credential-availability-machine).

This page owns one column of
[that table](credential-availability.md#the-credential-availability-machine) —
terminal evidence and cause — and states only that column here. A chain that
already observed a qualifying provider failure carries that last observed cause
and its `ProviderError` evidence. A turn that reached an exhausted pool before
issuing any call instead carries the distinct `credential_pool_exhausted`
preparation cause together with the frozen policy's durable member-exclusion
evidence; it never fabricates provider evidence and never borrows a stale
provider cause, because no provider request was issued for it to have observed.
A parked turn carries no terminal evidence at all: it has not terminalized, and
is not one of this page's terminal outcomes.

**Implemented behavior — typed pool exhaustion.** A sealed
`CredentialPoolExhaustedModelCallTurn` carries the pool identity separately from
the ordinary failed-turn projection. The guarded transition requires an active
turn whose current attempt has no model call, ends that attempt `KnownFailure`,
terminalizes the turn `Failed`, and appends the ordinary `TurnFailed { turn }`
marker. Post-failure exhaustion instead preserves the last member's terminal
provider evidence while returning a distinct pool-exhausted application outcome.
Both forms are durable and cannot be reconstructed as a single account failure.

**Committed unimplemented functionality — process-level exclusion evidence.**
The richer process event at
[process protocol](process-protocol.md#credential-pool-preparation-failure)
remains absent. Its implementing child adds the complete nonempty evidence list
in policy-member order, including exclusion generation or predecessor
correlation and optional reset, without changing the typed domain cause above.

The same child adds the selecting immutable pool-policy identity to every
pool-selected `Prepared` call as an insert-only authorization fact beside its
credential reference. Reconstitution requires that policy to contain the pinned
profile with the expected target adapter and delivery kind. Every observation-
derived trigger action reloads this call-pinned revision, so an explicit session
credential update that commits while the provider interaction is in flight
cannot change the action applied to its result.

Each successor durably records the predecessor call it follows and the cause
that authorized it, so a chain reads as evidence rather than as two calls that
happen to share a turn. A goal-mode turn whose pool exhaustion selects no wait
blocks with the ordinary `execution_failure` reason ([goal-mode](goal-mode.md));
one that selects a wait remains the current goal turn and appends nothing,
because no terminal disposition exists yet. The discriminator is wait selection
and not the configured value, so a `park` pool whose members this turn's own
chain exclusions have all removed blocks like any other failure rather than
staying current forever.

## Provider observation classification

Classification is an adapter contract consuming the full-request-send boundary
([runtime-substrate](runtime-substrate.md)); the daemon never reinterprets SDK
errors by retryability or exception type. The runtime bridge
(`crates/model-provider-runtime/src/lib.rs`) maps the runtime's typed terminal
evidence ([runtime-substrate](runtime-substrate.md) owns how evidence is
derived) to exactly one disposition:

| Terminal evidence                                                            | Disposition   |
| ---------------------------------------------------------------------------- | ------------- |
| `Completed` (supported ordered assistant content)                            | `Completed`   |
| `Refused`                                                                    | `Refused`     |
| `ProviderError` (any kind, incl. rate limit, credential rejection, overload) | `KnownFailed` |
| `ProvenUnsent(CancelledBeforeSend)`                                          | `Cancelled`   |
| other `ProvenUnsent` (proof of no acceptance)                                | `KnownFailed` |
| `CancellationConfirmed`                                                      | `Cancelled`   |
| `BoundaryLoss` (loss after possible acceptance, incl. timeouts)              | `Ambiguous`   |

The bridge maps `Refused` evidence only after every provider-reported identity
passes the provider-target identity rule below. A different-lineage identity
fails the adapter stage closed as `ProviderTargetSubstituted` before terminal
evidence is mapped, including when that evidence is `Refused`. Once identity
validation passes, that refusal evidence arises only from an authenticated
complete exchange by the runtime layer's contract
([runtime-substrate](runtime-substrate.md)), not a condition rechecked here.
Empty text blocks are dropped without creating invalid entries, and so are
thinking blocks whose text is empty — the Claude 5-family omitted-display shape,
which carries only a provider replay signature that no durable representation
could replay. The provider documents the resulting tool continuation as graceful
degradation, disabling thinking for that request rather than rejecting it.
Tool-call parts with a `ToolUse` finish become the normalized proposals owned by
[tool-loop](tool-loop.md); thinking with actual text or redacted-thinking still
fails the adapter stage closed because no durable semantic representation
exists. Scripted providers declare their exact terminal observation; nothing is
inferred from timing or injected I/O errors.

For `Completed`, `Refused`, `ProviderError`, and `BoundaryLoss`, the bridge also
copies the runtime terminal evidence's final absorbed `TokenUsage` fields into
the correlated observation verbatim. This is reported evidence. Classification
does not derive usage from the disposition, content, context, or provider
family. The observation commit stores those fields atomically with the terminal
disposition. A commit-ambiguity reread returns `AlreadyCommitted` only when the
durable disposition, closure, and every independently nullable usage field equal
the retained observation; different or newly absent usage is conflicting
evidence, not an equal replay.

### Provider-target identity

The requested selection, the pinned resolved target, and the provider-reported
identity stay three separate facts ([runtime-substrate](runtime-substrate.md));
the bridge is the one place that relates the third to the second, for every
identity the exchange reported — early observations and terminal evidence alike,
since the rule is timing-sensitive. Exactly one of three relations holds
(INV-014):

- **Exact.** The reported identity equals the configured exact spelling.
- **Alias concretion.** The reported identity is the configured spelling
  followed by `-` and a *dated snapshot qualifier* (`YYYYMMDD` or `YYYY-MM-DD`).
  This is the same logical target named in its canonical concrete form, not a
  mismatch: the daemon configures one family and the provider echoes which
  pinned snapshot of that family served the request. Classification proceeds
  normally and the concrete identity is recorded as sanitized evidence of what
  actually served.
- **Different lineage.** Anything else — another family, a delivery or speed
  variant, or a family-name extension that is not a full date — is a
  *substitution*: the provider served a model the daemon never authorized. This
  is a distinct outcome, never collapsed into the alias case and never into an
  ordinary provider failure. It fails the adapter stage closed today, because
  the durable substitution provenance it would have to record does not exist yet
  (see Open edges).

The relation is derived from the configured target's own family, never from a
table of known provider identifiers, so a newly published model needs no code
change. Requiring a full date shape — rather than any trailing segment — is what
keeps a *version* extension of the same family name from being read as a
snapshot: against a configured `claude-opus-4`, `claude-opus-4-5` extends the
family by one digit and stays a different lineage, while
`claude-haiku-4-5-20251001` against a configured `claude-haiku-4-5` is the same
lineage made concrete.

A provider that documents its own substitution signal feeds that rule rather
than bypassing it: the Anthropic adapter recognizes the server-side `fallback`
content block, reports the model it names as continuing the turn through the
ordinary reported-identity fact, and refuses to treat the response as completion
material at all ([runtime-substrate](runtime-substrate.md)). Two guarantees
follow, and only two. The response can never complete as the resolved target's
output, whatever the block names. And when the block names another lineage — the
case a real server-side fallback produces, including the sticky follow-up turns
that carry no block but do report the substituting identity — the relation above
classifies it as a substitution. A block that names the configured target itself
is a provider self-contradiction: it still cannot complete, but it classifies as
ambiguity rather than substitution, because the substitution classification is
carried by the identity and no durable marker-only evidence exists to carry it
(see Open edges).

### Operator diagnostics

Every classified outcome and every fail-closed bridge defect carries a stable,
sanitized cause code alongside the shared
[operator failure class](runtime-substrate.md#operator-failure-taxonomy): the
class says how bad the failure is, the cause code says what happened. The codes
are fixed tokens — provider response text, request or response bodies,
credential material, and user content can never reach one (INV-035) — and the
runtime's own exhaustive `ProviderErrorKind` classification is carried verbatim
rather than restated, so the adapter taxonomy and the operator vocabulary cannot
drift apart. A provider-reported identity retained for diagnostics is
credential-redacted by the adapter and length-bounded by the bridge before it
can reach a log line. The bridge emits the cause code for each fail-closed
defect, each non-completing classification, each accepted alias concretion, and
each trustworthy capability-preparation failure — the pre-send outcome the
application commits as `KnownFailed`, whose closed
`UnsupportedOperation`/`CredentialUnavailable`/`CredentialUnusable` vocabulary
maps to tokens without its adapter-rendered detail text. A substitution
additionally carries the bounded identity that actually served, so an operator
can name the model the provider used. The runtime crates themselves remain
logging-free ([runtime-substrate](runtime-substrate.md)).

**Committed unimplemented functionality.** No present session surface carries a
structured-output contract. For program-driven turns under the
[program substrate](program-substrate.md), the accepted input records the
program's declared output schema, that schema flows through turn preparation
into the prepared model operation, and the runtime boundary enforces it — the
turn's outcome payload validates against the declared schema or the turn reports
its failure, never an unvalidated approximation. The
[model-runtime substrate](runtime-substrate.md) already admits an optional
per-call structured-output contract; this paragraph constrains the session path
between them: nothing may assume a prepared model operation carries no output
contract, and the prepared-operation shape must stay extensible to the recorded
schema without reinterpreting existing calls.

## Terminal outcomes

`apply_terminal_observation` derives one of seven outcomes from fresh state, and
persistence commits it atomically with its outbox rows
([persistence-protocol](persistence-protocol.md)):

- **Completed without tools.** The call ends `Completed`; the attempt ends
  `TurnCompleted`; ordered assistant text is followed by `TurnCompleted`, and
  the turn terminalizes through the existing final-response all-or-nothing
  boundary.
- **Completed with tools.** The call ends `Completed`; ordered assistant text
  and logical tool-use entries plus their request records commit atomically, the
  attempt ends as a tool-round yield, and the turn stays active. Approval,
  execution, result projection, and preparation of the next call follow
  [tool-loop](tool-loop.md). A physical call completion is therefore never
  treated alone as proof that the logical turn completed.
- **KnownFailed.** The call ends `KnownFailed`; definitive provider-error
  evidence additionally retains only its closed `ProviderErrorKind`
  classification as the optional provider-failure cause — never provider prose.
  An unstopped attempt ends `KnownFailure`. Unless the same atomic observation
  admits the availability-successor path above, the turn fails with a
  `TurnFailed` entry and terminal frontier; an admitted successor instead keeps
  the turn active without either. A stop-requested attempt ends
  `AfterCancellation(KnownFailure)` and still fails, and cannot admit a
  successor; the physical result has not proven cancellation.
- **Cancelled.** Without the exact applied-interrupt proof, a physical
  cancellation is an unstopped known failure. With the exact proof — carried
  directly by the atomic interrupt transition before any call exists or for an
  unsent `Prepared` call, or retained by `StopRequested` for an issued call —
  the attempt ends `AfterCancellation(Cancelled)`, one `TurnCancelled` marker
  extends the starting or call frontier, and the turn ends `Cancelled { cause }`
  rather than failed or ambiguous.
- **Refused.** The call ends `Refused`; the attempt ends `TurnRefused`; the turn
  terminalizes `Refused` atomically with an equal-content terminal frontier. No
  refusal-content entry exists yet (INV-018; open edge).
- **Ambiguous.** The call ends terminally `Ambiguous`; an unstopped attempt ends
  `Ambiguous` (live) or `Lost` (startup), and the turn enters the durable
  `awaiting_model_call_recovery` phase carrying the exact wait set (that one
  call) while retaining the session slot. No semantic entry or frontier is
  created.
- **ReconciliationRequired.** When that same unacknowledged ambiguity has an
  applied-interrupt proof, the attempt instead ends
  `AfterCancellation(Ambiguous)` (live) or `AfterCancellation(Lost)` (startup).
  The turn terminalizes with the exact model-call wait set and
  `InterruptRequiresReconciliation` marker, an equal-content terminal frontier,
  and a typed reconciliation outbox record, releasing the slot. The same result
  applies when an interrupt is accepted after an unstopped ambiguity already
  entered `AwaitingRecoveryDecision`: the terminal call remains unchanged, and
  its ended attempt remains the original `WithoutStop(Ambiguous|Lost)` evidence.
  The exact later interrupt proof is carried by the turn's reconciliation marker
  and correlated accepted successor instead of rewriting that evidence.
  Independently, the turn-liveness runtime can spend one durably claimed
  automatic recovery attempt on an unstopped wait. After revalidating that the
  exact call and ended attempt still own it, the aggregate uses
  `AutomaticModelCallRecovery { attempt }` as its typed reason and commits the
  same equal-content frontier and reconciliation outbox record. This treatment
  does not claim a provider result; the call remains terminal `Ambiguous`.

Completion and refusal races against `StopRequested` end through their typed
`AfterCancellation` dispositions while retaining their ordinary turn outcomes.

Every terminal turn outcome, including proof-bearing reconciliation, atomically
reclassifies each pending steering input into a fresh queued successor turn at
its original acceptance position (`NoSafePointBeforeTerminal`), inheriting the
source turn's effective configuration; see
[turn-lifecycle-and-scheduling](turn-lifecycle-and-scheduling.md) (INV-016).

## Serialization and locking

Every model-call transaction — prepare, capability-failure closure,
authorize-send, observation commit, both rereads, and startup recovery — issues
the per-session `session_scheduler` row lock (`FOR UPDATE`,
`crates/persistence/src/lock_inventory.rs`) as its first statement. The reviewed
lock statement bundles a session-existence probe (startup's variant also reads
the active turn) into the same SELECT, so lock-before-read is guaranteed at
statement granularity, not within the statement. Why: one lock statement issued
first in every transaction makes per-session serialization total and lock-order
cycles impossible. The in-process per-attempt dispatch gate is the only other
ordering primitive; in this slice the execution service is its sole consumer.
Interrupt application deliberately does not acquire it: once `InFlight` commits,
the call is issued work, so a later interrupt durably requests cancellation and
the runtime signal races any provider progress without claiming that acceptance
was prevented.

## Crash, restart, and supervision

signalboxd (`apps/signalboxd/src/lib.rs`, `main.rs`) wraps execution in
`FatalExecutionSupervisor`: a post-activation stage failure — after at most one
same-incarnation reconciliation pass when retained evidence exists — raises a
fatal signal, the scheduler stops (in-flight work bounded by a shutdown grace
window), and the process exits nonzero so the next incarnation's startup scan
regains authority. Why: startup recovery is the one audited path that classifies
an issued call from durable evidence, so a live process that cannot construct a
trustworthy result must stop rather than improvise. An eligibility pass raises
the same signal whenever a durable stage it owns reports
`Infrastructure { commit_ambiguous: true }` from the guarded counted activation
commit, since only that next scan can decide what committed. The scheduled pass
no longer prepares automatic compaction. The connection runtime raises the same
signal through its recovery handle for an explicit compaction command reporting
that class, and still answers the client `commit_ambiguous`: a connection
handler holds no prepared record to terminalize, replay of the command finds it
pending, and a fresh command finds the nonterminal call, so the restart is the
only remedy and nothing else would ask for it. Attachment unavailability is not
a stage failure under this paragraph: it carries no ambiguous durable effect,
returns the nonfatal deferred result above, and leaves the scheduler running. A
deterministic capability-preparation defect first attempts the guarded unsent
known-failure closure above and raises the fatal signal only after that closure
commits or fails; therefore a successful closure leaves no `Prepared` call for
restart to repeat.

Startup recovery (`crates/persistence/src/startup.rs`), inside the same
per-session locked transaction as the general scan (INV-034):

- an evidence-free turn ends its abandoned attempt `Lost`, fails the turn, and
  reclassifies all pending steering instead of deferring startup;
- a durable `Prepared` call proves no send authorization existed. Reconstitution
  validates the call's exact stored frontier; when preparation consumed
  steering, that is the complete extended snapshot and checked steering suffix
  described above, not the turn's unextended starting snapshot. Startup leaves
  the call, attempt, and turn unchanged, and the ordinary scheduler later
  retries preparation of that same unsent call;
- a durable unstopped `InFlight` call with no surviving evidence ends
  `Ambiguous`, the abandoned attempt ends `Lost`, and the turn parks in
  `awaiting_model_call_recovery`, where the independent bounded reconciliation
  runtime takes responsibility after startup;
- a durable `CancellationRequested` call reconstructs its applied interrupt,
  ends the attempt `AfterCancellation(Lost)`, and terminalizes
  `ReconciliationRequired` with that call as the exact ambiguity set.

Recovery is configuration-independent: `require_live_execution_for_restart`
passes no configured catalog and rebuilds target authority from the stored
call's own selection and target facts, so a deployment-configuration change can
never block or alter classification of an issued call. Recovery never itself
resumes an attempt, redispatches a call, or assumes a request was or was not
sent. A retained `Prepared` call is driven only by a later ordinary scheduler
pass, while issued calls still follow the terminal recovery classifications
above.

## Composition and harness

Production composition wires `PostgresModelCallRepository` (all four transaction
roles), the in-process gate, and `RuntimeModelCallProvider` over the
configuration-selected Anthropic HTTP, OpenAI HTTP, Claude CLI, or Codex CLI
runtime, with the domain target catalog, runtime model catalog, and exact
adapter routes built from one versioned static configuration file. Direct HTTP
runtimes reread their credential files; each CLI runtime uses its ambient login,
which is the only delivery the present composition supplies for one. Selecting a
CLI credential from the profile's own delivery is committed unimplemented
functionality owned by
[configuration-and-credentials](configuration-and-credentials.md). The
`signalbox-debug` binary (`apps/signalboxd/src/bin/signalbox-debug.rs`) drives
one session through the real scheduler and PostgreSQL path with either a
deterministic scripted reply or an explicit `--anthropic` smoke mode, then
prints the semantic transcript; it is deliberately not the client protocol.

## Open edges

- Durable provider-target evidence (the designed `ProviderTargetEvidence`,
  mismatch-selects-`KnownFailed`, and post-completion invalidation) is
  unimplemented. Three consequences: an accepted alias concretion records the
  concrete served identity only as operator diagnostics, not as a durable
  per-call provenance row; a substitution fails closed with an operator error,
  so a substituted call is classified `Ambiguous` by restart rather than
  `KnownFailed` live; and substitution is carried entirely by the reported
  identity, so a provider fallback marker naming the configured target itself
  classifies as ambiguity. Carrying the marker as typed evidence in its own
  right would add a provider-neutral runtime-vocabulary variant that both
  adapters would have to construct and redact, and is routed through the
  provider provenance schema in
  [Model fallback and provenance](../open-questions.md#model-fallback-and-provenance).
- Unstopped ambiguity recovery is a parked state only: no user decision,
  `DuplicateRiskAccepted`, replacement call, or outcome-authority transfer is
  implemented. Stop-caused ambiguity terminalizes proof-bearing reconciliation,
  but no later reconciliation workflow is implemented.
- Availability-successor chains are visible only after the fact: no client
  surface renders that a successor is being selected. That transient visibility
  surface remains routed through
  [Model fallback and provenance](../open-questions.md#model-fallback-and-provenance).
- Streaming deltas are collected but never delivered as transient drafts, and
  the designed early-observation pause/commit/resume path is unimplemented.
- The aggregate admits at most one call per turn attempt; the tool loop creates
  continuation attempts and calls in the same logical turn.
- A refused turn commits no refusal-content semantic entry; the variant remains
  an open edge in [sessions-and-transcript](sessions-and-transcript.md).
- Imported source-event, absence, and non-text entries remain model-invisible
  under the conservative projection. Richer rendering remains routed through the
  open [model-input projection](../open-questions.md#model-input-projection),
  whose accepted implementation would update this projection and the frontier
  extension owners in
  [turn-lifecycle-and-scheduling](turn-lifecycle-and-scheduling.md) and
  [sessions-and-transcript](sessions-and-transcript.md);
  [conversation-import](conversation-import.md) continues to own only normalized
  imported source content.
- Same-incarnation retained-evidence reconciliation gets exactly one production
  pass (`reconcile_retained_once`) before fatal escalation; repeated
  same-incarnation drains are exercised only by tests.
- The daemon/session system prompt remains sourced only from the calling turn's
  frozen defaults epoch: preparation loads that optional bounded prompt and the
  bridge sets `ModelOperation::system` from it exactly or to `None`
  ([sessions-and-transcript](sessions-and-transcript.md)). **Committed
  unimplemented functionality — workspace-instruction region.** The
  instruction-admission slice adds a separate optional typed
  `WorkspaceInstructionRegion` to `PreparedModelOperation` and carries it
  unchanged into `ModelOperation::workspace_instructions`. Preparation rebuilds
  it from the exact manifest-backed admitted bytes, inserts it once after system
  policy and before conversation history, and authenticates its manifest before
  provider spawn. It is never concatenated into `system`, converted to a user or
  tool message, or sourced from an adapter loader. The region's exact bytes and
  subordinate authority are owned by
  [workspace instructions](workspace-instructions.md); richer composition of
  other system-prompt sources remains deferred under
  [configuration categories](../open-questions.md#configuration-categories).

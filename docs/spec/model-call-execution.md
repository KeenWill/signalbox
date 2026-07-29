# Model-call execution

This page describes the implemented model-call orchestration chain as verified
against the implementing stack through PR #201 (`agent/tool-loop-proof`):
rendering a context frontier into provider messages, the staged prepare /
authorize-send / commit-observation effects, assistant content, intra-turn tool
rounds and final turn completion, provider failure classification into physical
dispositions, and the retry prohibition. Tool requests, approvals, attempts, and
continuation are owned by [tool-loop](tool-loop.md). Turn and attempt lifecycle
law lives in [turn-lifecycle-and-scheduling](turn-lifecycle-and-scheduling.md);
semantic entries and frontiers in
[sessions-and-transcript](sessions-and-transcript.md); storage protocol and the
outbox in [persistence-protocol](persistence-protocol.md); the typed
model-runtime layer in [runtime-substrate](runtime-substrate.md); daemon
startup, scheduling, and shutdown composition in
[turn-lifecycle-and-scheduling](turn-lifecycle-and-scheduling.md); and model
configuration and credentials in
[configuration-and-credentials](configuration-and-credentials.md). The
`apps/signalboxd` supervision and `signalbox-debug` code homes this page names
were verified through PR #258 (`agent/signalboxd-rename`); the
[provider-target identity](#provider-target-identity) rule and the sanitized
model-call cause codes were verified through PR #280
(`agent/provider-identity-normalization`). The complete frontier-payload
projection and identity-before-terminal-evidence precedence were verified
through PR #288 (`agent/audit-fix-docs-coherence`); the session system prompt on
the prepared operation was verified through PR #286
(`agent/session-system-prompt`). Provider-reported token evidence retention and
exact commit-ambiguity comparison were verified through PR #301
(`agent/token-usage`); the empty-thinking completion rule was verified through
PR #305 (`agent/sonnet-streamed-tool-use`). The context-summary projection and
dedicated compaction-call evidence were verified through this PR
(`agent/context-compaction-core`); the version-twenty-two explicit trigger,
pre-activation context guard, configured prompt, and provider-native input
counting were verified against `agent/context-compaction-protocol`. The
runner-placement rendering and executable session-tool snapshot paragraphs are
the foundation proposal at the bottom of their implementing stack and become
verified only with those child pull requests. Invariant tags cite
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

The same terminal transition stores the provider's four token-usage fields —
input, output, cache-creation input, and cache-read input — on the `model_call`
row. Each field is independently nullable: null means the provider did not
report that field, while a reported zero remains zero. Calls closed from
`ProvenUnsent`, `CancellationConfirmed`, capability failure, or restart recovery
have all four fields unreported because no provider usage evidence exists.
Historical rows likewise remain unreported. The terminal-row immutability rule
makes this evidence write-once; no later path estimates, normalizes, or corrects
it.

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
authorized.

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

- `OriginAcceptedInput` renders as a user message with its checked accepted
  input content;
- `SteeringAcceptedInput` renders as a user message with the referenced accepted
  input's checked content;
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
  provider bridge projects it as an injected user-role message whose exact text
  is
  `Signalbox session event: runner placement changed to revision {revision} with profile {profile}; prior runner-local execution state is unavailable.`
  The braces are replaced by the canonical decimal revision and exact
  `workspace-restricted` or `ambient` token. Missing, stale, cross-session, or
  non-successor placement authority fails rendering instead of inventing text;
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

The prepared model operation carries one immutable `ExecutableToolSnapshot`, not
the unfiltered process registry. Preparation includes every daemon-only tool;
includes a combined-locus tool whenever its daemon executor is available; and
includes a runner-only tool only when the session placement can bind that exact
declaration to current execution authority. A pinned placement uses its frozen
tool inventory and current matching registration. An ordinary unpinned request
includes a runner-only definition only when a currently live registration
satisfies its selector, sandbox, workspace, repository, and credential
availability. An exact-identity selector binds that runner and registration
revision for a possible first dispatch, so its loss produces
`RunnerLostBeforePin`. A capability-class selector freezes the class and
required availability, not a runner identity; the eventual first dispatch may
select only a then-current satisfying registration. If none remains, the
proposal closes known-failed as `ToolUnavailableBeforePin` without creating an
attempt or placement, because no runner execution was authorized.
`RunnerAbandoned` exposes daemon-executable tools only. `RunnerLost` and
`RunnerLostBeforePin` cannot prepare a new model operation while the turn awaits
owner recovery. An operation prepared before loss retains its frozen snapshot
and physical-call disposition, but a runner-only proposal from it cannot
authorize against the lost locus. A combined-locus definition remains executable
through its daemon locus when runner availability disappears; an already frozen
runner selection never silently falls back after the provider returns.

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
it does not start provider interaction until authorization is resolved. Before
authorization, an automatic compaction also retries transient database failures
while loading its selected transcript range, retaining the live `Prepared` call
as provably unsent rather than consuming that queued turn's sole automatic
attempt. An integrity failure still terminalizes the unsent call. After a
successful provider result, the daemon retains the summary and its usage in
memory until the exact completion is durably applied or replayed.

The explicit version-twenty-two `compact_session` request names a session and an
optional semantic transcript position. Absence selects the latest safe terminal
or pre-call boundary. The command records no projection preference: once its
summary result commits, later model inputs in that session follow the projection
rule. Command replay returns the same compaction identity, call, exact through
position, summary entry, and result frontier.

Preparation also rejects a freshly minted summary-entry, result-frontier, or
compaction identity that already names a durable record, so the daemon remints
and retries before any provider interaction exactly as it does for a colliding
call identity; the rejected claim rolls back, leaving the owner-global command
reusable. A uniqueness violation observed later, while applying the completion,
is a decided fact rather than a retryable database failure: the completion fails
closed and its in-flight call is left to startup recovery, because the prepared
identities are pinned by then and every identical retry fails the same way.

Every catalog model selection also declares `context_window_tokens` as a
required nonzero integer beside `max_output_tokens`; configuration is invalid
when the maximum output reservation exceeds the context window. The window is
operator-declared per selection and is never inferred from provider/model names.
Before activating an eligible turn, the daemon renders its prospective initial
ordinary call and obtains the exact input-token count from that selection's
provider adapter. The call fits only when that input count plus the selection's
full `max_output_tokens` reservation is at most the declared context window;
checked addition fails closed on overflow. When the requested total exceeds the
window, the turn is not activated and the ordinary call is not sent; the daemon
runs compaction through the latest safe boundary, reloads the resulting complete
frontier, renders and counts again, and proceeds only when the recounted input
plus the same output reservation fits. A compaction result that still cannot fit
fails closed rather than looping or guessing a different limit. The first
automatic prepare durably and immutably associates its compaction command with
the queued turn, and at most one automatic command may name that turn in its
session. A later scheduler pass for the same queued turn therefore fails closed
without issuing another dedicated compaction call when the already-compacted
frontier still exceeds the limit.

Both triggers share the same compaction transaction and provider-call lifecycle.
An explicit command first resolves its owner-global replay state; an equal
applied command returns its original receipt even when the current deployment no
longer resolves the original selection or compaction credential. Configuration
and credential resolution occur only for an unseen command.

Provider interaction remains outside database transactions, and the summary
range loader selects only the exact source-qualified range fixed by the Prepared
call rather than materializing the complete physical transcript. Restart treats
a completed summary boundary as ordinary validated frontier evidence. Startup
atomically terminalizes a standalone Prepared compaction call as `KnownFailed`
and an InFlight call as `Ambiguous`, marks its exactly correlated pending
command failed, and produces no summary or result frontier. Any missing,
duplicate, or mismatched command/call correlation fails closed.

For the automatic guard, the exact call identity used during provider-native
counting is retained. Once the count fits, one scheduler-locked transaction
revalidates the activation, commits it, and creates that exact no-steering
Prepared call. Steering accepted after that transaction remains pending for a
later call and cannot enter the already-counted operation.

## Automatic context compaction

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
   failure in a separate guarded transaction; an adapter defect is an operator
   failure and commits no provider-failure closure.
3. **Authorize-send transaction.** After acquiring the process-shared
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
4. **Provider interaction (no transaction).** The provider port is invoked at
   most once per invocation, and exactly once only after the `InFlight` commit
   is known. It consumes the capability exactly once and returns one
   provider-neutral terminal observation bound to the sealed issued correlation
   (session, turn, attempt, call, target, frontier). Its runtime
   `CancellationSignal` is the shared durable signal defined above.
5. **Commit-observation transaction.** A fresh transaction reloads and
   revalidates complete authority — it never trusts the pre-send projection —
   checks the observation's correlation against fresh state, and atomically
   commits the call disposition, attempt and turn transitions, semantic entries,
   terminal frontier, and outbox rows.

Failure keeps its stage: `ModelCallExecutionError` names which of prepare,
render, capability, capability-failure commit, capability-failure reread,
authorization, authorization reread, authorization reconciliation, provider, or
observation commit failed.

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
`crates/persistence/src/model_execution.rs`): a database error with SQLSTATE
08007 or 40003, or any non-database error while awaiting `COMMIT`, is ambiguous;
a server-rejected commit is a plain non-ambiguous failure. The identity
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
outcome (INV-025, INV-026); a known failure fails the attempt and turn, and
ambiguity parks the turn for recovery. A later scheduler pass never treats an
issued unclassified call as fresh authorization. Why: a lost acknowledgement
cannot prove the provider did not act, so repetition risks undisclosed duplicate
provider effects and spend; honest ambiguity is preferred to an invented
exactly-once claim.

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
the correlated observation verbatim. Classification does not derive usage from
the disposition, content, context, or provider family. The observation commit
stores those fields atomically with the terminal disposition. A commit-ambiguity
reread returns `AlreadyCommitted` only when the durable disposition, closure,
and every independently nullable usage field equal the retained observation;
different or newly absent usage is conflicting evidence, not an equal replay.

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
- **KnownFailed.** The call ends `KnownFailed`; an unstopped attempt ends
  `KnownFailure`, and the turn fails with a `TurnFailed` entry and terminal
  frontier. A stop-requested attempt instead ends
  `AfterCancellation(KnownFailure)` and still fails; the physical result has not
  proven cancellation.
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
`Infrastructure { commit_ambiguous: true }` — the guarded counted activation
commit and automatic compaction preparation alike — since only that next scan
can decide what committed. The connection runtime raises it through the same
handle for an explicit compaction command reporting that class, and still
answers the client `commit_ambiguous`: a connection handler holds no prepared
record to terminalize, replay of the command finds it pending, and a fresh
command finds the nonterminal call, so the restart is the only remedy and
nothing else would ask for it.

Startup recovery (`crates/persistence/src/startup.rs`), inside the same
per-session locked transaction as the general scan (INV-034):

- an evidence-free turn ends its abandoned attempt `Lost`, fails the turn, and
  reclassifies all pending steering instead of deferring startup;
- a durable `Prepared` call proves no send authorization existed; the call ends
  `KnownFailed`, the abandoned attempt ends `Lost`, and the turn fails,
  reclassifying pending steering. Before closure, reconstitution validates the
  call's exact stored frontier; when preparation consumed steering, that is the
  complete extended snapshot and checked steering suffix described above, not
  the turn's unextended starting snapshot;
- a durable unstopped `InFlight` call with no surviving evidence ends
  `Ambiguous`, the abandoned attempt ends `Lost`, and the turn parks in
  `awaiting_model_call_recovery`;
- a durable `CancellationRequested` call reconstructs its applied interrupt,
  ends the attempt `AfterCancellation(Lost)`, and terminalizes
  `ReconciliationRequired` with that call as the exact ambiguity set.

Recovery is configuration-independent: `require_live_execution_for_restart`
passes no configured catalog and rebuilds target authority from the stored
call's own selection and target facts, so a deployment-configuration change can
never block or alter classification of an issued call. Recovery never resumes an
attempt, redispatches a call, or assumes a request was or was not sent.

## Composition and harness

Production composition wires `PostgresModelCallRepository` (all four transaction
roles), the in-process gate, and `RuntimeModelCallProvider` over the Anthropic
runtime, with the domain target catalog and runtime model catalog built from one
versioned static configuration file and a reread credential file
([configuration-and-credentials](configuration-and-credentials.md)). The
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
- Unstopped ambiguity recovery is a parked state only: no owner decision,
  `DuplicateRiskAccepted`, replacement call, or outcome-authority transfer is
  implemented. Stop-caused ambiguity terminalizes proof-bearing reconciliation,
  but no later reconciliation workflow is implemented.
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
- The one system-prompt source is the calling turn's frozen defaults epoch: the
  prepare transaction loads that epoch's optional bounded prompt, rendering
  binds it onto the prepared operation, and the bridge sets the runtime
  operation's `ModelOperation::system` field from it on every call
  (`crates/model-runtime/src/operation.rs`), exactly or `None`
  ([sessions-and-transcript](sessions-and-transcript.md)). Composition from
  additional sources remains deferred under the open
  [configuration categories](../open-questions.md#configuration-categories).

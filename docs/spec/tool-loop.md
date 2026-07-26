# Tool loop

This page specifies the implemented daemon-owned tool subsystem as verified
against the implementing stack rooted at PR #193 (`agent/tool-loop-spec`); the
`signalboxd` name this page states for the catalog-wiring composition root was
verified through PR #258 (`agent/signalboxd-rename`), and the Tier 0 catalog
extension through PR #265 (`agent/tool-batch-tier0`). The Tier 1 code-host
catalog extension is verified through PR #270 (`agent/tool-batch-tier1`), and
the failed-attempt operator event together with the credential-shaped code-host
detail through PR #285 (`agent/dev-instance-code-host-credential`). It owns
logical tool requests, approval policy and decisions, physical tool attempts,
result admission, intra-turn continuation, crash classification, the compiled
registry, and the daemon-local catalog. Turn and attempt lifecycle law lives in
[turn-lifecycle-and-scheduling](turn-lifecycle-and-scheduling.md); semantic
entry vocabulary in [sessions-and-transcript](sessions-and-transcript.md);
model-call staging and provider translation in
[model-call-execution](model-call-execution.md); durable-command identity in
[identity-and-commands](identity-and-commands.md); and relational mechanics in
[persistence-protocol](persistence-protocol.md). Invariant tags cite
[the invariant catalog](../invariants.md).

## Intra-turn rounds and request batches

One turn spans the complete propose → decide → execute → result → continue loop.
A model call is one physical round inside that turn. A completed response with
no tool request appends `TurnCompleted` and terminalizes the turn exactly as
before. A completed response containing one or more tool requests never
terminalizes the turn: it ends the current turn attempt as a tool-round yield
and keeps the active slot while the batch is resolved. A later model call uses a
fresh turn attempt in the same turn. Why: a turn is the logical conversational
outcome, while a model call and a turn attempt are physical tenures that may
repeat without changing that logical identity (INV-004, INV-006).

A completed response carries ordered assistant text and tool proposals. For each
proposal the application supplies one fresh UUIDv7 `ToolRequestId`; the domain
assigns a zero-based ordinal among tool proposals in that producing call. The
producing call, name, normalized arguments, and ordinal form one immutable
`ToolRequest` record. The name is 1–64 ASCII letters, digits, underscore, or
hyphen. `NormalizedToolArguments` has two closed arms. `Json` stores a decoded
JSON value as compact text with object keys in lexical order; `Undecodable`
stores the exact bounded UTF-8 text emitted by the provider adapter after that
adapter applies its preparation-time credential scrub when JSON decoding fails.
Undecodable text must also exclude U+0000, mirroring the result-content
admission. Both arms must fit within 1 MiB before and after normalization. This
preserves malformed arguments as bounded, identity-safe evidence without
pretending they are JSON. An undecodable value, or valid JSON that does not
decode against the selected tool's argument type, becomes a typed execution
error later.

The same transaction that classifies the producing call `Completed` appends one
`AssistantText` or `AssistantToolUse { producing_call, request }` semantic entry
per supported nonempty response part, preserving response order, and inserts
every request record. Empty text blocks are omitted at the provider boundary and
create no semantic entry; tool proposals are never omitted. The request row is
the sole content authority: the semantic entry contains only the call/request
references and never copies the name or arguments (INV-005). Request identity,
call ownership, and ordinal are unique within the producing call, so equal
proposals remain distinct logical requests.

All requests produced by one call are one batch. Approval decisions are resolved
in proposal order, and the turn parks on the earliest undecided request.
Execution does not begin until the batch has no undecided approval. The next
model round does not begin until every request has one durable logical
resolution: executed, denied, or closed by turn end.

## Approval policy and decision sources

Every request has an approval state separate from its execution state. The
implemented decision sources are:

- `OwnerCommand` — one applied owner-global durable decision command;
- `PolicyAuto` — the registry declaration selected automatic approval; and
- `SessionBlanket` — the turn's frozen dangerous blanket posture selected
  automatic approval.

`SessionOverride` and `JudgeRecommendation` are typed additive vocabulary but
have no storage encoding or producer. In particular, an automated source never
constructs `OwnerCommand` or claims owner agency (INV-020).

The implemented precedence below governs daemon-local execution and sessions
without a credential profile. Runner dispatch under a selected credential
profile first resolves the pair posture specified by
[runner protocol and placement](runner-protocol.md#credential-profiles-and-approval):
after the frozen dangerous blanket, pair-level `Automatic` authorizes the exact
pair and `SessionPolicy` or an absent pair requires confirmation. The later
application stack must add a distinct durable credential-policy decision source;
this foundation does not encode one into the current persistence vocabulary.

Daemon-local policy resolution uses this accepted precedence:

1. the frozen session posture `DangerousToolAutoApproval::ApproveAll`;
2. a future exact per-tool session override;
3. the registry default (`Auto` or `Confirm`); then
4. fail-closed `Confirm` when no declaration exists.

Only steps 1, 3, and 4 have producers in this slice. The producing-call
completion transaction resolves policy independently for every proposal: the
blanket posture records `SessionBlanket`, registry auto records `PolicyAuto`,
and confirm leaves that request undecided. Thus a frozen automatic decision may
exist after an earlier confirmation wait without bypassing it; only
owner-command decisions must form a proposal-order prefix. After each owner
command, the earliest remaining undecided confirmation is the next wait, while
already frozen automatic decisions require no later command. Why: recording the
selected source makes unattended operation inspectable without laundering policy
as human consent.

The blanket is a field of each immutable `VersionedSessionConfigurationDefaults`
value and is named `DangerousToolAutoApproval::{Disabled, ApproveAll}`. Safe
session creation uses `Disabled`. Replacement installs a complete later defaults
version through the existing `ReplaceSessionDefaults` command. Origin acceptance
freezes the posture into `EffectiveConfiguration` alongside model selection;
steering-derived work inherits its source turn's frozen value. A later defaults
replacement never changes queued, active, or completed work (INV-008).

An owner decision is the canonical `DecideToolRequest` command: owner-global
`DurableCommandId`, exact `ToolRequestId`, and either `Approve` or
`Deny { reason }`. A denial reason is absent or 1–1024 bytes of non-control
Unicode with no leading/trailing POSIX whitespace; it is therefore safe to
render without copying unbounded or terminal-control content. Equality excludes
only the command identifier. The version-eight `decide_tool_request` request in
[process-protocol](process-protocol.md#client-requests) is the client surface
that issues this command; its wire posture requires a denial reason even though
the command admits an absent one. Registry lookup precedes current-state
validation; equal replay returns the recorded applied-or-rejected result,
cross-kind or different-payload reuse conflicts, and a pre-commit failure claims
no identity (INV-012).

The consume-and-proceed transaction locks the owning session, validates that the
request is the turn's earliest undecided request, records the command and
`OwnerCommand` decision, and then either parks on the next undecided request or
creates a fresh prepared turn attempt when the batch's approval inventory is
complete. An approval cannot revive a denied, executed, or turn-closed request.
A denial creates no tool attempt (INV-027).

Deny-and-continue is the command's ordinary meaning: the denial becomes an error
tool result at the continuation boundary and the turn continues. There is no
separate denial source that can claim cancellation authority. Deny-and-end
composes that same recorded denial with the existing applied-interrupt stop
path; the interrupt remains the proof-bearing authority for ending the turn
(INV-029, INV-037). The caller first records the denial (and resolves any
earlier approval-order obligations); once decision progression opens the
executing phase, it submits the interrupt. An interrupt alone against an
approval wait is not a denial and does not bypass the decision command. A
terminal stop materializes the denial result before its terminal marker. This is
two independently durable commands, not one atomic deny-and-end command; after
decision progression opens execution, the ordinary dispatch-gate race between
remaining tool work and the interrupt applies. On the wire this composition is
`decide_tool_request` followed by `stop_turn`
([process-protocol](process-protocol.md#client-requests)); a `stop_turn` against
the parked wait records the typed
`interrupt_unavailable_while_awaiting_approval` rejection and leaves the wait
intact.

## Registry, placement, and effect metadata

The application `ToolCatalog` port supplies immutable daemon-local
`ToolDefinition` values: name, model-facing description, argument JSON Schema,
permission default (`Auto` or `Confirm`), and the stored two-class crash
classification used by the implemented local attempt machinery.

The runner foundation adds one immutable daemon-owned `RunnerToolDeclaration`
per runner-advertisable name. It carries a required checked model-facing
description and canonical JSON-object argument schema, the required three-way
`RunnerToolEffectClass` (`Pure`, `Idempotent`, or `SideEffecting`), and one
nonempty `ToolAdmissibleLoci` value (`DaemonOnly`, `RunnerOnly { selector }`, or
`DaemonOrRunner { selector }`). Pure implies idempotent; idempotent work may
change state but is safe to repeat. The combined locus prefers the session's
attached eligible runner, falling back to daemon-local execution. Declarations
are static per tool; a model or runner cannot select another locus per call.
Every runner-only tool therefore still has one authoritative definition for
model advertisement and argument validation. The typed placement and
runner-dispatch law is owned by
[runner protocol and placement](runner-protocol.md).

The current daemon-local application catalog remains one process-lifetime
immutable compiled value. Its existing `EffectFree` declaration maps to
`RunnerToolEffectClass::Pure`, and `ExternalEffect` maps to
`RunnerToolEffectClass::SideEffecting`; no current local declaration can project
`Idempotent`. Before a shared name can use a daemon locus, the later application
adapter must validate exact model-facing description and schema, permission
equality, and this effect mapping against the authoritative runner declaration;
it also compiles the schema into the executable validator used before dispatch.
A mismatch is unavailable, never a choice between two policies. Consolidating
the typed representations requires the later application and persistence stack
and no migration is introduced here. Catalog lookup and iteration are ports
rather than a static global, but runtime rebinding and deployment compatibility
for outstanding requests are not implemented; they require the durable
definition-revision decision recorded under Open edges.

Each provider operation carries one exact definition snapshot. Initial approval
for proposals returned by that operation is derived from that same advertised
snapshot, never from a later catalog lookup. A dynamic catalog change while the
provider call is in flight therefore cannot upgrade a proposal from `Confirm` to
unattended execution.

The registry is advisory input to policy and execution, never request-content
authority. A model may propose an unknown name; absent a frozen `ApproveAll`
blanket, fail-closed policy requires confirmation, and an approved unknown
request produces a typed `UnknownTool` error without invoking an executor.
Because the attempt schema requires a closed effect class, preparation records
`EffectFree` as a non-dispatching sentinel when no declaration exists. The
preflight transaction closes that attempt before authorization and before the
executor boundary; the sentinel is not a claim that an unknown tool is safe to
run. A declaration added or removed after the request was recorded does not
rewrite its name or arguments.

Effect class controls crash classification, not permission identity. In the
current daemon-local executor, a crash-lost prepared attempt, or an in-flight
attempt declared `EffectFree`, closes `KnownFailed` and fails the current turn
honestly; version one performs no automatic local retry. A crash-lost in-flight
attempt declared `ExternalEffect` closes `Ambiguous`, ends the abandoned turn
attempt `Lost`, and parks the turn in `AwaitingRecoveryDecision` naming that
exact tool attempt (INV-025, INV-026, INV-034). Runner lease loss uses the
separate re-lease law in
[runner protocol and placement](runner-protocol.md#effect-classes-and-runner-leases);
re-leasing one fenced runner attempt is not the current local executor
fabricating a new physical attempt.

## Serialized staged execution

Tool execution is daemon-local and in-process behind the application
`ToolExecutor` port. The executor receives checked request content and returns
evidence; it cannot write transcript, request, attempt, approval, or turn state
(INV-024). Execution is serialized in this slice:

- approval visits requests in proposal order;
- a turn has at most one live tool attempt;
- approved requests execute strictly in proposal order; and
- each attempt reaches a durable terminal state before the next attempt is
  created.

After all approvals resolve, the fresh current turn attempt owns the batch's
execution and continuation. For each next approved request:

1. **Prepare transaction.** The application mints a UUIDv7 `ToolAttemptId` and
   commits a `Prepared` attempt row before executor work. It fixes the request,
   owning turn, issuing turn attempt, effect class, and
   `ToolDispatchGeneration::first()`.
2. **Authorize transaction.** Fresh locked state validates that the request is
   approved, is the earliest unresolved executable request, and still belongs to
   the issuing current turn attempt; it transitions the tool attempt to
   `InFlight` and the turn attempt from `Prepared` to `Running` when necessary.
3. **Execution.** No database transaction spans the in-process effect. The
   executor receives a correlation containing request, tool attempt, issuing
   turn attempt, and dispatch generation and returns one evidence value.
4. **Commit-result transaction.** Fresh locked state validates the complete
   correlation and that the dispatch generation is current before changing the
   attempt. A stale or duplicate result cannot advance logical state (INV-011,
   INV-021). The row moves monotonically to `Completed`, `KnownFailed`, or
   `Ambiguous` and never reopens. An `Ambiguous` result atomically ends the
   issuing turn attempt as `WithoutStop(Ambiguous)` and moves the lifecycle to
   `awaiting_tool_recovery` correlated with that exact attempt. The logical
   orchestration has yielded to a durable wait; the stored attempt disposition
   remains the exact physical ambiguity classification.

If the authorization commit acknowledgement is ambiguous, execution does not
begin from the returned error. While retaining the dispatch gate and exact
request, the application rereads the attempt under the scheduler lock.
`Prepared` proves non-consumption and returns the infrastructure failure;
`InFlight` restores the exact authorization fence and may enter the executor. An
inconclusive reread retains that authority state for another identical reread,
so neither retry nor crash classification can be inferred from a lost commit
response.

A process-shared turn-keyed dispatch gate orders immediate interrupts against
physical-attempt checkpointing, prepared-attempt preflight, the authorize →
executor → result-commit window, in-flight crash classification, and the
all-resolved continuation checkpoint. Before inserting the next attempt, acting
on a loaded prepared attempt, or preparing continuation, tool execution acquires
the gate and revalidates the loaded batch; an interrupt that already consumed
the batch produces `NoWork`, while an interrupt that arrives later waits behind
the checkpoint, preflight, or continuation. Tool execution holds the gate
through a preflight closure or from before authorization until the returned
evidence commits; interrupt handling acquires the same gate before its atomic
command transaction. A pass that sees an `InFlight` attempt also acquires the
gate and reloads that attempt before classifying prior-process crash loss, so a
same-incarnation executor holding the gate finishes first. An interrupt that
wins before authorization closes the checkpointed attempt as crash-lost and
terminalizes without entering the executor. An interrupt that waits behind
executor work reloads the committed result before closing the batch, so it
cannot strand an issued request or roll back its command.

If the executor returns an operator failure without trustworthy evidence after
authorization, the service retains the dispatch gate and applies the attempt's
effect-class crash-loss transition before surfacing that failure. A failed
classification retains the exact attempt identity and permit for another
classification pass, and the returned combined error preserves both the executor
failure and the classification failure. Evidence carrying a different dispatch
correlation follows the same classification-before-release path, surfacing the
correlation mismatch only after closure or together with a failed
classification. The durable attempt therefore cannot remain `InFlight` after the
gate becomes available to an interrupt.

If trustworthy executor evidence returns but its commit fails, the service
retains that exact correlated observation as an opaque linear same-incarnation
value. A later pass rereads the exact attempt first: `Pending` recommits the
unchanged observation, while `AlreadyCommitted` finishes without invoking the
executor again. The service never downgrades still-owned evidence to restart
crash loss.

Unknown names, `Undecodable` arguments, and argument-schema decode failures end
their prepared attempt `KnownFailed` with `UnknownTool` or `InvalidArguments`
error evidence without crossing the executor boundary. An executor-reported
failure becomes `ExecutionFailed`. These typed errors resolve the logical
request and are visible to the next model round; they do not by themselves fail
the turn. Physical ambiguity remains a turn-level recovery wait and does not
become an ordinary error result.

Because a resolved request is otherwise a conversation between the daemon and
the model, admitting a `KnownFailed` observation also emits one operator
telemetry event carrying the dispatched catalog name, the closed error kind, and
the session and turn identities — never the bounded error detail, tool
arguments, or any response content. Admission is the single site: it covers
every executor behind the one dispatch trait and the failures admission itself
substitutes for oversized or null-bearing results. Completed and ambiguous
observations emit nothing here; ambiguity is carried by the recovery wait above.
Preflight failures that never reach admission — unknown names and
argument-decode failures — are likewise silent, being model-authored rather than
deployment facts. Telemetry field discipline is
[identity-and-commands](identity-and-commands.md#durable-command-telemetry-correlation)
scope.

An interrupt against a tool recovery wait does not reinterpret or erase the
ambiguous attempt. It materializes exactly one reference-only result per request
in proposal order: completed or known-failed attempts use `ToolExecutionResult`,
denials use `ToolDenied`, and the ambiguous request plus any request without an
ordinary result use `ToolClosed`. The turn then terminalizes as
`ReconciliationRequired` on that prefix-extending frontier, with the exact tool
attempt as its ambiguity set and the applied-interrupt proof. Logical closure
therefore leaves a provider-renderable conversation while the typed lifecycle
and outbox boundaries retain the physical tool-attempt uncertainty instead of
fabricating a model call or an execution result (INV-005, INV-006, INV-025,
INV-029, INV-037).

The schema independently enforces no live tool attempt while the lifecycle is
`awaiting_tool_approval`, at most one nonterminal tool attempt per turn,
immutable attempt authorization facts, insert-as-`prepared`, the permitted
monotonic transition matrix, and terminal immutability. A later
concurrent-executor migration can relax exactly the one-live-attempt guard and
substitute a fan-out / join strategy behind the same ports; the all-resolved
continuation barrier does not change.

## Result authority and the continuation boundary

One terminal tool-attempt row owns executed output. `ToolResultContent` is a
closed additive algebra whose implemented content arm is `Text`; a text value
may be empty, must exclude U+0000, and is admitted only through a 1 MiB UTF-8
bound. A result larger than the bound is replaced by the typed `ResultTooLarge`
error; oversized bytes are never persisted. Error evidence is a closed kind plus
an optional detail and is stored once on the attempt row. A present detail is
1–4,096 UTF-8 bytes, contains no control character, and has no leading or
trailing POSIX whitespace; it is otherwise retained exactly. Domain construction
and the database constraint enforce the same admission rule.

Semantic tool-result entries contain references only:

- `ToolExecutionResult { attempt }` references executed success/error evidence;
- `ToolDenied { request }` references the request's durable denial; and
- `ToolClosed { request }` references a request closed because its turn ended
  before it could complete ordinary execution, whether it remained undecided or
  was approved but not yet attempted. A crash-lost attempt has durable
  `KnownFailed` evidence and therefore uses `ToolExecutionResult`.

No result entry copies output, error detail, or denial reason. Attempt evidence
commits as soon as execution ends, independently of semantic projection. Once
every request in the batch is executed or denied, one continuation transaction:

1. appends exactly one result entry per request in proposal order;
2. consumes every pending steering input in ascending acceptance position and
   appends its semantic entry after the tool results;
3. derives the exact prefix-preserving frontier extension; and
4. creates the next round's `Prepared` model call against that frontier.

When at least one request entered execution, the continuation turn attempt
already entered `Running` during tool authorization. It owns the new `Prepared`
call without moving backward; send authorization advances only the call to
`InFlight` and leaves the attempt `Running`. A denial-only batch never
authorized an effect, so its continuation attempt remains `Prepared` while it
owns the new `Prepared` call. Reconstitution and the deferred database assertion
admit `(Running, Prepared)` or `(Prepared, Prepared)` only for a
continuation-chain attempt whose exact call frontier contains the current
batch's complete durable result evidence.

Those effects commit or roll back together (INV-036). A newly prepared call ends
the invocation and is reloaded before provider capability preparation,
preserving the existing staged-call discipline. If the call completes with
another tool batch the loop repeats in the same turn; if it proposes no tools,
its assistant text and `TurnCompleted` marker terminalize the turn.

At most 32 requests may appear in one completed provider tool response. A
response with a thirty-third request closes the producing model call as
`KnownFailed` without creating a partial batch, request record, or tool-use
entry. At most 32 provider rounds in one turn may complete with admitted tool
requests. The application counts distinct producing calls for the current turn,
so every multi-request batch counts once and inherited tool history from earlier
turns does not count. After the thirty-second batch resolves, the ordinary
continuation transaction still projects all results and creates its fresh
`Prepared` call; model execution closes that checkpoint as `KnownFailed` before
provider capability preparation or send. The normal known-failure boundary then
fails the turn honestly. These durable-content bounds avoid wall-clock policy
and ensure one model-controlled response or chain cannot retain the progressing
slot indefinitely.

If an applied stop terminalizes before continuation, the same materialization
algorithm appends results for executed and denied requests, closes every request
that did not complete ordinary execution as `ToolClosed` in proposal order, then
appends the proof-bearing terminal marker. The consumed result projection is
bound to the interrupted turn: reusing this turn's current frontier identity is
not sufficient, and a projection prepared for another turn cannot terminalize
this turn with foreign request results even when the yielded source frontier
matches. A prepared or effect-free crash loss that fails the turn uses that same
proposal-ordered materialization before `TurnFailed`; the crash-lost
`KnownFailed` attempt becomes `ToolExecutionResult`, while every other request
without an ordinary result becomes `ToolClosed`. A request can therefore never
remain an open logical dependency behind a terminal turn (INV-006).

## Approval waits and restart

`AwaitingApproval { request }` is a stored active-turn phase. It names the exact
earliest undecided request, retains the session's progressing slot, and has no
current turn attempt. Complete reconstitution validates the request's session,
turn, producing call, batch order, undecided state, and the absence of any live
turn or tool attempt. Raw request identity is not approval-wait evidence.

Startup scanning leaves an approval wait unchanged. It never fabricates an
approval or denial, advances to a later request, expires the wait, or creates an
attempt. Pending approval has no timeout and may wait indefinitely (INV-010).
The activated execution pass returns while approval is pending, releasing its
bounded scheduler worker. A durably applied final decision advances the stored
phase to running; the durable eligibility sweep includes that active tool round,
and the next pass reloads the exact batch before continuing. Rejected or
uncommitted commands leave the approval phase unchanged and create no resumable
hint. The same sweep inventories a running batch after restart, including one
whose decision committed before the prior process stopped, so progress does not
depend on process-local wake memory.

Running phases use the staged tool-attempt crash classification above; parked
external-effect ambiguity is never automatically retried. Version one permits
only proof-bearing interruption to terminalize that wait as reconciliation
required; resolving evidence and accepted-risk continuation remain open. Restart
requires the running batch's exact continuation turn attempt to remain current:
`Prepared` after a final decision or a denial-only batch, and `Running` after
physical execution began or a preflight failure produced terminal attempt
evidence. With no live model call, a batch is resumable when it has no current
tool attempt and either has an approved request not yet attempted or has durably
resolved every request. The next scheduler pass performs the ordinary
next-attempt or atomic result-projection-and-continuation transaction instead of
failing the turn or waiting for process-local wake state. Restart never requires
the current continuation attempt to disappear.

## Provider bridge and daemon catalog

The provider-neutral application operation carries ordered conversation messages
plus catalog declarations. The runtime bridge projects declarations to runtime
`ToolDefinition` values, maps `ToolCall` completion parts and the `ToolUse`
finish reason into normalized domain proposals, and renders `AssistantToolUse`
plus each result-reference entry back into paired assistant tool-call and user
tool-result message parts. It derives the provider-visible tool-call correlation
from `ToolRequestId`, so provider-native identifier types and messages never
cross the application boundary (INV-002). Every rendered result resolves its
referenced durable record first; missing or cross-wired content fails closed. If
a definitive provider completion contains a tool name or argument payload that
cannot enter the bounded domain vocabulary, the provider bridge converts that
authenticated response to the call's typed `KnownFailed` terminal observation.
It does not leave the already-issued call `InFlight`, persist the inadmissible
proposal, or partially commit the response. All text and tool proposals produced
by one model call are coalesced into one assistant message, and the
proposal-ordered results for that batch are coalesced into the immediately
following user message. Every provider-visible failure is this compact
provider-neutral JSON object: `{"error":{"detail":D,"kind":K}}`. `D` is the
admitted executor detail, admitted owner denial reason, or JSON null; `K` is
exactly `unknown_tool`, `invalid_arguments`, `execution_failed`,
`result_too_large`, `crash_lost`, `denied`, or `closed_by_turn_end`. Execution
failures select their stored error kind and detail, denial selects `denied` and
its reason, and terminal closure selects `closed_by_turn_end` with null detail.
OpenAI carries that JSON as ordinary tool-message content because its wire shape
has no failure flag; Anthropic also receives the provider-neutral failure flag.
Malformed proposal arguments remain exact after preparation-time credential
scrubbing on the durable request but replay as the exact provider-neutral JSON
object `{"signalbox_invalid_arguments":true}`, allowing the paired typed error
result to reach either provider without pretending the placeholder is durable
evidence.

The first compiled tool is `current_time`:

- optional argument `timezone` is an IANA time-zone name; absence selects `UTC`;
- permission default is `Auto`;
- effect class is `EffectFree`;
- an injected `CurrentTimeClock` supplies the instant, so offline tests never
  read wall clock; and
- success is text containing a compact JSON object with `datetime` as an RFC
  3339 timestamp to whole seconds and `timezone` as the exact accepted IANA
  identifier (or the `UTC` default). A recognized zone at an instant whose
  historical offset contains nonzero seconds closes as a typed execution failure
  because RFC 3339 cannot represent that offset without changing the instant.

An unknown time zone or wrong argument shape produces `InvalidArguments` error
evidence. An injected instant outside the supported civil-time range produces
known-failure evidence with detail
`current time is outside the supported range`. IANA lookup and offset conversion
use the focused `jiff` dependency; Signalbox owns only the port and result
contract, not a time-zone database implementation.

The same process-lifetime compiled catalog also declares the Tier 0 daemon
tools:

- `echo` requires exactly one `text` string and returns the same canonical
  compact `{"text": ...}` object. Its permission default is `Auto` and its
  effect class is `EffectFree`: execution observes no external state.
- `web_fetch` requires exactly one absolute HTTP(S) `url` no longer than 8 KiB.
  User information, fragments, and direct non-public IP destinations are
  invalid. Before dispatch, a domain must resolve to between one and 32
  addresses and every address must be public; the admitted addresses are pinned
  into the request client so connection setup cannot substitute a later DNS
  answer. Its permission default is `Auto`; its effect class is `ExternalEffect`
  because the remote server can observe a GET. One dispatch performs at most one
  credential-free request: ambient proxies, redirects, protocol retries, and
  idle reuse are disabled, TLS uses rustls with a TLS 1.2 floor, and a 15-second
  timeout bounds resolution and the exchange. The executor retains at most 64
  KiB of response bytes and at most 1,024 bytes of a valid content-type header.
  Success is compact JSON containing the exact requested `url`, numeric
  `status`, optional `content_type`, a lossy UTF-8 `body`, and `truncated`.
  Resolution, client-setup, and definite connection-establishment failure before
  request dispatch returns a fixed sanitized known failure; timeout, transport,
  or body loss after dispatch begins is commit-ambiguous. Truncation stops body
  consumption and never follows or issues another request.
- `session_status_update` requires one complete existing session-metadata shape:
  nullable `title`, complete `tags`, complete string-to-string `attributes`, and
  `archived`. Partial patches are invalid. The invocation's session is the
  target; no session identity is accepted from model arguments. Its permission
  default is `Confirm` and its effect class is `ExternalEffect`. Execution
  derives a durable command identity from the physical tool attempt, attributes
  the command and last-writer stamp to the exact `ToolRequestId`, and calls the
  existing metadata replacement application service. Argument validation admits
  the exact compact success receipt under the independent result-text bound
  before the write can begin. Success requires the writer's applied snapshot to
  match the admitted session and replacement, then returns that session identity
  and snapshot content as compact JSON; mismatch is a daemon defect,
  missing-session rejection is a fixed known failure, and ambiguous commit
  acknowledgement returns `Ambiguous` evidence. Metadata value and replacement
  mechanics remain owned by
  [sessions-and-transcript](sessions-and-transcript.md#session-metadata-and-list-projection).

The Tier 1 catalog adds ten GitHub change-request tools. Every operation is
`ExternalEffect` because GitHub observes its authenticated request. The six
read-only declarations — `change_request_summary`,
`change_request_changed_files`, `change_request_file_patch`,
`change_request_checks_status`, `change_request_review_threads`, and
`change_request_ci_job_log` — default to `Auto`. The four mutations —
`change_request_comment`, `change_request_thread_reply`,
`change_request_thread_resolve`, and `change_request_rerun_failed_jobs` —
default to `Confirm`. The normal approval transaction therefore authorizes each
mutation before the executor can resolve credentials or dispatch.

The declarations and compact result objects are:

- `change_request_summary` accepts checked `repository` (`owner/name`) and a
  positive `number`; it returns the number, title, optional body, state, draft
  posture, optional author, base and head refs, exact head revision, and browser
  URL.
- `change_request_changed_files` accepts `repository` and `number`; it returns
  the first page of at most 100 files, each with path, code-host status,
  additions, and deletions, plus `truncated`.
- `change_request_file_patch` accepts `repository`, `number`, and one
  repository-relative `path`; it searches that same first 100-file page and
  returns its file summary plus the optional code-host patch. A path outside the
  bounded page is a known failure rather than an unbounded pagination request.
- `change_request_checks_status` accepts `repository` and one exact lowercase
  40-hex `revision`; it returns that revision and the first page of at most 100
  check runs, each with id, name, status, optional conclusion, and URL, plus
  `truncated`.
- `change_request_comment` accepts `repository`, `number`, and one nonempty
  `body`; it returns the created comment id and URL.
- `change_request_review_threads` accepts `repository` and `number`; it returns
  the first 100 threads and, within each, the first 100 comments. A thread
  carries opaque id, resolution and outdated posture, path, optional line,
  comments, and `comments_truncated`; the outer result carries `truncated`.
- `change_request_thread_reply` accepts an opaque `thread_id` and nonempty
  `body`; it returns the created comment node id and URL.
- `change_request_thread_resolve` accepts one opaque `thread_id`; it returns
  that identity and the acknowledged resolution posture.
- `change_request_ci_job_log` accepts `repository` and a positive `job_id`; it
  returns that id, at most 64 KiB of lossy UTF-8 log text, and `truncated`.
- `change_request_rerun_failed_jobs` accepts `repository` and a positive
  workflow `run_id`; it returns the acknowledged run id.

Shared typed admission rejects extra object members; repositories are at most
256 bytes, paths 4 KiB, comment bodies and returned text fields 64 KiB, and
opaque node ids 512 bytes. A returned node id or head revision is admitted by
the same predicate its argument counterpart uses, so an identity a result
carries can always be passed back as an argument, and every returned URL is one
absolute credential-free HTTPS location. No result has more than 100 collection
members or more than 512 KiB of encoded JSON.

The production adapter uses fixed GitHub REST and GraphQL endpoints. It disables
ambient proxies, automatic redirects, protocol retries, and idle reuse; uses
rustls with a TLS 1.2 floor; sends the fixed GitHub REST version `2026-03-10`;
applies a 30-second whole-exchange timeout; and retains at most 512 KiB from any
JSON response. The authenticated job-log endpoint is the sole redirect-shaped
exchange: after exactly one 302 response, the adapter validates its bounded
HTTPS location, resolves and pins a wholly public destination set, and performs
one credential-free download with redirect following still disabled. Credential
delivery and redaction are owned by
[configuration-and-credentials](configuration-and-credentials.md).

A missing or unusable credential and a definitive client rejection produce only
fixed known-failure detail, and the two are told apart: credential bytes that
cannot form the authentication header never reach the code host, so they present
the credential-unavailable detail that a failed resolution already presents,
while a definitive rejection presents the code-host detail. A read transport or
server failure is an executor infrastructure failure. A mutation transport loss,
server failure, oversized or malformed success response, or malformed GraphQL
acknowledgement is commit-ambiguous; the durable tool attempt's `ExternalEffect`
classification parks crash-lost execution for recovery rather than silently
retrying it. The adapter never returns code-host response bodies as error
detail.

The merged catalog sorts declarations by checked tool name and rejects
duplicates during construction. Its executor dispatches only those same four
preexisting names and the ten code-host names; disagreement between the
advertised catalog and executor is classified as a daemon defect.

## Persistence boundaries

One migration removes `semantic_transcript_entry_tool_use_unavailable`, adds the
three result-entry shapes, and introduces append-only `tool_request`,
`tool_approval_decision`, and guarded `tool_attempt` tables. Deferred
constraints assert complete call-response/request-entry batches, approval-wait
evidence, result-entry materialization, and terminal closure. The session
scheduler row remains the first explicit lock for every turn-side transaction.
Preparing a model operation collects all frontier-referenced tool requests,
attempts, and approval decisions in one batched query per record family before
reconstructing provider history in frontier order; it performs no per-entry
database round trips while holding the scheduler lock.

`DecideToolRequest` joins the owner-global durable-command registry as its own
typed record family. Because adding the dangerous posture changes every
defaults-bearing canonical command payload, new `CreateSession`,
`CreateSessionFromImportedFrontier`, and `ReplaceSessionDefaults` records use
kind-scoped storage version 2; their version-1 records reconstitute with
`DangerousToolAutoApproval::Disabled`. `SubmitInput` remains version 1, and the
new decision command begins at version 1; registry inspection validates the
supported version set for the selected kind rather than applying one global
version constant.

## Open edges

- Execution-strategy configuration placement is recorded in
  [Tool safety](../open-questions.md#tool-safety).
- Model-declared approval expiry is recorded in
  [Tool safety](../open-questions.md#tool-safety).
- LLM-judge approval mechanics are recorded in
  [Tool safety](../open-questions.md#tool-safety).
- Per-tool session overrides and high-risk guardrails are recorded in
  [Tool safety](../open-questions.md#tool-safety).
- Rich result-content variants are recorded in
  [Tool safety](../open-questions.md#tool-safety).
- Durable tool-definition revisioning and safe deployment across outstanding
  requests are recorded in [Tool safety](../open-questions.md#tool-safety).
- Tool-attempt retry and ambiguous-wait resolution are recorded in
  [Tool safety](../open-questions.md#tool-safety).
- Runner placement and domain lease law are owned by
  [runner protocol and placement](runner-protocol.md); its later transport,
  persistence, and authentication edges remain recorded under
  [Scheduling and runners](../open-questions.md#scheduling-and-runners),
  [Protocols and persistence](../open-questions.md#protocols-and-persistence),
  and
  [Identity, credentials, and resource governance](../open-questions.md#identity-credentials-and-resource-governance).
- Client approval presentation is recorded under
  [Client scope](../open-questions.md#client-scope).
- Streaming tool deltas remain part of the model-streaming question in
  [Protocols and persistence](../open-questions.md#protocols-and-persistence).

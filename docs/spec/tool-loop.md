# Tool loop

This page specifies the implemented hub-owned tool subsystem as verified against
the implementing stack rooted at PR #193 (`agent/tool-loop-spec`). It owns
logical tool requests, approval policy and decisions, physical tool attempts,
result admission, intra-turn continuation, crash classification, the compiled
registry, and the first hub-local tool. Turn and attempt lifecycle law lives in
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
stores the exact provider-supplied UTF-8 text when JSON decoding fails. Both
arms must fit within 1 MiB before and after normalization. This preserves
malformed arguments as bounded evidence without pretending they are JSON. An
undecodable value, or valid JSON that does not decode against the selected
tool's argument type, becomes a typed execution error later.

The same transaction that classifies the producing call `Completed` appends one
`AssistantText` or `AssistantToolUse { producing_call, request }` semantic entry
per response part, preserving response order, and inserts every request record.
The request row is the sole content authority: the semantic entry contains only
the call/request references and never copies the name or arguments (INV-005).
Request identity, call ownership, and ordinal are unique within the producing
call, so equal proposals remain distinct logical requests.

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

Policy resolution uses this accepted precedence:

1. the frozen session posture `DangerousToolAutoApproval::ApproveAll`;
2. a future exact per-tool session override;
3. the registry default (`Auto` or `Confirm`); then
4. fail-closed `Confirm` when no declaration exists.

Only steps 1, 3, and 4 have producers in this slice. The blanket posture records
an approved decision sourced as `SessionBlanket`; registry auto records
`PolicyAuto`; confirm leaves the request undecided. Why: recording the selected
source makes unattended operation inspectable without laundering policy as human
consent.

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
Unicode with no leading/trailing whitespace; it is therefore safe to render
without copying unbounded or terminal-control content. Equality excludes only
the command identifier. Registry lookup precedes current-state validation; equal
replay returns the recorded applied-or-rejected result, cross-kind or
different-payload reuse conflicts, and a pre-commit failure claims no identity
(INV-012).

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
terminal stop materializes the denial result before its terminal marker.

## Registry and effect metadata

The application `ToolCatalog` port supplies immutable `ToolDefinition` values:
name, model-facing description, argument JSON Schema, permission default (`Auto`
or `Confirm`), and effect class (`EffectFree` or `ExternalEffect`). hubd wires a
compiled catalog, but catalog lookup and iteration are ports rather than a
static global. Later database, MCP, or runner-enrollment sources can compose
declarations behind that port without changing request, approval, execution, or
model-operation types.

Each provider operation carries one exact definition snapshot. Initial approval
for proposals returned by that operation is derived from that same advertised
snapshot, never from a later catalog lookup. A dynamic catalog change while the
provider call is in flight therefore cannot upgrade a proposal from `Confirm` to
unattended execution.

The registry is advisory input to policy and execution, never request-content
authority. A model may propose an unknown name; fail-closed policy requires
confirmation, and an approved unknown request produces a typed `UnknownTool`
error without invoking an executor. A declaration added or removed after the
request was recorded does not rewrite its name or arguments.

Effect class controls crash classification, not permission identity. A
crash-lost prepared attempt, or an in-flight attempt declared `EffectFree`,
closes `KnownFailed` and fails the current turn honestly; version one performs
no automatic retry. A crash-lost in-flight `ExternalEffect` attempt closes
`Ambiguous`, ends the abandoned turn attempt `Lost`, and parks the turn in
`AwaitingRecoveryDecision` naming that exact tool attempt (INV-025, INV-026,
INV-034).

## Serialized staged execution

Tool execution is hub-local and in-process behind the application `ToolExecutor`
port. The executor receives checked request content and returns evidence; it
cannot write transcript, request, attempt, approval, or turn state (INV-024).
Execution is serialized in this slice:

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
   `Ambiguous` and never reopens.

A process-shared turn-keyed dispatch gate orders immediate interrupts against
the authorize → executor → result-commit window. Tool execution holds the gate
from before authorization until the returned evidence commits; interrupt
handling acquires the same gate before its atomic command transaction. An
interrupt that wins before authorization closes the checkpointed attempt as
crash-lost and terminalizes without entering the executor. An interrupt that
waits behind executor work reloads the committed result before closing the
batch, so it cannot strand an issued request or roll back its command.

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

An interrupt against a tool recovery wait does not reinterpret or erase the
ambiguous attempt. It terminalizes the turn as `ReconciliationRequired` with an
equal-content frontier, the exact tool attempt as its ambiguity set, and the
applied-interrupt proof; the typed lifecycle and outbox boundaries retain the
tool-attempt reference instead of fabricating a model call (INV-006, INV-025,
INV-029, INV-037).

The schema independently enforces no live tool attempt while the lifecycle is
`awaiting_approval`, at most one nonterminal tool attempt per turn, immutable
attempt authorization facts, insert-as-`prepared`, the permitted monotonic
transition matrix, and terminal immutability. A later concurrent-executor
migration can relax exactly the one-live-attempt guard and substitute a fan-out
/ join strategy behind the same ports; the all-resolved continuation barrier
does not change.

## Result authority and the continuation boundary

One terminal tool-attempt row owns executed output. `ToolResultContent` is a
closed additive algebra whose implemented content arm is `Text`; a text value
may be empty, must exclude U+0000, and is admitted only through a 1 MiB UTF-8
bound. A result larger than the bound is replaced by the typed `ResultTooLarge`
error; oversized bytes are never persisted. Error evidence is a closed kind plus
an optional bounded sanitized detail and is stored once on the attempt row. When
present, the detail is 1–4096 UTF-8 bytes, has no leading or trailing
whitespace, and contains no control characters.

Semantic tool-result entries contain references only:

- `ToolExecutionResult { attempt }` references executed success/error evidence;
- `ToolDenied { request }` references the request's durable denial; and
- `ToolClosed { request }` references a request closed because its turn ended
  before it could complete ordinary execution, whether it remained undecided,
  was approved but not yet attempted, or owned an unsent checkpoint classified
  crash-lost by the interrupt boundary.

No result entry copies output, error detail, or denial reason. Attempt evidence
commits as soon as execution ends, independently of semantic projection. Once
every request in the batch is executed or denied, one continuation transaction:

1. appends exactly one result entry per request in proposal order;
2. consumes every pending steering input in ascending acceptance position and
   appends its semantic entry after the tool results;
3. derives the exact prefix-preserving frontier extension; and
4. creates the next round's `Prepared` model call against that frontier.

The same continuation turn attempt already entered `Running` when it authorized
the tool batch. It therefore owns the new `Prepared` call without moving
backward to `Prepared`; send authorization advances only the call to `InFlight`
and leaves the attempt `Running`. Reconstitution and the deferred database
assertion admit that pairing only for a continuation-chain attempt whose exact
call frontier contains durable tool-result evidence.

Those effects commit or roll back together (INV-036). A newly prepared call ends
the invocation and is reloaded before provider capability preparation,
preserving the existing staged-call discipline. If the call completes with
another tool batch the loop repeats in the same turn; if it proposes no tools,
its assistant text and `TurnCompleted` marker terminalize the turn.

If an applied stop terminalizes before continuation, the same materialization
algorithm appends results for executed and denied requests, closes every request
that did not complete ordinary execution as `ToolClosed` in proposal order, then
appends the proof-bearing terminal marker. A request can therefore never remain
an open logical dependency behind a terminal turn (INV-006).

## Approval waits and restart

`AwaitingApproval { request }` is a stored active-turn phase. It names the exact
earliest undecided request, retains the session's progressing slot, and has no
current turn attempt. Complete reconstitution validates the request's session,
turn, producing call, batch order, undecided state, and the absence of any live
turn or tool attempt. Raw request identity is not approval-wait evidence.

Startup scanning leaves an approval wait unchanged. It never fabricates an
approval or denial, advances to a later request, expires the wait, or creates an
attempt. Pending approval has no timeout and may wait indefinitely (INV-010).
Within one hub incarnation, the activated execution future remains parked on an
exact request-keyed wake. A durably applied owner decision wakes that future to
reload the batch and continue the same active turn; rejected or uncommitted
commands do not wake it. Running phases use the staged tool-attempt crash
classification above; parked external-effect ambiguity follows the existing
recovery-decision lifecycle and is never automatically retried.

## Provider bridge and `current_time`

The provider-neutral application operation carries ordered conversation messages
plus catalog declarations. The runtime bridge projects declarations to runtime
`ToolDefinition` values, maps `ToolCall` completion parts and the `ToolUse`
finish reason into normalized domain proposals, and renders `AssistantToolUse`
plus each result-reference entry back into paired assistant tool-call and user
tool-result message parts. It derives the provider-visible tool-call correlation
from `ToolRequestId`, so provider-native identifier types and messages never
cross the application boundary (INV-002). Every rendered result resolves its
referenced durable record first; missing or cross-wired content fails closed.
All text and tool proposals produced by one model call are coalesced into one
assistant message, and the proposal-ordered results for that batch are coalesced
into the immediately following user message. OpenAI carries typed failure JSON
as ordinary tool-message content because its wire shape has no failure flag;
Anthropic also receives the provider-neutral failure flag. Malformed proposal
arguments remain exact on the durable request but replay as an object-shaped
invalid-arguments placeholder, allowing the paired typed error result to reach
either provider without pretending the placeholder is durable evidence.

The first compiled tool is `current_time`:

- optional argument `timezone` is an IANA time-zone name; absence selects `UTC`;
- permission default is `Auto`;
- effect class is `EffectFree`;
- an injected `CurrentTimeClock` supplies the instant, so offline tests never
  read wall clock; and
- success is text containing a compact JSON object with `datetime` as an RFC
  3339 timestamp to whole seconds and `timezone` as the selected canonical name.

An unknown time zone or wrong argument shape produces `InvalidArguments` error
evidence. IANA lookup and offset conversion use the focused `jiff` dependency;
Signalbox owns only the port and result contract, not a time-zone database
implementation.

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
typed record family. Because adding the dangerous posture changes the canonical
payloads of both defaults-bearing command kinds, new `CreateSession` and
`ReplaceSessionDefaults` records use kind-scoped storage version 2; their
version-1 records reconstitute with `DangerousToolAutoApproval::Disabled`.
`SubmitInput` remains version 1, and the new decision command begins at version

1. Registry inspection validates the supported version set for the selected kind
   rather than applying one global version constant.

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
- Runner placement, authentication, and protocol are recorded under
  [Scheduling and runners](../open-questions.md#scheduling-and-runners) and
  [Identity, credentials, and resource governance](../open-questions.md#identity-credentials-and-resource-governance).
- Client approval presentation is recorded under
  [Client scope](../open-questions.md#client-scope).
- Streaming tool deltas remain part of the model-streaming question in
  [Protocols and persistence](../open-questions.md#protocols-and-persistence).

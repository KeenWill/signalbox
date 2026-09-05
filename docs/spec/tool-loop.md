# Tool loop

The tool loop turns a model's tool proposals into approved, executed, and
recorded results inside one turn, then hands the conversation back to the model.

## Overview

The tool loop owns logical tool requests, approval policy and decisions,
physical tool attempts, result admission, continuation within a turn, crash
classification, and the compiled daemon-local catalog. Turn and attempt
lifecycle belong to
[turn-lifecycle-and-scheduling](turn-lifecycle-and-scheduling.md), semantic
entry vocabulary to [sessions-and-transcript](sessions-and-transcript.md),
model-call staging and provider translation to
[model-call-execution](model-call-execution.md), command identity to
[identity-and-commands](identity-and-commands.md), and relational mechanics to
[persistence-protocol](persistence-protocol.md). Everything on this page runs on
the daemon locus; runner placement and dispatch belong to
[runner-protocol](runner-protocol.md). The application logic lives in
`crates/application/src/tool_loop.rs`.

One turn spans the complete propose, decide, execute, result, continue loop, and
a model call is one physical round inside that turn. All requests produced by
one call are one batch, and the rules on this page apply per batch.

Each proposal becomes one immutable `ToolRequest` record fixing its producing
call, name, normalized arguments, ordinal, and resolved approval posture.
Arguments that decode are stored as compact JSON with lexically ordered object
keys; arguments that do not decode are stored as the exact bounded text the
provider adapter emitted. Every request has an approval state separate from its
execution state. A daemon tool mapping may declare an approval posture:
automatic, delegated to a judge, or human. The selected posture is frozen into
every resulting request. The dangerous-tool blanket, disabled or approve-all, is
a field of the session's immutable configuration defaults and the frozen input
every automatic approval reads.

A user resolves an approval wait with the `DecideToolRequest` command, which
approves or denies one exact request. Denial ordinarily continues the turn: the
denial becomes an error tool result at the continuation boundary. A request
frozen as delegated goes to one dedicated approval-judge call, prepared and
authorized through the configured provider adapter with the session credential
snapshot. When a user disagrees with a judge denial, the
`OverrideDeniedToolRequest` command records a one-shot pre-approval for the next
proposal of the same command; each delegate denial admits at most one override
ever, and a second command is rejected.

The daemon composes one process-lifetime immutable registry from the implemented
tool families in `apps/signalboxd/src/daemon_tools.rs`: basic, blob-read, web,
code-host, workspace, conversation, plan, session-delegation, goal-declaration,
local Git, and execution tools. The workspace, conversation, local Git,
execution, and mapped GitHub families are composed only under the complete
mapped composition
([configuration-and-credentials](configuration-and-credentials.md)), and
blob-read is composed only when blob storage is configured. Each family's crate
or daemon module documents its tools.

Each approved request that reaches execution runs as one physical attempt
through staged transactions. A prepare transaction mints the attempt and commits
a `Prepared` row that fixes the request, owning turn, issuing turn attempt,
effect class, locus, and dispatch generation before any executor work. An
authorize transaction moves the attempt in flight under fresh locked state. The
executor then runs outside any transaction, and a commit transaction records its
evidence against the same correlation. A process-shared, turn-keyed dispatch
gate in `crates/application/src/tool_dispatch_gate.rs` orders immediate
interrupts against attempt checkpointing, preflight, the window from
authorization to result commit, crash classification, and the continuation
checkpoint.

A result entry in the transcript references a durable row rather than carrying
content, except a delegation result, which embeds the delegation outcome and its
optional content: an execution result names the terminal attempt, a denial names
the request, a delegation result names the `await_session` request whose child
produced it, foreground or background, and the child whose durable result
completed it, and `ToolClosed` names a request whose turn ended before it
completed ordinary execution, whether it was still undecided, approved but not
yet attempted, or ambiguous when an interrupt or automatic reconciliation
terminalized the turn. Once every request in a running batch is resolved, one
continuation transaction projects the results and prepares the next model call.
An approval wait is a stored active-turn phase that names the earliest undecided
request and survives restart.

## Design decisions

A turn is the logical conversational outcome; a model call and a turn attempt
are physical executions that may repeat without changing that identity.

Malformed arguments are retained as bounded text instead of being rejected, so
identity-safe evidence of the proposal survives without being treated as JSON.

An `AlwaysConfirm` permission exists so that no session blanket can silently
approve the tool; a configured automatic posture therefore never satisfies it.

A configured delegated posture does satisfy `AlwaysConfirm`, because a judge is
not a blanket but a distinct decider that can still deny or escalate.

Every decision records its source, so unattended operation is inspectable
without presenting policy as human consent, and only an explicit user, delegate,
or consumed-override decision emits an approval-decided event naming its
decider, its decision, and, for a delegate, its rationale.

A denial reason is bounded and free of control characters, so a client can
render it directly.

Delegation can narrow authority but never widen it.

No denial source can claim cancellation authority.

Deny-and-end is two independently durable commands, a denial and an interrupt,
not one atomic command.

A judge denial the user disagrees with is reversed forward, never in place: the
denial is terminal and the session may re-propose after it.

The override command carries the session identity, unlike the decision command,
because the recorded override is a session-scoped standing fact consumed by a
later proposal.

Override retirement counts approvals by any authority, because an override left
standing after another authority approved the same command would pre-approve a
repeat the session already let through. It counts only approvals after the
denial, because the same command is routinely approved earlier in a session.

The order of a denial and a later approval is structural, because none of these
append-only records carries a timestamp.

Override inventory is fixed when a call is checkpointed, at the cost of one
extra round, so no concurrent command can race a prepared call's approval
inputs.

Declarations are static per tool; a model or runner cannot select another locus
per call.

A registry mismatch makes the tool unavailable; it is never a choice between two
policies.

One request carries the whole catalog, so a provider refusing a single schema
refuses every exchange; a root-level union in a schema is a family-wide outage,
not a per-tool cost.

The compiled `git_push_configured` declaration is not registered, because no
production push transport exists to supply its transport authority.

The seven local Git tools perform no remote operation.

The daemon-local registry supplies no runner execution path.

The snapshot freeze exists because a catalog or runner change while a call is in
flight could otherwise upgrade permission, introduce an unavailable runner tool,
widen its selector, or move the locus.

Because the attempt schema requires a closed effect class, preparation records
`EffectFree` as a non-dispatching sentinel for an undeclared name. The preflight
transaction closes that attempt before authorization, and the sentinel is not a
claim that an unknown tool is safe to run.

Logical closure leaves a provider-renderable conversation while the lifecycle
and outbox boundaries retain the physical uncertainty, instead of fabricating a
model call or an execution result.

Each tool family owns its bounded capture, truncation, and completeness
evidence; no universal true-size field exists, because a traversal cannot always
know it.

The loop's bounds are on durable content, not on wall-clock time, so one
model-controlled chain cannot hold the progressing slot indefinitely or exhaust
daemon memory.

Pending approval has no timeout.

A tool recovery wait is terminalized only by a proof-bearing interruption or by
the automatic-reconciliation ledger's claim on that exact ambiguity; resolving
evidence and accepted-risk continuation are undecided in
[open questions](../open-questions.md).

After restart, a running batch with no current tool attempt continues through
the ordinary next-attempt or continuation transaction, never a recovery path
that fails the turn or waits for process-local wake state; a persisted prepared
or in-flight attempt takes the effect-class crash-loss path.

Foreground waiting on a child through `await_session` is a logical tool
transition that ends any physical attempt before committing the wait, so restart
resumes from durable wait and result rows and cannot duplicate an external
effect.

The error kind set stays closed; a family whose failures do not fit maps into it
and may fix the detail to its own closed token vocabulary.

`PreauthorizationRejected` separates a durable request-scoped resource or
visibility refusal from a malformed argument the model can correct by rewriting
the call.

## Boundary contracts

A tool executor receives checked request content and returns evidence. It writes
no transcript, request, attempt, approval, turn, placement, grant, or lease
state directly; a delegation executor persists through its port. One terminal
attempt row holds a tool's output. An execution result entry references that
row; no result entry copies the output. The tool registry is an input to policy
and execution; it never determines request content. Approval and dispatch use
the snapshot frozen when the proposal was made, never a later lookup.

Contracts owned elsewhere bind here: one recorded attempt per model call in
[model-call-execution](model-call-execution.md); no transaction across external
I/O and the lock protocol in [persistence-protocol](persistence-protocol.md);
command claim and replay, and provenance-only attribution, in
[identity-and-commands](identity-and-commands.md); transcript append and
compaction in [sessions-and-transcript](sessions-and-transcript.md).

Undecodable argument text is the exact bounded UTF-8 the provider adapter
emitted after its preparation-time credential scrub. An undecodable value, or
JSON that does not decode against the selected tool's argument type, becomes a
typed execution error at preflight. Empty text blocks are omitted at the
provider boundary and create no semantic entry; tool proposals are never
omitted. A tool-use entry carries only its call and request references and never
copies the name or arguments, and a result entry never copies output, error
detail, or denial reason. A response with more requests than the fixed
per-response cap closes the producing call `KnownFailed` and creates no partial
batch, request record, or tool-use entry. If a definitive completion carries a
tool name or argument payload that cannot enter the bounded domain vocabulary,
the provider bridge converts the response to a typed `KnownFailed` observation;
it does not leave the call in flight, persist the inadmissible proposal, or
partially commit the response.

Approval visits requests in proposal order and the turn parks on the earliest
undecided request. No request in a batch executes while any request in that
batch is undecided. A turn has at most one live tool attempt, and approved
requests execute in proposal order. The next model round does not begin until
every request has one durable logical resolution: executed, denied, or closed by
turn end.

A declaration without an explicit posture follows one precedence: an
`AlwaysConfirm` declaration is consulted before any blanket and stays undecided
under it, then the frozen approve-all blanket, then the registry default, then
fail-closed confirmation when no declaration exists. The approval posture is
part of the configuration a turn binds at origin acceptance
([sessions-and-transcript](sessions-and-transcript.md)), and steering-derived
work inherits the frozen value of its source turn. When the provider credential
boundary suppresses a request's whole argument object, the application records a
fixed `RuntimeSafety` denial and continues the same turn; it never dispatches
sentinel JSON to an executor.

The judge selection is an optional direct-selection mapping; without one, the
judge uses the exact direct selection of the request-producing call. The judge
prompt carries the session's commissioned goal, template, frozen system prompt,
and optional dispatch authority, each separately delimited and quoted as
untrusted evidence, and the prompt treats them as scope to compare with the
request, never as instruction. Outside a turn judged under the commissioned
generation's dispatch authority, which [repo-watch](repo-watch.md) owns, an
`EscalateToHuman` result stores the completed call but no decision and leaves
the same request parked. Under that authority the request stays parked when the
turn has pending steering, or when the session escalated earlier and the
authority still stands; an operator-commissioned dispatch keeps the park while
its authority stands. A `KnownFailed`, `Refused`, `Cancelled`, or `Ambiguous`
terminal judge call retains the attended park while immediately admitting a user
decision.

Deny-and-end composes the recorded denial with the applied-interrupt stop path,
and the interrupt remains the proof-bearing authority for ending the turn. An
interrupt alone against an approval wait is not a denial and does not bypass the
decision command. A committed session closure first records core-issued
lifecycle-closure denials for the outstanding approval waits, then applies its
interrupt.

Recorded overrides are frozen into each prepared model call in the same
transaction as the blanket posture. Two things retire an override: the consuming
`UserOverride` approval that names it, and an approval of the identical command
by any other authority after the denial. Across turns, later means the
acceptance position of the input that opened each turn; inside a turn, attempts
chain through their predecessor. An override substitutes only for the judge: a
human, `AlwaysConfirm`, or automatic selection is never overridden, and the
consuming request still freezes the delegated posture.

Each family supplies its compiled declarations and matching executor and owns
its exact argument schemas, permission defaults, effect classes, bounds, and
execution results. The catalog owns only their composition and the name-directed
executor, which covers exactly the composed families; disagreement between the
advertised catalog and the executor is a daemon defect. A tool name shared with
a runner declaration is admitted only when the model-facing definition and
permission are equal and the local effect class maps exactly, `EffectFree` to
`Pure` and `ExternalEffect` to `SideEffecting`. Effect class controls crash
classification, not permission identity.

The workspace read, workspace mutation, local Git, and execution families bind
one workspace root, and that root is per session;
[configuration-and-credentials](configuration-and-credentials.md) owns its
derivation. Executors for a derived root are composed once per session and
retained under a bound, so two concurrent sessions bound to distinct derived
roots take two independent serialization domains. A session whose derived
directory is absent binds the configured root and shares the one composition
made at startup. One session holds exactly one such domain at a time, and a
retained set a request still holds is never released. Every declaration a
workspace-root-bound family advertises is a property of the family's code, not
of the repository it binds. Local Git is the exception: it compiles the pinned
repository's object format into its argument validators, and session composition
refuses an object-format disagreement.

An `Ambiguous` result atomically ends the issuing turn attempt as
`WithoutStop(Ambiguous)` and moves the lifecycle to `awaiting_tool_recovery`
correlated with that exact attempt. A tool that executes and exits nonzero
returns bounded structured `ExecutionFailed` evidence and is `KnownFailed`.
Output admission applies the size, U+0000, credential-redaction, and correlation
checks before durable semantic projection. If the authorization commit
acknowledgement is ambiguous, execution does not begin from the returned error:
the application rereads the attempt under the scheduler lock, and an
inconclusive reread retains that authority state for another identical reread,
so neither a retry nor a crash classification is inferred from a lost commit
response.

Before inserting the next attempt, acting on a loaded prepared attempt, or
preparing continuation, tool execution acquires the dispatch gate and
revalidates the loaded batch. It holds the gate through a preflight closure, or
from before authorization until the returned evidence commits, and interrupt
handling acquires the same gate before its command transaction. A pass that sees
an in-flight attempt also acquires the gate and reloads the attempt before
classifying prior-process crash loss. An interrupt that waits behind executor
work reloads the committed result before closing the batch, so it cannot strand
an issued request or roll back its command. The durable attempt cannot remain in
flight after the gate becomes available to an interrupt.

If the executor returns an operator failure without trustworthy evidence after
authorization, the service retains the gate and applies the attempt's
effect-class crash-loss transition. A committed classification carrying an
infrastructure or identity-collision failure fails or parks the affected turn
without failing unrelated session execution. A fail-closed corruption or
caller-or-hub bug remains an error after classification closes the attempt, so
the fatal execution supervisor still stops scheduling;
[runtime-substrate](runtime-substrate.md) owns the failure classes. If
trustworthy evidence returns but its commit fails, the service retains that
exact correlated observation as an opaque linear same-incarnation value and
never downgrades still-owned evidence to restart crash loss.

Preflight errors and trustworthy executor-reported errors resolve the logical
request and are visible to the next model round; they do not by themselves fail
the turn. A crash-lost error appends its result suffix and then fails the turn,
so no next round observes it. Physical ambiguity remains a turn-level recovery
wait and never becomes an ordinary error result. An interrupt against a tool
recovery wait does not reinterpret or erase the ambiguous attempt. Without an
interrupt, the daemon claims the same ambiguity through the
automatic-reconciliation ledger, which
[turn-lifecycle-and-scheduling](turn-lifecycle-and-scheduling.md) owns, and
terminalizes through the same boundary. Only an admitted executor `KnownFailed`
observation emits the failed-attempt telemetry event, which carries only the
catalog name, the closed error kind, the session, and the turn; completed,
ambiguous, and preflight-only failures emit none.

A result larger than the bound is replaced by the typed `ResultTooLarge` error,
and oversized bytes are never persisted. The result-text and error-detail bounds
constrain every executor's capture policy; no executor widens the durable bound
or converts an otherwise bounded success into a failure because it omitted
additional result members. A crash-lost attempt has durable `KnownFailed`
evidence and therefore projects an execution result, not `ToolClosed`. Attempt
evidence commits as soon as execution ends, independently of semantic
projection.

Once every request in a running batch is resolved, one continuation transaction
appends exactly one result entry per request in proposal order, consumes every
pending steering input in ascending acceptance position and appends its entry
after the results, derives the exact prefix-preserving frontier extension, and
creates the next round's `Prepared` model call against that frontier. These
effects commit or roll back together. An interrupt or crash loss that ends the
turn appends the result suffix with its terminal marker and prepares no call.
When at least one request entered execution, the continuation turn attempt
already entered `Running` during authorization and owns the new call without
moving backward. An optional configured ceiling bounds the tool rounds one turn
may complete, and a policy of none sets no ceiling. After the last batch a
ceiling admits resolves, continuation still projects every result and creates
its `Prepared` call, and model execution closes that call `KnownFailed` before
capability preparation or send.

At most 256 MiB of projected frontier content may be rendered into one call's
provider messages. The bound counts every kind of content the render clones, not
tool evidence alone, and it is enforced once the projection names its entries
and before any content is cloned, so an over-bound frontier is refused rather
than materialized. The refusal closes the turn through the tool-round-limit
terminal cause before capability preparation or send.

The result projection a stop consumes is bound to the interrupted turn: reusing
the turn's current frontier identity is not sufficient, and a projection
prepared for another turn cannot terminalize it. A request can never remain an
open logical dependency behind a terminal turn.

Raw request identity is not approval-wait evidence; the wait is reconstituted
from the request's session, turn, producing call, batch order, and undecided
state. Startup scanning leaves an approval wait unchanged: it never fabricates a
decision, advances to a later request, expires the wait, or creates an attempt.
The activated execution pass returns while approval is pending, releasing its
scheduler worker. Rejected or uncommitted decision commands leave the approval
phase unchanged and create no resumable hint. The eligibility sweep inventories
a running batch after restart, including one whose decision committed before the
prior process stopped, so progress does not depend on process-local wake memory.
With no live model call, a batch is resumable when it has no current tool
attempt and either has an approved unattempted request or has durably resolved
every request. Restart never requires the current continuation attempt to
disappear.

The session-delegation and plan tools take their invoking session, turn, and
request from trusted dispatch correlation, never from model arguments; a child
or peer session identity is an ordinary schema argument. `send_session_message`
verifies that the invoker is exactly the parent or the child before appending a
message, and either side may call it while the other is active, idle, stopped,
or cancelled. Replaying an already delivered wait returns the same mode-specific
receipt or outcome. Before the application accepts a durable-wait or
durable-completion disposition from the delegation executor, persistence rereads
the parked batch and the ended dispatch fence, or authenticates the returned
correlation against the ended attempt; absent or cross-wired evidence fails
closed. A delegation effect commits in the same transaction as its terminal
tool-attempt row.

The provider bridge derives the provider-visible tool-call correlation from
`ToolRequestId`, so provider-native identifier types and messages never cross
the application boundary. Every rendered result resolves its referenced durable
record first, and missing or cross-wired content fails closed. All text and tool
proposals from one model call coalesce into one assistant message, and the
proposal-ordered results into the immediately following user-role message. Every
provider-visible failure is one compact provider-neutral JSON object with an
error member carrying detail and kind, except a non-content delegation outcome,
which renders its outcome, reason, and provenance. Malformed proposal arguments
stay exact on the durable request but replay to the provider as a fixed
placeholder object, so the paired typed error reaches either provider; the
placeholder is never durable evidence.

A `web_fetch` request's canonical origin must satisfy the deployment-owned
web-fetch catalog policy in
[configuration-and-credentials](configuration-and-credentials.md) before
dispatch, and the transport that carries an admitted request is stated in
[web-egress-threat-model](web-egress-threat-model.md). Failure before request
dispatch returns a fixed sanitized known failure; timeout, transport, or body
loss after dispatch begins is commit-ambiguous. Both web tools declare
`ExternalEffect`, and for both the shipped human posture supersedes the
declaration's confirm default and the session blanket, so a request parks before
it reaches its transport or credential boundary.

The blob tools authorize only digests present in attachment stubs in the
rendered frontier for the issuing turn. A visibility or budget closure resolves
the logical request before the request reaches the executor, and a store failure
is returned by the executor after it traverses and verifies the recorded
replicas. Both leave previously charged bytes charged and permit the next model
round; neither enters the crash-loss path nor fails the turn.
[blob-storage](blob-storage.md) owns the budgets. `session_status_update`
derives a durable command identity from the physical tool attempt and attributes
the command and last-writer stamp to the exact `ToolRequestId`.

Code-host read-only declarations default to automatic approval and the mutations
to confirmation, so the approval transaction authorizes each mutation before
credentials resolve. Every code-host declaration, reads included, is
`ExternalEffect`. `repository_read_file` and `repository_list_directory` require
an exact lowercase 40-hex commit revision and never default to a branch head.
Paths use canonical repository-relative spelling with no empty, dot, or parent
component; a bare dot names the repository root for a file read or directory
listing and is rejected as a changed-file patch path. A returned node id, head
revision, or continuation is admitted by the same predicate as its argument
counterpart, so it can be passed back as an argument. Every returned URL is one
absolute credential-free HTTPS location. No code-host result has more than 100
collection members or more than 512 KiB of encoded JSON. Every bounded
review-log list reports whether it is truncated together with its continuation
cursor, and a verdict never treats a partial evidence page as complete. The
reviewer verdict is parsed from review bodies and issue comments merged in
code-host timestamp order, and a usage-limit response is recognized separately
as one exact canonical text that supersedes an earlier verdict until a later
verdict arrives. Only the reviewer bot account supplies a verdict or a
usage-limit response, and a verdict must carry a line whose whole content is the
`Reviewed commit:` label followed by a 7-to-40-character hexadecimal revision,
with only emphasis or backtick markers around them. The last such line in the
latest activity that carries one is the verdict. A verdict whose revision does
not prefix the current head is stale and never counts as current convergence
evidence. The latest exact review request by an owner, member, or collaborator
with no later reviewer response marks the review in flight and blocks
convergence. The authenticated job-log endpoint is the sole redirect-shaped
exchange: after one 302 the adapter validates the location, pins a wholly public
destination set, and downloads credential-free. A read transport or server
failure is an executor infrastructure failure, while a mutation transport loss,
server failure, or malformed acknowledgement is commit-ambiguous.
`change_request_thread_reply` and `change_request_thread_resolve` query thread
ownership before they mutate, and a failure of that query classifies the
mutation as not dispatched rather than ambiguous. The adapter never returns
code-host response bodies as error detail.

Preparing a model operation collects all frontier-referenced requests, attempts,
and decisions in one batched query per record family, with no per-entry round
trips under the scheduler lock.

## Planned

- Pre-approval admissibility: a family may declare a request inadmissible before
  any approval decision, resolved at request level with a fourth
  `ToolInadmissible` result entry; see
  [tool-loop design](../design/tool-loop.md).
- Instruction admission: the commit-result and continuation transactions append
  an `InstructionAdmission` and a successor instruction manifest for a
  successful `instructions_read`; see
  [tool-loop design](../design/tool-loop.md).
- Child creation by `spawn_session`, the child's `DelegatedTask` origin, and
  delivery of its terminal result to the parent: no present surface creates the
  child, and the daemon rejects execution until the placement-owned creation
  transaction exists; see [tool-loop design](../design/tool-loop.md).
- Runner-locus execution rules: the lost-lease retry exception and the runner
  approval ladder; see [runner protocol design](../design/runner-protocol.md).

# Tool loop

The workspace-instruction effect of the continuation transaction is committed
unimplemented functionality. No present tool can create an instruction
admission, and the transaction therefore has the four implemented effects named
below.

This page specifies the implemented daemon-owned tool subsystem. Its
runner-locus paragraphs are committed unimplemented functionality that extends
the same laws to the runner locus;
[runner protocol and placement](runner-protocol.md#planned) owns their present
implementation status. This page owns logical tool requests, approval policy and
decisions, physical tool attempts, result admission, intra-turn continuation,
crash classification, the compiled registry, and the daemon-local catalog. Turn
and attempt lifecycle law lives in
[turn-lifecycle-and-scheduling](turn-lifecycle-and-scheduling.md); semantic
entry vocabulary in [sessions-and-transcript](sessions-and-transcript.md);
model-call staging and provider translation in
[model-call-execution](model-call-execution.md); durable-command identity in
[identity-and-commands](identity-and-commands.md); and relational mechanics in
[persistence-protocol](persistence-protocol.md). Invariant tags cite
[the invariant test index](../invariants.md).

## Intra-turn rounds and request batches

One turn spans the complete propose → decide → execute → result → continue loop.
A model call is one physical round inside that turn. A completed response with
no tool request appends `TurnCompleted` and terminalizes the turn. A completed
response containing one or more tool requests never terminalizes the turn: it
ends the current turn attempt as a tool-round yield and keeps the active slot
while the batch is resolved. A later model call uses a fresh turn attempt in the
same turn. Why: a turn is the logical conversational outcome, while a model call
and a turn attempt are physical tenures that may repeat without changing that
logical identity (INV-004, INV-006).

A completed response carries ordered assistant text and tool proposals. For each
proposal the application supplies one fresh UUIDv7 `ToolRequestId`; the domain
assigns a zero-based ordinal among tool proposals in that producing call. The
producing call, name, normalized arguments, ordinal, and resolved approval
posture form one immutable `ToolRequest` record. The name is 1–64 ASCII letters,
digits, underscore, or hyphen. `NormalizedToolArguments` has two closed arms.
`Json` stores a decoded JSON value as compact text with object keys in lexical
order; `Undecodable` stores the exact bounded UTF-8 text emitted by the provider
adapter after that adapter applies its preparation-time credential scrub when
JSON decoding fails. Undecodable text must also exclude U+0000, mirroring the
result-content admission. Both arms must fit within 1 MiB before and after
normalization. This preserves malformed arguments as bounded, identity-safe
evidence without treating them as JSON. An undecodable value, or valid JSON that
does not decode against the selected tool's argument type, becomes a typed
execution error later.

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
resolution: executed, denied, closed by turn end, or — for a family that
declares the pre-approval admissibility check below — closed inadmissible.

**Committed unimplemented functionality — pre-approval admissibility.** A family
may declare that a request is inadmissible on evidence available before any
approval decision, and where a family declares one it takes precedence over the
ordinary path for the same condition: an argument-schema failure for such a
family resolves here, request-level and with no attempt, rather than through the
prepared-attempt `KnownFailed` route that families without the declaration use.
The two cannot both run, and the declared check is the earlier of them. The
instruction family declares two: arguments that do not decode to its schema, and
a bundle outside the effective eligibility view, both specified by
[workspace instructions](workspace-instructions.md). Such a request resolves
before approval through a request-level transition that mints no attempt: it
records a fourth durable logical resolution, `closed_inadmissible`, carrying the
family's typed reason on the request itself, and creates no approval state, no
judge call, no attempt row, and no executor work. It must be request-level,
because a tool attempt names its issuing turn attempt and a batch parked on an
undecided approval has no current turn attempt to name — minting one here would
either orphan the attempt or force a turn attempt into existence under the
approval-wait constraints. Storing the reason on the request instead is sound
because nothing executed: there is no attempt history to explain, only a request
that was never admissible. The projection renders it through the same
provider-visible error object as any other typed failure, so no new result shape
reaches a provider. A request resolved this way is not undecided, so the batch
is not parked behind it and proposal order continues at the next request. It is
resolved for the continuation boundary too: it projects one `ToolInadmissible`
result entry in proposal order, and it satisfies the batch-complete condition
that creates the fresh continuation turn attempt. A batch whose only proposal is
inadmissible therefore still prepares its next model call rather than stalling
with nothing to project. Why this rather than deciding approval first: a
delegated decision needs evidence the daemon can only build for a bundle the
session may see, so asking a judge about an inadmissible request would mean
either exposing metadata the session is not entitled to or sending an
evidence-free prompt. No present family declares such a check.

## Approval policy and decision sources

Every request has an approval state separate from its execution state. The
implemented decision sources are:

- `UserCommand` — one applied user-global durable decision command;
- `PolicyAuto` — the selected registry or sandbox-profile default supplied
  automatic approval;
- `SessionBlanket` — the frozen dangerous blanket supplied daemon-local
  automatic approval;
- `SessionOverride` — an exact runner-placement tool override supplied automatic
  approval;
- `Delegate` — an authority-checked approval-judge call decided the request;
- `UserOverride` — a user-recorded one-shot override of a delegate denial
  supplied approval when the session re-proposed the denied command;
- `RuntimeSafety` — the provider credential boundary suppressed the complete
  argument object, so the producing-call transaction recorded a fixed denial
  before any executor could observe the request; and
- `LifecycleClosure` — a committed session closure supplied a reasonless core
  denial before its interrupt terminalized the live turn.

A delegated decision names the exact direct model selection and dedicated model
call that made it, and retains the judge rationale as nonempty text of at most
4,096 bytes. A user decision instead names its exact durable command. A consumed
user override names its override durable command and the exact delegate-denied
request it overrides — user agency exercised in advance through that command.
Automatic policy, runtime-safety denial, and lifecycle-closure denial have no
decider or rationale. The closure denial retains its core-issued durable command
as authority evidence. None of the automated paths can claim user agency
(INV-020).

Each daemon tool mapping may declare one approval posture: `Auto`, `Delegated`,
or `Human`. The selected posture is frozen into every resulting request. For
every definition, an explicit posture is authoritative: `Auto` records
`PolicyAuto`, `Delegated` parks for a judge, and `Human` parks for the user even
when the session blanket would otherwise approve. When the mapping omits the
posture, the precedence below applies.

An `AlwaysConfirm` permission narrows that rule for exactly one posture. The
declaration exists so that no session blanket can silently approve the tool, so
a configured `Auto` posture never satisfies it: automation there would remove
the decision the declaration requires. A configured `Delegated` posture does
satisfy it, because an approval judge is not a blanket but a distinct decider
that can still deny the request or escalate it to the user. A configured `Human`
posture leaves the stricter `AlwaysConfirm` outcome unchanged, since both await
the same user. With no posture configured, an `AlwaysConfirm` declaration parks
for a human under either session-blanket posture.

Daemon-local execution therefore leaves an `AlwaysConfirm` declaration undecided
unless an explicit `Delegated` posture is configured for it; the dangerous
blanket alone can never override that posture. All other declarations keep this
precedence:

1. the frozen session posture `DangerousToolAutoApproval::ApproveAll`;
2. the registry default (`Auto` or `Confirm`); then
3. fail-closed `Confirm` when no declaration exists.

Runner execution instead resolves approval from the immutable placement facts
that [runner protocol and placement](runner-protocol.md) owns:

1. an exact per-tool override, recording `SessionOverride` for `Auto` and
   leaving `Confirm` undecided;
2. otherwise the sandbox profile: a workspace-restricted placement approves
   every tool and an ambient placement approves only a pure tool, both recording
   `PolicyAuto`, and every other tool is left undecided; then
3. fail closed when no exact daemon-owned runner declaration exists.

The dangerous blanket has no runner rung. The producing-call completion
transaction resolves policy independently for every proposal after selecting its
admissible locus and immutable definition snapshot. A frozen automatic choice
may exist after an earlier confirmation wait without bypassing it; only explicit
user or delegate decisions must form a proposal-order prefix. After each user
command, the earliest remaining undecided confirmation is the next wait, while
already frozen automatic decisions require no later command. Why: recording the
selected source makes unattended operation inspectable without presenting policy
as human consent.

The blanket is a field of each immutable `VersionedSessionConfigurationDefaults`
value and is named `DangerousToolAutoApproval::{Disabled, ApproveAll}`. Explicit
session creation uses `Disabled`; template-derived creation copies the resolved
template's configured blanket. Replacement installs a complete later defaults
version through the existing `ReplaceSessionDefaults` command. Origin acceptance
freezes the posture into `EffectiveConfiguration` alongside model selection;
steering-derived work inherits the frozen value of its source turn. A later
defaults replacement never changes queued, active, or completed work (INV-008).

A user decision is the canonical `DecideToolRequest` command: user-global
`DurableCommandId`, exact `ToolRequestId`, and either `Approve` or
`Deny { reason }`. A denial reason is absent or 1–1024 bytes of non-control
Unicode with no leading/trailing POSIX whitespace; it is therefore safe to
render without copying unbounded or terminal-control content. Equality excludes
only the command identifier. The `decide_tool_request` request in
[process-protocol](process-protocol.md) is the client surface that issues this
command; its wire posture requires a denial reason even though the command
admits an absent one. Registry lookup precedes current-state validation; equal
replay returns the recorded applied-or-rejected result, cross-kind or
different-payload reuse conflicts, and a pre-commit failure claims no identity
(INV-012).

A delegated request first commits the same `awaiting_tool_approval` park as a
human request. The daemon then prepares and authorizes one dedicated approval
judge call through the configured provider adapter using the session credential
snapshot. The judge selection is an optional direct-selection mapping; when it
is absent, the exact direct selection chosen by the request-producing call is
used, so the default remains the judged session model tier. The call is visible
in model-call history with its selection, resolved provider target, credential
reference, state, disposition, and reported token usage. Its closed result is
`Approve`, `Deny`, or `EscalateToHuman`, always with rationale.

For a turn recorded in the generation a repository-watch dispatch commissioned,
preparation also reads the immutable dispatch authority linked to that dispatch.
Pull-request authority contains the dispatch identity, watched repository,
pull-request number, exact head commit, head repository and branch, and base
branch; branch authority contains the dispatch identity, repository, and branch.
The judge receives this structured authority beside the commissioned goal,
template, and frozen system prompt. A judged turn in any other generation of
that session — an unrelated successor goal it later accepted — resolves no such
authority, as does a turn no generation recorded: the dispatch described
neither. Those turns are prepared, judged, and escalated exactly as in a session
no dispatch created, which [repository watch](repo-watch.md) states from the
dispatch side. Every session-derived field is separately delimited and quoted as
untrusted evidence, and the judge prompt treats it as scope to compare with the
proposed request rather than as instruction. The context comes from the
append-only dispatch action and triggering event, not from mutable provider
state or text reconstructed from the goal.

The judge may approve or deny only a request frozen as `Delegated`. Outside a
repository-watch-dispatched session, including in an operator-commissioned
session judged under its recorded fence, an `EscalateToHuman` result stores the
completed call but no approval decision and leaves the same request parked. A
`KnownFailed`, `Refused`, `Cancelled`, or `Ambiguous` terminal judge call
likewise retains that attended park while immediately admitting a user decision,
so a terminal judge failure cannot prevent that decision. In a session judged
under repository-watch dispatch authority, no user attends the approval wait
unless steering accepted while the judge was outstanding still names the judged
turn. A repository-watch session that already recorded an escalation is also
attended: its exceptional block has no automatic resumption, so only an operator
could have resumed it. Either turn keeps the attended park described above: its
completed `EscalateToHuman` leaves the turn active and the request parked for
that user, exactly as in a session no dispatch created, and no steer is
reclassified or stranded by terminalization. Otherwise a completed
`EscalateToHuman` closes every unresolved request in the active batch as
`ToolClosed`, appends `TurnFailed`, terminalizes the turn with the completed
judge escalation as its typed cause, and records an append-only audit row
linking the judge call and rationale, request, dispatch action, terminal
attempt, failure entry, and terminal frontier. Repository-watch work atomically
blocks its still-authoritative goal under the exceptional no-resume policy and
participates in the ordinary release and re-arm rules. A generation stopped,
achieved, or superseded during the provider round-trip remains ended, so
reconciliation has nothing to resume.

A request frozen as `Human` admits only an escalation result from a delegate; a
delegate approval or denial is rejected by both domain reconstruction and
relational provenance constraints (INV-049). Thus delegation can narrow
authority but never widen it. A completed approve or deny atomically records the
decision and advances the same proposal-ordered batch transition used by a user
decision. Each explicit user or delegate decision emits one ordered
`ToolApprovalDecided` event carrying the decision, decider kind and identity,
and delegate rationale when present.

The consume-and-proceed transaction locks the owning session, validates that the
request is the turn's earliest undecided request, records the command and
`UserCommand` decision, and then either parks on the next undecided request or
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
([process-protocol](process-protocol.md)); a `stop_turn` against the parked wait
records the typed `interrupt_unavailable_while_awaiting_approval` rejection and
leaves the wait intact.

A judge denial the user disagrees with is reversed forward, never in place: the
denial is terminal (INV-027), and the session may re-propose after a denial
because the denial reason reaches the model at the continuation boundary. The
canonical `OverrideDeniedToolRequest` command — user-global `DurableCommandId`,
the owning `SessionId`, and the exact denied `ToolRequestId`; equality excludes
only the command identifier — records one one-shot pre-approval for that
re-proposal. Recording verifies every conjunct of the override predicate against
durable evidence, each with its own recorded rejection: the recorded approval is
a delegate denial (a user denial or any approval admits no override), the denial
is terminal (its denied-result entry is materialized, so a denial whose round is
still resolving cannot be overridden), the request belongs to the command's
session, and no override is already recorded for it — each denial admits at most
one override ever. The session is part of the canonical payload, unlike
`decide_tool_request`, because the recorded override is a session-scoped
standing fact consumed by a later proposal. An applied command durably links the
denied request, its denying judge call, and the override command.

Recorded overrides are frozen into each prepared model call in the same
transaction as the dangerous blanket posture, so consumption has blanket-frozen
semantics with no mid-call races: a prepared call's override inventory is part
of that call's immutable input, and an override recorded after the call is
checkpointed takes effect at the next prepared call, never at that one.

Only a still-effective override is frozen, and two things retire one: the
consuming `UserOverride` approval that names it, and an approval of the
identical command recorded by any other authority after the denial — the judge
approving a re-proposal it once denied, a user decision after escalation, or a
policy approval. The second is needed because the call that first carries a
denial cannot hold that denial's override, so its re-proposal is decided without
one, and an override left standing after that decision would pre-approve a
repeat of a command the session has already let through. Ordering here is
structural, because none of these append-only records carries a timestamp.
Across turns the order is the acceptance position of the input that opened each
turn; inside a turn each model call owns one turn attempt and attempts chain
through their predecessor, so a proposal counts as later when its turn was
accepted after the denial's or its attempt continues the denied proposal's. Both
are needed, because the re-proposal an override exists for is normally made in
the denial's own turn. The scope is required: the same command is routinely
approved and executed earlier in a session, long before a later proposal of it
is denied, and an unscoped rule would retire most overrides at the instant they
were recorded.

The continuation that first carries a denial to the model therefore never
carries that denial's override. That continuation is checkpointed by the same
transaction that projects the denied result, so at the instant its override
inventory freezes no override for the denial can exist — the user has not yet
been shown the denial to disagree with. The user overrides it, the model
re-proposes on the following call, and that call's frozen inventory carries the
override. The accepted cost is one extra round; the gain is that a prepared
call's approval inputs are fixed at checkpoint and no concurrent command can
race them.

When the completing call proposes a command whose initial selection would park
for the judge (`Delegated`) and an unconsumed recorded override matches the
exact denied command — equal tool name and equal normalized arguments — the
proposal records an immediate `UserOverride` approval at proposal time instead
of parking. Each recorded override is consumed at most once per response in
proposal order, and once ever durably: the consuming decision row names the
overridden denial through a UNIQUE column, so a second identical proposal parks
for the judge again. The override substitutes only for the judge: a `Human`,
`AlwaysConfirm`, or automatic selection is never overridden, and the consuming
request still freezes the `Delegated` posture. Consumption emits the same
ordered `ToolApprovalDecided` event as other explicit decisions, carrying the
override provenance, so the full audit chain — judge denial, override command,
consuming approval — stays queryable end to end.

## Registry, placement, and effect metadata

The application `ToolCatalog` port supplies immutable daemon-local
`ToolDefinition` values: name, model-facing description, argument JSON Schema,
permission default (`Auto`, `Confirm`, or `AlwaysConfirm`), optional approval
posture (`Auto`, `Delegated`, or `Human`), and the stored two-class crash
classification used by the implemented local attempt machinery.

Each runner-advertisable name has one immutable daemon-owned
`RunnerToolDeclaration`. It carries a required checked model-facing description
and canonical JSON-object argument schema, the required three-way
`RunnerToolEffectClass` (`Pure`, `Idempotent`, or `SideEffecting`), one nonempty
`ToolAdmissibleLoci` value (`DaemonOnly`, `RunnerOnly { selector }`, or
`DaemonOrRunner { selector }`), and the session capability its execution
requires. Pure implies idempotent; idempotent work may change state but is safe
to repeat. The combined locus prefers the session's attached eligible runner,
falling back to daemon-local execution. Declarations are static per tool; a
model or runner cannot select another locus per call. Every runner-only tool
therefore still has one authoritative definition for model advertisement and
argument validation. The typed placement and runner-dispatch law is owned by
[runner protocol and placement](runner-protocol.md).

The daemon-local application catalog is one process-lifetime immutable compiled
value. Its `EffectFree` declaration maps to `RunnerToolEffectClass::Pure`, and
`ExternalEffect` maps to `RunnerToolEffectClass::SideEffecting`; no local
declaration can project `Idempotent`. Before a shared name can use a daemon
locus, the application adapter validates exact model-facing description and
schema, permission equality, and this effect mapping against the authoritative
runner declaration; it also compiles the schema into the executable validator
used before dispatch. A mismatch is unavailable, never a choice between two
policies. The consolidated placement and policy snapshot is persisted. Catalog
lookup and iteration remain ports rather than a static global, but runtime
rebinding and deployment compatibility for outstanding requests are not
implemented; they require the durable definition-revision decision recorded
under Open edges.

<a id="version-one-workstation-tool-contracts"></a>

The workstation-facing registry is daemon-local and process-lifetime immutable.
The daemon composes it from these implemented families:

- basic tools (`current_time`, `echo`, and `session_status_update`);
- blob-read tools (`blob_metadata` and `blob_read`) when blob storage is
  configured;
- web fetch and search;
- code-host and mapped GitHub pull-request tools;
- mapped workspace read and mutation tools;
- mapped conversation tools;
- session plan tools and `goal_declare`;
- the mapped local Git tools `git_status`, `git_diff`, `git_log`, `git_stage`,
  `git_create_commit`, `git_branch_create`, and `git_branch_switch`; and
- the mapped execution tools `sandboxed_exec`, `unsandboxed_exec`, and
  `cargo_diagnostics`.

Each family supplies its compiled declarations and matching executor. The
family's code owns its exact argument schemas, permission defaults, effect
classes, bounds, and execution results; this cross-crate contract owns only
their composition into one daemon catalog and name-directed executor. Mapped
families are absent when their complete deployment configuration is absent.

The workspace read, workspace mutation, local Git, and execution families all
bind one workspace root, and that root is per session. Each session's root is
derived from the configured root by a fixed formula owned by
[configuration and credentials](configuration-and-credentials.md#daemon-tool-mapping-registry);
a session supplies no path and cannot select another session's root. The
executors bound to one root are composed once per session and retained under a
bound, so two sessions executing concurrently write two trees, hold two pinned
root descriptors, and take two independent serialization domains for the
mutation and Git families rather than one process-wide domain. One session takes
exactly one such domain at a time: a retained set a request still holds is never
released, so a second set is never composed beside one already mutating that
session's tree. Sessions with no derived directory share the configured root's
own composition.

The catalog stays one process-lifetime immutable compiled value across
per-session root binding. Every declaration a workspace-root-bound family
advertises — its name, description, schema, permission default, and effect class
— is a property of the family's code rather than of the repository it binds. The
one compiled value that is not is the local Git argument validator, which
carries the pinned repository's object format. A session whose repository
selects another format is therefore refused rather than validated against the
configured root's object format, and the refusal has two shapes because catalog
preflight runs before any session executor is resolved. An argument carrying a
full object identifier in the session's own format is refused at preflight as
invalid arguments, since the one compiled validator admits only the format it
was compiled with. Every other argument reaches composition, which rejects the
disagreeing repository and closes the request as a known tool failure whose
sanitized detail names the closed reason. Neither shape redirects the request to
another session's root.

Every advertised argument schema declares an object at its root and carries no
root keyword outside that object declaration (INV-055). One request carries the
whole catalog, so a provider that refuses a single schema refuses every exchange
offering it: a root-level union is a family-wide outage, not a per-tool cost. An
internally tagged argument type is therefore advertised as one object whose tag
property holds the variant vocabulary and names what each variant requires,
while its Rust type still decodes the tagged form unchanged; the advertised
schema alone widens, and each family's own argument validation still refuses
what the declaration excludes.

The exact required inputs and fail-closed startup validation are owned by
[configuration and credentials](configuration-and-credentials.md#daemon-tool-mapping-registry).
The mapping-free base composition remains available without local Git or
execution tools.

The implemented `git_push_configured` declaration is not part of the daemon
registry. No production `GitPushTransport` exists, so no production composition
can provide the transport authority that tool requires. The seven local Git
tools perform no remote operation. The transport design and any later
registration remain undecided under
[Daemon Git push transport](../open-questions.md#scheduling-and-runners).

This daemon-local registry supplies no runner execution path. The
[runner executable boundary](runner-protocol.md#planned) owns the present
implementation status and committed compatibility constraints; the
[runner workstation open question](../open-questions.md#scheduling-and-runners)
owns the remaining undecided registry work.

Each provider operation carries the exact session-executable definition and
locus snapshot prepared under [model-call execution](model-call-execution.md).
Runner-only definitions absent from current selected execution authority are not
advertised; `RunnerAbandoned` exposes daemon-executable declarations only, and
lost placement blocks preparation until user recovery. Initial approval and
dispatch for a proposal are derived from that same frozen snapshot, never from a
later catalog or registration lookup. A dynamic catalog or runner change while
the provider call is in flight therefore cannot upgrade permission, introduce an
unavailable runner tool, widen its frozen selector, or silently move the
selected locus.

The registry is advisory input to policy and execution, never request-content
authority. For daemon-local execution, a model may propose an unknown name;
absent a frozen `ApproveAll` blanket, fail-closed policy requires confirmation,
and an approved unknown request produces a typed `UnknownTool` error without
invoking an executor. Runner locus selection requires an exact declaration and
advertised availability before approval, so an unknown runner name is
unavailable and never reaches a runner lease. Because the attempt schema
requires a closed effect class, preparation records `EffectFree` as a
non-dispatching sentinel when no declaration exists. The preflight transaction
closes that attempt before authorization and before the executor boundary; the
sentinel is not a claim that an unknown tool is safe to run. A declaration added
or removed after the request was recorded does not rewrite its name or
arguments.

Effect class controls crash classification, not permission identity. In the
daemon-local executor, a crash-lost prepared attempt, or an in-flight attempt
declared `EffectFree`, closes `KnownFailed` and fails the current turn; version
one performs no automatic local retry. A crash-lost in-flight attempt declared
`ExternalEffect` closes `Ambiguous`, ends the abandoned turn attempt `Lost`, and
parks the turn in `AwaitingRecoveryDecision` naming that exact tool attempt
(INV-025, INV-026, INV-034). Runner lease loss uses the separate re-lease law in
[runner protocol and placement](runner-protocol.md); re-leasing one fenced
runner attempt is not the local executor fabricating a new physical attempt.

## Serialized staged execution

The application selects the admissible locus before authorization. Daemon-local
execution crosses the in-process `ToolExecutor` port. Runner execution crosses
the lease repository and checked wire adapter. Either executor receives checked
request content and returns evidence; neither can write transcript, request,
attempt, approval, turn, placement, grant, or lease state directly (INV-024).
Execution is serialized:

- approval visits requests in proposal order;
- a turn has at most one live tool attempt;
- approved requests execute strictly in proposal order;
- each attempt reaches a durable terminal state before the next attempt is
  created; and
- the version-one runner holds one process-wide execution permit across all
  sessions.

After all approvals resolve, the fresh current turn attempt owns the batch
execution and continuation. For each next approved request:

1. **Prepare transaction.** The application mints a UUIDv7 `ToolAttemptId` and
   commits a `Prepared` attempt row before executor work. It fixes the request,
   owning turn, issuing turn attempt, effect class, selected locus, sandbox
   profile when applicable, and `ToolDispatchGeneration::first()`.
2. **Authorize transaction.** Fresh locked state validates that the request is
   approved, is the earliest unresolved executable request, and still belongs to
   the issuing current turn attempt. For daemon execution it transitions the
   attempt to `InFlight` and the turn attempt to `Running` when necessary. For
   runner execution one repository transaction additionally consumes any
   workspace-ready evidence, pins initial placement and grant when needed,
   consumes the exact runner authorization, stores the offered lease, and moves
   the same attempt and turn states. A crash cannot commit `InFlight` without
   the correlated lease.
3. **Claim transaction.** The runner admits the complete immutable dispatch but
   cannot execute it. The daemon validates and commits the exact lease claim
   before acknowledging it. Only receipt of that acknowledgement plus the
   matching `dispatch` gives the runner an execution capability. This step is
   absent for daemon-local execution.
4. **Execution.** No database transaction spans the effect. The executor
   correlation contains runner and lease generation when applicable in addition
   to request, tool attempt, issuing turn attempt, and dispatch generation. Only
   the issued authorization or claimed-lease capability can bind returned
   evidence into a committable observation; raw durable facts can compare but
   cannot bind.
5. **Commit-result transaction.** Fresh locked state validates the complete
   correlation and current dispatch generation. For a runner result, one
   transaction also locks the current lease, proves that the claimed lease is
   the source of the observation, stores the lease completion, and consumes the
   result exactly once. A stale or duplicate result cannot advance either
   aggregate (INV-011, INV-021). The attempt moves monotonically to `Completed`,
   `KnownFailed`, or `Ambiguous` and never reopens. An `Ambiguous` result
   atomically ends the issuing turn attempt as `WithoutStop(Ambiguous)` and
   moves lifecycle to `awaiting_tool_recovery` correlated with that exact
   attempt.

**Committed unimplemented functionality — instruction admission effect.** For a
successful fresh `instructions_read`, the commit-result transaction also locks
the session's admitted-set head and atomically appends the
`InstructionAdmission` specified by
[workspace instructions](workspace-instructions.md) with the receipt-only
completed result. A stale head, failed read, or failed admission validation
discards the admission, not the round: the transaction commits the terminal
typed failure as this attempt's result and leaves the admitted-set head and
every existing admission untouched. Rolling both back would discard a completed
executor result and strand the attempt `InFlight`, which contradicts the
monotonic terminal transition required above and would block the serialized
batch behind an attempt that can never close. The distinction is that the
executor work already happened outside any transaction; what this transaction
decides is whether its evidence becomes an admission, and a rejected admission
is a recorded result, not grounds to roll back the attempt. Replay of an already
committed request returns the recorded receipt and admission link without
appending either again; a conflicting receipt or link is corruption. That head
lock's position in the repository-wide order and its mode belong to the
[persistence lock protocol](persistence-protocol.md), which carries it in the
same inventory as this transaction's scheduler lock; this page states that the
lock is taken, never where or how. No present tool supplies this effect.

The runner durably spools a terminal evidence envelope until `result_recorded`.
A process exit, timeout, supervisor loss, or channel loss after claim is not a
known tool failure; it enters the effect-class loss and ambiguity law. A tool
that executes and exits nonzero returns bounded structured `ExecutionFailed`
evidence and is `KnownFailed`. Output admission applies the existing size,
U+0000, credential-redaction, and correlation checks before durable semantic
projection.

If the authorization commit acknowledgement is ambiguous, execution does not
begin from the returned error. While retaining the dispatch gate and exact
request, the application rereads the attempt under the scheduler lock.
`Prepared` proves non-consumption and returns the infrastructure failure. For a
daemon locus, `InFlight` restores the exact authorization fence and may enter
the executor. For a runner locus, it must also load the exact offered or claimed
lease and resume only the corresponding wire phase; bare attempt state never
permits execution. An inconclusive reread retains that authority state for
another identical reread, so neither retry nor crash classification can be
inferred from a lost commit response.

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
effect-class crash-loss transition. A committed classification contains an
infrastructure or identity-collision failure as the ordinary `CrashClassified`
outcome, so the affected turn either fails or parks for reconciliation without
failing unrelated session execution. A fail-closed corruption or caller-or-hub
bug remains an error after that same classification closes the attempt, so the
daemon's fatal execution supervisor still stops scheduling. A failed
classification retains the exact attempt identity and permit for another
classification pass. It also retains whether closure belongs to prior-process
loss, an executor failure, or a correlation mismatch. An executor failure keeps
its safe class and cause token, so a later successful classification emits the
same nonfatal diagnostic or returns the same fatal class; a correlation mismatch
likewise resurfaces only after closure. The initial combined error preserves
both the executor failure or mismatch and the classification failure. The
durable attempt therefore cannot remain `InFlight` after the gate becomes
available to an interrupt.

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

Because a resolved request is otherwise recorded only in the session transcript,
admitting a `KnownFailed` observation also emits one operator telemetry event
carrying the dispatched catalog name, the closed error kind, and the session and
turn identities — never the bounded error detail, tool arguments, or any
response content. Admission is the single site: it covers every executor behind
the one dispatch trait and the failures admission itself substitutes for
oversized or null-bearing results. Completed and ambiguous observations emit
nothing here; ambiguity is carried by the recovery wait above. Preflight
failures that never reach admission — unknown names and argument-decode failures
— are likewise silent, being model-authored rather than deployment facts.
Telemetry field rules are owned by
[identity-and-commands](identity-and-commands.md#boundary-contracts).

An interrupt against a tool recovery wait does not reinterpret or erase the
ambiguous attempt. It materializes exactly one reference-only result per request
in proposal order: completed or known-failed attempts use `ToolExecutionResult`,
denials use `ToolDenied`, requests already resolved `closed_inadmissible` keep
`ToolInadmissible`, and the ambiguous request plus any request without an
ordinary result use `ToolClosed`. The turn then terminalizes as
`ReconciliationRequired` on that prefix-extending frontier, with the exact tool
attempt as its ambiguity set and the applied-interrupt proof. Logical closure
therefore leaves a provider-renderable conversation while the typed lifecycle
and outbox boundaries retain the physical tool-attempt uncertainty instead of
fabricating a model call or an execution result (INV-005, INV-006, INV-025,
INV-029, INV-037).

Without an interrupt, the daemon durably claims the same exact tool-attempt
ambiguity through the automatic-reconciliation ledger. Under the session
scheduler lock it rebuilds the complete batch, projects the same
proposal-ordered result suffix, and terminalizes through the same tool
reconciliation-required boundary. A concurrent authoritative transition wins and
supersedes the claim. Infrastructure or integrity failures are recorded and
retried with bounded backoff; after five attempts the wait stays visible with
operator action required.

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
and the database constraint enforce the same admission rule. These two bounds
constrain every executor's crate-owned capture policy. Each tool family owns its
bounded capture, truncation, and completeness evidence; no universal true-size
field is required when a traversal or collection cannot know it. No executor
widens the durable bound or converts an otherwise bounded success into a failure
merely because it omitted additional result members.

Semantic tool-result entries contain references only:

- `ToolExecutionResult { attempt }` references executed success/error evidence;
- `ToolDenied { request }` references the request's durable denial; and
- `ToolClosed { request }` references a request closed because its turn ended
  before it could complete ordinary execution, whether it remained undecided or
  was approved but not yet attempted. A crash-lost attempt has durable
  `KnownFailed` evidence and therefore uses `ToolExecutionResult`. **Committed
  unimplemented functionality.** A fourth entry, `ToolInadmissible { request }`,
  references a request resolved `closed_inadmissible` by the pre-approval check
  above. It references the request because that resolution mints no attempt,
  exactly as `ToolDenied` does, and the request row already carries the family's
  typed reason. No present family declares such a check, so no present
  transaction emits this entry.

No result entry copies output, error detail, or denial reason. Attempt evidence
commits as soon as execution ends, independently of semantic projection. Once
every request in the batch is executed, denied, or closed inadmissible, one
continuation transaction:

1. appends exactly one result entry per request in proposal order;
2. consumes every pending steering input in ascending acceptance position and
   appends its semantic entry after the tool results;
3. derives the exact prefix-preserving frontier extension; and
4. creates the next round's `Prepared` model call against that frontier.

**Committed unimplemented functionality — instruction admission continuation.**
When `instructions_read` is implemented, this transaction additionally folds the
batch's fresh durable successful instruction-admission rows in request order and
creates exactly one successor turn-instruction manifest authenticated by the new
`Prepared` model call. An idempotent replay receipt or an `already_admitted`
receipt contributes no row and cannot duplicate a bundle or alter the successor
manifest digest. No present tool or transaction supplies this fifth effect; the
compatibility constraint is that the four implemented effects and the successor
manifest must eventually commit or roll back together.

When at least one request entered execution, the continuation turn attempt
already entered `Running` during tool authorization. It owns the new `Prepared`
call without moving backward; send authorization advances only the call to
`InFlight` and leaves the attempt `Running`. A batch that authorized no effect
at all — denials only, inadmissible requests only, or any mixture of the two,
since neither kind creates an attempt — leaves its continuation attempt
`Prepared` while it owns the new `Prepared` call. Reconstitution and the
deferred database assertion admit `(Running, Prepared)` or
`(Prepared, Prepared)` only for a continuation-chain attempt whose exact call
frontier contains the current batch's complete durable result evidence.

Those effects commit or roll back together (INV-036). A newly prepared call ends
the invocation and is reloaded before provider capability preparation,
preserving the existing staged-call discipline. If the call completes with
another tool batch the loop repeats in the same turn; if it proposes no tools,
its assistant text and `TurnCompleted` marker terminalize the turn.

At most 32 requests may appear in one completed provider tool response. A
response with a thirty-third request closes the producing model call as
`KnownFailed` without creating a partial batch, request record, or tool-use
entry. At most 256 provider rounds in one turn may complete with admitted tool
requests. The application counts distinct producing calls for the current turn,
so every multi-request batch counts once and inherited tool history from earlier
turns does not count. After the 256th batch resolves, the ordinary continuation
transaction still projects all results and creates its fresh `Prepared` call;
model execution closes that checkpoint as `KnownFailed` before provider
capability preparation or send. At that enforcement site it emits a warning
carrying the limit and observed round count, and the guarded pre-send closure
carries `ToolRoundLimitReached`. The terminal event consequently uses
`tool_round_limit_reached`, distinct from `capability_known_failure` (INV-071).

The round ceiling bounds latency and provider spend; it does not bound retained
memory, because it multiplies against the 32-request batch bound and the 1 MiB
argument and result bounds. Retained content is therefore bounded separately: at
most 256 MiB of projected frontier content may be rendered into one call's
provider messages. The bound counts every kind of content the render clones, not
tool evidence alone — request arguments, result text, error detail, and denial
reasons, plus assistant text, context summaries, delegated task and peer-message
content, delivered delegation-outcome content, origin and steering user content,
and attested imported text. Assistant text carries no length bound of its own
beyond the transport cap on a single response, so a ceiling counting only tool
evidence would leave the same round multiplication unbounded through the entries
it clones without counting. The bound is enforced once the frontier projection
names its entries and before any of that content is cloned into messages — the
projection reads the durable frontier by reference for exactly that reason — so
an over-bound frontier is refused rather than materialized. Exceeding it closes
the checkpoint through the same pre-send contract as round saturation, emitting
a warning carrying the ceiling and the observed byte count and terminalizing as
`tool_round_limit_reached` (INV-071). Because one maximal round retains at most
64 MiB of tool evidence, the ceiling admits four maximal rounds and leaves the
round ceiling operative for the kilobyte-scale results executors return in
practice.

These durable-content bounds avoid wall-clock policy and ensure one
model-controlled response or chain cannot retain the progressing slot
indefinitely or exhaust daemon memory before its guard is reached.

If an applied stop terminalizes before continuation, the same materialization
algorithm appends results for executed, denied, and already-inadmissible
requests, closes every remaining request that did not complete ordinary
execution as `ToolClosed` in proposal order, then appends the proof-bearing
terminal marker. The consumed result projection is bound to the interrupted
turn: reusing this turn's current frontier identity is not sufficient, and a
projection prepared for another turn cannot terminalize this turn with foreign
request results even when the yielded source frontier matches. A prepared or
effect-free crash loss that fails the turn uses that same proposal-ordered
materialization before `TurnFailed`; the crash-lost `KnownFailed` attempt
becomes `ToolExecutionResult`, while every other request without an ordinary
result becomes `ToolClosed`. A request can therefore never remain an open
logical dependency behind a terminal turn (INV-006).

**Committed unimplemented functionality.** A request already resolved
`closed_inadmissible` is never reclassified by any of these paths. It has a
durable resolution and a typed reason before the interrupt or crash arrives, and
because it deliberately has no attempt it would otherwise fall into the
`ToolClosed` fallback and lose both, reporting a request refused before approval
as one closed by turn end. Its result entry is `ToolInadmissible` wherever these
algorithms name a materialization.

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

## Session-delegation tool family

The daemon catalog contains three automatic, daemon-local tools. Their invoking
session, turn, and request always come from trusted dispatch correlation and
never from model arguments.

- `spawn_session` declares `task` plus a `relationship` object. The relationship
  is either `background`, or `bound` with separately labeled `on_parent_stopped`
  and `on_parent_cancelled` actions (`keep_running`, `stop`, or `cancel`).
  **Committed unimplemented functionality.** No present tool or process
  execution surface creates the child; the daemon rejects execution until the
  placement-owned creation transaction implements the parent-directory default.
  That transaction must atomically create one delegated, no-ancestry child and
  its initial task work, close the spawning physical attempt with its matching
  receipt in that same transaction, then return the child session identity as a
  durable completion. Equal physical replay must return that child; a second
  child cannot attach to the request. Version one imposes no fixed
  active-child-count limit; admission must check the complete locked
  relationship inventory for request and child uniqueness.

- `await_session` takes the related child identity and `foreground` or
  `background`. Foreground converts the exact logical request into a durable
  child wait and produces its tool result only when the child's delivered
  content or typed terminal outcome exists. Background records delivery and
  immediately returns a registration receipt; completion later wakes the parent.
  When the result already exists, background still records delivery and returns
  `session_await_registered`, while foreground returns the child outcome.
  Replaying an already delivered wait returns that same mode-specific receipt or
  outcome.

- `send_session_message` takes the related peer identity and bounded nonempty
  content. It verifies that the invoker is exactly the parent or child, appends
  the next relationship message, and returns its identity and ordinal. Either
  side may call it while the other is active, idle, stopped, or cancelled.

The initial task is not accepted user input. Spawn records one `DelegatedTask`
semantic origin in the child, bound to the exact spawning request and its parent
session and turn; the task bytes are the checked `spawn_session` argument. The
child's first turn is a delegation-origin turn whose starting frontier contains
that entry, never an `OriginAcceptedInput`, and no `Actor::User` or
accepted-input row is invented. Exact replay reuses the same semantic entry and
turn origin.

`await_session` foreground parking is a logical tool transition, not a physical
executor kept in flight. It ends any current physical attempt before committing
`AwaitingChild`; restart therefore resumes from durable wait/result rows and
cannot duplicate an external effect. The delivered tool result is copied from
the child's terminal result record. The executor never reads or returns the
child transcript. The scheduling-aware executor returns a distinct
`DurableChildWait` disposition instead of terminal result evidence. Before the
application accepts that disposition, persistence rereads the complete parked
batch and exact ended dispatch fence; absent or cross-wired wait evidence fails
closed, and the generic observation path never attempts a second terminal
commit. This remains true when the result predates registration: the await
transaction parks the attempt and records its result delivery and wake
atomically, while the scheduling executor reports the same durable-wait
disposition so the scheduler resumes from stored result evidence.

If an await or message transaction's commit acknowledgement is ambiguous, the
executor replays that exact immutable request before reporting failure. A
committed first transaction returns its durable wait or completion, while an
uncommitted first transaction applies the replay-idempotent effect once.

Background await registration and peer-message append likewise end the physical
attempt in the same transaction as their delegation effect. Their scheduling
executor result is a distinct `DurableCompletion` disposition carrying the exact
ended dispatch correlation, not an encoded result awaiting a second commit. The
application authenticates that correlation against the ended attempt before
accepting it as already committed; an absent or cross-wired attempt fails
closed, and a failed reread retains the handoff for same-incarnation
reconciliation without repeating the effect.

Process-protocol execution of an in-flight peer-message attempt also closes the
attempt as `KnownFailed` in the message transaction when persistence proves a
definitive operation rejection and no message effect committed. Pre-execution
identity, correlation, and non-executable-state rejections do not terminalize an
attempt. A daemon-minted message identity collision is returned with that exact
identity rather than retried under a replacement identity.

The child's normal terminal completion transaction concatenates the definitive
ordered `AssistantText` entries from its proof-bearing completed call without a
separator and admits those exact bytes as `DelegationContent`; `await_session`
adapts that value to `ToolResultContent`. Zero entries or a concatenation beyond
the 1 MiB returned-result ceiling records `ChildFailed` with
`ChildResultUnavailable`. Task and message strings additionally must fit their
complete normalized JSON argument envelope, so that ceiling is not their exact
maximum. Execution failure, child cancellation, and proof-bearing parent-policy
outcomes materialize their closed results instead. This copy is part of the
child transition, not a later transcript projection. Duplicate observation is
idempotent by spawning request and cannot attach a late result to another parent
tool call.

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
following user-role message. Every provider-visible failure is this compact
provider-neutral JSON object: `{"error":{"detail":D,"kind":K}}`. `D` is the
admitted executor detail, admitted user denial reason, or JSON null; `K` is
exactly `unknown_tool`, `invalid_arguments`, `preauthorization_rejected`,
`execution_failed`, `result_too_large`, `crash_lost`, `denied`, or
`closed_by_turn_end`. Execution failures select their stored error kind and
detail, denial selects `denied` and its reason, and terminal closure selects
`closed_by_turn_end` with null detail. `K` stays closed as written; a family
whose failures do not fit it maps into it rather than extending it, and may fix
`D` to its own closed token vocabulary so the projection stays machine-readable.
**Committed unimplemented functionality.** The instruction family is the one
such mapping, fixed by [workspace instructions](workspace-instructions.md): its
four execution-stage failures select `execution_failed` with `D` set to exactly
one closed reason token and no other text, while its two pre-approval reasons,
which resolve before approval and create no attempt, select `invalid_arguments`
with `D` the token `not_eligible` or JSON null for arguments that did not
decode. OpenAI carries that JSON as ordinary tool-message content because its
wire shape has no failure flag; Anthropic also receives the provider-neutral
failure flag. Malformed proposal arguments remain exact after preparation-time
credential scrubbing on the durable request but replay as the exact
provider-neutral JSON object `{"signalbox_invalid_arguments":true}`, allowing
the paired typed error result to reach either provider without treating the
placeholder as durable evidence.

`current_time` is a compiled tool:

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
use the `jiff` dependency; Signalbox owns only the port and result contract, not
a time-zone database implementation.

The same process-lifetime compiled catalog also declares these daemon tools:

- `echo` requires exactly one `text` string and returns the same canonical
  compact `{"text": ...}` object. Its permission default is `Auto` and its
  effect class is `EffectFree`: execution observes no external state.
- `blob_metadata`, as owned by the [blob-read tool contract](blob-storage.md),
  requires exactly one canonical blob `digest`. It returns text containing
  compact JSON with that `digest`, canonical-decimal-string `byte_length`, and
  canonical-decimal-string `replica_count`. Its permission default is `Auto` and
  its effect class is `EffectFree`.
- `blob_read`, as owned by the [blob-read tool contract](blob-storage.md),
  requires exactly one canonical blob `digest` plus `offset_bytes` and
  `length_bytes` as canonical decimal-u64 strings. Length is 1 through 524,288
  bytes; checked offset plus length must lie within the blob. It returns text
  containing compact JSON with the `digest`, `offset_bytes`, and canonical
  padded `bytes_base64`. Its permission default is `Auto` and its effect class
  is conservatively `ExternalEffect`: recorded failover can issue an
  authenticated S3 GET observable to the object-store operator even when the
  selected replica for another execution is local. After its non-waiting
  direct-read admission, the scheduler releases pass capacity during store
  traversal and reacquires it before correlated result commit or crash-loss
  classification, as owned by the blob contract.
- `web_fetch` requires exactly one absolute HTTP(S) `url` no longer than 8 KiB.
  User information, fragments, and direct non-public IP destinations are
  invalid. Before dispatch, its canonical origin must satisfy the
  deployment-owned
  [web-fetch catalog policy](configuration-and-credentials.md#the-static-model-alias-and-web-fetch-catalog),
  which owns the origin bound, canonicalization, and absent-or-empty behavior;
  this admission gates execution. A domain must resolve to between one and 32
  addresses and every address must be public; the admitted addresses are pinned
  into the request client so connection setup cannot substitute a later DNS
  answer. Its permission default is `Confirm`; its effect class is
  `ExternalEffect` because the remote server can observe a GET. One dispatch
  performs at most one credential-free request: ambient proxies, redirects,
  protocol retries, and idle reuse are disabled, TLS uses rustls with a TLS 1.2
  floor, and a 15-second timeout bounds resolution and the exchange. The
  executor retains at most 64 KiB of response bytes and at most 1,024 bytes of a
  valid content-type header. Success is compact JSON containing the exact
  requested `url`, numeric `status`, optional `content_type`, a lossy UTF-8
  `body`, and `truncated`. Resolution, client-setup, and definite
  connection-establishment failure before request dispatch returns a fixed
  sanitized known failure; timeout, transport, or body loss after dispatch
  begins is commit-ambiguous. Truncation stops body consumption and never
  follows or issues another request.
- `web_search` requires exactly one nonblank `query` of at most 400 characters
  and 50 words. Its permission default is `Confirm`; its effect class is
  `ExternalEffect`. Production pins the Brave provider, its exact API origin,
  its sensitive credential header, and the `brave-search-primary` credential
  reference. One request asks for at most 20 results, retains at most 10, and
  accepts at most 512 KiB of provider response. Ambient proxies, redirects,
  retries, and idle reuse are disabled. Success is compact JSON containing
  bounded typed title, URL, and snippet components plus `truncated`; output and
  credential-scrubbing semantics are owned by the
  [web egress threat model](web-egress-threat-model.md).
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
  [sessions-and-transcript](sessions-and-transcript.md).

Both blob tools authorize only digests present in attachment stubs in the
rendered frontier for the issuing turn, under the owning
[blob-read tool contract](blob-storage.md). The read declaration's requested
decoded length and one logical-read unit are charged once by tool-request
identity to durable per-turn counters before authorization; replay never charges
twice. Before authorization, a digest absent from the rendered frontier closes
the `Prepared` attempt as `KnownFailed(PreauthorizationRejected)` with exact
fixed detail `blob_not_visible`; a reservation that would exceed 2,097,152 bytes
closes it the same way with exact fixed detail `blob_turn_byte_budget_exceeded`,
and a reservation that would exceed 64 logical reads closes it with exact fixed
detail `blob_turn_read_count_exceeded`. That kind is what separates a durable
request-scoped resource or visibility refusal from a malformed argument the
model can correct by rewriting the call, so the closed set carries it as its own
member rather than folding it into `InvalidArguments`. Any closure resolves the
logical request, crosses no executor or store boundary, leaves previously
charged bytes charged, and permits the next model round. A successful
reservation is not refunded by a later denial or failure. Store I/O occurs only
after durable authorization. An individual missing, corrupt, or unavailable
replica falls through to the next recorded candidate. Only after no candidate
verifies does the read become trustworthy content-silent `ExecutionFailed`
evidence with the respective exact fixed detail `blob_missing`, `blob_corrupt`,
or `blob_unavailable`. Any unavailable candidate takes precedence; otherwise any
readable candidate that fails verification selects `blob_corrupt`, and
`blob_missing` applies only when every candidate is absent. It resolves the
logical request and permits the next model round rather than entering the
effect-free crash-loss path or failing the turn; a later request may retry
`blob_unavailable` within the remaining per-turn budget. The compact result must
also fit the ordinary 1 MiB text-result bound; admission accounts for JSON and
base64 overhead rather than producing a result that the ordinary result boundary
would reject.

For both web tools, an explicit shipped `Human` posture supersedes the
declaration's `Confirm` default and the session blanket, so a request parks for
the user before entering its transport or credential boundary. The
[approval policy and decision sources](#approval-policy-and-decision-sources)
section owns that precedence and the durable approval flow.

The code-host catalog contains fourteen GitHub tools. Every operation is
`ExternalEffect` because GitHub observes its authenticated request. The ten
read-only declarations — `change_request_summary`,
`change_request_changed_files`, `change_request_file_patch`,
`repository_read_file`, `repository_list_directory`,
`change_request_checks_status`, `change_request_review_threads`,
`change_request_ci_job_log`, `change_request_stack_state`, and
`change_request_thread_inventory` — default to `Auto`. The four mutations —
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
  repository-relative `path`; it searches consecutive 100-file pages through
  GitHub's 3,000-file endpoint ceiling and returns the matching file summary
  plus the optional code-host patch. A complete search miss reports the fixed
  semantic detail `requested changed file was not found in the change request`;
  it is not presented as a host rejection. A next-page signal beyond page 30
  violates the bounded response contract and fails closed.
- `repository_read_file` accepts `repository`, one repository-relative `path`,
  and a required exact lowercase 40-hex commit `revision`; it never defaults to
  a branch head. An optional `line_range` has positive one-based `start` and
  inclusive `end` members with `start` no greater than `end`. A content outcome
  returns at most 64 KiB of exact UTF-8 text together with source and returned
  byte counts, requested and returned line bounds, returned line count,
  final-line completeness, and `truncated`. The returned byte count describes
  the emitted content after credential scrubbing. GitHub cannot address a raw
  blob by line, so a ranged read inspects the complete blob only when its
  reported source size is at most 1 MiB; a larger blob produces the distinct
  `line_range_unavailable` outcome with the requested range, source bytes, scan
  limit, and `truncated: true` without fetching the blob. Complete bounded
  ranged inspection classifies a NUL-bearing or non-UTF-8 blob as binary even
  when those bytes lie outside the selected lines. A ranged content result can
  claim truncation only when the exact source bytes observed inside the selected
  lines exceed the retained bytes; source bytes before the selection cannot be
  counted as discarded selected content. `path_not_found`, `revision_not_found`,
  `not_a_file` with the observed repository-object type, and `binary_file` with
  source bytes are separate non-content outcomes; every file outcome carries
  `truncated`. Before any Contents request, the GitHub adapter requests the
  bounded SHA representation of `/commits/{revision}`. Success must return the
  exact required revision; a different resolution, including a forty-hex branch
  or tag name that resolves to another commit, fails execution before repository
  content is read. An exact resolution pins the Contents request to that commit,
  and a Contents 404 then means `path_not_found`. A commit conflict proves an
  empty repository and returns `revision_not_found` without a Contents request.
  A commit 422 whose bounded JSON `message` is exactly
  `No commit found for SHA: {revision}` also proves absence without a Contents
  request; any other 422 remains failed execution. A commit 404 permits one
  Contents visibility probe pinned to the requested revision: only a bounded 404
  body that exactly names that revision as an absent ref returns
  `revision_not_found`; a generic 404 remains failed execution because metadata
  visibility alone cannot prove absence. A successful text or binary file read
  therefore uses three requests (commit resolution, Contents metadata, immutable
  blob), a directory result or path absence uses two (commit resolution and
  Contents), and revision absence uses one or two. Every request shares one
  30-second transaction budget, and every request after resolution names the
  resolved commit or immutable blob rather than a moving reference.
- `repository_list_directory` accepts `repository`, one repository-relative
  directory `path`, and the same required exact commit `revision`. An entries
  outcome retains at most 100 immediate children with path, repository-object
  type, and optional source byte size, plus observed and returned entry counts
  and `truncated`. Reaching the contents endpoint's 1,000-entry ceiling is
  explicitly incomplete even when GitHub supplies no continuation signal.
  `path_not_found`, `revision_not_found`, and `not_a_directory` with the
  observed repository-object type remain distinct outcomes, each carrying
  `truncated`.
- `change_request_checks_status` accepts `repository` and one exact lowercase
  40-hex `revision`; it returns that revision and the first page of at most 100
  check runs, each with id, name, status, optional conclusion, and URL, plus
  `truncated`.
- `change_request_comment` accepts `repository`, `number`, and one nonempty
  `body`; it returns the created comment id and URL.
- `change_request_review_threads` accepts `repository` and `number`; it returns
  the first 100 threads and, within each, the first 100 comments. A thread
  carries opaque id, resolution and outdated posture, path, optional line,
  comments, and `comments_truncated`; the outer result carries `truncated`. The
  configured code-host result-text byte bound applies to the complete encoded,
  credential-scrubbed result. Exhausting it retains an ordered
  thread-and-comment prefix, marks a shortened comment page with
  `comments_truncated`, and marks an omitted thread suffix with outer
  `truncated`.
- `change_request_thread_reply` accepts `repository`, `number`, an opaque
  `thread_id`, and nonempty `body`; it returns the created comment node id and
  URL. The named change request is the mutation's authority target: an opaque
  thread identity alone is globally scoped, so without these coordinates neither
  an approval decision over the arguments nor the executor could tell a thread
  in the granted change request from one anywhere else the credential reaches.
  Before dispatching the mutation, the GitHub adapter resolves the thread node
  and confirms the code host places it inside exactly that change request. A
  thread the code host does not place there — including an identity that
  resolves to no node or to a node of another type — fails closed with the fixed
  semantic detail
  `requested review thread was not found in the named change request`, and no
  mutation request is dispatched. Node absence is definitive only when every
  error beside the evaluated null carries the code host's typed not-found
  classification, or none accompanies it; any other field error proves nothing
  about the thread and reports the undispatched mutation instead. The repository
  comparison follows the code host's case-insensitive repository addressing; the
  number must match exactly. Ownership-check failures keep read classification:
  an infrastructure failure during the confirmation reports that the mutation
  was never dispatched and is never commit-ambiguous. A review thread never
  moves between change requests, so the confirmation cannot be invalidated
  between the two requests. The confirmation and the mutation share the
  transport's single 30-second exchange budget: the mutation receives only the
  time the confirmation left, and exhaustion before dispatch reports the
  undispatched mutation.
- `change_request_thread_resolve` accepts `repository`, `number`, and one opaque
  `thread_id` under the same pre-dispatch ownership confirmation as
  `change_request_thread_reply`; it returns that thread identity and the
  acknowledged resolution posture.
- `change_request_ci_job_log` accepts `repository` and a positive `job_id`; it
  returns that id, at most 64 KiB of lossy UTF-8 log text, and `truncated`.
- `change_request_rerun_failed_jobs` accepts `repository` and a positive
  workflow `run_id`; it returns the acknowledged run id.
- `change_request_stack_state` accepts `repository`, `number`, and an optional
  opaque GraphQL child-page `cursor`. It returns the current immediate-base,
  head, and default-branch refs and revisions; current-base commits absent from
  the head; default-branch commits absent from the current base chain; and one
  projected page of at most 100 immediate child requests whose base names the
  request's head branch. Each child carries the same merge-forward and
  default-chain comparison for its level. Children are discovered in the
  request's head repository. Comparisons use count-only GraphQL projections
  authenticated against each current base revision, with at most eight child
  comparisons in flight. The complete stack transaction has the transport's
  30-second aggregate deadline. The adapter then re-reads the request, current
  and default branches, and exact projected child page and rejects evidence if
  any revision, child inventory, or child-page continuation snapshot changed.
  The child page carries `children_truncated` and `children_next_cursor`.
- `change_request_thread_inventory` accepts `repository`, `number`, and an
  optional opaque GraphQL `cursor`. It returns the observed head revision and at
  most 100 review threads with exact resolution and outdated posture, path and
  optional line, first-comment author and `bot` / `human` / `unknown` class, the
  bounded first-comment finding title, and its `fix_named` / `declined` /
  `escalation_marker` / `undispositioned` class. A thread without a reply is
  undispositioned. `fix_named` requires a reply beginning with `Fixed in commit`
  or `Fixed in commits`, followed by exactly one space and an optionally
  backtick-delimited 7-to-40-hex commit token; `declined` requires a reply
  beginning `Declined:` and a nonempty reason. Only replies whose code-host
  association is `OWNER`, `MEMBER`, or `COLLABORATOR` can supply disposition
  evidence. The latest recognized fix or decline survives later non-disposition
  replies, while `escalation_marker` requires the trimmed last reply to equal
  the exact marker. Classification rejects a thread whose comment history
  exceeds the 100-comment read bound. The page carries `truncated` and
  `next_cursor`. Shared typed admission rejects extra object members;
  repositories are at most 256 bytes, paths 4 KiB, comment bodies and returned
  text fields 64 KiB, and opaque node ids and GraphQL cursors are at most 512
  bytes. Paths use canonical repository-relative spelling: no empty, dot, or
  parent component is admitted, with bare `.` reserved for the repository root.
  A returned node id, head revision, or stack or inventory continuation is
  admitted by the same predicate its argument counterpart uses, so those
  identities and continuations can be passed back as arguments. Every returned
  URL is one absolute credential-free HTTPS location. No result has more than
  100 collection members or more than 512 KiB of encoded JSON. Every bounded
  slog list reports whether it is truncated and the matching continuation
  cursor.

The production adapter uses fixed GitHub REST and GraphQL endpoints. It disables
ambient proxies, automatic redirects, protocol retries, and idle reuse; uses
rustls with a TLS 1.2 floor; sends the fixed GitHub REST version `2026-03-10`;
applies a 30-second whole-exchange timeout; and retains at most 512 KiB from an
ordinary JSON response. The exact-revision contents lookup admits at most
303,407,104 bytes of JSON ingress. That bound covers the 1,000-entry observation
ceiling plus one framing allowance, budgeting each entry for eleven worst-case
JSON-expanded fields: nine maximum repository paths, one separately admitted 4
KiB symlink target, one separately admitted 8 KiB submodule URL marker, and 8
KiB of fixed material, before projecting the shared result bound. Both
repository tools implement the commit preflight, absence classification, request
counts, and aggregate transaction deadline stated in their declarations above.
After exact resolution, each Contents request is pinned to the resolved commit;
a file read pins its subsequent request to the immutable blob identity returned
by that lookup rather than re-reading a moving reference. The authenticated
job-log endpoint is the sole redirect-shaped exchange: after exactly one 302
response, the adapter validates its bounded HTTPS location, resolves and pins a
wholly public destination set, and performs one credential-free download with
redirect following still disabled. Credential delivery and redaction are owned
by [configuration-and-credentials](configuration-and-credentials.md).

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

### Session plan tools

The process-lifetime daemon catalog always includes `plan_write` and `plan_read`
in both base and fully mapped production composition. `signalboxd` binds their
injected `SessionPlanPort` to `SessionPlanRepository` in production. Each
request takes its target session from the trusted tool-dispatch correlation;
neither schema accepts a session identity. Both declarations default to `Auto`.
`plan_write` is `ExternalEffect` because it appends durable state, while
`plan_read` is `EffectFree`.

`plan_write` accepts exactly one tagged operation. `create` takes nonempty text
of at most 4,096 Unicode scalars and creates a pending entry whose identity is
its positive creation-event ordinal; `revise` replaces the text of one positive
`entry_id`; `set_status` selects `pending`, `in_progress`, `completed`, or
`abandoned`; and `depends_on` links two existing entries. Text cannot contain
U+0000. Success appends one event with trusted provenance and returns compact
JSON. Missing entries and self or cyclic links yield typed known failures and
append nothing. A thirty-third distinct dependency for one entry yields the
typed `DependencyLimitReached` known failure and appends nothing; duplicate
links remain in history but fold once by first append.

`plan_read` accepts an optional positive exclusive `after_entry_id` cursor and
`include_history`, which defaults to false. It returns at most 100 folded
entries in creation order, including dependencies and derived `ready` or
`waiting` readiness: an entry is waiting exactly while any dependency is not
completed. The page carries `next_after_entry_id` and `plan_truncated`;
requested `history` contains at most 100 chronological events with independent
`history_truncated`. Compact admission may retain a smaller prefix while
preserving those labels.

The merged catalog sorts declarations by checked tool name and rejects
duplicates during construction. Its name-directed executor covers the exact
families composed into that catalog, including mapped families only when their
complete deployment configuration is present; disagreement between the
advertised catalog and executor is classified as a daemon defect.

## Persistence boundaries

Persistence holds the three result-entry shapes and append-only `tool_request`,
`tool_approval_decision`, and guarded `tool_attempt` tables. Deferred
constraints assert complete call-response/request-entry batches, approval-wait
evidence, result-entry materialization, and terminal closure. The session
scheduler row is the first explicit lock for every turn-side transaction.
Preparing a model operation collects all frontier-referenced tool requests,
attempts, and approval decisions in one batched query per record family before
reconstructing provider history in frontier order; it performs no per-entry
database round trips while holding the scheduler lock.

`DecideToolRequest` joins the user-global durable-command registry as its own
typed record family, and `OverrideDeniedToolRequest` likewise; the recorded
override row, its recording and consumption triggers, and the UNIQUE consumption
column are owned by [persistence protocol](persistence-protocol.md).
Defaults-bearing command records at kind-scoped storage version 1 reconstitute
with `DangerousToolAutoApproval::Disabled`. The current kind-scoped versions and
their compatibility gates are owned by
[identity and commands](identity-and-commands.md) and
[persistence protocol](persistence-protocol.md). Registry inspection validates
the supported version set for the selected kind rather than applying one global
version constant.

## Open edges

- Replacing direct approval-judge recommendations with graded risk and brief
  alignment is recorded under
  [Graded approval judging](../open-questions.md#graded-approval-judging).
- Dynamic execution-strategy policy beyond the two named runner profiles,
  model-declared approval expiry and additional high-risk guardrails are
  recorded in [Tool safety](../open-questions.md#tool-safety).
- Rich result-content variants and durable tool-definition revisioning across
  outstanding requests are recorded in
  [Tool safety](../open-questions.md#tool-safety).
- Durable storage for payloads larger than the 1 MiB result and 4,096-byte
  detail bounds, and the abuse controls a larger bound requires, are recorded in
  [Tool safety](../open-questions.md#tool-safety).
- General tool-attempt retry and ambiguous-wait resolution beyond the sealed
  runner-loss transitions are recorded in
  [Tool safety](../open-questions.md#tool-safety).
- Runner placement, local transport, workspace, profile, and lease law are owned
  by [runner protocol and placement](runner-protocol.md). Remote runner
  transport and multiple-runner scheduling are recorded under
  [Scheduling and runners](../open-questions.md#scheduling-and-runners),
  [Protocols and persistence](../open-questions.md#protocols-and-persistence),
  and
  [Identity, credentials, and resource governance](../open-questions.md#identity-credentials-and-resource-governance).
- Client approval presentation is recorded under
  [Client scope](../open-questions.md#client-scope).
- Streaming tool deltas are part of the model-streaming question in
  [Protocols and persistence](../open-questions.md#protocols-and-persistence).

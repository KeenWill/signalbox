# Tool loop

The user-vocabulary surface on this page was re-verified through PR #378
(`agent/user-vocabulary`).

This page specifies the implemented daemon-owned tool subsystem as verified
against the implementing stack rooted at PR #193 (`agent/tool-loop-spec`); the
`signalboxd` name this page states for the catalog-wiring composition root was
verified through PR #258 (`agent/signalboxd-rename`), and the Tier 0 catalog
extension through PR #265 (`agent/tool-batch-tier0`). The Tier 1 code-host
catalog extension is verified through PR #270 (`agent/tool-batch-tier1`), the
deterministic review-slog extension through PR #306 (`agent/review-slog-tools`),
the failed-attempt operator event together with the credential-shaped code-host
detail through PR #285 (`agent/dev-instance-code-host-credential`), the client
decision surface through PR #291 (`agent/turn-control-verbs`), and
runner-protocol batch reconstitution through PR #260
(`agent/runner-protocol-domain`). Template-derived blanket creation was verified
through PR #311 (`agent/session-templates-spec`), and the exact-origin
`web_fetch` egress policy and complete bounded file-patch lookup through PR #330
(`agent/audit-verified-fixes`). The exact-revision repository-read extension is
verified through PR #348 (`agent/repository-read-tools`) at implementation ref
`2a55dbb65440dfae31b339b6726fe5ace6dab24c`. The runner executable stack rooted
at this foundation proposal extends the same laws to the runner locus. The
non-overridable explicit-approval posture is verified through PR #366
(`agent/exec-tools`). This page owns logical tool requests, approval policy and
decisions, physical tool attempts, result admission, intra-turn continuation,
crash classification, the compiled registry, and the daemon-local catalog. Turn
and attempt lifecycle law lives in
[turn-lifecycle-and-scheduling](turn-lifecycle-and-scheduling.md); semantic
entry vocabulary in [sessions-and-transcript](sessions-and-transcript.md);
model-call staging and provider translation in
[model-call-execution](model-call-execution.md); durable-command identity in
[identity-and-commands](identity-and-commands.md); and relational mechanics in
[persistence-protocol](persistence-protocol.md). Invariant tags cite
[the invariant test index](../invariants.md). The runner-locus paragraphs in
this page are the foundation proposal at the bottom of their implementing stack
and become verified only with those child pull requests.

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

- `UserCommand` — one applied user-global durable decision command;
- `PolicyAuto` — the selected registry or sandbox-profile default supplied
  automatic approval;
- `SessionBlanket` — the frozen dangerous blanket supplied daemon-local
  automatic approval; and
- `SessionOverride` — an exact runner-placement tool override supplied automatic
  approval.

`JudgeRecommendation` remains typed additive vocabulary without a storage
encoding or producer. An automated source never constructs `UserCommand` or
claims user agency (INV-020).

Daemon-local execution first leaves an `AlwaysConfirm` declaration undecided;
the dangerous blanket cannot override that posture. All other declarations keep
this precedence:

1. the frozen session posture `DangerousToolAutoApproval::ApproveAll`;
2. the registry default (`Auto` or `Confirm`); then
3. fail-closed `Confirm` when no declaration exists.

Runner execution instead uses the immutable placement policy owned by
[runner protocol and placement](runner-protocol.md#sandbox-profiles-and-approval):

1. an exact per-tool override, recording `SessionOverride` for `Auto` and
   leaving `Confirm` undecided;
2. the selected profile default, recording `PolicyAuto` for `Auto` and leaving
   `Confirm` undecided; then
3. fail closed when no exact daemon-owned runner declaration exists.

The dangerous blanket has no runner rung. The producing-call completion
transaction resolves policy independently for every proposal after selecting its
admissible locus and immutable definition snapshot. A frozen automatic choice
may exist after an earlier confirmation wait without bypassing it; only
user-command decisions must form a proposal-order prefix. After each user
command, the earliest remaining undecided confirmation is the next wait, while
already frozen automatic decisions require no later command. Why: recording the
selected source makes unattended operation inspectable without laundering policy
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
[process-protocol](process-protocol.md#client-requests) is the client surface
that issues this command; its wire posture requires a denial reason even though
the command admits an absent one. Registry lookup precedes current-state
validation; equal replay returns the recorded applied-or-rejected result,
cross-kind or different-payload reuse conflicts, and a pre-commit failure claims
no identity (INV-012).

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
([process-protocol](process-protocol.md#client-requests)); a `stop_turn` against
the parked wait records the typed
`interrupt_unavailable_while_awaiting_approval` rejection and leaves the wait
intact.

## Registry, placement, and effect metadata

The application `ToolCatalog` port supplies immutable daemon-local
`ToolDefinition` values: name, model-facing description, argument JSON Schema,
permission default (`Auto`, `Confirm`, or `AlwaysConfirm`), and the stored
two-class crash classification used by the implemented local attempt machinery.

The runner foundation adds one immutable daemon-owned `RunnerToolDeclaration`
per runner-advertisable name. It carries a required checked model-facing
description and canonical JSON-object argument schema, the required three-way
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

The current daemon-local application catalog remains one process-lifetime
immutable compiled value. Its existing `EffectFree` declaration maps to
`RunnerToolEffectClass::Pure`, and `ExternalEffect` maps to
`RunnerToolEffectClass::SideEffecting`; no current local declaration can project
`Idempotent`. Before a shared name can use a daemon locus, the application
adapter validates exact model-facing description and schema, permission
equality, and this effect mapping against the authoritative runner declaration;
it also compiles the schema into the executable validator used before dispatch.
A mismatch is unavailable, never a choice between two policies. The runner
authority child stack persists the consolidated placement and policy snapshot.
Catalog lookup and iteration remain ports rather than a static global, but
runtime rebinding and deployment compatibility for outstanding requests are not
implemented; they require the durable definition-revision decision recorded
under Open edges.

The version-one workstation registry has exactly these runner-only tools and
effects:

- `workspace_read` is `Pure`;
- `workspace_write` and `workspace_edit` are `SideEffecting`;
- `git_fetch` is `Idempotent`;
- `git_clone`, `git_branch`, `git_commit`, and `git_push` are `SideEffecting`;
  and
- `shell_exec` and `build_test` are `SideEffecting`.

### Version-one workstation tool contracts

Every argument schema below is a JSON object with every named member required,
`additionalProperties: false`, and no nullable member. A workspace-relative
`path` is 1 through 4,096 UTF-8 bytes, uses `/` separators, has no U+0000,
leading slash, empty component, or `.` or `..` component, and resolves beneath
the session's exact writable root without following a symlink. That root is the
session repository when the placement requires a worktree and otherwise the
session's writable directory
([runner protocol and placement](runner-protocol.md#sandbox-profiles-and-approval)),
so these paths never presume a repository exists. A `cwd` is either `.` or such
a path resolving to an existing real directory. Text is UTF-8 with no U+0000.
File content and each edit operand are at most 1,048,576 bytes.

An `argv` is an array of 1 through 128 strings: element zero is nonempty and at
most 4,096 bytes, every later element is at most 8,192 bytes, the complete array
contains at most 65,536 UTF-8 bytes, and no element contains U+0000. It is
passed directly to `execve`; no shell, word splitting, interpolation, or
response-file expansion is inserted. A timeout is a JSON integer from 1 through
1,800 seconds. Captured bytes use exactly
`{ "encoding": "base64", "data": canonical_padded_base64, "total_bytes": canonical_decimal_string, "omitted_bytes": canonical_decimal_string }`.
Process success is
`{ "exit_code": 0, "stdout": captured_bytes, "stderr": captured_bytes }`.
Timeout returns `timed_out`. A signal or nonzero exit is known failure carrying
its signal or integer exit status and the bounded captured streams. Runner-side
infrastructure uncertainty after process start remains ambiguous according to
the tool effect.

Output caps are derived from the durable evidence that has to hold them rather
than chosen independently of it, and every cap is stated in decoded bytes so an
executor enforces it while it reads. A success commits as one
`ToolResultContent::Text` value under the 1 MiB bound below, so stdout and
stderr are each captured to at most 262,144 decoded bytes: padded base64 makes
each 349,528 bytes, and both encoded streams plus the result object's framing
and counters fit that bound with room for worst-case JSON escaping. A known
failure's durable detail is bounded at 4,096 bytes, which no pair of
megabyte-class streams can fit, so a failure captures at most 1,024 decoded
bytes per stream: 1,368 bytes of padded base64 each, 2,736 for the pair, leaving
more than a kibibyte for the failure object's framing, its signal or exit
status, and its per-stream size counters. The truncation marker below is
retained inside a stream's cap rather than added to it, so no marker can push an
encoded failure past the bound.

Truncation has one shape for success and failure alike, and it is honest. The
runner retains the head and tail of each stream within that stream's cap, keeps
counting what it discards, and reports `total_bytes` as the true produced size
and `omitted_bytes` as the exact count dropped; a truncated stream carries the
fixed marker `[signalbox: N bytes omitted]` at the cut, with `N` the same count.
Crossing a cap therefore neither terminates the process group nor changes the
outcome: a process that writes far past the cap and exits zero commits as a
success with truncated evidence and truthful sizes, never as a fabricated
failure. The direct-process caps make `result_too_large` unreachable for these
tools; it remains the admission backstop for any executor whose result exceeds
the durable bound. Why: the caps exist because storage has a limit, so the
honest report of a large success is a truncated success — reclassifying it as a
failure would tell the model the command did not work when it did. Payloads
genuinely larger than these caps need the storage architecture recorded under
[Tool safety](../open-questions.md#tool-safety), not a wider constant here.

Git `remote` is 1 through 64 ASCII bytes matching `[A-Za-z0-9][A-Za-z0-9._-]*`.
Before network use, every Git tool that reaches a remote applies the complete
effective-URL and multiplicity contract owned by
[runner protocol and placement](runner-protocol.md#workspace-provisioning-and-recovery);
this page defines no alternate destination set or count. The repository entry
used by the profile rule below is the same entry that owning binding selects.
When the placement selected a credential profile, that entry must name the exact
granted profile. A placement that selected no profile performs anonymous HTTPS
only for an entry that also names no profile; an entry that requires one fails
`credential_unavailable` rather than resolving a profile the placement never
selected. Every Git invocation additionally runs under the runner-forced
effective configuration — neutralized system and global configuration, a
command-line transport allowlist, forced HTTP-path credential queries when a
helper is installed, and disabled repository hooks — and that forced
configuration is the transport boundary while the owning effective-URL check is
the repository boundary it cannot supply. Validating the stored remote URL is
defense in depth above the pair, not a boundary itself. A `branch` is at most
255 bytes and is validated as the complete ref form `refs/heads/<input>` rather
than as a branch shorthand, so a token Git would expand — `@{-1}` and its kin —
is rejected instead of admitted as a literal name; a `ref` or refspec is 1
through 1,024 UTF-8 bytes, contains no U+0000, and is passed after option
termination. A commit message is 1 through 65,536 UTF-8 bytes with no U+0000.
Git commands never use a credential-bearing URL, force, delete, tag,
recurse-submodule, hook-bypass, amend, or signing option unless a later contract
adds that operation explicitly.

Every Git tool carries the same required `timeout_seconds` member as
`shell_exec`, on the same 1-through-1,800-second bound, and each declaration's
model-facing description names the spec-fixed default for its operation: 1,800
for `git_clone`, 900 for `git_push`, 600 for `git_fetch`, and 120 for
`git_branch` and `git_commit`. The runner enforces the deadline by terminating
the process group, so a stalled Git subprocess can no longer hold the runner's
single global execution permit while heartbeats stay healthy. What a deadline
produces is stated per operation rather than universally, because the operations
differ in whether the runner still knows what happened. A timed-out `git_fetch`,
`git_clone`, `git_branch`, or `git_commit` closes as the typed `timed_out` known
failure and carries its own retry-safety: fetch is retry-safe, so is clone,
whose incomplete destination the runner removes before reporting, and branch and
commit are locally recoverable and honestly retain any ref or index change
already made. A timed-out `git_push` is not a known failure at all: the remote
may already have accepted the update, so it follows the `SideEffecting`
runner-uncertainty law and is neither recorded as definitely failed nor retried
as though it were.

The exact tools are:

- `workspace_read` takes `{ "path": path }`. It opens one regular file
  descriptor-relatively, rejects a file larger than 1,048,576 bytes or invalid
  UTF-8, and returns `{ "content": string }` with the exact bytes decoded as
  UTF-8.
- `workspace_write` takes `{ "path": path, "content": text }`. The parent must
  already exist. The target may be absent or a regular nonsymlink file. It
  writes and fsyncs a sibling temporary, preserves an existing target mode or
  uses `0644` for a new file, atomically renames, fsyncs the parent, and returns
  `{ "bytes_written": canonical_decimal_string }`.
- `workspace_edit` takes
  `{ "path": path, "old_text": nonempty_text, "new_text": text, "occurrence": "one" | "all" }`.
  `one` requires exactly one byte-exact UTF-8 match; `all` requires at least one
  and replaces nonoverlapping matches from left to right. An output above the
  file-content bound fails before mutation. The atomic write rules and result
  are exactly `workspace_write`.
- `git_clone` takes
  `{ "repository": checked_repository_key, "destination": path, "timeout_seconds": timeout }`.
  Destination must not exist and its parent must be real. The runner resolves
  the named entry's canonical URL and optional credential profile, requires that
  optional profile to equal the placement's selection, requires Git's own
  expansion of that URL under the forced configuration to return it unchanged,
  clones anonymously when both profiles are absent and through the fixed helper
  when both carry the same name, and returns
  `{ "head": canonical_full_commit_hex | null, "branch": string | null, "unborn_branch": string | null }`.
  A populated repository returns its exact `head` with the nullable `branch` and
  a null `unborn_branch`. An empty repository is a success, not a failure and
  not a reason to remove the destination: it returns a null `head` and null
  `branch` together with `unborn_branch` naming the branch the first commit will
  be born on, which is exactly what `git clone` reports and what
  `git rev-parse HEAD` cannot. Exactly one of `head` and `unborn_branch` is
  present in every success. The three result members are result vocabulary, not
  arguments.
- `git_fetch` takes
  `{ "remote": remote, "refspecs": ref_array, "prune": boolean, "timeout_seconds": timeout }`,
  where the array has 0 through 32 unique refspecs. It validates the remote as
  above and performs one noninteractive fetch with no tags or submodules and
  optional prune. Success returns `{ "head": canonical_full_commit_hex | null }`
  for the unchanged checked-out HEAD, null when that HEAD is still unborn.
- `git_branch` takes
  `{ "name": branch, "start_point": ref, "timeout_seconds": timeout }`. The name
  must not already exist and the start point must resolve to one commit. It
  creates and checks out the branch without force, then returns
  `{ "branch": string, "head": canonical_full_commit_hex }`.
- `git_commit` takes
  `{ "message": commit_message, "paths": path_array, "timeout_seconds": timeout }`,
  where the array contains 1 through 256 unique paths. It stages exactly those
  paths, requires a nonempty resulting commit, uses the configured runner author
  name and email, neither amends nor signs, and returns
  `{ "commit": canonical_full_commit_hex }`. A failed commit honestly retains
  any index changes already made.
- `git_push` takes
  `{ "remote": remote, "branch": branch, "timeout_seconds": timeout }`. After
  validating the remote and, when one was granted, the exact credential profile,
  it pushes exactly `HEAD:refs/heads/{branch}` with upstream tracking and
  without force, deletion, or tags. Success returns
  `{ "remote": string, "branch": string, "commit": canonical_full_commit_hex }`.
- `shell_exec` takes `{ "argv": argv, "cwd": cwd, "timeout_seconds": timeout }`
  and applies the common direct-process result.
- `build_test` takes
  `{ "program": "cargo" | "mdformat", "arguments": argument_array, "cwd": cwd, "timeout_seconds": timeout }`,
  where `argument_array` has 0 through 127 elements and otherwise uses the
  `argv` element and total bounds. The executable is the exact real `cargo` or
  `mdformat` found in the declared read-only toolchain allowlist, not a
  workspace-controlled `PATH` entry, and the common direct-process result
  applies.

Git full commit hex is exactly 40 or 64 lowercase hexadecimal characters,
matching the repository object format. Every known-failure code is closed:
`invalid_arguments`, `path_unavailable`, `not_regular_file`, `symlink_refused`,
`content_too_large`, `invalid_utf8`, `match_not_found`, `match_not_unique`,
`repository_unavailable`, `credential_unavailable`, `git_refused`,
`process_failed`, `timed_out`, or `result_too_large`. Failure detail contains
only bounded tool output after exact injected-value redaction; it carries no
host path, configured URL, credential path, or credential value.

`workspace-restricted` defaults all ten tools to `Auto`; `ambient` defaults only
`workspace_read` to `Auto`. The runner derives advertisement and executor lookup
from this same compiled registry, while the daemon remains the declaration and
policy authority.

Each runner declaration also states the session capability its execution
requires, and that requirement — not registry membership — decides what a
session advertises. `workspace_read`, `workspace_write`, `workspace_edit`,
`shell_exec`, and `build_test` require only a writable root, which every
runner-backed session has. `git_fetch`, `git_branch`, `git_commit`, and
`git_push` require the session's writable root to be a repository worktree;
`git_clone` requires a writable root and one configured repository entry,
because it clones into that root instead of presuming the root is already a
worktree; and each operation that reaches a remote additionally requires the
optional credential profile its repository entry names. A session whose
composition does not satisfy a declaration's requirement is never offered that
declaration ([model-call execution](model-call-execution.md#frontier-rendering)
prepares the snapshot), so a session whose writable root is not a worktree
advertises the five writable-root tools, none of the four worktree Git tools,
and `git_clone` only when its selected runner advertises at least one repository
entry whose optional credential-profile name equals the optional profile that
session selected. For `git_fetch` and `git_push`, the required entry is instead
the exact repository key recorded by the workspace manifest, paired in the
current advertisement with that same optional profile. Every configured
repository entry names either one exact profile or explicit anonymous access
([configuration and credentials](configuration-and-credentials.md#runner-configuration)),
and the advertisement carries each key together with that optional profile
([runner protocol and placement](runner-protocol.md#advertised-catalogs-and-daemon-authority)),
so preparation decides both forms from the registration snapshot alone: a
profileless session matches anonymous access, a profiled session matches only
its exact grant, and a session whose required entry is absent or differently
paired advertises no affected remote Git tool. Which eligible key a `git_clone`
request names remains the runner's admission check; a worktree remote tool has
no alternate key because its manifest already fixed one. A
`repository_unavailable` or `credential_unavailable` refusal therefore names a
specific rejected argument or a post-preparation availability change rather than
a capability the session was told it had. Why: a requirement stated on the
declaration is checked once at preparation, while a requirement implied only by
a tool's argument contract can be discovered no earlier than the dispatch that
fails — and the key/optional-profile pairing is what makes this particular
requirement decidable at preparation, including anonymous access.

Each provider operation carries the exact session-executable definition and
locus snapshot prepared under
[model-call execution](model-call-execution.md#frontier-rendering). Runner-only
definitions absent from current selected execution authority are not advertised;
`RunnerAbandoned` exposes daemon-executable declarations only, and lost
placement blocks preparation until user recovery. Initial approval and dispatch
for a proposal are derived from that same frozen snapshot, never from a later
catalog or registration lookup. A dynamic catalog or runner change while the
provider call is in flight therefore cannot upgrade permission, introduce an
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
and the database constraint enforce the same admission rule. These two bounds
are the source of the direct-process output caps above: an executor that
produces more output than its durable evidence can hold truncates honestly and
reports the true sizes rather than widening the bound or converting a success
into a failure.

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
following user-role message. Every provider-visible failure is this compact
provider-neutral JSON object: `{"error":{"detail":D,"kind":K}}`. `D` is the
admitted executor detail, admitted user denial reason, or JSON null; `K` is
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
  invalid. Before dispatch, its canonical origin must satisfy the
  deployment-owned
  [web-fetch catalog policy](configuration-and-credentials.md#the-static-model-alias-and-web-fetch-catalog),
  which owns the origin bound, canonicalization, and absent-or-empty behavior;
  this admission gates automatic execution. A domain must resolve to between one
  and 32 addresses and every address must be public; the admitted addresses are
  pinned into the request client so connection setup cannot substitute a later
  DNS answer. Its permission default is `Auto`; its effect class is
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

The code-host catalog contains sixteen GitHub tools. Every operation is
`ExternalEffect` because GitHub observes its authenticated request. The twelve
read-only declarations — `change_request_summary`,
`change_request_changed_files`, `change_request_file_patch`,
`repository_read_file`, `repository_list_directory`,
`change_request_checks_status`, `change_request_review_threads`,
`change_request_ci_job_log`, `change_request_convergence_state`,
`change_request_stack_state`, `change_request_thread_inventory`, and
`review_gate_check` — default to `Auto`. The four mutations —
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
  lines exceed the retained bytes; source bytes before the selection cannot
  impersonate discarded selected content. `path_not_found`,
  `revision_not_found`, `not_a_file` with the observed repository-object type,
  and `binary_file` with source bytes are separate non-content outcomes; every
  file outcome carries `truncated`. Before any Contents request, the GitHub
  adapter requests the bounded SHA representation of `/commits/{revision}`.
  Success must return the exact required revision; a different resolution,
  including a forty-hex branch or tag name that resolves to another commit,
  fails execution before repository content is read. An exact resolution pins
  the Contents request to that commit, and a Contents 404 then means
  `path_not_found`. A commit conflict proves an empty repository and returns
  `revision_not_found` without a Contents request. A commit 422 whose bounded
  JSON `message` is exactly `No commit found for SHA: {revision}` also proves
  absence without a Contents request; any other 422 remains failed execution. A
  commit 404 permits one Contents visibility probe pinned to the requested
  revision: only a bounded 404 body that exactly names that revision as an
  absent ref returns `revision_not_found`; a generic 404 remains failed
  execution because metadata visibility alone cannot prove absence. A successful
  text or binary file read therefore uses three requests (commit resolution,
  Contents metadata, immutable blob), a directory result or path absence uses
  two (commit resolution and Contents), and revision absence uses one or two.
  Every request shares one 30-second transaction budget, and every request after
  resolution names the resolved commit or immutable blob rather than a moving
  reference.
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
  comments, and `comments_truncated`; the outer result carries `truncated`.
- `change_request_thread_reply` accepts an opaque `thread_id` and nonempty
  `body`; it returns the created comment node id and URL.
- `change_request_thread_resolve` accepts one opaque `thread_id`; it returns
  that identity and the acknowledged resolution posture.
- `change_request_ci_job_log` accepts `repository` and a positive `job_id`; it
  returns that id, at most 64 KiB of lossy UTF-8 log text, and `truncated`.
- `change_request_rerun_failed_jobs` accepts `repository` and a positive
  workflow `run_id`; it returns the acknowledged run id.
- `change_request_convergence_state` accepts `repository` and `number`. It
  returns the exact head and mergeable state; the current-head CI rollup and
  bounded check contexts; unresolved-thread identities; open and buried
  escalation-marker identities; all resolved or unresolved undispositioned
  threads; and reviewer-verdict evidence. Reviewer evidence merges review bodies
  and issue comments in code-host timestamp order. Only the exact
  `chatgpt-codex-connector` bot can supply a verdict or usage-limit response;
  the last complete, line-anchored `Reviewed commit:` record is the verdict, and
  only the complete canonical usage-limit response is starvation evidence. The
  evidence also reports usage-limit starvation and whether the latest explicit
  `@codex review` request by an owner, member, or collaborator has no later
  verdict or starvation response. A request tied with the latest response
  timestamp is treated as still pending because the code host does not expose a
  reliable order within that timestamp. Typed construction rejects an open
  escalation identity absent from the unresolved-thread evidence. Its derived
  verdict is `converged`, `converged_with_escalations`, `not_converged`, or
  `indeterminate`. A missing or non-successful CI rollup, conflicting
  mergeability, any unresolved non-escalation or undispositioned thread,
  reviewer starvation, or a still-pending request prevents convergence; an
  additive unknown mergeability state or any truncated evidence required for the
  verdict makes it indeterminate.
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
  `next_cursor`.
- `review_gate_check` accepts `repository`, `number`, and purpose
  `request_review_wave` or `declare_convergence`. It reads the same fresh typed
  evidence as the three slog tools, then re-reads stack ancestry and convergence
  after inventory. It rejects the composed read if either before-and-after
  evidence pair differs and uses the final stack and convergence reads for
  blockers, then purely derives `ready`, the exact head, and stable blocker
  codes with affected identities within the transport's 30-second aggregate
  deadline. Both purposes block when the three evidence sources name different
  heads, a review request is still in flight, evidence is incomplete, CI is not
  green, threads are undispositioned or unresolved, escalations are buried, or
  parent, default-chain, or immediate-child ancestry is unhealthy. Declaring
  convergence additionally requires the exact mergeable posture and an actual
  current-head reviewer verdict not followed by usage-limit starvation.
  Requesting a review wave is blocked when a completed reviewer verdict already
  covers the current head and no later usage-limit response requires a retry,
  because that quiet or all-declined wave concludes the loop.

Shared typed admission rejects extra object members; repositories are at most
256 bytes, paths 4 KiB, comment bodies and returned text fields 64 KiB, and
opaque node ids and GraphQL cursors are at most 512 bytes. Paths use canonical
repository-relative spelling: no empty, dot, or parent component is admitted,
with bare `.` reserved for the repository root. A returned node id, head
revision, or stack or inventory continuation is admitted by the same predicate
its argument counterpart uses, so those identities and continuations can be
passed back as arguments. Convergence-state cursors identify a diagnostic
truncation boundary; that tool and the gate remain indeterminate rather than
performing an unbounded continuation scan. Every returned URL is one absolute
credential-free HTTPS location. Typed construction rejects aggregate convergence
evidence whose overlapping bounded lists would exceed the shared encoded-result
limit. No result has more than 100 collection members or more than 512 KiB of
encoded JSON. Every bounded slog list reports whether it is truncated and the
matching continuation cursor; a verdict never silently treats a partial evidence
page as complete.

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

This catalog family is verified through PR #385 (`agent/plan-dependencies`) at
implementation ref `c9ca8ba54e2f93cb3a715321ffcae605ce925bed`.

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
append nothing; duplicate links remain in history but fold once by first append.

`plan_read` accepts an optional positive exclusive `after_entry_id` cursor and
`include_history`, which defaults to false. It returns at most 100 folded
entries in creation order, including dependencies and derived `ready` or
`waiting` readiness: an entry is waiting exactly while any dependency is not
completed. The page carries `next_after_entry_id` and `plan_truncated`;
requested `history` contains at most 100 chronological events with independent
`history_truncated`. Compact admission may retain a smaller prefix while
preserving those labels.

The merged catalog sorts declarations by checked tool name and rejects
duplicates during construction. Its executor dispatches only those same four
preexisting names, the two session-plan names, and the sixteen code-host names;
disagreement between the advertised catalog and executor is classified as a
daemon defect.

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

`DecideToolRequest` joins the user-global durable-command registry as its own
typed record family. Adding the dangerous posture originally advanced each
defaults-bearing command family to kind-scoped storage version 2; version-1
records reconstitute with `DangerousToolAutoApproval::Disabled`. Later
system-prompt and template provenance migrations advance the affected families
independently. The current kind-scoped versions and their compatibility gates
are owned by
[identity and commands](identity-and-commands.md#durable-command-records) and
[persistence protocol](persistence-protocol.md#relational-representation).
`SubmitInput` and `DecideToolRequest` remain version 1; registry inspection
validates the supported version set for the selected kind rather than applying
one global version constant.

## Open edges

- Dynamic execution-strategy policy beyond the two named runner profiles,
  model-declared approval expiry, LLM-judge approval, and additional high-risk
  guardrails are recorded in [Tool safety](../open-questions.md#tool-safety).
- Rich result-content variants and durable tool-definition revisioning across
  outstanding requests are recorded in
  [Tool safety](../open-questions.md#tool-safety).
- Durable storage for payloads larger than the 1 MiB result and 4,096-byte
  detail bounds, and the abuse controls a larger bound requires, are recorded in
  [Tool safety](../open-questions.md#tool-safety). The version-one output caps
  above are those bounds restated, not a judgment about how much output is
  useful.
- General tool-attempt retry and ambiguous-wait resolution beyond the sealed
  runner-loss transitions are recorded in
  [Tool safety](../open-questions.md#tool-safety).
- Runner placement, local transport, workspace, profile, and lease law are owned
  by [runner protocol and placement](runner-protocol.md). Remote runner
  transport and multiple-runner scheduling remain recorded under
  [Scheduling and runners](../open-questions.md#scheduling-and-runners),
  [Protocols and persistence](../open-questions.md#protocols-and-persistence),
  and
  [Identity, credentials, and resource governance](../open-questions.md#identity-credentials-and-resource-governance).
- Client approval presentation is recorded under
  [Client scope](../open-questions.md#client-scope).
- Streaming tool deltas remain part of the model-streaming question in
  [Protocols and persistence](../open-questions.md#protocols-and-persistence).

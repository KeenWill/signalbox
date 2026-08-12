# Program substrate

**Foundation contract.** This page owns the durable-execution contract for
registered programs: TypeScript orchestrators that drive sessions, evaluations,
and repository-watch reactions through a journaled effect protocol. The entire
surface below is committed ahead of code as Stage 0 of the substrate build,
verified against PR #580 (`agent/program-substrate-spec`); each paragraph
records the compatibility constraint it imposes on present surfaces. Model
execution remains owned by [model-call execution](model-call-execution.md) and
the [model-runtime substrate](runtime-substrate.md), sessions and turns by
[sessions and the transcript](sessions-and-transcript.md) and
[turn lifecycle and scheduling](turn-lifecycle-and-scheduling.md), goals by
[goal mode](goal-mode.md), tool dispatch by [tool loop](tool-loop.md), event
ingress by [repository watch](repo-watch.md), evaluation semantics by the
[evaluation system](eval-system.md), wire framing by
[process protocol](process-protocol.md), and durable storage idioms by
[persistence protocol](persistence-protocol.md).

## Programs and registration

**Committed unimplemented functionality.** No present surface registers or
executes programs. A program is TypeScript whose imports resolve only to the
versioned in-repo SDK package and to files inside its own registration set; any
other import is a registration error. Registration stores the exact TypeScript
source, the stripped JavaScript artifact, a digest of each, the SDK version, the
frame-contract version, and an explicit capability grant list, as immutable rows
keyed by unique program name and revision. Each digest is SHA-256 in lowercase
hexadecimal over an exact preimage: for the artifact, the stripped JavaScript
bytes; for the source, the registration set's files in byte-lexicographic path
order, each framed as its UTF-8 path, one zero byte, its content length in
decimal ASCII, one zero byte, then its content bytes. Both registration paths
and every later verifier compute identity from these preimages alone, so two
correct implementations cannot disagree about what a program is. Execution uses
only the stripped artifact; the TypeScript source is retained for reading and
re-verification. This constrains present schema planning: program identity is
`(name, revision)` plus content digests, and nothing may treat a mutable
location — a repository path, a branch, a file — as what a program *is*. Why:
digest identity is what lets an in-flight run keep meaning the code it started
with.

**Committed unimplemented functionality.** No present surface pins program
revisions to runs. Every run records the compiled digest and frame-contract
version it started under and finishes on that exact artifact; re-registration of
a name creates a new revision and never rebinds an in-flight run. Upgrading a
long-lived program is a deliberate act: cancel the old run, start the new
revision. A frame-contract change is handled the same way, per the pre-alpha
rule in `AGENTS.md`: no retired contract version is decoded by a newer host —
that would be compatibility machinery for deployments that do not exist — so a
run whose pinned contract version the current host no longer speaks is faulted
terminally at its next wake with a deterministic, journaled `contract_retired`
fault, and closing or continuing outstanding runs before a contract-changing
upgrade is an operational courtesy, not a spec obligation.

**Committed unimplemented functionality.** No present surface performs
registration-time type-checking. Two registration paths exist with one trust
boundary. The executable artifact is never accepted on trust: both paths derive
it from the submitted source with one pinned, deterministic type-strip transform
— parse and erase types, no checking — embedded identically in the CLI and the
daemon, so the artifact bytes are a pure function of the source bytes and the
reviewed source *is* the executed code; a submitted artifact whose digest
differs from the derived artifact's digest is rejected. The operator path runs
in the CLI: `tsc --strict` against the SDK's shipped declarations, then the
pinned strip, digest, insert. The agent path is a gated daemon tool on the
ordinary tool surface: a session submits the source, requested grants, and
digests; the daemon re-derives the artifact, verifies digests, enforces the
import allowlist and size bounds, and records which party claims the type-check
(`cli` or `registrant`). The daemon never type-checks. Type-checking is an
authoring aid, not a security boundary: a mistyped program can only fault inside
its own sandbox. The security boundary for registration is the same as for every
gated tool — the approval posture and the [approval judge](tool-loop.md), which
see the program name, the requested grant list, the source digest, and the
registering session's context. Why: agents must be able to create programs the
way they use any other tool, without the daemon acquiring a compiler toolchain
or the type system acquiring authority it cannot enforce.

**Committed unimplemented functionality.** No present surface grants program
capabilities. Every program row carries an explicit grant list drawn from a
closed vocabulary owned by this page; the initial vocabulary is `time`,
`random`, `sleep`, `subscribe`, `session`, `judge`, `exec-stage`, `corpus`,
`eval-record`, `blob`, and `register` (the registration tool itself, so a
program may register programs only when explicitly granted). A capability absent
from the grant list does not exist for that program: the host refuses the effect
before any authority is exercised. Grants are least-privilege from the first
registered program, and `register` attenuates: a program registering a program
may request for the child only a subset of its own grants, so no chain of
program-initiated registrations can mint a capability its root was never granted
— any expansion goes through the user-authorized registration paths.

## Execution, journal, and replay

**Committed unimplemented functionality.** No present surface executes program
artifacts. Programs run one at a time per run inside an embedded
JavaScript-engine isolate with no ambient filesystem, network, environment, wall
clock, or unvirtualized randomness; the only door out of the isolate is the
frame protocol below. The engine is the pinned `deno_core` crate family; the
standalone repository is archived and the deno monorepo is its source of truth,
so the pin is by crate version with upgrades taken deliberately. Every run
records the engine version it started under. No engine runtime is retained
across upgrades — per the pre-alpha rule in `AGENTS.md`, keeping superseded
engines resident would be compatibility machinery for deployments that do not
exist — so a run that wakes under a newer engine replays under it, protected by
the fault-not-diverge rule below: an engine-semantics change surfaces as a
nondeterminism fault whose payload names both engine versions, never as silent
divergence. A native engine failure is accepted as a daemon failure while every
registered program is the operator's own. Why: the isolate's closure is what
makes deterministic replay a structural property instead of an authoring
discipline.

**Committed unimplemented functionality.** No present surface journals program
effects. Every nondeterministic act crosses the frame protocol and is recorded
as append-only journal rows. Requests (what the program asked, in program order)
and deliveries (what the host answered, in delivery order) are both journaled.
Every request carries a per-run monotone request ordinal, and every `answer` and
`wake` names the request ordinal it resolves, so a delivery is unambiguous under
concurrency: delivery order fixes the interleaving, and the named ordinal fixes
which promise each delivery resolves — the same association during live
execution and replay. The frame vocabulary is: `now`, `random`, `sleep`,
`await_event`, `effect`, `scope`, and `terminal` requests; `answer`, `wake`,
`cancel`, and `fault` deliveries. Capability calls are `effect` frames named by
capability and method, so capability growth never changes the frame contract.
Effect failures are ordinary answer values a program branches on; only `fault`
terminates a run from outside, from its own closed cause set: `timeout`,
`memory`, `nondeterminism`, `program_error` (an uncaught exception or unhandled
promise rejection before `terminal`, carrying bounded, replay-stable evidence of
the error), `contract_retired`, and `journal_bound`. Faults are themselves
journaled so even a kill replays.

**Committed unimplemented functionality.** No present surface synchronizes
journal rows with effects, and the synchronization guarantee differs by effect
class. A transactional effect — one whose entire consequence is rows this daemon
writes, such as session creation, input submission, or evaluation recording —
commits its `answer` frame in the same transaction as the consequence, following
the transactional-outbox append idiom of the
[persistence protocol](persistence-protocol.md), so the journal and the world
cannot disagree. An external effect — a model call, a stage execution, anything
whose consequence lives outside this database — journals its `effect` request
before the operation is issued, and its outcome when the operation reports; a
crash between the two leaves a journaled request with no answer, and recovery
follows the capability's declared recovery rule: adopt the outcome when the
operation's own durable record proves it completed, re-issue only when the
capability declares the operation idempotent, and otherwise answer the request
with a journaled `ambiguous` outcome the program must branch on — mirroring the
external-effect ambiguity contract of [tool loop](tool-loop.md), which forbids
pretending an unresolved external loss did not happen. Why: one honest ambiguity
is recoverable; a false exactly-once claim is not.

**Committed unimplemented functionality.** No present surface replays program
runs. Resume discards nothing and restores nothing: a woken run re-executes its
artifact from the start while the host answers each request from the journal,
delivering answers in the journaled delivery order, and switches to live
execution exactly where the journal ends. A replayed request that differs from
its journaled twin is a nondeterminism fault that fails the run with both frames
recorded; divergence is never silent. Concurrent outstanding requests are
permitted — the journaled delivery order, with the microtask queue drained to
quiescence between deliveries, is what makes promise interleaving identical
across live execution and replay. Virtualized time advances only at journaled
points, and each randomness draw is journaled. Why: recording the delivery order
is the one discipline that buys unrestricted intra-program concurrency without
restricting the language.

**Committed unimplemented functionality.** No present surface bounds journal
growth. A long-lived program does not accumulate one unbounded journal:
`terminal` carries a `continue` outcome that ends the run and starts a successor
run row — same pinned artifact, explicit continuation arguments, predecessor
identity recorded — with a fresh journal, and the built-in dispatch program
described below continues after each handled event. Continuation is voluntary,
so the bound is enforced by the host, not assumed of the program: a configured
per-run frame bound terminates any run that reaches it with a deterministic,
journaled `journal_bound` fault, making the per-run replay bound a property of
every program, cooperative or not. No checkpointing or journal truncation
exists, because a journal that can be rewritten is not a journal. Why:
continuation keeps replay linear in one run's work while every historical run
remains a complete, immutable record, and the fault keeps that claim true for
programs that never continue.

**Committed unimplemented functionality.** No present surface parks program
runs. A run sleeping on a timer or subscription holds no isolate and no memory
beyond its rows; wake builds a fresh isolate and replays. Sleeping runs survive
daemon restarts by construction. Frame payloads inline below a fixed threshold;
a larger payload is stored as an immutable SHA-256-addressed blob under the
contract [blob storage](blob-storage.md) owns, journaled by digest, with the
storage class it travels under owned by that page's class-routing vocabulary. No
named-artifact aggregate is required or committed for payload offload: the
journal references immutable bytes by digest only. A session outcome journals as
the session identity, the exact turn and accepted-input identity that produced
it, and an outcome digest — never transcript content — because sessions are
already durable and the journal is thin coordination state only; the recorded
turn identity is what lets replay authenticate which of a session's turns
supplied a delivered answer.

**Committed unimplemented functionality.** No present surface re-executes
evaluation trials. Replay of a run's own journal (resume) and a sibling run with
a fresh journal over the same pinned artifact and arguments (repeat) are
distinct first-class operations: resume reproduces recorded nondeterminism,
repeat deliberately re-samples it. Evaluation repeats are repeats, never
resumes.

## Events, subscriptions, and cancellation

**Committed unimplemented functionality.** No present surface subscribes
programs to events. `await_event` records a durable subscription row naming an
event kind and filter over the vocabulary that [repository watch](repo-watch.md)
already stores durably, together with an activation frontier fixed at
registration strictly after the current durable event tail — the same watermark
discipline the structured-rule contract already uses — so a subscription can
never match an event at or before its activation. After a poll's events commit,
matching subscriptions produce wake rows keyed by the unique
subscription-and-event identity, so recovery re-matching is idempotent: an event
wakes a subscription exactly once, with no historical, missed, or duplicate wake
at the registration or recovery boundary. The scheduler resumes the woken runs.
Program subscription identity, delivery, and cancellation are decided by this
page; the previously open standing-subscription foundation question is narrowed
in the same diff to the client-facing callback surface it still owns. This
constrains repository-watch storage now: its cursor and event rows are the
substrate's event source and must remain readable by subscription matching. The
present structured-rule dispatch surface is committed to converge onto this
mechanism — the dispatch action becomes a built-in program, cut over after one
shadowed live event — after which rules are subscriptions.

**Committed unimplemented functionality.** No present surface cancels program
runs. Cancellation is a user command pair on the
[process protocol](process-protocol.md) from the substrate's first protocol
release: a cancel command naming the run, answered from a closed outcome set —
applied (the run is now `cancelled`), `not_found` (no such run), or
`already_terminal` naming the standing terminal state and result the command
found — with the same durable-command identity mechanics as every user command
in [identity and commands](identity-and-commands.md). A cancel never overwrites
a terminal outcome: the race against a run's own `terminal` is resolved by
whichever committed first, and the receipt reports the truth it found. Cancel
authority is user authority; an applied cancel is journaled as a `cancel` frame,
so a cancelled run replays to its cancellation. Programs receive no notice
beyond the journal: cancellation is terminal, not advisory.

## Driving sessions

**Committed unimplemented functionality.** No present surface lets programs
create sessions. The session capability composes the existing create-session,
input-submission, and turn-scheduling services, extended where the present
contracts have no room for program agency. Two extensions are committed in the
pages that own them. First, attribution: the committed program-issuance
extension to the closed actor algebra is recorded in its owning contract,
[identity and commands](identity-and-commands.md); this page adds only the
program-specific constraint that program-issued input names the issuing run
identity and is never recorded as user-issued. Second, provenance:
program-created sessions carry new creation-cause variants (`workflow`, and
`eval` per the [evaluation system](eval-system.md)) in the stored vocabulary of
[sessions and the transcript](sessions-and-transcript.md). Programs drive
sessions turn by turn: submit input, await that turn's outcome as a typed
payload, then branch. The [model-runtime substrate](runtime-substrate.md)
already carries an optional per-call structured-output contract, but the session
path from accepted input through turn preparation to the prepared model
operation does not; that carriage — a declared output schema recorded on the
program-issued input, flowing to the prepared call, enforced at the runtime
boundary — is committed here and updates
[model-call execution](model-call-execution.md) when implemented. Structure
inside one turn is deliberately out of contract: a turn is the model's autonomy
zone, governed by the same approval judge as every session, and a program that
needs intra-turn evidence reads the durable transcript through a read capability
after the fact. Credentials never enter the isolate; sessions, model calls,
clones, and stage executions all happen host-side under existing credential
machinery.

## Open edges

- Remote and out-of-process program hosts:
  [open-questions](../open-questions.md#program-substrate-and-evaluations).

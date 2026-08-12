# Program substrate

**Foundation contract.** This page owns the durable-execution contract for
registered programs: TypeScript orchestrators that drive sessions, evaluations,
and repository-watch reactions through a journaled effect protocol. The entire
surface below is committed ahead of code as Stage 0 of the substrate build,
verified against this PR (`agent/program-substrate-spec`); each paragraph
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
keyed by unique program name and revision. Execution uses only the stripped
artifact; the TypeScript source is retained for reading and re-verification.
This constrains present schema planning: program identity is `(name, revision)`
plus content digests, and nothing may treat a mutable location — a repository
path, a branch, a file — as what a program *is*. Why: digest identity is what
lets an in-flight run keep meaning the code it started with.

**Committed unimplemented functionality.** No present surface pins program
revisions to runs. Every run records the compiled digest and frame-contract
version it started under and finishes on that exact artifact; re-registration of
a name creates a new revision and never rebinds an in-flight run. Upgrading a
long-lived program is a deliberate act: cancel the old run, start the new
revision.

**Committed unimplemented functionality.** No present surface performs
registration-time type-checking. Two registration paths exist with one trust
boundary. The operator path runs in the CLI: `tsc --strict` against the SDK's
shipped declarations, strip, digest, insert. The agent path is a gated daemon
tool on the ordinary tool surface: a session submits the source, the stripped
artifact, requested grants, and digests; the daemon verifies the digests against
the submitted bytes, parses the artifact without executing it, enforces the
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
registered program.

## Execution, journal, and replay

**Committed unimplemented functionality.** No present surface executes program
artifacts. Programs run one at a time per run inside an embedded
JavaScript-engine isolate with no ambient filesystem, network, environment, wall
clock, or unvirtualized randomness; the only door out of the isolate is the
frame protocol below. The engine is the pinned `deno_core` crate family; the
standalone repository is archived and the deno monorepo is its source of truth,
so the pin is by crate version with upgrades taken deliberately. A native engine
failure is accepted as a daemon failure while every registered program is the
operator's own. Why: the isolate's closure is what makes deterministic replay a
structural property instead of an authoring discipline.

**Committed unimplemented functionality.** No present surface journals program
effects. Every nondeterministic act crosses the frame protocol and is recorded
as append-only journal rows in the same transaction as the effect it records,
following the transactional-outbox append idiom of the
[persistence protocol](persistence-protocol.md): a journal that says an effect
happened is never ahead of or behind the world. Requests (what the program
asked, in program order) and deliveries (what the host answered, in delivery
order) are both journaled. The frame vocabulary is: `now`, `random`, `sleep`,
`await_event`, `effect`, `scope`, and `terminal` requests; `answer`, `wake`,
`cancel`, and `fault` deliveries. Capability calls are `effect` frames named by
capability and method, so capability growth never changes the frame contract.
Effect failures are ordinary answer values a program branches on; only `fault`
(timeout, memory, nondeterminism) terminates a run from outside, and faults are
themselves journaled so even a kill replays.

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

**Committed unimplemented functionality.** No present surface parks program
runs. A run sleeping on a timer or subscription holds no isolate and no memory
beyond its rows; wake builds a fresh isolate and replays. Sleeping runs survive
daemon restarts by construction. Large frame payloads are stored by digest in
the content-addressed blob store once that storage stack lands, and inline below
a fixed threshold until then; a session outcome journals as the session identity
plus an outcome digest, never transcript content, because sessions are already
durable and the journal is thin coordination state only.

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
already stores durably; after a poll's events commit, matching subscriptions
produce wake rows, and the scheduler resumes the subscribed runs. This
constrains repository-watch storage now: its cursor and event rows are the
substrate's event source and must remain readable by subscription matching. The
present structured-rule dispatch surface is committed to converge onto this
mechanism — the dispatch action becomes a built-in program, cut over after one
shadowed live event — after which rules are subscriptions.

**Committed unimplemented functionality.** No present surface cancels program
runs. Cancellation is a user command pair on the
[process protocol](process-protocol.md) from the substrate's first protocol
release: a cancel command naming the run and a receipt confirming the terminal
`cancelled` state, with the same durable-command identity mechanics as every
user command in [identity and commands](identity-and-commands.md). Cancel
authority is user authority; a delivered cancel is journaled as a `cancel`
frame, so a cancelled run replays to its cancellation. Programs receive no
notice beyond the journal: cancellation is terminal, not advisory.

## Driving sessions

**Committed unimplemented functionality.** No present surface lets programs
create sessions. The session capability wraps the existing create-session,
input-submission, and turn-scheduling services without new machinery beneath it;
program-created sessions carry new creation-cause variants (`workflow`, and
`eval` per the [evaluation system](eval-system.md)) that join the stored
vocabulary of [sessions and the transcript](sessions-and-transcript.md), so
program-driven traffic is distinguishable in one predicate everywhere sessions
are read. Programs drive sessions turn by turn: submit input, await that turn's
outcome as a typed payload validated against the program's declared schema
through the structured-output path the
[model-runtime substrate](runtime-substrate.md) already provides per call, then
branch. Structure inside one turn is deliberately out of contract: a turn is the
model's autonomy zone, governed by the same approval judge as every session, and
a program that needs intra-turn evidence reads the durable transcript through a
read capability after the fact. Credentials never enter the isolate; sessions,
model calls, clones, and stage executions all happen host-side under existing
credential machinery.

## Open edges

- Remote or out-of-process program hosts (the frame protocol is the seam; only
  the in-daemon host is committed) — recorded in
  [open-questions](../open-questions.md) if and when a concrete need appears; no
  entry exists today.

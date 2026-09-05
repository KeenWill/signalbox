# Program substrate

This page owns the durable-execution contract for registered programs:
TypeScript orchestrators that drive sessions, evaluations, and repository-watch
reactions through a journaled effect protocol. Model execution is owned by
[model-call execution](model-call-execution.md) and the
[model-runtime substrate](runtime-substrate.md), sessions and turns by
[sessions and the transcript](sessions-and-transcript.md) and
[turn lifecycle and scheduling](turn-lifecycle-and-scheduling.md), goals by
[goal mode](goal-mode.md), tool dispatch by [tool loop](tool-loop.md), event
ingress by [repository watch](repo-watch.md), evaluation semantics by the
[evaluation system](eval-system.md), wire framing by
[process protocol](process-protocol.md), and durable storage idioms by
[persistence protocol](persistence-protocol.md).

## Programs and registration

**Implemented behavior.** The program-runtime crate executes one already
stripped JavaScript module and resolves only the canonical frame-contract-v1 SDK
specifier below. Its loader rejects every other static or dynamic import,
including relative files, and has no filesystem or network source. It maps the
canonical specifier to a host-supplied synthetic module. The current execution
surface exports `now`, `random`, `sleep`, and `awaitEvent`; each accepts exact
bytes and produces one typed request at the Rust boundary. Registration owns the
complete SDK surface and artifact admission described next.

**Committed unimplemented functionality.** No present surface registers
programs. A program is one TypeScript module whose imports resolve only to the
versioned in-repo SDK package; any other import — including relative files — is
a registration error, so the stripped artifact is complete and executable with
no bundler, no module graph, and no filesystem in the isolate. The SDK's
canonical bare-module specifier is `@signalbox/program-sdk/v<version>`, where
`<version>` is a positive canonical decimal integer with no leading zero; the
first frame-contract release admits exactly `@signalbox/program-sdk/v1`. Before
loading an artifact, the host registers a synthetic module under that exact
specifier which exports only the frame-producing SDK surface for the run's
recorded SDK version. Registration rejects every other specifier, including an
unversioned `@signalbox/program-sdk` name; type stripping preserves the
canonical import, and isolate loading resolves it only to this host-supplied
module, never through a filesystem or ambient module loader. Why: a
single-module contract keeps artifact identity one digest over one file's bytes.
Registration stores the exact TypeScript source, the stripped JavaScript
artifact, a digest of each, the SDK version, the frame-contract version, and an
explicit capability grant list, as immutable rows keyed by unique program name
and revision. Each digest is SHA-256 in lowercase hexadecimal over an exact
preimage: for the artifact, the stripped JavaScript bytes; for the source, the
module's UTF-8 bytes. Both registration paths and every later verifier compute
identity from these preimages alone, so two correct implementations cannot
disagree about what a program is. Execution uses only the stripped artifact; the
TypeScript source is retained for reading and re-verification. Program identity
is `(name, revision)` plus content digests, and nothing may treat a mutable
location — a repository path, a branch, a file — as program identity. Why:
digest identity is what keeps an in-flight run bound to the code it started
with.

**Committed unimplemented functionality.** No present surface pins program
revisions to runs. Every run records the exact program registration it executes
— the immutable `(name, revision)` row reference — together with the compiled
digest and frame-contract version it started under, and finishes on that exact
artifact under that exact registration's grant list and SDK version; a digest
alone is not identity, because identical bytes registered under different names
or grants are different programs, and a run's authority resolves only from its
recorded registration. Continuations record the same registration reference
alongside the predecessor. Re-registration of a name creates a new revision and
never rebinds an in-flight run. Upgrading a long-lived program means cancelling
the old run and starting the new revision. A frame-contract change is handled
the same way, per the pre-alpha rule in `AGENTS.md`: no retired contract version
is decoded by a newer host — that would be compatibility machinery for
deployments that do not exist — so a run whose pinned contract version the
current host no longer decodes is faulted terminally at its next wake with a
deterministic, journaled `contract_retired` fault, and the host does not require
outstanding runs to be closed or continued before a contract-changing upgrade.

**Committed unimplemented functionality.** No present surface performs
registration-time type-checking. Two registration paths exist with one trust
boundary. The executable artifact is never accepted on trust: both paths derive
it from the submitted source with one pinned, deterministic type-strip transform
— parse and erase types, no checking — embedded identically in the CLI and the
daemon, so the artifact bytes are a pure function of the source bytes and the
reviewed source is the executed code; a submitted artifact whose digest differs
from the derived artifact's digest is rejected. The operator path runs in the
CLI: `tsc --strict` against the SDK's shipped declarations, then the pinned
strip, digest, insert. The agent path is a gated daemon tool on the ordinary
tool surface: a session submits the source, requested grants, and digests; the
daemon re-derives the artifact, verifies digests, enforces the import allowlist
and size bounds, and records which party claims the type-check (`cli` or
`registrant`). The daemon never type-checks. Type-checking is an authoring aid,
not a security boundary: a mistyped program can only fault inside its own
sandbox. The security boundary for registration is the same as for every gated
tool — the approval posture and the [approval judge](tool-loop.md), which see
the program name, the requested grant list, the source digest, and the
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
program-initiated registrations can obtain a capability its root was never
granted — any expansion goes through the user-authorized registration paths.

## Execution, journal, and replay

**Implemented behavior.** The program-runtime crate runs one module per
execution attempt in a fresh embedded `deno_core` isolate, pinned exactly by
crate version. The isolate exposes no filesystem, network, environment, module
source, wall clock, or unvirtualized randomness. Before artifact evaluation the
host removes the engine's `Date`, `Temporal`, `Math.random`, `Intl`, `WeakRef`,
`FinalizationRegistry`, `SharedArrayBuffer`, `Atomics`, `WebAssembly`, and
`Deno.core` globals, along with the Intl-backed prototype methods that outlive
`Intl` itself — `toLocaleString`, `localeCompare`, and the locale-aware case
mappings. Each removal closes a path the others leave open: `Temporal` is a
second ambient clock reached without `Date`; the prototype methods return
results that follow the host's default locale and ICU data, which two hosts
running the same run need not share; and the shared buffer, its atomics, and
`WebAssembly` together remove the waiting primitives, since `Atomics.wait`
blocks the isolate thread, `Atomics.waitAsync` settles on a wall clock, and
`new WebAssembly.Memory({shared: true})` yields a shared buffer whose
`memory.atomic.wait32` blocks even when the `SharedArrayBuffer` global is gone.
Its only admitted asynchronous operation is the closed request op behind the
synthetic SDK module. A caller supplies the stripped artifact and an existing
run journal; no isolate is retained between attempts.

**Committed unimplemented functionality.** No admitted run-creation surface
invokes the host. Every admitted run records the engine version it started
under. At creation it also records the selected isolate heap ceiling and
per-live-turn execution budget. The execution budget is deterministic engine
work metering, not elapsed wall time: host queueing, daemon downtime, journal
I/O, and time awaiting an effect do not consume it, while replay consumes and
checks the same budget at the same execution points. Exhaustion produces the
journaled `memory` or `timeout` fault under the recorded limits regardless of
later configuration, host load, or machine speed. External operations use
capability-specific deadlines whose expiry is an ordinary journaled answer. No
engine runtime is retained across upgrades — per the pre-alpha rule in
`AGENTS.md`, keeping superseded engines resident would be compatibility
machinery for deployments that do not exist — so a run that wakes under a newer
engine replays under it, protected by the fault-not-diverge rule below. Replay
under a different engine may compare requests only while journaled twins remain;
if it reaches the journal tail without an earlier mismatch, the host journals a
`nondeterminism` fault whose payload names both engine versions and does not
permit that run to emit a new live request or terminal result. Thus an
engine-semantics change surfaces as a fault even when its first observable
difference would occur after the prior journal tail, never as silent divergence.
A native engine failure is accepted as a daemon failure while every registered
program is the operator's own. Why: the isolate's closure is what makes
deterministic replay a structural property instead of an authoring discipline.

**Implemented behavior.** The persistence crate provides one append-only frame
journal per program-run identity. Every nondeterministic act crosses the typed
frame protocol and is recorded as an immutable journal row. Requests (what the
program asked, in program order) and deliveries (what the host answered, in
delivery order) are both journaled. Each row also carries one contiguous global
journal position, which retains the request/delivery interleaving needed to know
whether the executor must emit its next request or receive a recorded delivery.
A per-run allocator serializes appends and the database checks that the global,
request, and delivery sequences advance contiguously together; resolution
references are unique and can name only an earlier answerable request.

Every request carries a per-run monotone request ordinal, and every `answer` and
`wake` names the request ordinal it resolves, so a delivery is unambiguous under
concurrency: delivery order fixes the interleaving, and the named ordinal fixes
which promise each delivery resolves — the same association during live
execution and replay. The frame vocabulary is: `now`, `random`, `sleep`,
`await_event`, `effect`, `scope`, and `terminal` requests; `answer`, `wake`,
`reject`, `cancel`, `run_cancel`, and `fault` deliveries. A `reject` names the
request ordinal whose frame the host refused, carries a reason from that request
kind's closed rejection vocabulary, and leaves the run live; it records a
protocol-level refusal before the requested transition, not an answer to an
effect. A request-scoped `cancel` always names the one affected request ordinal
and cannot terminalize a run; `run_cancel` has no request ordinal and
terminalizes the whole run. A `scope` request is a journaled declaration, never
answered: it carries an operation (`open` or `close`), its own per-run scope
ordinal, and its parent scope ordinal, recording the structured-concurrency tree
so that cancellation of a scope deterministically cancels exactly the
outstanding requests opened under it (each such cancellation is a `cancel`
delivery naming the affected request ordinal), and replay reproduces the same
tree from the same frames. Requests made outside any opened scope belong to the
root scope. Capability calls are `effect` frames named by capability and method,
so capability growth never changes the frame contract. Effect failures are
ordinary answer values a program branches on; only `fault` terminates a run from
outside, from its own closed cause set: `timeout`, `memory`, `nondeterminism`,
`program_error` (an uncaught exception or unhandled promise rejection before
`terminal`, carrying bounded, replay-stable evidence of the error),
`contract_retired`, `journal_bound`, and `payload_too_large`. Faults are
themselves journaled so even a kill replays. Frame payloads are currently exact
inline byte strings; the relational row keeps payload carriage separate from the
closed frame discriminators so a later digest column can offload new payloads
without rewriting existing inline rows.

**Committed unimplemented functionality.** The current host emits only the four
primitive answerable requests named above. No present executor applies generic
effects, scope cancellation, terminal-request admission, capability rejection,
or run terminalization. The typed journal can represent those frames and the
database retains their closed discriminators; producing them is owned by
registration, the executor, and the capabilities that enforce them. Module
fulfillment is only an execution-attempt observation; it does not terminalize
the durable run.

**Committed unimplemented functionality.** No present surface synchronizes
journal rows with effects, and the synchronization guarantee differs by effect
class. A transactional effect — one whose entire consequence is rows this daemon
writes, such as session creation, input submission, or evaluation recording —
commits its `answer` frame in the same transaction as the consequence, following
the transactional-outbox append idiom of the
[persistence protocol](persistence-protocol.md), so the journal and the
consequence cannot disagree. An external effect — a model call, a stage
execution, anything whose consequence lives outside this database — journals its
`effect` request before the operation is issued, and its outcome when the
operation reports; a crash between the two leaves a journaled request with no
answer, and recovery follows the capability's declared recovery rule: adopt the
outcome when the operation's own durable record proves it completed, re-issue
only when the capability declares the operation idempotent, and otherwise answer
the request with a journaled `ambiguous` outcome the program must branch on —
following the external-effect ambiguity contract of [tool loop](tool-loop.md),
which forbids treating an unresolved external loss as if it had not happened.
Why: a journaled ambiguity is recoverable; a false exactly-once claim is not.

**Implemented behavior.** The domain crate provides a checked replay cursor as
the executor-facing seam. Resume discards nothing and restores nothing: a woken
run re-executes its artifact from the start while the host answers each request
from the journal, delivering answers in the journaled delivery order, and
switches to live execution exactly where the journal ends. A replayed request
that differs from its journaled twin returns a typed nondeterminism failure
carrying both complete frames, which the persistence adapter can append as a
closed `fault` delivery; divergence is never silent and never a panic. The seam
yields at most one recorded delivery per step, committing the isolate host to
drain its microtask queue to quiescence between deliveries. A journal that
already records a terminal delivery never reaches that seam: because such a
delivery resolves no request and ends the attempt that recorded it, the first
one is the run's outcome and the journal names it without replay. Concurrent
outstanding requests are permitted — the journaled delivery order is what makes
promise interleaving identical across live execution and replay. Virtualized
time advances only at journaled points, and each randomness draw is journaled.
Why: recording the delivery order is what permits unrestricted intra-program
concurrency without restricting the language.

**Implemented behavior.** The program-runtime host applies that cursor to a real
JavaScript isolate. It assigns each request ordinal at the Rust boundary,
compares the complete request before taking live action, and persists a typed
nondeterminism fault on mismatch. A matching replay delivery is applied to its
named promise without consulting the live-delivery source. After each individual
delivery the host polls the engine to a microtask quiescence point before it
observes another request or applies another delivery. A recorded `run_cancel` or
`fault` anywhere in the loaded journal is resolved before any of that: the host
reads the recorded outcome and returns it before it creates an isolate, so an
attempt cannot displace an outcome already durable — not by requesting
immediately, not by blocking, and not by failing to compile at all. At the
durable tail it appends the request and the caller-supplied delivery through the
repository's conditional methods, each compare-and-append conditioned on the
tail this attempt loaded: a concurrent attempt that has already advanced that
tail makes the append insert nothing, and this attempt fails with a changed-tail
protocol error rather than extending a journal it no longer describes. It then
continues through the same promise-resolution path. The delivery-source seam
receives only the currently outstanding durable request frames; it is the
boundary capability executors implement. A module that throws is an isolate
failure carrying the engine's own message, never a completion: the engine
reports the exception through its event loop while the module's evaluation
future still fulfills, so the host reads the engine result first.

The implemented journal anchor pins only frame-contract version one. It is not a
complete run aggregate. Registration identity, artifact digest, SDK and engine
versions, capability grants, heap and execution budgets, frame bound, and
payload ceiling are absent until their owning registration, run-admission, and
configuration producers supply and enforce them. Extending the run aggregate
correlates the journal anchor to it rather than backfilling guessed values.

**Committed unimplemented functionality.** No present surface bounds journal
growth. A long-lived program does not accumulate one unbounded journal:
`terminal` carries a `continue` outcome that ends the run and starts a successor
run row — same pinned artifact, explicit continuation arguments, predecessor
identity recorded — with a fresh journal. A terminal request is admissible only
when every earlier answerable request ordinal has an `answer`, `wake`, or
request-scoped `cancel` delivery; otherwise the host rejects it with a
deterministic `reject` delivery naming that terminal request and reason
`outstanding_requests`, without committing terminal state. The program may then
await or cancel the outstanding requests and submit another terminal request;
`program_error` is exclusively the terminal fault defined above. Thus no
external effect can complete after the journal closes. For `continue`, one
database transaction commits the terminal frame, the predecessor's terminal
state, and exactly one successor row. The successor has a unique predecessor
reference, so retry after an ambiguous commit returns the already-created row
rather than losing or duplicating the continuation. The built-in dispatch
program described below continues after each handled event. Continuation is
voluntary, so the bound is enforced by the host, not assumed of the program:
each run records at creation the frame bound selected from configuration, that
recorded bound governs the run for its whole life regardless of later
configuration changes — a run's terminal outcome is determined by its own
durable facts, never by which configuration was live at its final wake — and a
run reaching its recorded bound terminates with a deterministic, journaled
`journal_bound` fault, making the per-run replay bound a property of every
program, cooperative or not. No checkpointing or journal truncation exists. Why:
continuation keeps replay linear in one run's work while every historical run
remains a complete, immutable record, and the fault enforces that bound for
programs that never continue.

**Committed unimplemented functionality.** No present surface parks program
runs. A run sleeping on a timer or subscription holds no isolate and no memory
beyond its rows; wake builds a fresh isolate and replays. Sleeping runs survive
daemon restarts by construction. Frame payloads inline below a fixed threshold;
a larger payload is stored as an immutable SHA-256-addressed blob under the
contract [blob storage](blob-storage.md) owns, journaled by digest, routed under
the daemon-derived `program_journal` storage class that page's routing
vocabulary commits for this use — never an operation-selected class. No
named-artifact aggregate is required or committed for payload offload: the
journal references immutable bytes by digest only. Run admission requires that
class to have a configured route and records `blob_storage.max_blob_bytes` as
the run's payload ceiling. The inline threshold is no greater than that ceiling,
and any request or delivery whose canonical payload encoding exceeds the
recorded ceiling is replaced before journal insertion or blob ingest by a
bounded, journaled `payload_too_large` fault carrying the recorded maximum and
observed byte length. The recorded ceiling governs replay despite later
configuration changes. A session outcome journals as the session identity, the
exact turn and accepted-input identity that produced it, and an outcome digest —
never transcript content — because sessions are already durable and the journal
is thin coordination state only; the recorded turn identity is what lets replay
authenticate which of a session's turns supplied a delivered answer.

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
registration strictly after the current durable event tail, except for the
built-in dispatch continuation handoff defined below. An ordinary subscription
therefore can never match an event at or before its activation. After a poll's
events commit, matching subscriptions produce wake rows keyed by the unique
subscription-and-event identity. An `await_event` subscription is one-shot: the
transaction inserting its first wake also atomically marks the subscription
consumed, and matching excludes consumed subscriptions. A uniqueness constraint
permits at most one wake for the subscription itself, so concurrent matching of
two events chooses the first in durable event order and cannot resolve one
request ordinal twice. Recovery re-matching is therefore idempotent, with no
historical, missed, stale, or duplicate wake at the registration or recovery
boundary. The scheduler resumes the woken runs. Repository watch's cursor and
event rows are the substrate's event source and must remain readable by
subscription matching. The present structured-rule dispatch surface is committed
to converge onto this mechanism — the dispatch action becomes a built-in
program. Shadowing is only a validation step and never owns delivery. The
rule-to-program cutover and frontier-ownership contract is owned by
[repository watch](repo-watch.md#deduplication-concurrency-and-audit); this page
only consumes the resulting subscription and transferred dispatch state. For
each handled event, the built-in program's `continue` transaction records the
consumed event as the successor's inherited exclusive event frontier. When that
successor issues `await_event`, activation uses this inherited frontier rather
than the then-current event tail and immediately matches the first eligible
durable event after it before waiting for future events. Events committed while
the predecessor handles its wake are therefore included in durable order, while
the consumed event itself cannot be delivered twice.

**Committed unimplemented functionality.** No present surface cancels program
runs. The cancel command, its receipt, and their closed reply algebra are owned
by the wire-owning contract, [process protocol](process-protocol.md), with the
same durable-command identity mechanics as every user command in
[identity and commands](identity-and-commands.md); this page owns only the
run-state semantics. Those semantics: a cancel never overwrites a terminal
outcome — the race against a run's own `terminal` is resolved by whichever
committed first, and the receipt reports the state it found (`not_found` and
`already_terminal` are outcomes, not errors). Cancel authority is user
authority; an applied cancel is journaled as a `run_cancel` delivery carrying
the command identity and no request ordinal, so a cancelled run replays to its
cancellation independently of how many requests were outstanding. Any
request-scoped `cancel` deliveries needed to reconcile outstanding operations
are committed before the `run_cancel`; they retain their distinct
ordinal-bearing shape and do not terminalize the run. Programs receive no notice
beyond the journal: cancellation is terminal, not advisory.

## Driving sessions

**Committed unimplemented functionality.** No present surface lets programs
create sessions. The session capability composes the existing create-session,
input-submission, and turn-scheduling services, extended where the present
contracts do not provide for program-issued actions. Two extensions are
committed in the pages that own them. First, attribution: the committed
program-issuance extension to the closed actor algebra is recorded in its owning
contract, [identity and commands](identity-and-commands.md); this page adds only
the program-specific constraint that program-issued input names the issuing run
identity and is never recorded as user-issued. Second, provenance: the committed
`workflow` and `eval` creation-cause extensions are recorded in their owning
contract, [sessions and the transcript](sessions-and-transcript.md); this page
adds only the constraint that every program-created session carries one and
joins back to its creating run. Programs drive sessions turn by turn: submit
input, await that turn's outcome as a typed payload, then branch. The
declared-output-schema carriage that typed turn outcomes require — recorded on
the program-issued input, flowing to the prepared model operation, enforced at
the runtime boundary — is committed in its owning contract,
[model-call execution](model-call-execution.md); this page adds only the
constraint that a program's turn awaits that validated payload or the turn's
failure, nothing looser. Structure inside one turn is deliberately out of
contract: within a turn the model acts autonomously, governed by the same
approval judge as every session, and a program that needs intra-turn evidence
reads the durable transcript through a read capability after the fact.
Credentials never enter the isolate; sessions, model calls, clones, and stage
executions all happen host-side under existing credential machinery.

## Open edges

- Remote and out-of-process program hosts:
  [open-questions](../open-questions.md#program-substrate-and-evaluations).

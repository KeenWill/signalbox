# Workflows

The workflows layer runs JavaScript in a closed isolate and compiled trusted
Rust programs through one journal, so a run can be re-executed to the same
point.

## Overview

The workflow-runtime crate supplies the host and the persistence crate supplies
the frame journal. The host, `WorkflowHost`, runs one stripped JavaScript module
per execution attempt in a fresh embedded `deno_core` isolate. The module's only
admitted import is the canonical SDK specifier, which the loader resolves to a
host-supplied synthetic module. The isolate exposes no filesystem, network,
environment, module source, wall clock, or unvirtualized randomness. Its only
asynchronous operation is the closed request op behind that SDK module. The SDK
exports generic capability effects and four primitives: the virtual clock, a
randomness draw, a sleep, and an event wait. Each takes exact bytes and produces
one typed request at the Rust boundary.

After module evaluation, the JavaScript host invokes a default export as a
function without a receiver, passing the run's retained input as a `Uint8Array`.
It awaits the returned `Uint8Array` through the same request and replay loop and
records those exact bytes as the successful result. A module without a default
export completes with empty result bytes. Invalid entrypoints and nonbyte
results fail the execution attempt without recording success.

The journal is one append-only sequence of frames per program-run identity, held
by `ProgramJournalRepository`. A frame is a request (what the program asked) or
a delivery (what the host answered). Requests are journaled in program order,
deliveries in delivery order, and every row carries one contiguous global
position, so their interleaving is retained. The request, delivery, and fault
vocabularies are closed; the domain crate's `RequestKind`, `DeliveryKind`, and
`FaultCause` and the migration's check constraints fix their members. Primitive
requests, effects, capability refusals, nondeterminism faults, and user
cancellation and successful completion are produced.

Resume discards nothing and restores nothing. A retained cancellation, fault or
accepted Terminal/Answer pair names the run's outcome; the host returns it
without loading executable code or creating an isolate. Any other woken run
re-executes its module from the start; `ReplayCursor` answers each request from
the journal in delivery order, and execution goes live where the journal ends.
Live primitive requests are answered through `LiveDeliverySource`; granted
effects reach host-side `EffectExecutor` implementations.

`ProgramRegistrationRepository` stores immutable registrations keyed by name and
revision. Its executable is either JavaScript with SHA-256 digests of exact
source and stripped artifact bytes, or a compiled native catalog entry/revision
and SHA-256 digest of the exact executing binary. JavaScript registration
requires source bytes and computes their digest; native registration accepts
only `NativeProgramRegistrationRequest`. Each registration carries explicit
grants from `ProgramCapability`; identical bytes under distinct names or grant
lists remain distinct programs. A run pins its registration and immutable exact
input bytes; the registration records its executable and grants.
Program-initiated registration requires `register` and admits only a subset of
the registrant's grants; user registration may widen grants under a new key. The
host resolves the executable bound to the run, and session capability issuance
requires a registered session grant. Registration and run creation take
caller-supplied identities: an equal retry returns the recorded value; different
content or a different registration binding or input conflicts.

`WorkflowHost::execute_registered` loads the pinned executable and grants;
ungranted requests receive a journaled refusal before any executor acts.
Recovery first adopts a proven durable outcome, reissues only operations
declared idempotent, and otherwise journals an ambiguous answer. The register
executor checks child-grant attenuation and adopts a matching immutable
registration after a lost answer. A recovered request that conflicts at either
the registration identity or name/revision records the same `ProgramError` as a
live conflict. If neither key is registered, recovery journals an ambiguous
answer.

`SessionEffects` composes host-side session creation and input services.
Unsupported operations and invalid session requests receive a journaled refusal.
A session grant can create a workflow session or submit one ordinary turn and
await its terminal outcome. Recovery adopts the exact durable command receipt;
reissuing either operation uses that same command identity. Turn answers carry
the session, turn, accepted-input identity, terminal disposition, and a SHA-256
digest of that metadata and the terminal frontier when present, without
transcript bytes. The host retains credentials and signals ordinary session
eligibility; the approval judge governs the turn, with no program hook inside
it. Program cancellation wakes a waiting session operation and retains the run's
terminal outcome.

A journaled `run_cancel` delivery is terminal: the host returns the cancelled
outcome and creates no isolate.

Cancel authority is user authority. Cancel is a command with ordinary durable
command identity ([identity and commands](../spec/identity-and-commands.md)),
and its wire message pair belongs to
[process protocol](../spec/process-protocol.md). An applied cancel is journaled
as one `run_cancel` delivery that carries the command identity and no request
ordinal, so a cancelled run replays to its cancellation however many requests
were outstanding.

Successful completion records result bytes in a `Terminal` request and an
immediately following `Answer` acknowledgement. A terminal request emitted with
outstanding work receives `RejectReason::OutstandingRequests`; it cannot be
accepted after that work drains. Only those deliveries may resolve a terminal
request. No frame may follow accepted success. JavaScript modules without a
default export return empty bytes after their requests drain.
`ProgramJournal::result` reads retained result bytes without execution.

Cancellation addresses retained journal streams. A cancel serialized before
completion prevents success; after accepted success it records
`already_terminal` with state `succeeded` and the retained result bytes.
Cancelled and faulted receipts carry a null result. Equal cancellation retries
replay their recorded outcome without appending frames.

## Durable primitives

`DurablePrimitives` answers empty `Now` and `Random` requests with Unix
milliseconds and a uniformly sampled operating-system u64 respectively. Values
use decimal strings in checked JSON payloads; recorded answers replay without
reading the clock or drawing randomness. `Sleep` carries an absolute
`deadline_unix_ms` at request admission and receives `Wake` with that deadline
once wall time reaches it, including after restart. Sleep-only waits use timers
without opening journal listeners.

`AwaitEvent` carries a typed `program_answers` source run and an exclusive
journal position (`after`, zero for the beginning). It receives the next
retained `Answer` delivery's exact payload bytes and position. The outstanding
request's run and ordinal identify each wait. Source reads occur before
listening, after subscription and after every wake; PostgreSQL answer
notifications are hints filtered to the requested source runs. The SDK's typed
`primitives` wrappers preserve full-width values as decimal strings.

## Daemon runner

The fenced daemon owns one local workflow runner. User-authorized registration
and start admission retain executable identity, grants and exact input before
waking it. Startup resumes registered runs without a terminal outcome, and an
active run has one attempt at a time. At a quiescent sleep or event wait, the
host drops the JavaScript isolate or native root future; the daemon retains only
the outstanding requests until a delivery is recorded, then reconstructs the run
by replay. Cancellation wakes and drops blocked host operations. Shutdown
interrupts non-yielding JavaScript and drains attempts; restart replays the
retained requests and deliveries. JavaScript loading or execution errors,
stalled programs, child registration conflicts and unavailable granted effects
record a per-run `ProgramError` fault; other runs continue and restart retains
that outcome. Recovery retries an unanswered unavailable effect into the same
fault. A concurrent terminal outcome is preserved.

The Linux compiled catalog includes `clock` revision `1`: its input is a
big-endian u64, and its result concatenates that input and a journaled
big-endian u64 Unix time in seconds. The runner resolves admitted JavaScript
artifacts from their registrations without requiring a native catalog. Empty
`Now` requests receive the SDK's typed Unix-millisecond answer. Registration
effects and durable primitives are composed; other effects are not composed. The
process protocol and CLI expose registration, start, read and cancellation.
`program register REGISTRATION_ID REGISTRATION_JSON` reads a registration
description; `program start RUN_ID REGISTRATION_ID --input FILE` admits the
program codec's exact input bytes. `program read RUN_ID` prints retained input,
state and result as JSON after the run identity, with typed byte-extent markers
when the process frame budget truncates input or result;
`program cancel RUN_ID --command-id COMMAND_ID` preserves durable cancellation
identity. Successful cancellation receipts carry frame-bounded result prefixes
with typed byte-extent markers; stored results remain complete. Native and
JavaScript runtime input decoders check the encoded input before program code
runs.

## Native programs

`NativeCatalog` selects compiled `NativeProgram` implementations with checked
`NativeValue` input decoding before program code and checked output encoding. An
entry/revision binds to one program type for the process lifetime; another
catalog may select that type but cannot bind the same key to a different type.
The Linux host hashes its running image through `/proc/self/exe` once,
preserving its digest across executable path replacement. Catalog construction
returns `Unsupported` on other platforms. An unfinished run whose catalog entry,
revision or binary digest is unavailable journals `ContractRetired`; retained
terminal outcomes require no catalog resolution. JavaScript child registration
admits only JavaScript artifacts.

One locally polled Rust root future sequentially awaits `WorkflowContext`
requests. Both adapters share request admission, grant checks, exact-byte
replay, delivery persistence and retained results. Native program and codec
errors journal `ProgramError`. The context exposes only the existing primitives
and effects; direct I/O, ambient nondeterminism, task spawning and concurrent
awaits are forbidden by the trusted authoring contract, enforced through code
review and replay tests rather than a sandbox.

## Design decisions

The engine is pinned by exact crate version and upgraded deliberately. Why: the
standalone `deno_core` repository is archived and the deno monorepo is its
source of truth, so a crate version is the only stable pin.

The typed journal and the database carry every frame discriminator, including
those nothing produces yet; producing a frame stays with the registration,
executor, or capability slice that can enforce its transition.

Concurrent outstanding requests are permitted, and the host drains the isolate's
microtasks after each delivery before it selects the next. Why: a delivery can
unblock a further request, so recorded delivery order with that drain makes
promise interleaving identical in live execution and replay without restricting
the language.

No checkpointing or journal truncation exists, because a journal that can be
rewritten is not a journal; the migration's triggers reject deletion, update,
and truncation of journal rows.

## Boundary contracts

`RepoWatchEffects` admits `repo.nextRuleEvent`, `repo.commitEvaluation` and
`repo.submitPending` under the `repo-watch` grant. Programs receive checked
rule/event context and propose ordered template actions without database
handles. [Repository watch](repo-watch.md) owns revalidation and effect
receipts; the adapter verifies a matching durable journal request and answer
before releasing a receipt, including delivery in a successor run.

The canonical SDK specifier is `@signalbox/program-sdk/v<version>`, where the
version is a positive decimal integer with no leading zero. Frame-contract
release one admits exactly `@signalbox/program-sdk/v1`. The module loader in the
workflow-runtime crate resolves that specifier alone and rejects every other
import, including relative files and the unversioned name.

The SDK's `defineProgram` decodes input bytes before calling the program body
and encodes its result through explicit runtime codecs. `jsonCodec` declarations
reject statically non-JSON values; decoding and encoding also check values at
runtime. Both pass a frozen snapshot of own data properties to the validator.
Encoding serializes a frozen snapshot of validated own data properties; SDK
payload intrinsics are captured before program evaluation, and
`Function.prototype.call` is immutable for native request dispatch. Typed
`register`, `session.create`, and `session.turn` wrappers validate method inputs
and answer records; refusals and cancellation remain typed deliveries. Session
defaults versions are decimal strings in TypeScript and encode as exact JSON
integers without conversion through `Number`.

[`tooling/programs`](../../tooling/programs/package.json) owns SDK declarations
and strict TypeScript examples. `pnpm --dir tooling/programs run check` checks
their types; `pnpm --dir tooling/programs run build` emits each example as one
JavaScript module. `pnpm --dir tooling/programs run check:fixtures` builds into
a clean temporary directory and compares the complete emitted file inventory and
contents with the isolate fixtures. CI checks types and fixture drift and runs
the fixture checker's regression tests.

Every nondeterministic act a program performs crosses the typed frame protocol
and is recorded as an immutable journal row. No capability answers a program
outside the journal. No single enforcer spans every capability; the frame types
in `crates/domain/src/program_journal.rs` are the protocol.

Every request carries a per-run monotone request ordinal, and every delivery
that resolves a request names that ordinal. Delivery order fixes the
interleaving and the named ordinal fixes which promise resolves, identically in
live execution and replay. `DeliveryKind::resolves` carries that ordinal.

[Repository watch](repo-watch.md) owns its durable cursor and event rows inside
the module boundary. No out-of-module matcher reads those tables.

A malformed journal row is typed corruption under the fail-closed reconstitution
contract in [persistence protocol](persistence-protocol.md).

The host session capability verifies a retained run before fixing its input
actor. The application submit boundary accepts that attribution without granting
authority.

## Planned

- Payload offload to SHA-256 blobs under the `program_journal` storage class;
  every payload is inline today ([design](../design/workflows.md)).

# Workflows design

Workflows is the daemon layer for typed Rust and TypeScript programs. This
committed unbuilt design extends [workflows](../spec/workflows.md); program
definitions, program-run identities and `@signalbox/program-sdk/v1` keep their
names.

## Trust model

Native Rust programs are compiled-in trusted daemon code. Program logic obtains
time, randomness, identities and external effects only through
`WorkflowContext`; direct I/O, task spawning and ambient nondeterminism are
forbidden by the authoring contract, enforced by code review and replay tests,
not a sandbox. The context exposes no database, provider, filesystem, credential
or Tokio handle.

The host locally polls one Rust root future with sequential context awaits,
polling it to its next request or completion after each delivery. JavaScript
retains the closed isolate and concurrent-promise semantics of `WorkflowHost` in
[`crates/workflow-runtime/src/lib.rs`](../../crates/workflow-runtime/src/lib.rs).
Both drivers share request admission, grants, exact-byte replay and delivery
persistence; effects use `EffectExecutor` recovery in
[`crates/workflow-runtime/src/effects.rs`](../../crates/workflow-runtime/src/effects.rs).
Requests commit before effects and deliveries commit before code resumes.

## Identities

`ProgramRegistration` in
[`crates/domain/src/program_registration.rs`](../../crates/domain/src/program_registration.rs)
retains its caller-supplied identity, immutable name/revision key and grants.
Its executable variant is either JavaScript artifact bytes with exact source and
artifact SHA-256 digests, or a compiled Rust catalog entry/revision with the
SHA-256 digest of the exact executing daemon binary. The host computes that
binary digest once; it is not an embedded self-hash. The variant belongs to the
registration record, without a second registry table.

Native registration selects compiled catalog code; JavaScript child registration
cannot upload native code or mint catalog entries. Program-initiated
registration requires the register grant and attenuates child grants. Equal
registration retries return the retained registration; changed content
conflicts.

Each run pins its registration and native executable identity. A retained
terminal outcome is readable without resolving executable code; an unfinished
native run whose pinned binary is unavailable journals `ContractRetired`. It
never replays through a changed binary. There is no native loading or
multi-version runtime registry.

## Input and result

User-authorized register, start and read commands use the
[process protocol](../spec/process-protocol.md) and its durable command
identity. Start retains immutable exact input bytes with the run's registration
binding; an equal retry requires the same run identity, registration and input.
Read returns retained run state and terminal result bytes.

Rust's `NativeProgram` has associated `Input` and `Output` types with checked
encoding and decoding and an async `run(context, input)` entry. The catalog
erases concrete types only after admission, and input decoding precedes program
code. TypeScript's `defineProgram` takes a runtime input decoder and result
encoder; strict `tsc` checking and type stripping happen before registration.
The emitted artifact is one module importing only `@signalbox/program-sdk/v1`.

Effect method input/output records have checked Rust and TypeScript codecs;
full-width integer identities use decimal strings and exact payloads use byte
arrays. Domain values, wire payloads and storage rows remain distinct.

## Completion

Successful return encodes the result in `RequestKind::Terminal`; a resolving
`DeliveryKind::Answer` acknowledges it after other requests drain. The accepted
pair is durable success, with result bytes in the terminal request. An explicit
terminal request while other work is outstanding receives
`RejectReason::OutstandingRequests`.

`ProgramJournal` and its terminal lookup in
[`crates/domain/src/program_journal.rs`](../../crates/domain/src/program_journal.rs),
reconstitution in
[`crates/persistence/src/program_journal.rs`](../../crates/persistence/src/program_journal.rs),
SQL enforcement, host outcome lookup and cancellation receipts recognize that
pair without re-executing the program. No new frame kind or separate success
table is required. Runtime exceptions and native program errors journal
`ProgramError`. Operator cancellation retains user authority, wakes waiting
operations and wins a terminal race.

## Waits

A sleep or daemon event wait is identified by its outstanding request's run
identity and ordinal. Sleep admission persists its deadline; replay and restart
reuse it. `AwaitEvent` names a typed source and durable position; delivery reads
retained source events before listening and rechecks after wake. In-memory
notifications are hints. Primitive deliveries use `LiveDeliverySource` in
[`crates/workflow-runtime/src/lib.rs`](../../crates/workflow-runtime/src/lib.rs)
and the shared journal.

The daemon retains incomplete run admission, serializes attempts for each run
and resumes from the pinned registration, immutable input and journal after
restart. At a quiescent external wait it drops program memory and reconstructs
execution on wake, allowing other runs to proceed. Waiting effects observe run
cancellation and daemon shutdown. There is no standing-subscription aggregate,
scheduler policy, workflow graph language, checkpoint or concurrency helper.

## Offload

A frame payload below a fixed inline threshold stays inline in the journal row.
A larger payload, up to the configured blob maximum, becomes an immutable
SHA-256-addressed blob, and the row references it by digest only. The blob is
routed under the daemon-derived `program_journal` storage class that
[blob storage](../spec/blob-storage.md) reserves, never under an
operation-selected class.

`InlineFramePayload` holds exact bytes and never a digest. The persistence row
records whether a payload is inline or a blob, and the repository hydrates blob
bytes before it reconstitutes the frame, so replay compares identical bytes
however the row was stored. Offload preserves frame kinds and existing inline
rows; each larger payload is stored once and loaded as exact bytes. Journals are
not truncated.

## Repository watch

[Repository watch design](repo-watch.md).

# Workflows

The workflows layer runs a JavaScript program in a closed isolate and journals
every nondeterministic act, so a run can be re-executed to the same point.

## Overview

Two parts are built: an isolate host in the workflow-runtime crate and a frame
journal in the persistence crate. The host, `WorkflowHost`, runs one stripped
JavaScript module per execution attempt in a fresh embedded `deno_core` isolate.
The module's only admitted import is the canonical SDK specifier, which the
loader resolves to a host-supplied synthetic module. The isolate exposes no
filesystem, network, environment, module source, wall clock, or unvirtualized
randomness. Its only asynchronous operation is the closed request op behind that
SDK module. The SDK exports generic capability effects and four primitives: the
virtual clock, a randomness draw, a sleep, and an event wait. Each takes exact
bytes and produces one typed request at the Rust boundary.

The journal is one append-only sequence of frames per program-run identity, held
by `ProgramJournalRepository`. A frame is a request (what the program asked) or
a delivery (what the host answered). Requests are journaled in program order,
deliveries in delivery order, and every row carries one contiguous global
position, so their interleaving is retained. The request, delivery, and fault
vocabularies are closed; the domain crate's `RequestKind`, `DeliveryKind`, and
`FaultCause` and the migration's check constraints fix their members. Primitive
requests, effects, capability refusals, nondeterminism faults, and user
cancellation are produced.

Resume discards nothing and restores nothing. A registered run's journal that
already holds a terminal delivery, one that ended the run instead of answering a
request, names the run's outcome; the host returns that outcome and creates no
isolate. Any other woken run re-executes its module from the start;
`ReplayCursor` answers each request from the journal in delivery order, and
execution goes live where the journal ends. Live primitive requests are answered
through `LiveDeliverySource`; granted effects reach host-side `EffectExecutor`
implementations.

`ProgramRegistrationRepository` stores immutable registrations keyed by name and
revision, recording SHA-256 digests of exact source and stripped artifact bytes.
Each registration carries explicit grants from `ProgramCapability`; identical
bytes under distinct names or grant lists remain distinct programs. A run pins
its registration, whose row records its artifact and grants. Program-initiated
registration requires `register` and admits only a subset of the registrant's
grants; user registration may widen grants under a new key. The host loads the
artifact bound to the run, and session capability issuance requires a registered
session grant. Registration and run creation take caller-supplied identities: an
equal retry returns the recorded value; different content or a different
registration binding conflicts.

`WorkflowHost::execute_registered` loads the pinned artifact and grants;
ungranted requests receive a journaled refusal before any executor acts.
Recovery first adopts a proven durable outcome, reissues only operations
declared idempotent, and otherwise journals an ambiguous answer. The register
executor checks child-grant attenuation and adopts a matching immutable
registration after a lost answer.

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

Cancellation addresses retained journal streams; their terminal states are
`cancelled` and `faulted`, and the receipt carries a null result because no
durable program return value is recorded.

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

The canonical SDK specifier is `@signalbox/program-sdk/v<version>`, where the
version is a positive decimal integer with no leading zero. Frame-contract
release one admits exactly `@signalbox/program-sdk/v1`. The module loader in the
workflow-runtime crate resolves that specifier alone and rejects every other
import, including relative files and the unversioned name.

The SDK's `defineProgram` decodes input bytes before calling the program body
and encodes its result through explicit runtime codecs. `jsonCodec` checks JSON
values during decoding and encoding. Decoding passes a frozen snapshot of own
data properties to the validator. Encoding serializes a frozen snapshot of
validated own data properties; SDK payload intrinsics are captured before
program evaluation. Typed `register`, `session.create`, and `session.turn`
wrappers validate method inputs and answer records; refusals and cancellation
remain typed deliveries. Session defaults versions are decimal strings in
TypeScript and encode as exact JSON integers without conversion through
`Number`.

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

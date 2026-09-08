# Program substrate

The program substrate runs a JavaScript program in a closed isolate and journals
every nondeterministic act, so a run can be re-executed to the same point.

## Overview

Two parts are built: an isolate host in the program-runtime crate and a frame
journal in the persistence crate. The host, `ProgramHost`, runs one stripped
JavaScript module per execution attempt in a fresh embedded `deno_core` isolate.
The module's only admitted import is the canonical SDK specifier, which the
loader resolves to a host-supplied synthetic module. The isolate exposes no
filesystem, network, environment, module source, wall clock, or unvirtualized
randomness. Its only asynchronous operation is the closed request op behind that
SDK module. The SDK exports four primitives: the virtual clock, a randomness
draw, a sleep, and an event wait. Each takes exact bytes and produces one typed
request at the Rust boundary.

The journal is one append-only sequence of frames per program-run identity, held
by `ProgramJournalRepository`. A frame is a request (what the program asked) or
a delivery (what the host answered). Requests are journaled in program order,
deliveries in delivery order, and every row carries one contiguous global
position, so their interleaving is retained. The request, delivery, and fault
vocabularies are closed; the domain crate's `RequestKind`, `DeliveryKind`, and
`FaultCause` and the migration's check constraints fix their members. The four
primitive answerable requests, nondeterminism faults, and user cancellation are
produced; no executor applies effects, scope cancellation, terminal admission,
or capability rejection.

Resume discards nothing and restores nothing. A journal that already holds a
terminal delivery, one that ended the run instead of answering a request, names
the run's outcome; the host returns that outcome and creates no isolate. Any
other woken run re-executes its module from the start; `ReplayCursor` answers
each request from the journal in delivery order, and execution goes live where
the journal ends. Live requests are answered through the `LiveDeliverySource`
seam, which receives only the outstanding durable request frames; that seam is
the boundary later capability executors implement.

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
program-runtime crate resolves that specifier alone and rejects every other
import, including relative files and the unversioned name.

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

- Capability refusal before host authority is exercised
  ([design](../design/program-substrate.md)).
- Recovery of external effects by adopted outcome, idempotent re-issue, or
  journaled ambiguous answer; no executor applies effects
  ([design](../design/program-substrate.md)).
- Payload offload to SHA-256 blobs under the `program_journal` storage class;
  every payload is inline today ([design](../design/program-substrate.md)).
- Session outcome frames carrying session, turn, and input identities and an
  outcome digest; no session outcome executor exists
  ([design](../design/program-substrate.md)).
- Turn-by-turn session driving with no contract inside a turn; no session
  outcome executor exists ([design](../design/program-substrate.md)).
- Host-side execution of every credentialed operation, so credentials never
  enter the isolate; the closed isolate is built and its executors are not
  ([design](../design/program-substrate.md)).

# Program substrate

The program substrate runs a JavaScript program in a closed isolate and journals
every nondeterministic act, so a run can be re-executed to the same point.

## Map

Two parts are built: an isolate host in the program-runtime crate and a frame
journal in the persistence crate. The host, `ProgramHost`, runs one
already-stripped JavaScript module per execution attempt in a fresh embedded
`deno_core` isolate, pinned exactly by crate version; no isolate survives
between attempts. The module's only admitted import is the canonical SDK
specifier, which the loader resolves to a host-supplied synthetic module. The
isolate exposes no filesystem, network, environment, module source, wall clock,
or unvirtualized randomness. Its only asynchronous operation is the closed
request op behind that SDK module. The SDK exports four primitives: the virtual
clock, a randomness draw, a sleep, and an event wait. Each takes exact bytes and
produces one typed request at the Rust boundary.

The journal is one append-only sequence of frames per program-run identity, held
by `ProgramJournalRepository`. A frame is a request, what the program asked, or
a delivery, what the host answered. Requests are journaled in program order and
deliveries in delivery order, and every row carries one contiguous global
position, so the interleaving of the two is retained. The request, delivery, and
fault vocabularies are closed; the domain crate's `RequestKind`, `DeliveryKind`,
and `FaultCause` and the migration's check constraints fix their members. Only
the four primitive answerable requests are emitted today. No executor applies
effects, scope cancellation, terminal admission, capability rejection, or run
terminalization, and only the nondeterminism fault is produced.

Resume discards nothing and restores nothing. A woken run re-executes its module
from the start; `ReplayCursor` answers each request from the journal in delivery
order, and execution goes live exactly where the journal ends. Live requests are
answered through the `LiveDeliverySource` seam, which receives only the
currently outstanding durable request frames; that seam is the boundary later
capability executors implement.

The journal's stream row pins only frame-contract version one. It is not a run
aggregate: no row records a program's registration, grants, or budgets. The
capability vocabulary is closed and fixed by `ProgramCapability` and the
migration, and no code grants or exercises a capability. Registration,
capability executors, event subscriptions, cancellation, and session driving
have no present code; the Not built section lists the committed items.

## Decisions

The engine is pinned by exact crate version and upgraded deliberately. Why: the
standalone `deno_core` repository is archived and the deno monorepo is its
source of truth, so a crate version is the only stable pin.

The typed journal and the database carry every frame discriminator, including
those nothing produces yet; producing a frame stays with the registration,
executor, or capability slice that can enforce its transition.

Concurrent outstanding requests are permitted. Recording the delivery order is
the one discipline that makes promise interleaving identical in live execution
and replay without restricting the language.

No checkpointing or journal truncation exists, because a journal that can be
rewritten is not a journal; the migration's triggers reject deletion, update,
and truncation of journal rows.

## Contracts

The canonical SDK specifier is `@signalbox/program-sdk/v<version>`, where the
version is a positive decimal integer with no leading zero. Frame-contract
release one admits exactly `@signalbox/program-sdk/v1`. The module loader in the
program-runtime crate resolves that specifier alone and rejects every other
import, relative files and the unversioned name included.

Every nondeterministic act a program performs crosses the typed frame protocol
and is recorded as an immutable journal row. No capability answers a program
outside the journal. No single enforcer spans every capability; the frame types
in `crates/domain/src/program_journal.rs` are the protocol.

Every request carries a per-run monotone request ordinal, and every delivery
that resolves a request names that ordinal. Delivery order fixes the
interleaving and the named ordinal fixes which promise resolves, identically in
live execution and replay. `DeliveryKind::resolves` carries the name.

Repository watch's durable cursor and event rows are this substrate's event
source. They stay readable to a matcher outside
[repository watch](repo-watch.md), which owns them.

A malformed journal row is typed corruption under the fail-closed reconstitution
contract in [persistence protocol](persistence-protocol.md).

## Not built

- Program registration under an identity of name, revision, and content digests;
  no present surface registers a program
  ([design](../design/program-substrate.md)).
- A run's authority resolved from its recorded registration; no present row
  binds a run to a registration ([design](../design/program-substrate.md)).
- Capability grants, with an ungranted capability refused before any authority
  is exercised; no present code grants or refuses a capability
  ([design](../design/program-substrate.md)).
- Attenuation of the register grant along program-initiated registrations; no
  program registers programs today ([design](../design/program-substrate.md)).
- Recovery of external effects by adopted outcome, idempotent re-issue, or
  journaled ambiguous answer; no executor applies effects
  ([design](../design/program-substrate.md)).
- Payload offload to SHA-256 blobs under the `program_journal` storage class;
  every payload is inline today ([design](../design/program-substrate.md)).
- Session outcome frames carrying session, turn, and input identities and an
  outcome digest; no session capability exists
  ([design](../design/program-substrate.md)).
- Run cancellation as a user command journaled as a `run_cancel` delivery
  carrying the command identity; no present surface cancels a run
  ([design](../design/program-substrate.md)).
- Turn-by-turn session driving with no contract inside a turn; no session
  capability exists ([design](../design/program-substrate.md)).
- Host-side execution of every credentialed operation, so credentials never
  enter the isolate; the closed isolate is built and its executors are not
  ([design](../design/program-substrate.md)).

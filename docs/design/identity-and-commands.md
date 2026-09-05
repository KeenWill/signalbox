# Identity and commands design

This design is not built; it extends
[identity-and-commands](../spec/identity-and-commands.md).

## Goal

Build the items the identity and command subsystem has committed to but lacks:
registry kinds for the runner recovery commands, production generators for the
four identity types that lack one, and the optional runner placement the two
creation payloads lack. `Actor` gains a program arm that submit-input records,
and create-session adoption stays an explicit maintainer choice, so a
program-driven turn is never recorded as user-issued.

## Design

Three runner recovery command kinds join the registry and its closed kind
constraint: replace a lost runner, abandon a lost runner, and promote a pending
runner. Each has one typed record family keyed by command identifier and follows
the claim protocol on the spec page. Their semantics belong to
[runner-protocol](../spec/runner-protocol.md).

Replacement is the one command in this set that can span more than one
transaction, and only when it provisions a workspace. A replacement that needs
no provisioning, such as one recovering a runner lost before its placement was
pinned, claims and terminates in a single transaction. A provisioning
replacement's first transaction claims the registry identity, stores the
complete immutable request row, and stores a single-use provisioning
authorization; the request row alone satisfies typed-record completeness while
provisioning crosses the runner boundary. The handler then waits without holding
a database transaction while the pending runner returns or replays its workspace
receipt. The terminal transaction appends exactly one result row and installs
the replacement or its typed rejection; no success or rejection response exists
before that row commits. Equal replay during provisioning joins the same durable
operation and can neither start another workspace nor acquire another meaning.
Startup resumes an unterminated request before it admits clients.

Abandonment is one ordinary claim-and-terminal-result transaction. Promotion is
the one command in the set whose payload names no session, because the fact it
acts on is that this daemon's active runner is durably gone. It carries the
command identifier and the pending enrollment request it promotes, in one
claim-and-terminal-result transaction.

`ProviderTargetEvidenceId` gains a UUIDv7 generator with the durable
provider-target evidence that
[model-call-execution](../spec/model-call-execution.md) defers; no slice writes
that evidence today. `WorkspaceId`, `GitRemoteMintId`, and
`GitRemoteWithdrawalId` gain write paths and generators in the workspace store
and its operator verbs; their registry kinds and tables already exist. Each
generator mints immediately before the domain transition that creates the fact,
as the spec page requires of every generator.

The optional runner placement enters the imported-creation and create-session
payloads at a new storage version above each kind's current maximum, and every
later version carries it. A row carrying a placement is written only from that
version on, so a reader that predates the field rejects the row instead of
reconstructing a placement-less payload. A row at an earlier version
reconstitutes with no placement. The placement is a caller-supplied semantic
field, so it participates in replay equality in both creation modes, including
template-derived creation. A replay carrying a different placement, or a
placement where the first handling had none, is conflicting reuse.

`Actor` gains a program arm: a verified reference to the issuing program run,
constructible only by the program substrate's host-side session capability, with
the same validated-reference and no-conferred-authority semantics as every other
arm. Submit-input gains a program admissibility path that fixes that actor.

Repository watch and commissioned dispatch create no program run, so the program
arm does not cover their initial inputs. Whether they gain a module arm naming
the dispatch is pending an owner decision; until it is made those inputs keep
the user attribution the spec page documents.

The program arm follows the existing storage convention: a closed `actor_kind`
spelling, a variant-shaped reference column under a check constraint, and
inclusion in replay equality and hashing. It enters the submit-input record at
storage version 4, so its spelling and reference column are written only from
that version on. A version-3 row does not carry it and reconstitutes under the
actor its stored kind names; a version-3 row that carries it is corruption. The
reader accepts both versions.

Create-session actor adoption is a maintainer choice made explicitly; every
existing version carries no actor and reconstitutes without one, and a version
that adds the field states how each earlier version reconstitutes.

## Compatibility constraints

- The actor storage convention stays extensible to a program arm and a module
  arm, and nothing assumes the submit-input actor is always the user.
- Replace-session-defaults gains no actor field until a non-user boundary issues
  it.
- Imported-creation version 4 and create-session version 5 stay unwritten; no
  writer uses either number and the decoders keep rejecting them.
- Only a replacement that provisions a workspace spans more than one
  transaction; every other new kind, and every replacement that needs no
  provisioning, is one claim-and-terminal-result transaction.

## Acceptance criteria

- Each new command kind has a registry kind, a typed record family under the
  deferred typed-record trigger and the append-only trigger, a hand-written
  structural equality that excludes the command identifier, and a closed-kind
  test that names it.
- An equal replay of a runner replacement during provisioning returns the
  pending disposition; after the result row commits it returns that result.
- A runner replacement claimed but unterminated at process exit is resumed at
  startup before clients are admitted, and terminates under its own identifier.
- The workspace store and its operator verbs write `WorkspaceId`,
  `GitRemoteMintId`, and `GitRemoteWithdrawalId` rows from production
  generators, and no Postgres column gained an identity-generating default.
- `ProviderTargetEvidenceId` has a production generator once durable
  provider-target evidence lands.
- The new imported-creation and create-session versions decode a placement, and
  versions 4 and 5 stay unsupported.
- Every version from the new one on carries the placement, rows at earlier
  versions reconstitute it absent, and replay equality compares it.
- A program-issued submit-input records the program actor, and replaying its
  identifier under the user actor is conflicting reuse.
- Submit-input writes the program actor only at storage version 4, and version-3
  rows reconstitute without it.
- No telemetry site emits a command identifier after the change.

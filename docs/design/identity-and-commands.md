# Identity and commands design

This design is not built; it extends
[identity-and-commands](../spec/identity-and-commands.md).

## Goal

Build the items the identity and command subsystem has committed to but lacks:
registry kinds for the runner recovery commands, production generators for three
identity types that have storage but no writer, and the two reserved storage
versions that carry runner placement. Actor attribution extends to every command
family that records a caller's intent, including a program arm, so a
program-driven turn is never recorded as user-issued.

## Shape

Three runner recovery command kinds join the registry and its closed kind
constraint: replace a lost runner, abandon a lost runner, and promote a pending
runner. Each has one typed record family keyed by command identifier and follows
the claim protocol on the spec page. Their semantics belong to
[runner-protocol](../spec/runner-protocol.md).

Runner replacement is the one multi-transaction command in this set. Its first
transaction claims the registry identity, stores the complete immutable request
row, and stores a single-use provisioning authorization; the request row alone
satisfies typed-record completeness while provisioning crosses the runner
boundary. The handler then waits without holding a database transaction while
the pending runner returns or replays its workspace receipt. The terminal
transaction appends exactly one result row and installs the replacement or its
typed rejection; no success or rejection response exists before that row
commits. Equal replay during provisioning joins the same durable operation and
can neither start another workspace nor acquire another meaning. Startup resumes
an unterminated request before it admits clients.

Abandonment is one ordinary claim-and-terminal-result transaction. Promotion is
the one command in the set whose payload names no session, because the fact it
acts on is that this daemon's active runner is durably gone. It carries the
command identifier and the pending enrollment request it promotes, in one
claim-and-terminal-result transaction.

`ProviderTargetEvidenceId` gains a UUIDv7 generator in the slice that writes
provider target evidence. `WorkspaceId`, `GitRemoteMintId`, and
`GitRemoteWithdrawalId` gain generators in the workspace store and its operator
verbs; their registry kinds and tables already exist. Each generator mints
immediately before the domain transition that creates the fact, as the spec page
requires of every generator.

Imported-creation storage version 4 and create-session storage version 5 carry
an optional runner placement in the command payload. The placement is a
caller-supplied semantic field, so it participates in replay equality in both
creation modes, including template-derived creation. A replay carrying a
different placement, or a placement where the first handling had none, is
conflicting reuse. Each version's decoder accepts the payload and the supported
version set for its kind becomes contiguous.

`Actor` gains a program arm: a verified reference to the issuing program run,
constructible only by the program substrate's host-side session capability, with
the same validated-reference and no-conferred-authority semantics as every other
arm. Submit-input gains a program admissibility path that fixes that actor.
Storage follows the existing convention: a new closed `actor_kind` spelling, a
variant-shaped reference column under a check constraint, and inclusion in
replay equality and hashing.

Replace-session-defaults gains an actor field through a new kind-scoped storage
version. Every earlier version reconstitutes with the user actor, which is
truthful because its only constructor fixed it. Create-session actor adoption is
a maintainer choice made explicitly; every existing version carries no actor and
reconstitutes without one, and a version that adds the field states how each
earlier version reconstitutes.

## Constraints on present code

- The actor storage convention stays extensible to a program arm, and nothing
  assumes the submit-input actor is always the user.
- Imported-creation version 4 and create-session version 5 stay reserved; no
  writer reuses either number for another payload, and the decoders keep
  rejecting them until the placement payload lands.
- Runner replacement is the only new kind that spans more than one transaction;
  every other new kind is one claim-and-terminal-result transaction.

## Acceptance

- Each new command kind has a registry kind, a typed record family under the
  deferred typed-record trigger and the append-only trigger, a hand-written
  structural equality that excludes the command identifier, and a closed-kind
  test that names it.
- An equal replay of a runner replacement during provisioning returns the
  pending disposition; after the result row commits it returns that result.
- The three identity types have production generators and no Postgres column
  gained an identity-generating default.
- Version 4 imported-creation and version 5 create-session records decode,
  compare placement in replay equality, and the supported version sets are
  contiguous.
- A program-issued submit-input records the program actor, and replaying its
  identifier under the user actor is conflicting reuse.
- A replace-session-defaults record at the new version carries its actor, and
  every earlier version reconstitutes with the user actor.
- No telemetry site emits a command identifier after the change.

# Identity and commands design

This design is not built; it extends
[identity-and-commands](../spec/identity-and-commands.md).

## Goal

Build the items the identity and command subsystem has committed to but lacks:
registry kinds and typed records for runner recovery and a production generator
for provider-target evidence. `Actor` gains a program arm that submit-input
records, and create-session adoption stays an explicit maintainer choice, so a
program-driven turn is never recorded as user-issued.

## Design

A replacement staged behind an in-flight call or its tool batch completes or
retires at the [turn-lifecycle boundary](turn-lifecycle-and-scheduling.md).

`ProviderTargetEvidenceId` gains a UUIDv7 generator with the durable
provider-target evidence that
[model-call-execution](../spec/model-call-execution.md) defers; no slice writes
that evidence today.

`Actor` gains a program arm: a verified reference to the issuing program run,
constructible only by the program substrate's host-side session capability, with
the same validated-reference and no-conferred-authority semantics as every other
arm. Submit-input gains a program admissibility path that fixes that actor.

Repository watch creates no program run and emits only a held `create_session`
command. It submits no initial input, so the program actor does not apply.

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

- The actor storage convention stays extensible to a program arm, and nothing
  assumes the submit-input actor is always the user.
- Replace-session-defaults gains no actor field until a non-user boundary issues
  it.
- Workspace-provisioning or staged replacement may span claim and
  terminal-result transactions; every other new kind is one
  claim-and-terminal-result transaction.

## Acceptance criteria

- Each new command kind has a registry kind, a typed record family under the
  deferred typed-record trigger and the append-only trigger, a hand-written
  structural equality that excludes the command identifier, and a closed-kind
  test that names it.
- An equal replay of a runner replacement during provisioning or staging returns
  the pending disposition; after the result row commits it returns that result.
- A runner replacement claimed but unterminated at process exit is resumed at
  startup before clients are admitted, and terminates under its own identifier.
- `ProviderTargetEvidenceId` has a production generator once durable
  provider-target evidence lands.
- A program-issued submit-input records the program actor, and replaying its
  identifier under the user actor is conflicting reuse.
- Submit-input writes the program actor only at storage version 4, and version-3
  rows reconstitute without it.
- No telemetry site emits a command identifier after the change.

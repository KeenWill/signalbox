# Identity and commands design

This design is not built; it extends
[identity-and-commands](../spec/identity-and-commands.md).

## Goal

Add a production generator for provider-target evidence with its durable write
path.

## Design

`ProviderTargetEvidenceId` gains a UUIDv7 generator with the durable
provider-target evidence that
[model-call-execution](../spec/model-call-execution.md) defers; no slice writes
that evidence today.

## Acceptance criteria

- `ProviderTargetEvidenceId` has a production generator once durable
  provider-target evidence lands.

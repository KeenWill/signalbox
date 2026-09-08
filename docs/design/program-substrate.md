# Program substrate design

This design extends [program substrate](../spec/program-substrate.md) with
program-journal payload offload.

## Design

A frame payload below a fixed inline threshold stays inline in the journal row.
A larger payload, up to the configured blob maximum, becomes an immutable
SHA-256-addressed blob, and the row references it by digest only. The blob is
routed under the daemon-derived `program_journal` storage class that
[blob storage](../spec/blob-storage.md) reserves, never under an
operation-selected class.

## Compatibility constraints

- `InlineFramePayload` holds exact bytes and never a digest. The persistence row
  records whether a payload is inline or a blob, and the repository hydrates
  blob bytes before it reconstitutes the frame, so replay compares identical
  bytes however the row was stored. Frame kinds and existing inline rows do not
  change.
- Blob storage keeps the `program_journal` storage class reserved and
  daemon-derived.

## Acceptance criteria

- A payload above the inline threshold and within the configured blob maximum is
  stored once as a blob under the `program_journal` class and journaled by
  digest, and the loaded frame carries the exact bytes.

# Workflows design

This committed unbuilt design extends [workflows](../spec/workflows.md).

## Input and result

Effect method input/output records have checked Rust and TypeScript codecs;
full-width integer identities use decimal strings and exact payloads use byte
arrays. Domain values, wire payloads and storage rows remain distinct. Effects
that only native programs may call may use Rust-only method records.

## Completion

JavaScript runtime exceptions journal `ProgramError`.

## Waits

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

## Evaluations

[Evaluation workflows](eval-system.md).

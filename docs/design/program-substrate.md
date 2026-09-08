# Program substrate design

This design extends [program substrate](../spec/program-substrate.md) with
program-journal payload offload and host-side session driving.

## Design

A frame payload below a fixed inline threshold stays inline in the journal row.
A larger payload, up to the configured blob maximum, becomes an immutable
SHA-256-addressed blob, and the row references it by digest only. The blob is
routed under the daemon-derived `program_journal` storage class that
[blob storage](../spec/blob-storage.md) reserves, never under an
operation-selected class.

A session capability composes the existing session services host-side. A program
drives a session one turn at a time: it submits input, awaits that turn's
outcome, and branches. Program-issued input and program-created sessions carry
the attribution and creation cause that
[identity and commands](../spec/identity-and-commands.md) and
[sessions and the transcript](../spec/sessions-and-transcript.md) define. The
delivered outcome journals the session identity, the turn and accepted-input
identity that produced it, and an outcome digest. Transcript content never
enters the journal; the session is already durable, and the recorded turn
identity lets replay authenticate which turn supplied the answer. Structure
inside one turn is out of contract: the turn is the model's autonomy zone,
governed by the same approval judge as every session
([tool loop](../spec/tool-loop.md)).

Credentials never enter the isolate. Sessions, model calls, clones, and stage
executions happen host-side under the credential machinery in
[configuration and credentials](../spec/configuration-and-credentials.md); the
isolate receives only journaled answers.

## Compatibility constraints

- The isolate bootstrap stays closed: nothing new reaches a program except
  through the frame protocol, and no executor moves into the isolate.
- `InlineFramePayload` holds exact bytes and never a digest. The persistence row
  records whether a payload is inline or a blob, and the repository hydrates
  blob bytes before it reconstitutes the frame, so replay compares identical
  bytes however the row was stored. Frame kinds and existing inline rows do not
  change.
- Blob storage keeps the `program_journal` storage class reserved and
  daemon-derived.
- `DeliveryKind::RunCancel` keeps a payload able to carry a command identity and
  never gains a request ordinal.
- No schema or code treats a repository path, branch, or file as a program's
  identity, and no code derives a run's authority from artifact bytes alone.
- Session, turn, and accepted-input identities stay durable identifiers a
  journal row can reference.
- The approval judge stays the only gate inside a turn; no program-facing hook
  is added inside a turn.

## Acceptance criteria

- A payload above the inline threshold and within the configured blob maximum is
  stored once as a blob under the `program_journal` class and journaled by
  digest, and the loaded frame carries the exact bytes.
- A journaled session outcome contains identities and a digest and no transcript
  bytes.
- No code path passes a credential value, path, or reference into the isolate;
  every credentialed operation runs host-side.

# Program substrate design

Nothing in this document is built; it extends
[program substrate](../spec/program-substrate.md) with the committed design for
registration, grants, capability executors, cancellation, and session driving.

## Goal

A program is registered under a durable identity, runs under an explicit grant
list, drives sessions and other effects host-side, recovers from a crash without
a false exactly-once claim, and is cancelled by user authority. The journal
stays thin coordination state.

## Shape

A registration is an immutable row keyed by program name and revision. It
records the content digests of the program: one over the exact source bytes and
one over the stripped artifact bytes. Program identity is name plus revision
plus those digests. A repository path, a branch, or a file never identifies a
program. A run records the registration it executes, and its authority resolves
only from that row. Identical bytes registered under two names, or under two
grant lists, are two programs.

Every registration carries an explicit grant list drawn from the closed
capability vocabulary, `ProgramCapability`. A capability outside the run's grant
list does not exist for that run: the host journals a refusal of the request and
exercises no authority. The `register` grant attenuates: a program holding it
may request for a child only a subset of its own grants, so no chain of
program-initiated registrations obtains a capability its root lacked; widening a
grant list goes through a user-authorized registration.

A capability declares, per operation, how recovery treats an `effect` request
that has no answer after a crash. Recovery adopts the outcome when the
operation's own durable record proves it completed. It re-issues the operation
only when the capability declares the operation idempotent. Otherwise it answers
the request with a journaled ambiguous outcome the program must branch on. This
follows the external-effect ambiguity contract in
[tool loop](../spec/tool-loop.md), which forbids treating an unresolved external
loss as if it had not happened.

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

Cancel authority is user authority. Cancel is a command with ordinary durable
command identity ([identity and commands](../spec/identity-and-commands.md)),
and its wire message pair belongs to
[process protocol](../spec/process-protocol.md). An applied cancel is journaled
as one `run_cancel` delivery that carries the command identity and no request
ordinal, so a cancelled run replays to its cancellation however many requests
were outstanding.

Credentials never enter the isolate. Sessions, model calls, clones, and stage
executions happen host-side under the credential machinery in
[configuration and credentials](../spec/configuration-and-credentials.md); the
isolate receives only journaled answers.

## Constraints on present code

- The isolate bootstrap stays closed: nothing new reaches a program except
  through the frame protocol, and no executor moves into the isolate.
- `InlineFramePayload` remains the only holder of payload bytes, so a
  digest-backed representation is added there without changing frame kinds or
  rewriting inline rows.
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

## Acceptance

- Registering identical bytes under two names or two grant lists yields two
  programs, and a run's grants are read from its registration row and from
  nowhere else.
- An effect request for an ungranted capability is refused and journaled before
  the executor performs any host action.
- A program-initiated registration requesting a grant its registrant lacks is
  refused.
- After a crash between an external effect's request and its answer, recovery
  yields exactly one of an adopted outcome, a re-issue of a declared-idempotent
  operation, or a journaled ambiguous answer, and never a silent re-issue.
- A payload above the inline threshold and within the configured blob maximum is
  stored once as a blob under the `program_journal` class and journaled by
  digest, and replay reads it by that digest.
- A journaled session outcome contains identities and a digest and no transcript
  bytes.
- An applied cancel appears as one `run_cancel` delivery carrying the command
  identity, and replay of that journal ends at the cancellation.
- No code path passes a credential value, path, or reference into the isolate;
  every credentialed operation runs host-side.

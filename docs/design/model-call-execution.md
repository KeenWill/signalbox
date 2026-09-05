# Model-call execution design

This document describes committed work that is not built; it extends
[model-call-execution](../spec/model-call-execution.md).

## Goal

The daemon renders admitted attachments as typed provider-neutral content parts,
and provider-native delivery follows the open rendering decision. It renders
runner-placement changes to the model and advertises only the tools executable
in the session. It reuses a successful attachment verification within a turn. It
emits a process-level event carrying the complete exclusion evidence of a
pre-call pool exhaustion. It checks at reconstitution that the pinned pool
policy contains the pinned profile with the expected adapter and delivery kind.
It carries a program-declared structured-output contract through preparation
into runtime enforcement. It records durable provider-target evidence, pending
the per-call provenance schema decision. It lets a user resolve an unstopped
ambiguity. It carries the workspace-instruction region into the model operation
as its own typed part.

## Design

An accepted-input attachment part renders as a typed provider-neutral content
part, and the bridge maps it to the provider's native part. Provider-native
media rendering is undecided in [open-questions](../open-questions.md), so that
mapping is pending that decision. Media with no admitted projection keeps the
bounded textual stub. Modality admission is owned by
[blob-storage](../spec/blob-storage.md).

A runner-placement change entry, itself not built and owned by
[sessions-and-transcript](../spec/sessions-and-transcript.md), renders as a
structured placement change carrying the positive placement revision and the
selected sandbox profile. The bridge emits one of two exact injected user-role
messages, chosen by profile, with the braces replaced by the canonical decimal
revision. For `workspace-restricted`:
`Signalbox session event: runner placement changed to revision {revision} with profile workspace-restricted; the prior placement can no longer execute. The successor writable root and working directory are now active. Relocation did not delete prior files; they may still exist, but only paths exposed inside the successor restricted workspace are reachable.`
For `ambient`:
`Signalbox session event: runner placement changed to revision {revision} with profile ambient; the prior placement can no longer execute. The successor working directory is now active. Relocation did not delete prior files, and they may remain reachable at their previous paths through the invoking user's filesystem; check before recreating or overwriting them.`
Missing, stale, cross-session, or non-successor placement authority fails
rendering instead of inventing text. The same profile-specific text renders
every relocation, including a working-directory move on the same runner.

The prepared model operation carries one immutable snapshot of the tools
executable in this session, not the unfiltered process registry. Each entry
binds the exact definition, its permission and effect policy, and the selected
executable locus. The bridge maps only these entries to provider tool
definitions, and a tool absent from the snapshot is an unknown proposal for that
operation. Preparation includes every daemon-only tool, a combined-locus tool
whenever its daemon executor is available, and a runner-only tool only when the
session placement binds that declaration to current execution authority. A
pinned placement uses its frozen tool inventory and current matching
registration; an unpinned request includes a runner-only definition only when a
live registration satisfies its selector, sandbox, workspace, repository, and
credential availability. A declaration defined relative to a session repository
is included only for a session with a repository worktree, and a declaration
whose capability requires a credential profile only for a session granted one.
Credential matching between a repository entry and the session's grant is owned
by [configuration-and-credentials](../spec/configuration-and-credentials.md);
selector binding and loss consequences by [tool-loop](../spec/tool-loop.md) and
[runner-protocol](../spec/runner-protocol.md).

A bounded turn-scoped verification inventory records each attachment digest's
successful verification keyed by the store's immutable-generation token. A later
range in the same turn reuses that record instead of streaming and verifying the
replica again. The token is supplied by a blob-store adapter under
[blob-storage](../spec/blob-storage.md).

The pre-call pool-exhaustion failure emits one process-level event carrying the
complete nonempty evidence list in policy-member order. Each member carries its
exclusion generation or predecessor correlation and an optional reset. The typed
domain cause does not change. The wire shape is owned by
[process-protocol](../spec/process-protocol.md).

The durable pool policy gains, per member, the target adapter and delivery kind
that the present policy does not record. Reconstitution of a pool-selected call
verifies that the pinned policy contains the pinned profile with the expected
adapter and delivery kind, and fails closed otherwise.

The accepted input of a program-driven turn records the program's declared
output schema. Preparation carries that schema into the prepared model
operation's output contract, the runtime enforces it, and the turn's outcome
payload validates against the schema or the turn reports its failure. The
runtime operation already carries an optional output contract; the session path
into it does not exist. The program side is owned by
[program-substrate](../spec/program-substrate.md).

The durable per-call provenance schema is undecided in
[open-questions](../open-questions.md), so the evidence row and the rules that
depend on it are pending that decision. Each call's provider-reported identity
is recorded as a provider-target evidence row. The domain types in
`crates/domain/src/provider_evidence.rs` exist; the persistence and aggregate
wiring do not. An accepted alias concretion becomes a durable per-call
provenance row under that decision. A mismatch selects `KnownFailed` live
instead of failing the adapter stage closed. A mismatch discovered after
completion invalidates the completed call, unique by invalidated call. The
provider fallback marker becomes typed provider-neutral evidence that both HTTP
adapters construct and redact, so a marker naming the configured target carries
the substitution on its own.

From the awaiting-recovery phase, a user decision resolves an unstopped
ambiguity. Accepting the duplicate risk records `DuplicateRiskAccepted` as the
turn treatment and authorizes a replacement call on a new attempt. Whether that
replacement may reuse the ambiguous call's credential profile is undecided in
[open-questions](../open-questions.md), so its credential authority is pending
that decision. Outcome authority transfers from the ambiguous call to the
replacement, so at most one call determines the turn's outcome. The ambiguous
call stays terminal and unchanged.

The prepared model operation carries a separate optional typed
workspace-instruction region unchanged into the model operation's
workspace-instructions slot. Preparation rebuilds it from the exact
manifest-backed admitted bytes, inserts it once after system policy and before
conversation history, and authenticates its manifest before provider spawn. It
is never concatenated into the system prompt, converted to a user or tool
message, or sourced from an adapter loader. The region's bytes and authority are
owned by [workspace-instructions](../spec/workspace-instructions.md).

## Compatibility constraints

The accepted-input part order and the textual stub projection stay as
blob-storage defines them, and nothing assumes a rendered user message is
text-only.

The runtime operation's tool list stays a function of preparation, and a
tool-call part naming a tool outside that list stays an unknown proposal that
enters the confirmation path instead of a bridge rejection.

Later attachment ranges in a turn reverify until the turn-scoped inventory above
lands, and nothing caches a verification without an immutable-generation token.

The typed domain cause of pre-call exhaustion and the sealed pool-exhausted turn
transition do not change when the process event lands.

The durable pool policy carries no adapter or delivery-kind fields until those
fields land, and nothing assumes it does.

Nothing assumes a prepared model operation carries no output contract, and the
prepared-operation shape stays extensible to the recorded schema without
reinterpreting existing calls.

Substitution is carried entirely by the reported identity; an alias concretion
is operator diagnostics while the provenance schema stays undecided; a
substituted call is `Ambiguous` by restart.

The parked awaiting-recovery phase and the configured automatic reconciliation
attempt budget are the only present recovery behaviors. The terminal ambiguous
call is never rewritten, and a later interrupt proof is carried by the
reconciliation marker and its correlated successor, not by rewriting the ended
attempt.

The model operation's system prompt carries only the frozen defaults prompt, and
no present code concatenates any other source into it.

## Acceptance criteria

A turn with an image attachment renders as a typed provider-neutral content
part, and the transcript entries are unchanged. Native delivery becomes a
criterion when the open rendering decision adopts it.

A relocated session's next call carries exactly one placement-change message
with the profile-specific text, and a session composed without a workspace
advertises exactly the tools that can execute in it.

A second attachment range in one turn reads only that range, conditional on the
pinned generation, and does not verify the full replica again.

A pre-call pool exhaustion produces one process event whose member list equals
the frozen policy's members in order, and the domain cause is unchanged.

Reconstituting a pool-selected call whose pinned profile is absent from the
pinned policy, or whose adapter or delivery kind differs, fails closed.

A program-driven turn whose response violates its declared schema fails with a
typed cause and never commits an unvalidated payload.

An alias concretion leaving a durable provenance row, a substitution ending the
call `KnownFailed` live, and a marker naming the configured target classifying
as substitution become criteria when the open provenance schema decision lands.

A user can accept the duplicate risk on a parked ambiguity, and the original
call stays terminal `Ambiguous`. A replacement call running on a new attempt
becomes a criterion when the open credential-authority decision lands.

A session with admitted workspace instructions sends them as one region between
system policy and history, and the system prompt is unchanged.

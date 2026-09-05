# Tool loop design

This design is not built; it extends [tool-loop](../spec/tool-loop.md) with
three capabilities: pre-approval admissibility, the instruction admission
effect, and child creation by `spawn_session`.

## Goal

Pre-approval admissibility lets a tool family refuse a request on evidence
available before any approval decision, so no judge or user is asked about a
request the session may not see and no attempt is minted for it.

The instruction admission effect makes a successful `instructions_read` admit a
workspace-instruction bundle in the same transaction that commits the tool
result, and commits the successor instruction manifest together with
continuation.

Child creation gives `spawn_session` one placement-owned transaction that
creates a delegated child and its initial task work.

## Design

A family declares an admissibility check for a condition it can evaluate before
approval. Where a family declares one, that check takes precedence over the
ordinary prepared-attempt `KnownFailed` route for the same condition; the two
never both run. The instruction family declares two: arguments that do not
decode to its schema, and a bundle outside the effective eligibility view, both
specified by [workspace-instructions](../spec/workspace-instructions.md).

An inadmissible request resolves before approval through a request-level
transition. It records a fourth durable logical resolution,
`closed_inadmissible`, carrying the family's typed reason on the request itself,
and creates no approval state, no judge call, no attempt row, and no executor
work. The reason lives on the request because nothing executed: there is no
attempt history to explain. The transition is request-level because a tool
attempt names its issuing turn attempt, and a batch parked on an undecided
approval has no current turn attempt to name.

A request resolved this way is not undecided, so the batch is not parked behind
it and proposal order continues at the next request. It is resolved for the
continuation boundary too: it projects one `ToolInadmissible { request }` result
entry in proposal order, and it satisfies the batch-complete condition that
creates the continuation turn attempt. A batch whose only proposal is
inadmissible still prepares its next model call. The projection renders it
through the same provider-visible error object as any other typed failure; the
instruction family's two pre-approval reasons select `invalid_arguments` with
the detail `not_eligible` or JSON null. No new result shape reaches a provider.

For a successful fresh `instructions_read`, the commit-result transaction also
locks the session's admitted-set head and atomically appends the
`InstructionAdmission` specified by
[workspace-instructions](../spec/workspace-instructions.md) with the
receipt-only completed result. A stale head, failed read, or failed admission
validation discards the admission, not the round: the transaction commits the
typed failure as the attempt's result and leaves the admitted-set head and every
existing admission untouched. The executor work already happened outside any
transaction; what this transaction decides is whether its evidence becomes an
admission, and a rejected admission is a recorded result, not grounds to roll
back the attempt. Replay of an already committed request returns the recorded
receipt and admission link without appending either again; a conflicting receipt
or link is corruption. The head lock's position and mode in the repository-wide
lock order belong to [persistence-protocol](../spec/persistence-protocol.md).

The continuation transaction folds the batch's fresh successful admission rows
in request order and creates exactly one successor turn-instruction manifest
authenticated by the new `Prepared` model call. An idempotent replay receipt or
an `already_admitted` receipt contributes no row and cannot duplicate a bundle
or alter the successor manifest digest.

`spawn_session` declares a task plus a relationship, either background or bound
with separately labeled actions for the parent stopping and the parent being
cancelled. The creation transaction atomically creates one delegated no-ancestry
child and its initial task work, closes the spawning physical attempt with its
matching receipt, derives the child's placement default from its parent's
directory, and returns the child session identity as a durable completion. The
child's initial task is not accepted user input: the spawn transition records a
`DelegatedTask` origin bound to the spawning request and its parent session and
turn, and the child's first turn starts from that entry with no accepted-input
row or user actor invented. Equal physical replay returns that child and reuses
the same semantic entry and turn origin; a second child cannot attach to the
request. There is no fixed active-child-count limit; admission checks the
complete locked relationship inventory for request and child uniqueness.

The tool result delivered to the parent is copied from the child's terminal
result record, and the executor never reads or returns the child transcript. The
child's terminal completion concatenates the ordered assistant text entries from
its proof-bearing completed call without a separator and admits those bytes as
the delegation content. A completion with no assistant text, or text over the
delegation-content bound, instead records a failed outcome carrying the
`ChildResultUnavailable` reason. Duplicate observation is idempotent by spawning
request and cannot attach a late result to another parent tool call.

## Compatibility constraints

The four implemented continuation effects and the successor manifest must
eventually commit or roll back together, so the continuation transaction stays
one transaction that can carry a fifth effect.

A request resolved `closed_inadmissible` is never reclassified by any terminal
materialization path. Interrupt, crash-loss, and reconciliation materializations
must keep a per-request resolution lookup that can carry a fourth resolution and
emit `ToolInadmissible` for it instead of falling into the `ToolClosed`
fallback.

Attempts move monotonically to a terminal state, and no path rolls back a
completed executor result; the admission effect is decided inside that rule, not
by widening it.

The daemon-local error kind set stays closed; the instruction family maps into
`execution_failed` and `invalid_arguments` and adds no kind.

The spawn port rejects execution unconditionally today. That rejection stays
until the creation transaction exists, and no other surface creates the child.

## Acceptance criteria

A request a declaring family marks inadmissible resolves before approval with no
approval state, no judge call, no attempt row, and no executor work; it projects
one `ToolInadmissible` entry in proposal order, and a batch containing only
inadmissible requests still prepares its next model call.

Interrupt, crash-loss, and reconciliation materializations preserve
`ToolInadmissible` for a request resolved `closed_inadmissible`.

A successful `instructions_read` commits its result and its
`InstructionAdmission` in one transaction. A stale head or failed validation
commits the typed failure and leaves the admitted set untouched. Replay appends
nothing and returns the recorded receipt and link. Continuation creates exactly
one successor manifest with the `Prepared` call, and the five effects commit or
roll back together.

`spawn_session` creates one child and its task work, closes the spawning attempt
with its receipt, and returns the child identity in one transaction. Equal
replay returns the same child, semantic entry, and turn origin. A second child
cannot attach to the request, and uniqueness is checked against the locked
relationship inventory.

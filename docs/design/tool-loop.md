# Tool loop design

This design is not built; it extends [tool-loop](../spec/tool-loop.md) with
lost-lease retry takeover, pre-approval admissibility, and the instruction
admission effect.

## Goal

Pre-approval admissibility lets a tool family refuse a request on evidence
available before any approval decision, so no judge or user is asked about a
request the session may not see and no attempt is minted for it.

The instruction admission effect makes a successful `instructions_read` admit a
workspace-instruction bundle in the same transaction that commits the tool
result, and commits the successor instruction manifest together with
continuation.

## Design

An offered lease is already dispatched. With durable no-execution proof, loss
permits a checked successor generation for every effect class while retaining
the unexecuted attempt, as the [runner contract](../spec/runner-protocol.md)
requires. Without that proof, side-effecting work receives crash classification;
pure or idempotent work instead retains the lost attempt and requires a fresh
physical attempt. When either retry path needs a successor runner, all preceding
requests first reach durable resolution, but none of the batch's result entries
is projected yet. A distinct pre-continuation takeover transaction then locks
the session, batch, retained lost attempt, and staged replacement; revalidates
that every preceding request is resolved and that this request alone is
recovery-pending; installs the successor placement; and consumes the staged
replacement before a retry lease can be offered, using the
[runner installation transaction](runner-protocol.md). It neither appends
tool-result entries nor prepares a continuation. This recovery takeover is the
exception to waiting for the lost offered request to resolve. The request
remains recovery-pending until its retry resolves or
[terminalization wins](turn-lifecycle-and-scheduling.md). If the retry resolves
first, later requests execute in proposal order. The transaction offering a
fresh retry attempt terminalizes the retained lost attempt as superseded by that
fresh attempt atomically with the retry offer, without an extra result entry.
Once the whole batch resolves, the ordinary continuation transaction projects
every result in proposal order, appends the pending relocation entry, and
prepares the next call. Retry correlation crosses the placement boundary and
therefore validates the lost attempt and successor placement generations without
requiring their runner ids to match. An executor-dispatched attempt otherwise
completes or receives its effect-class crash classification; placement loss
never rewrites or cancels it.

A family declares an admissibility check for a condition it can evaluate before
approval. Where a family declares one, that check takes precedence over the
ordinary prepared-attempt `KnownFailed` route for the same condition; the two
never both run. The instruction family declares two: arguments that do not
decode to its schema, and a bundle outside the effective eligibility view, both
specified by [workspace-instructions](../spec/workspace-instructions.md).

An inadmissible request resolves before approval through a request-level
transition. It records the durable logical resolution `closed_inadmissible`,
carrying the family's typed reason on the request itself, and creates no
approval state, no judge call, no attempt row, and no executor work. The reason
lives on the request because nothing executed: there is no attempt history to
explain. The transition is request-level because a tool attempt names its
issuing turn attempt, and a batch parked on an undecided approval has no current
turn attempt to name.

A request resolved this way is not undecided, so the batch is not parked behind
it and proposal order continues at the next request. It is also resolved for the
continuation boundary: it projects one `ToolInadmissible { request }` result
entry in proposal order and satisfies the batch-complete condition that creates
the continuation turn attempt, and a batch whose only proposal is inadmissible
still prepares its next model call. The projection renders it through the same
provider-visible error object as any other typed failure, and no new result
shape reaches a provider; the instruction family's two pre-approval reasons
select `invalid_arguments` with the detail `not_eligible` or JSON null.

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

Attempts move monotonically to a terminal state, and no path rolls back a
completed executor result; the admission effect is decided inside that rule, not
by widening it.

The daemon-local error kind set stays closed; the instruction family maps into
`execution_failed` and `invalid_arguments` and adds no kind.

## Acceptance criteria

Offered attempts lost with durable no-execution proof permit a checked successor
generation for every effect class, retaining the unexecuted attempt. Without
that proof, side-effecting offered attempts receive effect-class crash
classification. Both proof-backed retries of any effect class and pure or
idempotent retries without proof use pre-continuation takeover when a successor
runner is needed: after every preceding request resolves, that transaction
installs the successor and consumes the staged replacement before the retry
lease is offered, without requiring the old and new runner ids to match. It
projects no result and prepares no continuation. Unless terminalization closes
the retained attempt and request and suppresses retry, the request remains
recovery-pending until that retry resolves; later requests then execute in
proposal order before the ordinary continuation transaction projects the
complete batch. Executor-dispatched attempts otherwise complete or receive
effect-class crash classification; placement loss never rewrites or cancels
them.

Offering a fresh retry atomically leaves the retained lost attempt terminal and
superseded by that retry, without an extra result entry or two live attempts.

A request a declaring family marks inadmissible resolves before approval with no
approval state, no judge call, no attempt row, and no executor work; it projects
one `ToolInadmissible` entry in proposal order, and a batch containing only
inadmissible requests still prepares its next model call.

A successful `instructions_read` commits its result and its
`InstructionAdmission` in one transaction. A stale head or failed validation
commits the typed failure and leaves the admitted set untouched. Replay appends
nothing and returns the recorded receipt and link. Continuation creates exactly
one successor manifest with the `Prepared` call, and the five effects commit or
roll back together.

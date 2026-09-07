# Process protocol design

This design is not built; it extends
[process-protocol.md](../spec/process-protocol.md) with the wire surfaces the
owner has committed and the daemon and terminal client do not implement.

## Goal

Future implementation of these surfaces under protocol version 1 must pair each
daemon handler with its terminal-client consumer in the same change:
configuration reload, program-run cancellation, runner placement facts, and the
typed projection of credential-pool exhaustion and of the
credential-availability wait.

## Design

Configuration reload is one `reload_configuration { command_id }` request
carrying a user-global command identity. An equal command retry reports busy
while pending or replays its stored result without re-reading configuration.
Reloads, including startup replay, run serially from configuration read through
the terminal result. Core first commits the reload command with a complete
checked snapshot of every reloadable section, the rule-set digest, and the prior
snapshot for refusal recovery as durable intent. The replacement snapshot
contains the model catalog, session-template catalog, and repository-watch
configuration, including rules, convergence targets, template, interval,
credential path, webhook listener settings, and hook map. Reload pauses sweep
admission and stops and joins active sweep attempts and affected ingestion tasks
before rule activation and event-tail capture. Only after that activation
commits does core reconcile convergence targets from the retained intent. On a
stale or conflicting rule-revision rejection, recovery first validates the prior
snapshot against the current startup-only sections. If invalid, it terminalizes
the intent with `configuration_reload_failed` without resuming workers, and
startup recovery fails. Otherwise it resumes ingestion and sweep admission under
the prior snapshot before terminalizing the intent with
`configuration_reload_failed`, without replacing the running configuration.
Other failures after either stops leave the intent pending until recovery
installs the replacement snapshot and resumes them before terminalizing the
claim. The [reload-intent input](ownership-seam.md) delivers rule activation
only, and the module activates the rules atomically and idempotently by command
identity and digest. The activation transaction captures each repository's
current event tail and retains it for idempotent replay. Before replay activates
any retained effects, startup validates the retained reloadable snapshot
together with the on-disk startup-only sections; incompatibility fails startup
and leaves the intent pending. Startup replays any undelivered intent from its
retained payload even if the configuration files changed, before terminalizing
its claim. Success returns
`configuration_reloaded { command_id, reloaded_sections }`, whose sections are
an array of the closed values `model_catalog`, `session_templates`, and
`repo_watch`. Failure returns
`configuration_reload_failed { command_id, phase, reason }`, sanitized as
startup logs are. Which sections reload and the validate-then-swap rule belong
to [configuration-and-credentials.md](../spec/configuration-and-credentials.md).

Program-run cancellation is the request
`cancel_program_run { run_id, command_id }` and the receipt
`program_run_cancellation_receipt { command_id, run_id, outcome }`. The outcome
is `applied { terminal_state: "cancelled", result: null }`, `not_found`, or
`already_terminal { terminal_state, result }` naming the standing terminal state
and result the command found. An identical request bearing the same `command_id`
replays its stored receipt even if the run's standing state later changes; the
same identity with a different payload is conflicting reuse. Run-state semantics
belong to [program-substrate.md](../spec/program-substrate.md); this pair, its
version-1 encoding, and the closed receipt algebra belong here, and a later
incompatible shape requires a new protocol version.

Runner placement facts are a paged `read_runner_status` read beside the built
`runner_state_transition` event. The read carries `page_size` 1 through 100 and
an exclusive keyset `after`, and opens with `runner_status_start`, then returns
`runner_status` only for a null `after`, then `runner_operation_failure` and
`runner_workspace_leak` messages followed by
`runner_status_end { runner_count, failure_count, leak_count, next_after }`.
`after` and `next_after` are null or one tagged cursor object naming the last
row the page emitted. The projection carries a pending provisioning-only
successor's enrollment-request identity and its authority state, because
`promote_pending_runner` names that identity. The `operation_failure` variant
carries the runner, the `operation_kind`, the refused operation's complete
correlation arm that kind selects, one closed daemon-actionable `category`, and
the runner-authored `detail` object with its bounded `code`, `message`, and
structured `payload`; the `workspace_leak` variant carries the runner, fact
kind, locator, entry digest, and the leak fact's optional session and placement
revision. Both variants are exclusive, failures order before leaks, and the
traversal each one continues belongs to
[persistence-protocol.md](../spec/persistence-protocol.md). The category set is
exactly the closed daemon-actionable set the runner wire carries, member for
member, so every retained failure is serializable. The daemon bounds the detail
and retains the runner's text unchanged, following the `operation_failed`
contract in [runner-protocol.md](runner-protocol.md). The detail is untrusted
runner-authored text, so the status projection is a transformed view of that
retained record: it applies the diagnostic-evidence redaction in
[process-protocol.md](../spec/process-protocol.md), removing host and credential
paths, before exposing it. The event notifies a follower of each live runner
transition above its snapshot cursor; the snapshot's runner projection carries
the current state on reconnect. The event family is the extension point for
later runner facts: a new fact adds a state and its members to this event kind,
never a second kind. A snapshot's session summary carries the same runner
object, with connection health present exactly for a pinned placement.

The credential projection adds
`failed_credential_pool_exhausted { terminal_frontier_id, terminal_attempt_id, failure_entry_id, pool_policy_id, policy_members, members }`
as a `transcript_turn` state variant,
`turn_credential_pool_exhausted { turn_id, terminal_attempt_id, failure_entry_id, terminal_frontier_id, pool_policy_id, policy_members, members }`
as its live event, and the read
`read_credential_pool_policy { session_id, turn_id, pool_policy_id }` answered
by `credential_pool_policy { pool_policy_id, policy_members }`. The read is
admitted only when the caller may read the named session and its named turn
references that exact immutable policy; a mismatch is `unknown_pool_policy`, and
the response reconstitutes the policy header and membership rows directly rather
than copying either failure projection. `policy_members` is the immutable
policy's complete ordered array of profile references; `members` has the same
length, and each evidence item's `profile` equals the same-ordinal
`policy_members` value. Each item carries `profile`, a nullable
`reset_at_unix_ms`, and one closed `exclusion`:
`profile_quarantine { record_generation }`,
`membership_exclusion { record_generation }`,
`session_displacement { record_generation }`,
`chain_exclusion { predecessor_model_call_id }`, or
`headroom_reserve { observed_headroom_percent, reserve_percent }` without a
generation. A member satisfying several exclusions reports exactly one, chosen
in that order, widest scope first, so two producers cannot describe one
exhaustion differently. `reset_at_unix_ms` is present only when every exclusion
active for the member at the failure commit expires at the reset it reports, and
is then the latest of them. The snapshot and event carry no credential bytes,
path, provider prose, or current-configuration lookup, and the projection is
never paginated or truncated; configuration admission bounds each profile and
pool name to 256 UTF-8 bytes and each pool to 1,024 members so the duplicated
evidence fits one frame under worst-case JSON escaping. The
credential-availability wait projects as an active turn state retaining the same
turn and session slot. The after-call wait-transition failure projects the
predecessor call identity alongside its call-free terminal attempt. The endings
these shapes project belong to
[credential-availability.md](../spec/credential-availability.md).

## Compatibility constraints

Chain exclusions remain insert-only and turn-local and are neither listed nor
clearable.

`runner_state_transition` is an admitted version-1 event variant that the daemon
projects when the outbox carries a runner state transition. Every other request
and message above is outside the closed inventories in `crates/process-protocol`
until the daemon and client implement its surface together.

No response code is reserved for an authorization failure, because client
identity, authentication, authorization, and revocation are undecided.

A pre-call credential-pool exhaustion terminalizes its turn through the generic
failed projection; no present producer emits the typed state or event.

Admitting `park` in static configuration alone does not make the wait reachable;
whatever first makes either wait reachable includes this projection.

## Acceptance criteria

Every request and message above decodes under version 1 with unknown fields
rejected, and each surface ships with its daemon handler and the terminal-client
consumer it lacks; a follower-visible addition also ships the native client's
decoder and projection updates.

An equal `command_id` replay returns the stored receipt for
`cancel_program_run`.

A follower learns a live runner loss, change, or relocation through
`runner_state_transition` and the current runner state from its snapshot, and
every retained runner failure serializes in the status read.

In the exhaustion projection, `members` and `policy_members` are equal in length
and order, the snapshot state and the live event carry identical `members`, the
policy read returns the same inventory, and the client exposes the terminal
state only after those checks pass.

The credential-availability wait projects as an active state that keeps its turn
and session slot, and reconnect and live follow project the same typed cause.

# Process protocol design

This design is not built; it extends
[process-protocol.md](../spec/process-protocol.md) with the wire surfaces the
owner has committed and the daemon and terminal client do not implement, apart
from the terminal client's existing `spawn_session` half.

## Goal

Ten surfaces land under protocol version 1, each with its daemon handler and its
terminal-client consumer in the same change: provisioning an `oauth` credential
profile, re-provisioning it after a rejected daemon-owned refresh, deleting it,
credential-exclusion administration, configuration reload, program-run
cancellation, runner placement facts, `spawn_session`, cascade metadata on stop
receipts, and the typed projection of credential-pool exhaustion and of the
credential-availability wait. The terminal client already sends `spawn_session`
and validates its receipt, so that surface needs only its daemon transaction.

## Design

OAuth administration has three requests, `provision_oauth_credential`,
`reprovision_oauth_credential`, and `delete_oauth_credential`, each carrying
`profile` and a user-global `command_id`. Provisioning and re-provisioning emit
`oauth_credential_authorization { command_id, profile, user_code, verification_uri }`
for the device-authorization exchange owned by
[configuration and credentials](configuration-and-credentials.md). `user_code`
is 1 through 256 printable ASCII bytes; `verification_uri` is an absolute
`https` URI without user information or fragment, 1 through 4,096 UTF-8 bytes.
Invalid fields fail as `device_endpoint_rejected` before progress is emitted.
Each request ends with
`oauth_credential_receipt { command_id, profile, outcome }`, whose outcome is
`provisioned`, `already_provisioned`, `reprovisioned`, `deleted`,
`already_deleted`, `not_provisioned`, `abandoned`, `superseded`, or
`failed { reason }`, where `reason` is one of the fieldless values
`unknown_profile`, `non_oauth_profile`, `device_endpoint_rejected`,
`device_endpoint_failed` for transport failure of the initial
device-authorization request, `access_denied` for operator denial,
`polling_expired`, `token_endpoint_failed`, `token_response_without_identity`,
or `account_independence_failed`. A request naming an undeclared profile returns
`failed { reason: unknown_profile }`; a request naming a profile whose delivery
is not `oauth` returns `failed { reason: non_oauth_profile }`. Neither starts an
exchange or mutates credential state. Initial provisioning returns
`already_provisioned` without starting an exchange when the profile holds
authorization. Re-provisioning returns `not_provisioned` without starting an
exchange when no authorization is stored. Only re-provisioning replaces
authorization; it clears every OAuth delivery-origin quarantine, including
refresh and tuple-mismatch quarantines, only on success. Deletion ends its
delivery-origin quarantine and removes stored authorization and cached access
tokens, preventing future dispatches while retaining the configured registration
and referenced history; a child already holding a copied token finishes its
invocation. Each profile retains a generation that every deletion advances.
Provisioning and re-provisioning commit authorization and advance that
generation atomically only when their starting generation is current and, for
re-provisioning, authorization is still stored; otherwise the transaction
records `superseded` without storing authorization. The claim retains
authorization details before their first emission and while pending. An equal
request with the same `command_id` reports busy while pending and replays those
details when available, or returns its stored receipt without repeating the
exchange or deletion; conflicting reuse is rejected. Startup terminalizes each
pending provisioning or re-provisioning claim with an `abandoned` receipt;
another provisioning attempt requires a new `command_id`. The credential
mutation and terminal receipt commit atomically under the command claim protocol
in [identity and commands](../spec/identity-and-commands.md).

Credential-exclusion administration is one `list_credential_exclusions` read
carrying `page_size` and `after`, and one `clear_credential_exclusion` mutation
carrying a user-global `command_id` and one closed `target` object. The target
is `profile_quarantine { profile, record_generation }`,
`membership_exclusion { pool_policy_id, profile, record_generation }`,
`session_displacement { session_id, pool_policy_id, profile, record_generation }`,
or
`chain_exclusion { session_id, turn_id, pool_policy_id, profile, predecessor_model_call_id }`.
The read lists every active exclusion the mutation admits, as its exact target
object, and omits exactly the records the mutation rejects; the filter turns on
the exclusion's origin, never on the profile's delivery. A quarantine minted by
a rejected daemon-owned OAuth refresh is rejected; a quarantine minted by a
failed `codex_home` identity walk is accepted, because the walk reruns at every
preparation and re-quarantines a still-broken home. `page_size` is 1 through
100; `after` is null or one complete target object and is an exclusive keyset
cursor. Results sort by target tag in the order above, then by each field's
canonical order: UTF-8 bytes for configured names, UUID bytes for durable
identities, numeric order for generations. The read opens with
`credential_exclusion_start`, then one `credential_exclusion` per row, then
`credential_exclusion_end { exclusion_count, next_after }` with a null
`next_after` only at the end. The mutation marks exactly the named active
generation or predecessor correlation cleared. A newer active generation at the
target's own exact scope returns `stale_generation` before the named older
generation is considered; the scope is the profile and origin for a profile
quarantine, the pool policy and profile for a membership exclusion, and the
session, pool policy, and profile for a session displacement. A target with no
exactly matching retained record is `unknown_credential_exclusion`; an exact
record an earlier command already cleared is `already_cleared`. Success returns
`credential_exclusion_cleared { target, outcome }` with outcome `cleared` or
`already_cleared`, and the inactive record is retained so the second answer is
durable. An equal `command_id` replay returns its stored receipt before current
state is evaluated. Both operations are authorized as every other request is:
reaching the owner-private socket is the authority.

Configuration reload is one `reload_configuration { command_id }` request
carrying a user-global command identity. An equal command retry reports busy
while pending or replays its stored result without re-reading configuration.
Reloads, including startup replay, run serially from configuration read through
the terminal result. Core first commits the reload command with a complete
checked snapshot of every reloadable section, the rule-set digest, and the prior
snapshot for refusal recovery as durable intent. The replacement snapshot
contains the model catalog, session-template catalog, and repository-watch
configuration, including rules, convergence targets, template, interval,
credential path, webhook listener settings, and hook map. Core pauses sweep
admission and stops and joins active sweep attempts before reconciling
convergence targets from the retained intent. Reload stops and joins affected
ingestion tasks before rule activation and event-tail capture. A stale or
conflicting rule-revision rejection restores the prior convergence targets and
resumes ingestion and sweep admission under the prior snapshot before
terminalizing the intent with `configuration_reload_failed`, without replacing
the running configuration. Other failures after either stops leave the intent
pending until recovery installs the replacement snapshot and resumes them before
terminalizing the claim. The [reload-intent input](ownership-seam.md) delivers
rule activation only, and the module activates the rules atomically and
idempotently by command identity and digest. The activation transaction captures
each repository's current event tail and retains it for idempotent replay.
Before replay activates any retained effects, startup validates the retained
reloadable snapshot together with the on-disk startup-only sections;
incompatibility fails startup and leaves the intent pending. Startup replays any
undelivered intent from its retained payload even if the configuration files
changed, before terminalizing its claim. Success returns
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
`replace_lost_runner`, `abandon_lost_runner`, and `promote_pending_runner` are
planned wire commands whose durable request, replay, and recovery semantics stay
in [identity-and-commands.md](../spec/identity-and-commands.md) and
[runner-protocol.md](../spec/runner-protocol.md).

`spawn_session` carries a bounded `task` and the closed relationship object and
returns `session_spawned { tool_request_id, child_session_id, relationship }`.
The placement-owned creation transaction that implements the parent-directory
default creates the child; it preserves the exact-request and authority rules of
the delegation contracts on the spec page, and the task string fits both the
delegation-content ceiling and its complete normalized JSON argument envelope.

A successful cascade receipt for `stop_goal` or `stop_turn` carries the selected
`descendant_scope` and the exact count of recorded descendant dispositions, so a
zero-child choice and an unperformed cascade cannot be confused. An equal
durable-command retry returns those stored values without re-evaluating the
cascade.

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
`session_displacement { record_generation }`, or
`chain_exclusion { predecessor_model_call_id }`. A member satisfying several
exclusions reports exactly one, chosen in that order, widest scope first, so two
producers cannot describe one exhaustion differently. `reset_at_unix_ms` is
present only when every exclusion active for the member at the failure commit
expires at the reset it reports, and is then the latest of them. The snapshot
and event carry no credential bytes, path, provider prose, or
current-configuration lookup, and the projection is never paginated or
truncated; configuration admission bounds each profile and pool name to 256
UTF-8 bytes and each pool to 1,024 members so the duplicated evidence fits one
frame under worst-case JSON escaping. The credential-availability wait projects
as an active turn state retaining the same turn and session slot. The after-call
wait-transition failure projects the predecessor call identity alongside its
call-free terminal attempt. The endings these shapes project belong to
[credential-availability.md](../spec/credential-availability.md).

## Compatibility constraints

`spawn_session` and `session_spawned` are admitted version-1 variants: the
daemon decodes `spawn_session` and rejects it without mutation, and no daemon
path produces `session_spawned`. `runner_state_transition` is an admitted
version-1 event variant that the daemon projects when the outbox carries a
runner state transition. Every other request and message above is outside the
closed inventories in `crates/process-protocol` until the daemon and client
implement its surface together.

No response code is reserved for an authorization failure, because client
identity, authentication, authorization, and revocation are undecided.

A pre-call credential-pool exhaustion terminalizes its turn through the generic
failed projection; no present producer emits the typed state or event.

Admitting `park` in static configuration alone does not make the wait reachable;
whatever first makes either wait reachable includes this projection.

Exclusion records retain the generation and predecessor correlations the clear
mutation names, and a cleared record is retained inactive rather than deleted.

## Acceptance criteria

Every request and message above decodes under version 1 with unknown fields
rejected, and each surface ships with its daemon handler and the terminal-client
consumer it lacks; a follower-visible addition also ships the native client's
decoder and projection updates.

The exclusion listing and the clear mutation agree on what is clearable for
every exclusion origin, and a delivery-origin OAuth-refresh quarantine is
neither listed nor clearable.

An equal `command_id` replay returns the stored receipt for
`clear_credential_exclusion`, `cancel_program_run`, and a cascading stop, and
`stale_generation` is evaluated only for a fresh command and only within the
target's own scope.

A follower learns a live runner loss, change, or relocation through
`runner_state_transition` and the current runner state from its snapshot, and
every retained runner failure serializes in the status read.

In the exhaustion projection, `members` and `policy_members` are equal in length
and order, the snapshot state and the live event carry identical `members`, the
policy read returns the same inventory, and the client exposes the terminal
state only after those checks pass.

The credential-availability wait projects as an active state that keeps its turn
and session slot, and reconnect and live follow project the same typed cause.

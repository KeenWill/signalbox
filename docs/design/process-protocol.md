# Process protocol design

This design is not built; it extends
[process-protocol.md](../spec/process-protocol.md) with the wire surfaces the
owner has committed and the daemon and terminal client do not implement.

## Goal

Future implementation of these six surfaces under protocol version 1 must pair
each daemon handler with its terminal-client consumer in the same change:
provisioning an `oauth` credential profile, re-provisioning it after a rejected
daemon-owned refresh, deleting it, configuration reload, runner placement facts,
and the typed projection of the credential-availability wait.

## Design

OAuth administration uses the requests and receipts on the
[spec page](../spec/process-protocol.md). Provisioning and re-provisioning emit
`oauth_credential_authorization` for the device-authorization exchange owned by
[configuration and credentials](configuration-and-credentials.md); invalid
progress fields fail as `device_endpoint_rejected` before progress is emitted.
Initial provisioning returns `already_provisioned` without starting an exchange
when the profile holds authorization. Re-provisioning returns `not_provisioned`
without starting an exchange when no authorization is stored. Only
re-provisioning replaces authorization; it clears every OAuth delivery-origin
quarantine, including refresh and tuple-mismatch quarantines, only on success.
Deletion addresses retained OAuth state by profile identity even when its
declaration is absent or its delivery is no longer `oauth`. Deletion acquires
the profile row lock that dispatch holds through the token-copy step, then ends
its OAuth delivery-origin quarantine and removes stored authorization and cached
access tokens, preventing future dispatches while retaining any current
registration and referenced history; a child already holding a copied token
finishes its invocation. Each profile retains a generation that every deletion
advances. Provisioning and re-provisioning commit authorization and advance that
generation atomically only when their starting generation is current and, for
re-provisioning, authorization is still stored; otherwise the transaction
records `superseded` without storing authorization. The final transaction also
revalidates, serialized with catalog replacement, that the profile exists with
`oauth` delivery and the exchange's canonical OAuth tuple; otherwise it records
`failed { reason: registration_changed }` without storing authorization. The
claim retains authorization details before their first emission and while
pending. An equal request with the same `command_id` reports busy while pending
and replays those details when available, or returns its stored receipt without
repeating the exchange or deletion; conflicting reuse is rejected. Startup
terminalizes each pending provisioning or re-provisioning claim with an
`abandoned` receipt; another provisioning attempt requires a new `command_id`.
The credential mutation and terminal receipt commit atomically under the command
claim protocol in [identity and commands](../spec/identity-and-commands.md).

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

The credential-availability wait projects as an active turn state retaining the
same turn and session slot. The after-call wait-transition failure projects the
predecessor call identity alongside its call-free terminal attempt. The endings
these shapes project belong to
[credential-availability.md](../spec/credential-availability.md).

## Compatibility constraints

A headroom-only exclusion reports
`headroom_reserve { observed_headroom_percent, reserve_percent }` without a
generation, last.

A credential-wide transient exclusion reports
`transient_exclusion { observation_model_call_id }` after `chain_exclusion` and
before `headroom_reserve`.

A null `record_generation` names an action row that predates the projection
table and is not clearable by generation.

Chain exclusions remain insert-only and turn-local and are neither listed nor
clearable.

`runner_state_transition` is an admitted version-1 event variant that the daemon
projects when the outbox carries a runner state transition. The OAuth
administration envelopes and progress message are admitted version-1 variants.
Every other request and message above is outside the closed inventories in
`crates/process-protocol` until the daemon and client implement its surface
together.

No response code is reserved for an authorization failure, because client
identity, authentication, authorization, and revocation are undecided.

Admitting `park` in static configuration alone does not make the wait reachable;
whatever first makes either wait reachable includes this projection.

## Acceptance criteria

Every request and message above decodes under version 1 with unknown fields
rejected, and each surface ships with its daemon handler and the terminal-client
consumer it lacks; a follower-visible addition also ships the native client's
decoder and projection updates.

A follower learns a live runner loss, change, or relocation through
`runner_state_transition` and the current runner state from its snapshot, and
every retained runner failure serializes in the status read.

The credential-availability wait projects as an active state that keeps its turn
and session slot, and reconnect and live follow project the same typed cause.

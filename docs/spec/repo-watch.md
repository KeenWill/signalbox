# Repository watch

Repository watch defines the checked vocabulary and module-owned persistence for
observing repositories and reducing accepted facts into held session commands.

## Overview

A pure differ (`derive_repo_watch_events`) compares canonical repository and
pull-request observations and produces immutable `RepoWatchEvent` values in
deterministic order. Each event has a UUID, a repository, a tagged pull-request
or branch target, a closed payload, and a source-independent content identity.
The closed event-kind vocabulary is:

- `pull_request_opened`
- `pull_request_closed`
- `pull_request_merged`
- `head_changed`
- `mergeable_state_changed`
- `checks_completed`
- `check_run_completed`
- `branch_workflow_run_completed`
- `review_submitted`
- `thread_opened`
- `thread_resolved`
- `labeled`
- `unlabeled`
- `base_advanced`
- `reaction_changed`

Rules are versioned `RepoWatchRule` values. Fields within one matcher are
conjunctive and rules are evaluated independently. The checked matcher owns the
repository, event-kind, pull-request context, label, draft, author,
mergeability, and conclusion predicates. A rule carries a nonempty ordered
action list, singleton scope, and cooldown. Its content digest covers its full
versioned semantics.

The module schema contains thirteen tables:

- `repository_state` and `pr_state` are mutable provider-state projections. A
  repository row fences complete frontier commits with a generation and the
  digest of the last candidate; a generation mismatch is stale unless the
  complete frontier and ordered event batch exactly replay the immediately
  succeeding commit. A candidate equal to the stored frontier advances no
  generation when it carries no events and is rejected when it carries events. A
  complete candidate replaces the stored frontier, including removing streams it
  omits.
- `frontier` holds one mutable occurrence counter per recurring event stream; an
  advance is an UPSERT and a retired pull-request stream is releasable by
  DELETE.
- `gh_event` retains the complete immutable matcher payload, content identity,
  closed producer discriminator, per-repository event ordinal, frontier
  generation, batch ordinal, and recording time. Accepted facts are never
  updated or deleted.
- `rule` holds the active checked revision and content digest, while
  `rule_revision` retains revision history needed by module dispatch records;
  `rule_field_fingerprint` binds each identity field to its checked digest. One
  reconciliation validates a repository's complete configured set and commits
  every activation and deactivation atomically. An activation records the
  repository event tail after which that revision may evaluate facts, and
  deactivation retains its lineage. The highest recorded revision of each
  retired identity is permanent; only lower inactive revisions not named by a
  retained dispatch are releasable.
- `dispatch_ledger` records command identity, dispatch reference, rule revision,
  source event, command family, an opaque core encoding of the exact checked
  payload, and settlement.
- `webhook_delivery`, `webhook_body`, and `webhook_disposition` retain one
  authenticated delivery under its caller-selected expiry.
- `core_event_cursor` records module application progress.
- `rule_evaluation_cursor` records each rule revision's last evaluated
  repository event, including nonmatches and suppressed dispatches.

The repository-event reducer evaluates the existing checked rule matcher and
returns one batch with a fresh dispatch identity for each matching rule. It
turns each dispatch action into a `create_session` command through a core-owned
factory. The reducer forces the resulting session to start held and owned with
an external finish condition. Core supplies the command payload and therefore
mints core identities.

The convergence sweep is a periodic pass that runs beside repository watch.
Repository watch owns event-driven dispatch; the sweep supplies liveness for
watched pull requests whose provider events stopped arriving. It uses
`signalbox-convergence` for its predicate and owns its fenced commission,
durable retry and park records, and configuration throttle. It is opt-in twice:
`[repository_watch.convergence_sweep]` supplies one review-response session
template and the timing policy, and each repository lists its
`convergence_pull_requests`. The `[convergence]` policy supplies the shared
predicate described in [review workflows](review-workflows.md). The sweep
evaluates one revalidated snapshot and uses that verdict for its decision.

The daemon composes the repository-watch module when `[repository_watch]` is
configured and enabled. Dispatch actions and lifecycle reactions are retained in
strictly increasing, unique action-ordinal order. The module submitter recovers
exact retained payloads and invokes core session handlers through the daemon's
command adapter.

## Design decisions

Each repository frontier commit includes the complete current repository and
pull-request projections, expected generation, complete identity candidate, and
ordered event occurrences in one transaction. The projection retains the
complete normalized differ observation, including signal-reviewer provenance,
pull requests, workflows, and branch heads. A generation mismatch is stale
unless that complete input exactly replays the immediately succeeding commit. An
empty event batch with an unchanged cursor is unchanged only when both stored
projections also match; a projection-only change advances the generation and
records the complete commit digest. Projection timestamps use PostgreSQL
microsecond precision in both the stored comparison and commit identity. Each
newly accepted occurrence receives the next positive per-repository event
ordinal and records its producer, frontier generation, and position within that
frontier batch.

Every table is derived or module-local state and its migration declares its
growth class and release condition. The code implements no pruning pass. The
required `numeric_bounds.repository_watch_webhook_retention` duration governs
each webhook delivery's `expires_at`, measured from `received_at`. The example
configuration sets this bound to seven days; the daemon supplies no code
default.

Repository and pull-request UPSERTs replace one complete current projection.
Webhook admission writes delivery metadata, exact authenticated bytes, and a
pending disposition atomically. Reusing a delivery identity with equal content
is a replay; different content is a conflict. Settlement changes a pending
disposition exactly once. The authenticated listener admits exact payload bytes
before waking the repository task. The listener admits bodies through the
persistence ceiling of 26,214,400 bytes and rejects larger bodies with HTTP 413.
Primary intake wakes only when settlement changes a pending disposition to
applied; shadow intake settles as ignored without a wake. Equal delivery replays
return HTTP 202. A settled replay neither wakes ingestion nor changes its
disposition, including after a mode reload; a pending replay resumes intake.
Conflicting identity reuse returns HTTP 409, and storage failures return HTTP
503\. A frontier release supplies its observed generation and is stale after any
intervening frontier commit.

The module's repository task serializes polling and webhook wakes. Poll
intervals are start-to-start; a wake received during an attempt waits for that
attempt to finish and does not postpone the periodic poll deadline. Each attempt
reloads its committed comparison baseline and frontier, including compacted
merged pull requests and their head repository identities, fetches a complete
observation, and commits the differ's facts with their poll or webhook lineage.
Failed observations leave the prior committed state intact. The daemon starts
these tasks, the configured webhook listener, and one serialized command worker
beside the convergence sweep, and drains them before closing its database.

The webhook listener authenticates the configured hook identity, secret, and
repository before accepting a delivery. An empty resolved webhook secret is
unavailable. Primary hooks wake the repository task; shadow hooks acknowledge
without waking it. The runtime's `reload_configuration` reconciles rule
revisions and replaces listener settings inside the reload. Enabled rule
templates must resolve before composition or reload. Stale or conflicting rule
revisions fail reload without replacing the running configuration. Same-address
changes swap the path and hook map atomically; address changes bind a
replacement before retiring the running listener, and a bind failure preserves
the running settings. In-flight deliveries retry against the replacement
configuration.

Lifecycle reactions accept only `session_terminal` or `goal_changed` inputs and
only `release_start` or sticky-stop lifecycle commands. These are the command
forms used for convergence release and stale-work termination; no module lease
table or scheduler join exists. Each reaction in a multi-action batch has its
own one-based ordinal. A reaction naming a committed dispatch remains admissible
after its rule is deactivated; deactivation prevents only new matched
dispatches.

The module retains a command in `dispatch_ledger` before submission and applies
settlement lifecycle events to pending ledger rows. `SessionCreated` settles the
next pending create action for its repository-watch dispatch and records the new
session; replaying the event cannot settle another action. Identity reuse is
idempotent only when all retained command metadata agrees. One dispatch
reference names exactly one rule revision and event evaluation, including its
complete ordered action batch. Pending ledger rows remain recoverable without
the removed or inactive rule, and newly resolved template or configuration
values cannot replace the committed payload.

Rule evaluation starts after the active revision's activation tail and resumes
from its durable cursor. Matching facts acquire the configured singleton before
commands and evaluation progress commit together. Pull-request scope keys the
provider number; stack scope keys the root of the open base/head-branch
component; repository scope keys the repository; rule scope spans repositories.
Each key belongs to one rule revision. A live dispatch suppresses later matching
facts. Nonsticky session termination releases its action; cooldown begins when
the last action releases. A sticky stop keeps redispatch suppressed for that
key.

Goal commissioning or resumption releases the dispatched session's held start
gate. Goal achievement or a user-stopped goal issues a parent-only sticky stop.
Reactions retain their original rule and action even after configuration removes
the rule. The module commits lifecycle effects before advancing its application
cursor; the daemon acknowledges the corresponding seam event afterward.

The command adapter copies complete resolved template defaults without initial
input or repository credentials and stamps the module issuer on creation claims.
Pending submission follow-ups remain retryable after core command settlement,
including interruption of a live turn whose session is closing. Synchronous
command-identity conflicts settle as rejected before submission continues to the
next action.

## Boundary contracts

The v2 crate depends on the ownership seam as its only Signalbox dependency. It
consumes the seam's lifecycle events and emits only the seam's checked session
commands. It cannot import core persistence, qualify `public` tables, or name
another module schema.

The module retains an authenticated, HTTPS-only GitHub client for API-relative
GET requests and GraphQL observation queries. It receives no database handle.
The daemon's repository-specific client loader rereads the configured credential
file on each load and returns only an authenticated client handle. Credential
and client-construction failures have distinct redacted error classes.

The module's dedicated PostgreSQL login role owns `mod_repo_watch`, has no
membership path back to the core identity, and has no table privileges in
`public`. Module SQL uses an unqualified search path confined to its schema.
Core and other modules receive no privileges on the module tables.

A created session indexes its retained rule revision, event, dispatch, and
action ordinal, so lifecycle reaction planning survives rule removal and process
restart. Equal evaluation recovery finds the retained batch before considering
newly reserved dispatch or command identities. A lifecycle reaction targets the
session named by its trigger. A synchronous create-session command-identity
conflict is recorded as rejected on its retained action; an applied creation
settles from its `SessionCreated` event.

Contracts this page relies on but does not own: module-state pruning and outbox
retention permission in [persistence protocol](persistence-protocol.md), session
command behavior, and the module event/command/database boundary in the
[ownership seam](ownership-seam.md).

## Planned

Repository-watch dispatch provenance is planned in the
[repository watch design](../design/repo-watch.md).

Restart-persistent poll caching is planned in the
[repository watch design](../design/repo-watch.md).

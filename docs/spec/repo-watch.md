# Repository watch

Repository watch has a checked domain vocabulary and a compiled-in v2 ownership
module. The module is not dispatched: the daemon starts no repository-watch
poller, webhook listener, convergence sweep, command worker, lease-expiry task,
or operator projection route. Enabling dispatch requires an owner-approved code
change that composes a worker; there is no latent runtime switch.

## Domain vocabulary

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

Each repository frontier commit includes the complete current repository and
pull-request projections, expected generation, complete identity candidate, and
ordered event occurrences in one transaction. A generation mismatch is stale
unless that complete input exactly replays the immediately succeeding commit.
An empty event batch with an unchanged cursor is unchanged only when both stored
projections also match; a projection-only change advances the generation and
records the complete commit digest.

## Ownership boundary

The v2 crate depends on the ownership seam as its only Signalbox dependency. It
consumes the seam's lifecycle events and emits only the seam's checked session
commands. It cannot import core persistence, qualify `public` tables, or name
another module schema.

The module retains an authenticated, HTTPS-only GitHub client for API-relative
GET requests. It is an external-I/O capability and receives no database handle.

The module's dedicated PostgreSQL login role owns `mod_repo_watch`, has no
membership path back to the core identity, and has no table privileges in
`public`. Module SQL uses an unqualified search path confined to its schema.
Core and other modules receive no privileges on the module tables.

Initial dispatch actions and lifecycle reactions are retained and submitted in
strictly increasing, unique action-ordinal order. A created session indexes its
retained rule revision, event, dispatch, and action ordinal, so lifecycle
reaction planning survives rule removal and process restart. Equal evaluation
recovery finds the retained batch before considering newly reserved dispatch or
command identities. A lifecycle reaction targets the session named by its
trigger. Before submitting a later action, the dispatcher records a synchronous
create-session command-identity conflict as rejected; an applied creation
settles from its `SessionCreated` event.

The module schema contains twelve tables:

- `repository_state` and `pr_state` are mutable provider-state projections. A
  repository row fences complete frontier commits with a generation and the
  digest of the last candidate; a generation mismatch is stale unless the
  complete frontier and ordered event batch exactly replay the immediately
  succeeding commit. A candidate equal to the stored frontier advances no
  generation when it carries no events and is rejected when it carries events. A
  candidate that omits a retained stream is stale and changes nothing.
- `frontier` holds one mutable occurrence counter per recurring event stream; an
  advance is an UPSERT and a retired pull-request stream is releasable by
  DELETE.
- `gh_event` retains normalized event identity, target, recording time, and a
  caller-selected `retain_until`; a row is releasable after that boundary when
  no pending module command names it.
- `rule` holds the active checked revision and content digest, while
  `rule_revision` retains revision history needed by module dispatch records;
  `rule_field_fingerprint` binds each identity field to its checked digest.
- `dispatch_ledger` records command identity, dispatch reference, rule revision,
  source event, command family, an opaque core encoding of the exact checked
  payload, and settlement.
- `webhook_delivery`, `webhook_body`, and `webhook_disposition` retain one
  authenticated delivery under its caller-selected expiry.
- `core_event_cursor` records module application progress.

Every table is derived or module-local state and its migration declares its
growth class and release condition. The code implements no pruning pass and
selects no retention duration.

## Reducer and dispatch ledger

The repository-event reducer evaluates the existing checked rule matcher and
turns each dispatch action into a `create_session` command through a core-owned
factory. The reducer forces the resulting session to start held and owned with
an external finish condition. Core supplies the command payload and therefore
mints core identities.

Lifecycle reactions accept only `session_terminal` or `goal_changed` inputs and
only `release_start` or sticky-stop lifecycle commands. These are the command
forms used for convergence release and stale-work termination; no module lease
table or scheduler join exists. Each reaction in a multi-action batch has its
own one-based ordinal. A reaction naming a committed dispatch remains admissible
after its rule is deactivated; deactivation prevents only new matched
dispatches.

The module records a command in `dispatch_ledger` before submission and applies
settlement lifecycle events to pending ledger rows. `SessionCreated` settles the
next pending create action for its repository-watch dispatch and records the new
session; replaying the event cannot settle another action. Identity reuse is
idempotent only when all retained command metadata agrees. One dispatch
reference names exactly one rule revision and event evaluation, including its
complete ordered action batch. Recovery reads pending ledger rows without the
removed or inactive rule, then core decodes and resubmits the exact retained
payload rather than newly resolved template or configuration values.

## Ingest

Repository and pull-request UPSERTs replace one complete current projection.
Webhook admission writes delivery metadata, exact authenticated bytes, and a
pending disposition atomically. Reusing a delivery identity with equal content
is a replay; different content is a conflict. Settlement changes a pending
disposition exactly once.

Repository watch has no dedicated runtime, webhook listener, convergence task,
operator routes, or public-schema persistence surface. Its durable state is the
module-owned projection and command ledger described above.

Contracts this page relies on but does not own: module-state pruning and outbox
retention permission in [persistence protocol](persistence-protocol.md), session
command behavior, and the module event/command/database boundary in the
[ownership seam](ownership-seam.md).

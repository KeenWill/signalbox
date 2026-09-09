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

Provider-keyed `ChecksCompleted`, `CheckRunCompleted`, and `ReviewSubmitted`
identities frame the repository, pull-request number and head SHA, event kind,
and provider suite or run ID with completion generation, or review ID; mutable
pull-request presentation and payload fields are excluded. Check-run occurrence
sequences distinguish later conclusion changes.

Rules are versioned `RepoWatchRule` values. Fields within one matcher are
conjunctive and rules are evaluated independently. The checked matcher owns the
repository, event-kind, pull-request context, label, draft, author,
mergeability, and conclusion predicates. A rule carries a nonempty ordered
action list, singleton scope, and cooldown. Its content digest covers its full
versioned semantics.

The module schema contains sixteen tables:

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
- `webhook_pull_wake` coalesces pending PR observations and retains failed
  attempt counts and the last failure.
- `poll_cache_reviewers` and `poll_cache_page` retain the selected reviewer set
  and accepted conditional REST snapshots.
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
evaluates one revalidated snapshot and uses that verdict for its decision. After
restoring a parked session, re-enrollment waits for scheduler nudge capacity.
The target retains the session reference until that handoff is acknowledged, so
restart retries an interrupted handoff. Removed targets hand their restored
sessions to a task that waits for nudge capacity without blocking startup
recovery. The task clears each removed-target handoff after its nudge is
retained.

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
growth class and release condition. Observation commits prune expired compact
merged baselines. The required
`numeric_bounds.repository_watch_webhook_retention` duration governs each
webhook delivery's `expires_at`, measured from `received_at`. The example
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

Operator status reports each configured repository's last successful
observation, last poll start and outcome, last newly accepted webhook delivery,
and events recorded in the current process. Poll evidence includes
webhook-triggered fetches. These measurements reset at process startup and
survive configuration reloads; replayed events and deliveries do not advance
their counts or delivery timestamp.

The module's repository task serializes polling and webhook wakes. Poll
intervals are start-to-start; a wake received during an attempt waits for that
attempt to finish and does not postpone the periodic poll deadline. Each attempt
reloads its committed comparison baseline and frontier, including compacted
merged pull requests and their head repository identities, and commits the
differ's facts with their poll or webhook lineage. Terminal pull requests leave
the ordinary baseline when observed; merged subjects retain a compact baseline
until `numeric_bounds.repository_watch_webhook_retention` elapses from their
merge time, and discussion reads run only for open subjects. Compact entries
missing their merge time are dropped without discarding the ordinary
predecessor, dated compact entries, or event frontier. Workflow reads query
completed runs by distinct current head SHA for the default branch and open
pull-request same-repository head branches; prior completions for those branches
remain comparison input. Each observation admits at most 1,000 REST and GraphQL
requests combined; exhausting that budget rejects the incomplete observation.
Check inventories exceeding GitHub's 1,000-suite commit limit and workflow
searches exceeding GitHub's 1,000-result cap also reject the observation. Failed
observations leave the prior committed state intact. The daemon starts these
tasks, the configured webhook listener, and one serialized command worker beside
the convergence sweep, and drains them before closing its database.

The webhook listener authenticates the configured hook identity, secret, and
repository before accepting a delivery. An empty resolved webhook secret is
unavailable. Primary pull-request, review, and check deliveries durably coalesce
their named pull requests in `webhook_pull_wake` and wake the repository task.
Each queued pull request is fetched and admitted independently against the
committed baseline; the command worker evaluates its events without waiting for
a poll. For an unseen PR, the final snapshot also derives its labels, reviews,
threads, and completed checks independently of the observation source. Durable
facts at the comparison generation distinguish reopened PRs from unseen PRs;
reopening does not synthesize their existing snapshot facts. A drain with any
failed targeted read reports a partial outcome. Queue storage failures report
`store_failed`. Successful admission clears only the consumed delivery; a newer
delivery remains pending. Failed observations retain their failure and attempt
count. After three failed attempts, that row is skipped until a new delivery
resets it. Other queued PRs continue; periodic polls reconcile repository-wide
state independently. Startup wakes resume eligible pending rows. Shadow hooks
acknowledge without queuing or waking. The runtime's `reload_configuration`
reconciles rule revisions and replaces listener settings inside the reload.
Enabled rule templates must resolve before composition or reload. Stale or
conflicting rule revisions fail reload without replacing the running
configuration. Same-address changes swap the path and hook map atomically;
address changes bind a replacement before retiring the running listener, and a
bind failure preserves the running settings. In-flight deliveries retry against
the replacement configuration.

Lifecycle reactions accept `session_terminal`, `goal_changed`, and retained
`pull_request_closed` or `pull_request_merged` facts and emit only
`release_start` or sticky-stop lifecycle commands. These are the command forms
used for convergence release and stale-work termination; no module lease table
or scheduler join exists. Each reaction in a multi-action batch has its own
one-based ordinal. A reaction naming a committed dispatch remains admissible
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
values cannot replace the committed payload. A recorded checkout head settles
provisioning replay without configuration. Checkout recovery resolves the
credential from configuration using the retained repository identity. An
unconfigured repository terminalizes the dispatch as `repository_unconfigured`
and closes its held session through a parent-only nonsticky lifecycle stop.

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
A close or merge fact with a repository event ordinal after the dispatch event
issues a parent-only sticky stop for its live dispatched session, with
`pull_request_closed` or `pull_request_merged` retained as the ledger reason; an
already terminal session is left alone. Reactions check durable core terminal
facts before recording retirement or submitting its stop, including facts still
pending at the module cursor. Retirement scans retain discovered terminal times
on the dispatch ledger. A queued retirement whose session has ended is rejected
locally as `session_already_terminal`. Reactions retain their original rule and
action even after configuration removes the rule. The module commits lifecycle
effects before advancing its application cursor; the daemon acknowledges the
corresponding seam event afterward.

Pull-request dispatch atomically creates the session with a provisioning hold
that public start and ownership releases cannot clear. Input accepted during
provisioning remains queued until the checkout head is recorded and the command
adapter clears the provisioning hold and releases the start gate, nudging turn
eligibility.

The command adapter copies complete resolved template defaults without initial
input or repository credentials and stamps the module issuer on creation claims;
pull-request dispatch provisions the watched repository at the session's derived
workspace root, on the retained head branch and SHA, before completing held
creation. Checkout execution clones the process runner pinned by the daemon's
startup tool composition. The ledger records checkout path `.` and the
provisioned SHA. Git uses the polling credential only in its invocation
environment, scoped to the watched repository URL; redirects are refused and
fork heads are fetched unauthenticated. Provisioning failure retires the
dispatch as `checkout_provisioning_failed` with the failing step and exit status
and stops the session. Retired dispatches and terminal sessions lose their
provisioned checkout even when disabled (at next startup if no runtime exists);
startup scavenges retained checkouts awaiting removal even with pending
submissions or no repository-watch configuration. The ledger retains the
provisioning workspace root and core session identity before filesystem work;
cleanup uses them without waiting for `SessionCreated` settlement or consulting
current configuration. Cleanup includes locations retained before checkout
recording. Settled cleanup prevents repeated provisioning. The ledger records
creation ownership and the directory device/inode before publishing a new
directory at the session path. Unpublished staging names derive from the
dispatch id and permit empty-directory cleanup before ownership recording;
cancellation leaves staging for replay or cleanup. Replay resumes staged
provisioning only when its identity matches the retained identity. Before
publication on Linux, provisioning writes the dispatch id to the directory's
`user.signalbox.dispatch` extended attribute; `.git/signalbox-dispatch` replaces
that evidence after clone. Cleanup checks the attribute while present and the
file marker otherwise. Directories not created by the dispatch are neither
adopted nor removed. Removal verifies retained identities before changing
permissions; both original and renamed paths require the matching dispatch
marker before traversing contents. A missing or mismatched identity leaves
removal pending. If that pathname is absent, cleanup searches its direct
siblings under the derived session parent for the retained device/inode and
matching dispatch marker; without a matching marker it settles without deleting.
The parent is retained. The removal migration settles existing provisioned rows
without a retained location. Removal restores owner search permission before
marker lookup, owner read permission on the marker, and owner directory
permissions before traversal, and refuses mount crossings; it reports
unsupported outside Linux and leaves cleanup pending. Pending submission
follow-ups remain retryable after core command settlement, including
interruption of a live turn whose session is closing. Synchronous
command-identity conflicts settle as rejected before submission continues to the
next action.

Dispatched pull-request sessions whose watched repository configures
`push_credential_file` can use `git_push_configured` for their retained head
branch on `origin` at `https://github.com/<owner>/<repo>.git`; fork heads are
unavailable because that destination is the watched repository.

## Boundary contracts

The v2 crate depends on the session ownership crate as its only Signalbox
dependency. It consumes the seam's lifecycle events and emits only the seam's
checked session commands. It cannot import core persistence, qualify `public`
tables, or name another module schema.

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
newly reserved dispatch or command identities. A core lifecycle reaction targets
the session named by its trigger. A synchronous create-session command-identity
conflict is recorded as rejected on its retained action; an applied creation
settles from its `SessionCreated` event.

Contracts this page relies on but does not own: module-state pruning and outbox
retention permission in [persistence protocol](persistence-protocol.md), session
command behavior, and the module event/command/database boundary in the
[session ownership](session-ownership.md).

Reload stops and joins ingestion and active convergence attempts before rule
activation, then publishes the selected catalogs before reconciling convergence
targets, using an empty target set when disabled. Disabling repository watch
stops and joins its pollers, listener, and sweep, retaining checkout cleanup;
enabling composes them from the retained configuration. The runtime replaces
polling tasks when repositories, intervals, credential paths, or signal
reviewers change. Listener removal stops and joins its server; same-address
reload swaps routing atomically, and an address change binds the replacement
before retiring the old listener. Bind refusal leaves the running listener
intact.

Each bounded canonical poll resource or page key retains its HTTP validators and
an accepted typed snapshot sufficient to reconstruct its normalized contribution
and nested-fetch identities. This transport state is separate from events and
rules and contains no raw provider JSON, credential values, or reactions from
actors outside the configured signal-reviewer set. Before every poller
composition, including startup and re-enablement, the runtime compares the
persisted reviewer set with configured signal reviewers and invalidates both
validators and snapshots when they differ. After restart with an unchanged set,
the first complete poll sends conditional requests for every traversed resource
with a persisted validator. After a complete accepted observation, cache
retention removes untraversed resources and terminal pull-request pages;
unchanged responses retain their traversed pages.

Dispatched sessions retain the repository-watch creation cause, module actor,
and dispatch reference. Their provenance resolves the existing dispatch ledger
row and its created session, rule revision, event, action ordinal, repository,
and optional pull request, without a second stored copy of the dispatch or
session identity. Transcript snapshots and browser session descriptors project
that retained origin as soon as creation commits, before ledger settlement; the
browser workspace displays it. Held creation submits no initial input and
creates no turn.

## Planned

- Repository-watch orchestration through [workflows](../design/workflows.md)
  ([design](../design/repo-watch.md)).

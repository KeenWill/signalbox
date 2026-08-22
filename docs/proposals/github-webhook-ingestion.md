# GitHub webhook ingestion for repository watch

Status: proposed decision; design only.

## Placement and authority

The repository has no proposal-page convention, so this page starts
`docs/proposals/`. It is a decision for review, not implemented behavior, and
does not change [`docs/spec/repo-watch.md`](../spec/repo-watch.md). The bottom
implementation pull request will update that owning specification when the
approved behavior exists.

This proposal assumes GitHub.com repository webhooks, one development daemon,
PostgreSQL shared with repository watch, and a host with no ordinary public
ingress. Public reachability is deployment-owned. The daemon knows nothing about
Tailscale, tunnels, relays, or exposure vendors.

## Decided architecture

The daemon adds a plain HTTP listener on a configured socket address. That
listener admits GitHub webhook requests, verifies their signatures, durably
deduplicates delivery GUIDs, and queues accepted bodies for asynchronous
processing. A deployment exposes the listener to GitHub; Tailscale Funnel is the
reference recipe for the single development host.

Webhook processing is a second producer of the existing `repo_watch_event`
stream. It uses the same normalized event construction and content-derived
identity as polling, and it advances `repo_watch_cursor` in the transaction that
appends an event batch. The existing rule evaluator and dispatcher cannot tell
which producer won except through audit metadata.

Polling remains in two forms:

- a slow complete reconciliation sweep that verifies every normalized projection
  and recovers current state after missed deliveries; and
- coalesced targeted queries for mergeability and aggregate check rollups,
  triggered immediately by relevant webhooks.

Rollout starts in shadow mode with today's poll cadence unchanged. Webhook
deliveries are verified and projected, but only polling writes cursors and
events. A parity view compares the two producers by content identity. Webhook
writes enable only after a real workday of representative activity has no
unexplained parity mismatch; only then does the complete poll move to its slow
cadence.

The accepted tradeoff is that webhook availability belongs to the deployment. If
the host, Funnel, another exposure mechanism, or the local listener is down, the
daemon receives nothing until reachability returns. Full polling remains the
correctness fallback; it cannot reconstruct every transient occurrence.

## Verified current identity and required correction

The current implementation does **not** yet have the content-derived event
identity required by this decision. The exact current mechanism is
`UuidV7RepoWatchEventIdGenerator`; its `next_event_id()` constructs
`RepoWatchEventId` with `Uuid::now_v7()`. The `repo_watch_event` table uniquely
keys that random UUID and separately keys
`(repository, cursor_generation, event_ordinal)`.
`PostgresRepoWatchStore::exact_replay` compares a candidate and event batch
including those generated IDs. None of those identities is derived from event
content.

Implementation therefore begins by adding the exact shared identity
`RepoWatchEventContentIdentityV1`. It is a 32-byte SHA-256 digest over a
length-framed, domain-separated `RepoWatchEventOccurrenceV1`:

1. the literal domain `signalbox/repo-watch/event-content-identity/v1`;
2. repository and event version;
3. the event kind and only its stable occurrence payload members, excluding
   incidental current-context fields such as title, body, labels unrelated to
   the transition, draft state, and author;
4. the event kind's source-independent stream key; and
5. that stream's next positive occurrence sequence from the canonical cursor.

The occurrence key prevents two equal-looking facts in one history from
collapsing. It is closed by event kind:

- PR lifecycle, head, label, and context streams use PR number, kind, and the
  label or field identity where applicable;
- check suites use provider suite ID and suite `updated_at`;
- check runs use provider run ID and `completed_at`;
- reviews use provider review ID and submitted time;
- threads use provider thread ID;
- workflows use workflow ID, run ID, and run attempt;
- branch advances use branch; and
- mergeability and reactions use PR plus their subject identity.

The occurrence frame retains the complete target and payload on the admitted
event row, but those incidental snapshot fields do not enter content identity.
Both producers allocate `last_sequence + ordinal_within_stream` while deriving a
batch, so two results for one stream in one batch cannot receive the same
sequence. The resulting per-stream frontiers advance atomically with the cursor
and event batch. Both producers reading the same stream frontier derive the same
next sequences; a later equal transition therefore has a different identity,
while a racing webhook and poll do not. Same-stream multi-event fixtures are a
required prerequisite. Provider keys distinguish independent immutable facts.
Both transports must possess the same occurrence members or request a targeted
refresh rather than invent them.

`RepoWatchEventId` remains the UUID reference used by dispatch and audit rows. A
candidate UUID is retained only by the row that wins content-identity insertion.
A unique `(content_identity_version, content_identity)` constraint, not the
UUID, deduplicates producers. The poller and webhook worker both call the same
identity function; neither has a transport-specific fallback. This prerequisite
is a behavior and schema change for the implementation stack, not a claim about
the current tree. `PostgresRepoWatchStore::exact_replay` and event-batch
equality likewise compare content identity version, content identity, ordering
position, and the other stable event fields; candidate `RepoWatchEventId` values
are excluded because a replay allocates new candidates.

## Existing pipeline seam

Today `RepositoryWatchTask` builds a complete `RepoWatchObservation`, invokes
`derive_repo_watch_events`, and submits one optimistic `RepoWatchCommitRequest`.
`PostgresRepoWatchStore::commit` inserts `repo_watch_cursor` and its ordered
`repo_watch_event` rows atomically. The
`repo_watch_event_requires_cursor_commit` trigger prevents later event inserts
against an old cursor generation.

Webhook processing preserves that seam. It decodes one admitted delivery into a
guarded patch of the latest canonical observation, calls the existing differ,
derives `RepoWatchEventContentIdentityV1` for each result, and submits the same
commit shape. It never writes a rule evaluation, creates a session, invokes an
action, or bypasses complete pull-request context.

The `repo_watch_event` schema requires that context: PR number, current head SHA
and repository, base and head branch, title, body, complete canonical labels,
draft state, and optional author. Payload columns are closed by `event_kind`.
The webhook producer must satisfy the same constructors and database checks as
the poller.

## Boundary frames

The design touches these exact frames. “Frame” means a typed boundary value, not
a process-protocol message.

1. `GitHubWebhookHttpV1`: method, configured path, selected GitHub headers, and
   exact bounded body bytes. HTTP admission and signature verification own it.
2. `RepoWatchWebhookDeliveryV1`: repository, hook ID, delivery GUID, event,
   optional action, receipt sequence, body digest, and exact body reference.
   Action is required only for event families whose GitHub payload contract
   defines it; `push` and `ping` are actionless. It contains no session or
   credential.
3. `RepoWatchObservationPatchV1`: closed monotonic additions, guarded
   replacements, and targeted-refresh hints decoded from one delivery.
4. `RepoWatchEventOccurrenceV1`: source-independent members needed to identify
   one normalized fact occurrence.
5. `RepoWatchEventContentIdentityV1`: the shared digest of the occurrence frame.
6. Existing `RepoWatchObservation`, `RepoWatchEvent`, `PullRequestContext`, and
   `BranchContext` remain the state, fact, and downstream dispatch frames.

The listener is a separate supervised runtime from the process-protocol socket
and the repository poll tasks. GitHub-originated bytes never become
process-protocol, session, tool, or credential authority.

## Exposure is deployment-owned

### Reference recipe: Tailscale Funnel

The reference deployment binds the configured daemon listener to
`127.0.0.1:<port>`, assigns one fixed webhook path, and uses `tailscale funnel`
to expose only that loopback HTTP target over a public `*.ts.net` HTTPS URL.
GitHub is configured with that HTTPS URL, JSON payloads, SSL verification, a
repository-specific secret, and only the required events.

The deployment recipe is:

```text
tailscale funnel --bg --set-path=/github/webhooks http://127.0.0.1:<port>
```

The GitHub payload URL is the HTTPS Funnel URL with `/github/webhooks` appended.

Funnel is public internet exposure, not tailnet-only Tailscale Serve. The
deployment must restrict the tailnet `funnel` node attribute, keep the daemon
backend on loopback, and publish no health, metrics, process-protocol, or
administrative route on the Funnel port. The URL path is routing, not
authentication. Funnel terminates TLS on the node before forwarding plain HTTP
to the configured listener; signature verification remains end-to-end payload
authentication.

### Rejected alternatives

A generic reverse tunnel is not the reference because its account and tunnel
token become ingress controls, it may terminate TLS off-host, and a plain tunnel
does not improve availability. A small durable relay would buffer host outages,
but adds a public service, a second copy of every webhook secret, payload
storage outside the host, and another trusted component able to forge events.
The accepted decision keeps those costs out of this slice and accepts
deployment-owned availability.

The daemon does not encode Funnel commands, hostnames, forwarded-header rules,
or a provider selector. A later managed-exposure interface may own provisioning,
health, and durable buffering if availability requirements change. It must be a
separate reviewed boundary; this design adds no exposure-provider abstraction.

## HTTP admission and signature verification

Configuration supplies a listener address, one path, and one webhook-secret file
per watched repository. The polling token and webhook secret are distinct
references and cannot alias. Credential files follow the existing bounded-read,
rotation, redaction, absolute-path, and per-repository rules.

The listener first performs a bounded `X-GitHub-Hook-ID` lookup that must select
exactly one configured watched repository and its webhook secret. HMAC
verification uses only that selected secret; after verification, the bounded
envelope parser requires the body repository `full_name` to match that selected
repository. This is the repository-specific credential boundary owned by
[`docs/spec/repo-watch.md`](../spec/repo-watch.md#designed-for-version-two-webhook-transport).

The listener accepts `POST` only. It rejects unsupported transfer/content
encodings, duplicated singleton headers, malformed or over-limit declared
lengths, and any streamed body crossing the reviewed hard ceiling. It reads the
exact bytes once while computing HMAC-SHA-256, requires one canonical
`X-Hub-Signature-256: sha256=<64 lowercase hex>` header, and compares the
decoded 32-byte MAC in constant time before JSON parsing. These exact-body and
bounded-input rules are owned by
[`docs/spec/repo-watch.md`](../spec/repo-watch.md#designed-for-version-two-webhook-transport).

After signature success, admission requires:

- a canonical `X-GitHub-Delivery` UUID;
- a positive configured `X-GitHub-Hook-ID`;
- a bounded ASCII `X-GitHub-Event`;
- `application/json` content type;
- a body repository `full_name` equal to the selected configured repository; and
- a subscribed event name, with `ping` used only for health proof; and
- an action only when that event family's payload contract defines one.

An authenticated, repository-bound delivery with an unknown event or action is
admitted to durable intake and later receives the `ignored` disposition. It is
not rejected before that disposition can be recorded.

The listener never trusts source IP, `Forwarded`, or `X-Forwarded-*` for
authentication. A deployment may allowlist GitHub's published hook addresses as
defense in depth, but HMAC remains mandatory. Logs contain identifiers, sizes,
status, and bounded codes only—never secrets, signatures, bodies, PR text, or
rendered JSON.

The endpoint responds `202` only after durable local intake. Before that
response, it parses only a bounded authenticated envelope sufficient to obtain
the body repository and optional action and to perform replay-conflict checks.
Full payload mapping, observation patching, GitHub queries, cursor commits, and
dispatch happen asynchronously, outside GitHub's ten-second response window.

## Replay protection and local intake

`(hook_id, delivery_id)` is the delivery replay key. The selected repository is
stored as conflict-checked data rather than as part of that key, so reuse across
repositories remains detectable. GitHub retains the GUID on explicit redelivery.
An equal duplicate returns `202` without a second queue item. The same key with
different repository, event, optional action, or body digest retains the first
record, rejects the request, and raises a metadata-only security signal.
Fixtures cover each conflicting field.

Delivery GUID tombstones and body digests remain permanent. Exact bodies remain
until terminal processing and for seven days, then an operational sweep may
delete them. Receipt time is audit data, not a freshness proof: GitHub's HMAC
does not cover a timestamp.

One worker processes each repository's accepted sequence. GitHub may deliver
events out of order, so receipt order is not provider chronology. Each patch
uses the strongest available generation; an older replacement is `superseded`,
an equal state is `duplicate_state`, and a change with no safe freshness guard
becomes a targeted query.

## Tables and view

The implementation touches these exact existing tables:

- `repo_watch_cursor`: written by either producer with the complete resulting
  canonical observation; its current shape otherwise remains.
- `repo_watch_event`: adds `content_identity_version`, `content_identity`, and
  `producer`, where producer is `poll` or `webhook`; the content identity is
  unique and every existing closed event column remains authoritative.
- `repo_watch_rule_activation` and `repo_watch_rule_evaluation`: unchanged
  consumers of the event frontier.
- `repo_watch_dispatch_batch`, `repo_watch_dispatch_action`,
  `repo_watch_dispatch_delivery_intent`, and `repo_watch_dispatch_delivery`:
  unchanged downstream audit and delivery tables.

It adds exactly four intake/shadow tables:

- `repo_watch_webhook_delivery`: permanent delivery identity, headers, body
  digest, receipt sequence/time, and repository.
- `repo_watch_webhook_payload`: exact bounded body bytes, deletable only after
  terminal disposition and retention.
- `repo_watch_webhook_projection`: shadow or write-mode candidate ordinal,
  projection kind (`event` or `targeted_query`), nullable
  `RepoWatchEventContentIdentityV1`, closed event encoding, and occurrence-key
  encoding for each projected fact or refresh hint.
- `repo_watch_webhook_disposition`: one append-only terminal result per
  delivery—`projected`, `committed`, `duplicate_state`, `superseded`, `ignored`,
  or `quarantined`—plus resulting cursor generation when applicable.

The read-only `repo_watch_webhook_parity` view full-outer-joins event
projections and poll-written `repo_watch_event` rows on
`(content_identity_version, content_identity)`, while refresh hints form
`not_directly_mapped` rows. It exposes repository, delivery, event/action,
projected kind, poll event ID and position, latencies, and one status:
`matched`, `webhook_only`, `poll_only`, or `not_directly_mapped`. It does not
influence commits or dispatch.

In shadow mode, polling remains the only event writer and
`repo_watch_webhook_projection` records what the webhook producer would have
written from the same cursor frontier. In write mode, insertion of the unique
content identity decides the winning producer. The cursor/event transaction also
records webhook disposition; a crash retries only a delivery without a terminal
disposition.

## Webhook mapping

Every adapter produces a guarded observation patch or a targeted query. Unknown
events/actions are durably ignored, not guessed.

| GitHub delivery                                      | Direct observation change and existing event                                                                        |
| ---------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------- |
| `pull_request: opened`, `reopened`                   | Hydrate/add or reopen complete PR context -> `PullRequestOpened`; targeted mergeability supplies the required state |
| `pull_request: closed`                               | Set closed or merged -> `PullRequestClosed` or `PullRequestMerged`                                                  |
| `pull_request: synchronize`                          | Guarded head replacement -> `HeadChanged`; schedule mergeability and rollup queries                                 |
| `pull_request: labeled`, `unlabeled`                 | Replace canonical complete labels -> `Labeled` or `Unlabeled`                                                       |
| Other context-changing `pull_request` actions        | Update title, body, draft, author, or branches; no new kind                                                         |
| `pull_request_review: submitted`                     | Union provider review identity -> `ReviewSubmitted`                                                                 |
| `pull_request_review: dismissed`                     | Preserve version-one no-dismissal behavior; no event                                                                |
| `pull_request_review_thread: resolved`, `unresolved` | Guard thread state -> `ThreadResolved` or `ThreadOpened`                                                            |
| `check_run: completed`                               | Union provider run completion -> `CheckRunCompleted`; schedule aggregate rollup query                               |
| `check_suite: completed`                             | Schedule aggregate rollup query; no direct aggregate event                                                          |
| `workflow_run: completed`                            | Guard workflow/run/attempt -> `BranchWorkflowRunCompleted`                                                          |
| non-deletion `push` on `refs/heads/*`                | Guard branch head -> `BaseAdvanced` for affected open PRs                                                           |
| deletion `push` on `refs/heads/*`                    | Remove that branch-head projection; never store the all-zero `after` sentinel or synthesize `BaseAdvanced`          |
| `ping`                                               | Record endpoint health only                                                                                         |

A PR patch preserves prior checks, reviews, threads, and reactions because a PR
payload is not their complete projection. A first-seen reopened PR is hydrated
before commit. A check run without an unambiguous current-head match becomes a
targeted query. A workflow event is direct only when its head repository is the
watched repository.

## Polling responsibilities

Mergeability remains queried because GitHub calculates it asynchronously and no
webhook is an authoritative transition. `opened`, `reopened`, and `synchronize`
schedule one coalesced PR query. Only its normalized result may emit
`MergeableStateChanged`.

Aggregate check rollups remain queried because deliveries can be missing,
reordered, rerequested, or incompletely associated with fork heads. Relevant PR,
check-run, and check-suite deliveries coalesce one head-SHA refresh. Only the
complete suite/run projection may emit `ChecksCompleted`; an individual
`check_run: completed` may still emit `CheckRunCompleted` directly.

Reaction state remains full-poll-only because GitHub exposes no matching
webhook. Complete reviews, threads, branch heads, checks, workflows, lifecycle,
and context remain in the slow sweep as verification and missed-state recovery.

Targeted queries use the existing per-repository polling credential, conditional
requests, and bounded request deadlines, including the existing 30-second client
timeout. Network requests execute outside the repository lock. A response is
applied under the lock only after its captured repository generation still
matches; otherwise it is discarded or re-derived. Their failures delay only
dependent facts and do not stop webhook intake. The complete poll retains its
current cadence throughout shadow mode, then moves to one hourly sweep after the
parity gate. That interval is the simplest rollout default and remains
configurable.

## Ordering and cross-producer deduplication

One repository lock serializes webhook patches, targeted queries, and complete
polls. Two producers may derive from cursor generation N, but only one can
commit N+1. The loser reloads and re-derives. If both observed the same
occurrence from the same prior observation, they produce the same
`RepoWatchEventContentIdentityV1`; the unique constraint admits one event. If
the winner already advanced normalized state, the loser emits nothing.

`repo_watch_event` remains ordered by `(cursor_generation, event_ordinal)` and
means Signalbox observation order. Neither delivery receipt order nor GitHub
payload timestamps rewrite committed order. Random `RepoWatchEventId` remains a
reference allocated to the one admitted row, never the deduplication key.

A targeted result updates only its resource. A full reconciliation may replace a
complete projection, but it preserves a newer provider occurrence learned by
webhook when the response cannot prove that occurrence absent. A writer that
cannot prove non-regression fails closed and leaves the delivery retryable.

## Failure modes and fallback

| Failure                                  | Behavior and fallback                                                                            |
| ---------------------------------------- | ------------------------------------------------------------------------------------------------ |
| Host, exposure path, or listener down    | GitHub delivery fails; complete polling continues and deployment owns restoration                |
| PostgreSQL down                          | Listener does not return `202`; polling commits also fail until storage returns                  |
| Invalid signature or repository mismatch | Reject before mapping; polling is unaffected                                                     |
| Duplicate or redelivered GUID            | Equal delivery returns `202`; conflicting content is rejected and alerted                        |
| Out-of-order delivery                    | Generation guard suppresses it or triggers a targeted query                                      |
| Unknown payload shape                    | Quarantine without event synthesis; full sweep verifies current state                            |
| Poll credential rate-limited             | Direct webhook facts continue; mergeability, rollups, reactions, and reconciliation become stale |
| Webhook secret compromised               | Disable that hook, rotate its secret, and operate poll-only during recovery                      |
| Both webhook and polling unavailable     | No new facts commit; alert as total repository-ingress outage                                    |

GitHub does not automatically retry failures. An operator may redeliver a failed
delivery from GitHub's UI or API during its three-day retention window; this is
a deployment runbook, not daemon tunnel management. The slow full sweep
reconstructs current state and retained provider identities after longer gaps.
It cannot reconstruct an add followed by remove, resolve followed by unresolve,
or another transient sequence completed entirely during the outage. That loss is
the accepted availability cost.

Disabling the listener or webhook writer restores poll-only operation. Already
committed webhook events remain ordinary immutable facts and require no rewrite.

## Shadow parity gate and rollout

1. Land `RepoWatchEventContentIdentityV1` and make the existing poller populate
   it. Back it with the unique `repo_watch_event` identity constraint and
   content-identity fixtures before adding a second producer.
2. Add the local listener, four intake/shadow tables, parity view,
   configuration, and targeted query scheduler behind disabled webhook write
   mode.
3. Bind the listener to loopback, deploy the documented Funnel recipe, configure
   per-repository secrets/events, and complete one full poll before enabling the
   hook.
4. Enable shadow mode. Verify, store, and project webhooks while polling alone
   writes at its unchanged cadence. Exercise invalid HMAC, replay, reordering,
   listener outage, database outage, and redelivery.
5. Observe one real workday containing ordinary PR lifecycle, pushes, labels,
   reviews, threads, checks, and workflow completions. The gate requires no
   unresolved `webhook_only` or `poll_only` row for a directly mapped event;
   `not_directly_mapped` must be explained by the table above.
6. Enable webhook writes per repository while retaining the current polling
   cadence for another verification window. A mismatch disables only that
   repository's writer.
7. After write-mode parity remains clean, change the complete reconciliation
   sweep to hourly. Keep webhook-triggered mergeability and check-rollup queries
   and never disable the complete sweep.

This pull request implements none of those steps. It seeks approval of their
order and contracts.

## Assumptions fixed for implementation

- Repository webhooks, not an organization hook or GitHub App, match the current
  per-repository secret and credential boundary.
- The listener defaults to loopback and plain HTTP; TLS and public exposure are
  deployment responsibilities.
- Exact payloads remain seven days; GUID and digest tombstones remain permanent.
- “One real workday” means one normal active local workday, not a synthetic soak
  or a quiet 24-hour interval.
- Implementation selects the body ceiling from recorded public payload fixtures
  plus reviewed headroom; this proposal raises no existing safety ceiling.

## Public references

- Admission follows GitHub's
  [webhook best practices](https://docs.github.com/en/webhooks/using-webhooks/best-practices-for-using-webhooks)
  and
  [signature guidance](https://docs.github.com/en/webhooks/using-webhooks/validating-webhook-deliveries).
- Failure recovery follows GitHub's
  [failed-delivery](https://docs.github.com/en/webhooks/using-webhooks/handling-failed-webhook-deliveries)
  and
  [three-day redelivery](https://docs.github.com/en/webhooks/testing-and-troubleshooting-webhooks/redelivering-webhooks)
  guidance.
- Mapping uses GitHub's
  [payload inventory](https://docs.github.com/en/webhooks/webhook-events-and-payloads);
  the reference exposure uses Tailscale's
  [Funnel documentation](https://tailscale.com/kb/1223/funnel).

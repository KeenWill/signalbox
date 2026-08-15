# GitHub webhook ingestion for repository watch

Status: proposed for owner decision; design only.

## Document placement and authority

The repository has no existing proposal directory or proposal-page convention.
This page therefore starts `docs/proposals/`: it records a decision for review,
not implemented behavior. It deliberately does not edit
[`docs/spec/repo-watch.md`](../spec/repo-watch.md). If the proposal is accepted,
the bottom implementation pull request must update that owning specification as
the implementation becomes true.

This proposal assumes GitHub.com repository webhooks, one Signalbox daemon on a
development host with no ordinary public ingress, and PostgreSQL shared by the
existing repository-watch runtime. The host can make outbound HTTPS requests and
is a Tailscale node. There is no production compatibility burden because
Signalbox is pre-alpha.

## Decision summary

Add a webhook ingress beside, not inside, the process-protocol listener. Use a
small public relay with a durable bounded queue as the normal endpoint. The
relay verifies and durably records each GitHub delivery before returning `202`;
the daemon drains it over outbound authenticated HTTPS. The daemon verifies the
original signature again, records the delivery, converts it into a typed
observation patch, and commits that patch through the existing
`RepoWatchObservation` -> `derive_repo_watch_events` -> `repo_watch_cursor` plus
`repo_watch_event` path.

Keep one slower complete poll as verification and recovery. Keep targeted polls
for facts that a webhook cannot authoritatively project. A webhook is a
low-latency observation transport, not a second dispatcher: it never writes a
rule evaluation, creates a session, or invokes an action directly.

The relay is recommended over Tailscale Funnel or a generic reverse tunnel
because the development host is expected to be unavailable sometimes and GitHub
does not automatically retry failed deliveries. GitHub exposes manual or API
redelivery for only the preceding three days. A relay removes host uptime from
that short recovery window while preserving the host's no-public-ingress
posture.

## Existing seam

Today one `RepositoryWatchTask` builds a complete canonical
`RepoWatchObservation`, passes the previous and current observations to
`derive_repo_watch_events`, and submits one optimistic `RepoWatchCommitRequest`.
`PostgresRepoWatchStore::commit` inserts a `repo_watch_cursor` generation and
its ordered `repo_watch_event` rows in one transaction. The database trigger
`repo_watch_event_requires_cursor_commit` prevents an event from being appended
outside the transaction that records its cursor generation.

That is the transport-neutral seam. Webhook processing must enter above the
differ and below transport decoding. Writing a `RepoWatchEvent` directly from an
HTTP payload would bypass complete pull-request context, cursor coherence,
reaction-filter provenance, optimistic concurrency, and cross-transport
deduplication.

The `repo_watch_event` row requires more than GitHub's event name. Every
pull-request event carries the PR number, current head SHA and repository, base
and head branches, title, body, complete canonical labels, draft state, and
optional author. Payload columns are closed by `event_kind`; for example,
`head_changed` needs both SHAs, `review_submitted` needs reviewer, state, and
commit, and `branch_workflow_run_completed` is the sole branch target. A webhook
adapter must produce a coherent full observation from which the existing differ
can produce exactly those rows.

## Proposed components and frames

The data crosses five explicitly distinct frames. “Frame” here means a typed
boundary representation, not a process-protocol message.

1. `GitHubDeliveryHttpV1` is the exact HTTP request: method, bounded path,
   selected headers, and uninterpreted body bytes. Signature verification owns
   this frame.
2. `RelayDeliveryV1` is the relay queue record: repository route, hook ID,
   delivery GUID, event name, relay sequence, receipt time, exact body bytes,
   and the original signature header. The relay assigns sequence only after its
   durable write commits.
3. `RepoWatchWebhookDeliveryV1` is the daemon's admitted delivery record. It
   contains the same identity and bytes plus a body digest; it has no credential
   and no authority-bearing user or session field.
4. `RepoWatchObservationPatchV1` is a closed application frame. It describes
   only monotonic additions, guarded replacements, or a targeted-refresh hint.
   It is never stored as a `RepoWatchEvent` and rules cannot inspect it.
5. `RepoWatchObservation` and `RepoWatchEvent` remain the existing canonical
   state and fact frames. Downstream dispatch still emits only the existing
   `PullRequestContext` or `BranchContext` JSON frame.

The flow is:

```text
GitHub -> public relay -> durable relay queue
                           |
                           v
daemon outbound drain -> verified local delivery -> observation patch
                                                   |
full/targeted poll --------------------------------+
                                                   v
repo_watch_cursor + repo_watch_event -> existing rule/dispatch pipeline
```

Receiver, queue drainer, and poll scheduler are separate supervised tasks. A
saturated receiver cannot consume poll permits, and a rate-limited poller cannot
stop the drainer.

## Endpoint exposure decision

### Option A: Tailscale Funnel

Funnel gives the Tailscale node a public `*.ts.net` HTTPS URL and proxies a
specific public path to a loopback listener. It hides the host's ordinary
address, keeps the backend bound to loopback, and the Funnel relay does not
decrypt the end-to-end TLS stream. It is the fewest-component route for a
development spike.

Its security boundary is easy to misunderstand: Funnel is public internet
exposure, not tailnet-only Serve. Any caller can spend receiver CPU and body
budget before HMAC rejection. Enabling Funnel changes tailnet policy, the
feature remains beta, and availability is coupled to the host, `tailscaled`, the
Funnel control plane, and the daemon listener. It provides no durable buffer. A
sleeping host turns deliveries into GitHub failures.

Use Funnel only for initial payload capture and end-to-end tests. Proxy one
non-secret path to a loopback-only listener and serve no health, metrics,
process-protocol, or administrative route there.

### Option B: hosted reverse tunnel

A reverse-tunnel agent can make an outbound connection to a provider such as
Cloudflare Tunnel or ngrok and publish the local listener without opening the
host firewall. It can offer stable DNS, request limits, and useful delivery
diagnostics.

Unless configured for end-to-end TLS passthrough, the provider terminates TLS
and can read webhook payloads. The provider account and tunnel token become
ingress-control credentials. Like Funnel, a plain tunnel has no acceptance
queue: the host, agent, listener, and provider must all be live when GitHub
sends. Provider access gates are generally unusable because GitHub cannot
complete an interactive login. HMAC still authenticates GitHub, but it does not
hide the body from a TLS-terminating tunnel.

Choose this only when an already-approved provider supplies a durable webhook
queue as part of the product. In that case it is the relay option below, not a
mere tunnel.

### Option C: small durable relay — recommended

Run one narrowly scoped HTTPS service on a small public host or managed edge
runtime. It exposes only `POST /github/v1/<repository-route>`, verifies a
repository-specific HMAC, durably enqueues the exact delivery, and returns
`202`. Signalbox opens an outbound authenticated drain to the relay; the relay
does not join the tailnet and cannot initiate a connection to the development
host.

This is the only option that decouples GitHub acceptance from host uptime. It
also keeps the host's tailnet identity and ordinary services entirely off the
public route. The cost is a second operational component, a second copy of each
webhook secret, temporary payload storage outside the host, and a service whose
compromise can forge deliveries because it knows those secrets. Mitigate that
cost with one secret per repository, no GitHub API credential at the relay,
encrypted storage, short payload retention, permanent compact delivery-ID
tombstones, strict byte and rate limits, and outbound-only daemon access.

The relay contract is only enqueue, ordered bounded drain, acknowledge, and
queue-age reporting. It cannot map Signalbox types, decide freshness, call
GitHub, or dispatch work.

## HTTP admission and signature verification

GitHub is configured with `content_type = json`, SSL verification enabled, a
repository-specific random secret, and only the required events. The endpoint
accepts `POST` only. It rejects transfer or content encodings the receiver does
not implement, multiple instances of a singleton GitHub header, a malformed or
over-limit declared length, and any streamed body that crosses the reviewed hard
ceiling.

The relay selects a configured repository from the route before reading its
secret. It reads the exact body bytes once while incrementally computing
HMAC-SHA-256. It requires `X-Hub-Signature-256` with one `sha256=` value and
compares the decoded 32-byte MAC in constant time. It does not decode JSON,
normalize Unicode, decompress, reserialize, or trust proxy-added body fields
before that comparison.

After the MAC passes, admission requires:

- a canonical `X-GitHub-Delivery` UUID;
- a positive configured `X-GitHub-Hook-ID`;
- a bounded ASCII `X-GitHub-Event` value;
- `application/json` content type;
- a body repository `full_name` equal, after the existing repository
  normalization, to the route and configured hook; and
- an event/action pair in the subscribed inventory, with `ping` admitted only as
  a health proof.

The daemon repeats the HMAC over the untouched relayed bytes and repeats the
route, hook, repository, event, and action checks. Transport authentication
between daemon and relay is separate mTLS or a relay-drain credential and is not
accepted as a substitute for GitHub's signature.

Secrets use the existing credential-file discipline on the daemon: absolute
per-repository references, bounded reads, no secret bytes in configuration,
logs, tables, telemetry, or errors. The polling token and webhook secret are
distinct and may not share a file reference. Rotation stages a new secret at
both verifiers, updates GitHub, then removes the old verifier after the
three-day redelivery horizon. This overlap is secret rotation, not wire-version
compatibility.

## Replay protection and durable intake

The relay and daemon each enforce uniqueness on `(hook_id, delivery_id)`. GitHub
reuses `X-GitHub-Delivery` for an explicit redelivery, so an exact repeat
returns success without enqueuing or processing again. Reuse of the same key
with a different body digest, event name, or repository is a security incident:
retain the first record, reject the conflicting request, and emit metadata-only
telemetry.

Delivery identity tombstones are permanent. Raw bytes remain through terminal
disposition and the three-day recovery window, then may be swept. Receipt time
is audit data, not freshness proof, because the signature covers no timestamp.

The relay acknowledges GitHub only after its queue transaction commits. The
daemon acknowledges the relay only after the local intake transaction commits.
Processing and cursor commit happen asynchronously after local intake, so
GitHub's ten-second response deadline never includes a GitHub API call, the
differ, or dispatch.

## Tables and transaction boundaries

The Signalbox migration adds exactly four tables:

- `repo_watch_webhook_delivery`: permanent identity tombstone and admission
  metadata. Its primary key is `(repository, hook_id, delivery_id)`; it stores
  relay sequence, event name, nullable action, body SHA-256, and receipt times.
- `repo_watch_webhook_payload`: the exact bounded body and original signature,
  keyed by and restricted to `repo_watch_webhook_delivery`. It is operational
  queue material and may be deleted only after terminal disposition and the
  retention bound.
- `repo_watch_webhook_disposition`: one append-only terminal result per
  delivery: `committed`, `duplicate_state`, `superseded`, `ignored`,
  `reconciled`, or `quarantined`, with the resulting cursor generation when one
  committed. An error code is bounded and carries no payload text.
- `repo_watch_event_origin`: one row per new `repo_watch_event`, identifying
  `webhook`, `verification_poll`, or `reconciliation_poll` and optionally the
  delivery key. It is audit provenance only and cannot affect rule matching.

Local intake inserts `repo_watch_webhook_delivery` and
`repo_watch_webhook_payload`; conflict is accepted only when immutable metadata
and digest match. Processing locks the repository, reloads its cursor, applies
one patch, derives events, inserts the next `repo_watch_cursor`, inserts
`repo_watch_event` and `repo_watch_event_origin`, and records disposition in one
transaction. An unchanged patch records `duplicate_state` or `superseded`
without a cursor generation. A crash retries when no disposition exists.

Existing `repo_watch_cursor` and `repo_watch_event` remain the only ingestion
tables visible to the dispatcher. `repo_watch_rule_activation` and
`repo_watch_rule_evaluation` continue to define the consumption frontier.
`repo_watch_dispatch_batch`, `repo_watch_dispatch_action`,
`repo_watch_dispatch_delivery_intent`, and `repo_watch_dispatch_delivery` remain
downstream and structurally unchanged. No webhook table references a session,
goal, command, rule, singleton, or dispatch table.

The relay has one corresponding implementation-owned `relay_delivery` queue
record with the same hook/delivery identity, exact body, monotonic relay
sequence, lease expiry, and acknowledgement time. Its physical database is an
operational choice; its wire contract is `RelayDeliveryV1`.

## Mapping to the existing event vocabulary

Every adapter either makes a guarded observation patch or requests a targeted
refresh. Unsupported event/action pairs are durably `ignored` and trigger no
event. Unknown newly added actions are not guessed.

| GitHub event and action                              | Observation change                                                                               | Possible existing fact                            |
| ---------------------------------------------------- | ------------------------------------------------------------------------------------------------ | ------------------------------------------------- |
| `pull_request: opened`, `reopened`                   | Add or reopen the PR from its complete payload context; use `unknown` until mergeability refresh | `PullRequestOpened`, then `MergeableStateChanged` |
| `pull_request: closed`                               | Set closed or merged from the payload's merged flag                                              | `PullRequestClosed` or `PullRequestMerged`        |
| `pull_request: synchronize`                          | Replace head only when the PR generation is newer                                                | `HeadChanged`                                     |
| `pull_request: labeled`, `unlabeled`                 | Replace the complete canonical label set                                                         | `Labeled` or `Unlabeled`                          |
| Other context-changing `pull_request` actions        | Refresh title, body, draft, author, and branches without inventing a new kind                    | Usually none                                      |
| `pull_request_review: submitted`                     | Union the immutable provider review identity and supported state                                 | `ReviewSubmitted`                                 |
| `pull_request_review: dismissed`                     | Retain version-one behavior; no dismissal fact                                                   | None                                              |
| `pull_request_review_thread: resolved`, `unresolved` | Target-refresh the thread projection because the payload has no trusted transition generation    | `ThreadResolved` or `ThreadOpened`                |
| `check_run: completed`                               | Union run ID, completion time, name, and conclusion for matching current heads                   | `CheckRunCompleted`                               |
| `check_suite: completed`                             | Mark the head's aggregate check projection dirty                                                 | A later polled `ChecksCompleted`                  |
| `workflow_run: completed`                            | Guard by workflow ID, run ID, attempt, repository, branch, and completion                        | `BranchWorkflowRunCompleted`                      |
| `push` on `refs/heads/*`                             | Target-refresh that branch head; ignore tags and deletions until full reconciliation             | `BaseAdvanced` for each affected open PR          |
| `ping`                                               | Record endpoint health only                                                                      | None                                              |

There is no GitHub webhook event for reaction additions and removals on the
three version-one reaction subjects. Those facts remain polling-only. Issue and
review-comment webhooks do not imply a reaction and are not substitutes.

A check run without an unambiguous current PR head match requests targeted
reconciliation rather than trusting a possibly incomplete payload array. A
workflow run is accepted only when its head repository is the watched
repository, preserving the current fork exclusion.

A pull-request patch preserves prior checks, reviews, threads, and reactions;
those projections are not present in the pull-request payload. A first-seen
reopened PR is completely hydrated before commit so a later poll cannot re-emit
historical nested facts as new.

## Ordering and freshness

GitHub documents that webhook deliveries can arrive in a different order from
the underlying events. Relay sequence therefore defines receipt order only.
`repo_watch_event` continues to mean durable observation order, not claimed
provider chronology.

One worker serializes commits per repository. Each patch carries the strongest
provider generation available:

- PR replacements use the payload's `updated_at` plus action-specific state;
- reviews union by provider review ID and submitted time;
- completed check runs use run ID and `completed_at`;
- workflow completions use workflow ID, run ID, run attempt, and completion;
- unversioned thread and branch changes require a targeted current-state read.

A patch older than the cursor's retained provider generation is `superseded`.
Equal immutable facts are `duplicate_state`. If an adapter cannot prove that a
replacement is non-regressive, it schedules targeted reconciliation rather than
applying arrival order. This rule is intentionally conservative: low latency is
optional; cursor regression is not.

The cursor must retain PR `updated_at` for these guards. It is transport
metadata, not a rule field or payload. Existing retained provider identities for
reviews, threads, checks, and workflows are reused.

## Deduplication across webhook and polling

Delivery deduplication and fact deduplication are different layers. The delivery
GUID stops replayed HTTP work. Cross-transport fact deduplication comes from the
shared cursor and pure differ.

Both the webhook worker and poller load generation N and propose generation N+1.
Only one wins the existing optimistic commit. The loser reloads the winner's
observation and reapplies its input. If both observed the same lifecycle, label,
review, check run, thread state, workflow attempt, or branch head, the second
derivation emits no event. Random `RepoWatchEventId` values are therefore never
used as semantic deduplication keys.

Polling preserves newer immutable identities learned from a webhook when a
partial API projection omits them. A complete poll may replace a complete
projection; a targeted response updates only its resource. The same repository
lock and cursor compare cover all three writers.

`repo_watch_event_origin` records which observation won. It does not attempt to
assign one provider occurrence to two sources. A later duplicate source is
visible through the delivery disposition or poll metrics, not a second event.

## What remains polled

Mergeability remains polled because GitHub calculates it asynchronously and no
webhook provides an authoritative transition. A pull-request delivery can carry
null, unknown, or stale mergeability. An opened PR is admitted with `unknown`;
targeted and verification polls later produce the ordinary
`MergeableStateChanged` transition.

Aggregate check rollups remain polled. Check-run and check-suite deliveries can
be missing, reordered, rerequested, associated incompletely with fork heads, or
arrive before all runs in a suite settle. The current poll deliberately walks
all suite attempts and every run through suite inventory. Only that complete
projection may produce `ChecksCompleted`; a `check_run: completed` delivery may
still produce its individual `CheckRunCompleted` fact.

Reaction state remains polled because GitHub exposes no matching webhook. Full
review and thread projections remain in verification polls to recover a lost
submission or resolution. Branch heads and workflow inventories remain in full
polls to detect deleted branches, omitted runs, and foreign-head ambiguity.

The verification poll is complete but slower than today's cadence. Targeted
polls coalesce by `(repository, resource)`, retain conditional requests, and use
the same credential boundary. Rate limits delay those facts without stopping
webhook intake.

## Missed-delivery reconciliation

The relay is the first recovery layer: it retains accepted deliveries while the
daemon or host is down and drains them oldest-first when the daemon returns. An
existing cursor must be established before the GitHub hook is enabled, so a
normal restart drains queued deliveries before its next full poll.

The second layer derives the GitHub hook-delivery frontier from permanent local
delivery records, lists metadata after relay outage, and requests redelivery of
failures or missing GUIDs. This gap-only path uses a separate Webhooks
read/write credential; the ordinary poll token does not gain hook
administration.

GitHub does not automatically redeliver failures, and its delivery UI/API keeps
only the last three days. The reconciler must alert before that horizon expires.
Every requested redelivery carries its original GUID and passes normal replay
handling.

The third layer is the complete state poll. It reconstructs durable current
state after any gap and naturally emits transitions still represented by that
state or immutable provider inventories. It cannot reconstruct an add followed
by remove, resolve followed by unresolve, multiple workflow completions no
longer in the retained projection, or another transient sequence that began and
ended during the gap. A gap older than three days is therefore recorded and
alerted as lossy; the design does not claim otherwise.

## Failure modes and fallback

| Failure                                  | Immediate behavior                                    | Recovery/fallback                                                                                  |
| ---------------------------------------- | ----------------------------------------------------- | -------------------------------------------------------------------------------------------------- |
| Development host or daemon down          | Relay returns `202` and queues                        | Drain before the next scheduled full poll                                                          |
| Relay cannot reach daemon                | Queue age rises; GitHub acceptance continues          | Outbound drain retries with bounded backoff; polls continue                                        |
| Relay down or saturated                  | GitHub records failures; no automatic retry           | Verification poll continues; gap reconciler requests redelivery within three days                  |
| PostgreSQL unavailable                   | Daemon does not ack relay records                     | Relay lease expires and redelivers; poll commits also wait/fail                                    |
| Invalid signature or repository mismatch | Reject before JSON mapping                            | Polling is unaffected; alert on a bounded failure-rate threshold                                   |
| Secret rotation mismatch                 | One verifier rejects the delivery                     | Restore overlap, request redelivery, and poll current state                                        |
| Unknown event/action or payload shape    | Quarantine or ignore without guessing                 | Full poll verifies current state; implementation adds support deliberately                         |
| Out-of-order delivery                    | Stale guard suppresses or requests targeted refresh   | Shared cursor records only non-regressive state                                                    |
| Poll credential rate-limited             | Webhook-backed facts continue                         | Delay mergeability, rollups, reactions, and reconciliation until reset                             |
| Webhook secret compromised               | Forged signed traffic is possible for that repository | Rotate only that secret, disable its hook, retain poll-only operation, audit GUID/digest conflicts |
| Both webhook path and poll unavailable   | No new facts can commit                               | Alert as total ingress outage; queues preserve only already accepted deliveries                    |

Disabling webhook processing is a safe rollback: stop draining or mark the
transport disabled and restore the present polling cadence. Existing webhook-
derived `repo_watch_event` rows remain ordinary immutable facts; no
compatibility shim or data rewrite is needed.

## Security posture

HMAC authenticates bytes, not intent. Payload strings still pass existing domain
constructors and limits. Logs contain identifiers, sizes, statuses, and stable
codes, never secrets, signatures, raw bodies, PR text, or rendered JSON. Global,
per-route, concurrent-body, queue-depth, and rate limits apply before parsing.
GitHub source-IP allowlisting is optional defense in depth, never an HMAC
replacement.

The relay can read temporary payloads and forge deliveries because it holds the
HMAC secret. It receives no poll or hook-administration token, database
credential, Tailscale key, process socket, or session authority. Repository A's
route cannot select repository B's secret, queue, cursor, or task.

## Migration and rollout

1. Accept this decision document. No runtime behavior changes in this pull
   request.
2. Add the four Signalbox tables, closed delivery/patch frames, configuration,
   and tests behind a disabled webhook transport. Update the owning repo-watch
   specification and domain spine only for the surfaces that implementation
   actually makes public.
3. Deploy the relay with a synthetic signed-fixture endpoint, queue limits,
   drain authentication, payload retention, and no GitHub hook. Exercise relay
   loss, duplicate GUIDs, changed-body replays, and daemon/DB downtime.
4. Complete one full poll and record its cursor. Configure per-repository
   webhook secrets and hooks, initially in shadow mode: verify, store, map, and
   compare patches, but let polling remain the only cursor writer.
5. Compare shadow projections with full polls for lifecycle, head, labels,
   reviews, threads, individual checks, workflows, and branches. Unknown or
   divergent mappings block write enablement for that event/action only.
6. Enable webhook cursor commits per repository and event/action, retaining the
   current full polling cadence. Prove cross-source duplicates collapse and
   queue recovery preserves event order.
7. Enable the delivery-gap reconciler and run outage drills shorter and longer
   than the three-day GitHub horizon. Confirm lossy gaps are explicit.
8. After an owner-approved observation window, lengthen the complete polling
   interval. Keep targeted mergeability and check-rollup refreshes and never
   disable the complete verification poll.
9. Remove any temporary Funnel or reverse-tunnel endpoint after the relay hook
   is healthy. Rotation and poll-only rollback remain documented runbooks.

Rollout is per repository. A failure or mismatch disables only that repository's
webhook writer and restores its prior poll cadence; it does not change another
repository's credential, queue, or cursor.

## Assumptions selected for this proposal

- Repository hooks are the smallest fit for the current per-repository boundary;
  no approved durable broker already exists.
- Event history remains Signalbox observation order, and rollout establishes a
  full cursor before hook enablement.
- Raw payloads remain seven days; compact identity/digest tombstones are
  permanent.
- Complete verification starts hourly; targeted refreshes are event-driven.
- Implementation selects a reviewed body ceiling from fixtures and headroom;
  this proposal raises no existing safety ceiling.

## Owner rulings requested

1. Approve the small durable relay as the normal endpoint and Funnel only as a
   bring-up tool, accepting the relay's secret and temporary-payload trust.
2. Approve a separate hook-administration credential for three-day gap recovery
   rather than broadening the ordinary per-repository polling credential.
3. Approve seven-day raw-payload retention and permanent GUID/digest tombstones,
   subject to implementation-time size-ceiling review.
4. Approve hourly complete verification as the rollout default, with a later
   operational adjustment based on measured quota and detection latency.

## Public references

- Admission follows GitHub's
  [webhook best practices](https://docs.github.com/en/webhooks/using-webhooks/best-practices-for-using-webhooks)
  and
  [signature guidance](https://docs.github.com/en/webhooks/using-webhooks/validating-webhook-deliveries).
- Recovery follows GitHub's
  [failed-delivery](https://docs.github.com/en/webhooks/using-webhooks/handling-failed-webhook-deliveries)
  and
  [three-day redelivery](https://docs.github.com/en/webhooks/testing-and-troubleshooting-webhooks/redelivering-webhooks)
  guidance.
- Mapping uses GitHub's
  [payload inventory](https://docs.github.com/en/webhooks/webhook-events-and-payloads);
  exposure tradeoffs use Tailscale's
  [Funnel documentation](https://tailscale.com/kb/1223/funnel).

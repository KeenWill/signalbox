# Repository watch and event dispatch

**Foundation contract.** Repository watch is a credentialed external-ingress
boundary from its first operation: the daemon holds a distinct credential-file
reference for each configured repository, reads secret bytes only for that
repository's request, and never gives a dispatched session the watch credential.
Per-repository tokens carry the least GitHub scope needed to read the configured
signals. This is the C0 confused-deputy boundary: a credential for one
repository cannot authorize a request to another repository. A repository
without a credential-file reference is invalid configuration and is not watched;
a repository absent from the list is not watched; an absent repository-watch
section means that the subsystem does not start. Dispatched sessions retain the
approval posture of their named session templates, without authority inherited
from the watcher.

**Foundation contract.** This bottom specification diff owns the
four-pull-request repository-watch stack. The version-one domain vocabulary and
validation shapes were verified against PR #430 (`agent/repo-watch-spec`). The
persistence and rule-dispatch behavior below is verified against PR #446
(`agent/repo-watch-dispatch`). The polling and differ behavior below, the goal a
dispatch commissions with its session, the binding of the dispatched work turn
to that goal's generation, and the occupied-refusal obligation and collapsed
current-state delivery are verified against PR #812
(`agent/repo-watch-dispatch-loop`). The request-envelope behavior is verified
against this PR (`agent/daemon-ops-overnight`). Runtime-relevance release,
held-slot diagnostics, and terminal-target cutoff are also verified against this
PR. The provider members the poller adopts as check-suite and check-run
completion generations are verified against PR #541
(`fix/check-run-updated-at`). Eager merge-forward dispatch is verified against
PR #886 (`agent/eager-merge-forward`). Requeue after non-converged dispatch
termination is verified against PR #894
(`agent/dispatch-requeue-on-invalidation`). The source-independent event
occurrence identity, its durable frontier, the commit-time coalescing of a
restated occurrence, and the storage migration are verified against this PR
(`agent/repo-watch-content-identity`). The authenticated webhook intake, its
ingress ceilings, shadow projection, parity view and causes, and targeted
refresh behavior are verified against this PR
(`agent/repo-watch-webhook-receiver`). Webhook drain liveness and stall
reporting are verified against this PR (`agent/webhook-projection-drain`).

## Configuration and credential boundary

**Implemented behavior.** Repository watch has one optional, versioned TOML
section containing a list of repositories and a list of signal-reviewer logins.
Each repository entry names exactly one `namespace/name` repository, a positive
polling interval, and its own credential file reference. Duplicate repositories,
watch credentials shared between repositories or with the daemon's session
GitHub tools, missing credential-file references, unknown keys, unsupported
versions, zero intervals, malformed values, and invalid entries fail
configuration before either runtime starts. Other daemon GitHub credentials do
not substitute for a missing repository-watch credential reference. Credential
paths are absolute and cannot contain parent-directory components, so lexical
and filesystem aliases cannot bypass duplicate-reference validation. On Unix,
existing files are also compared by device and inode, so hard links cannot
bypass the boundary. Final and intermediate symlink components are resolved even
while their target file or directory is absent; later creation of that target
therefore cannot turn two admitted references into one shared credential. The
daemon's session GitHub credential reference is likewise normalized before this
overlap check, including when an intermediate lexical component does not yet
exist.

**Implemented behavior.** The section also accepts versioned structured rules.
Invalid rules, unknown fields, duplicate rule identities, unsupported versions,
more than 128 rules, more than 32 actions per rule, non-whole-second cooldowns,
or cooldowns beyond signed 64-bit seconds fail startup configuration before
polling begins.

**Implemented behavior.** Repository identities normalize to ASCII lowercase at
construction. Both slug segments are nonempty ASCII letters, digits, dots,
hyphens, or underscores; neither segment is `.` or `..`. GitHub human,
managed-user, and App-bot logins likewise normalize to ASCII lowercase so exact
author matching and signal-reviewer filtering use GitHub's case-insensitive
identity semantics.

**Implemented behavior.** Credential files follow the house credential-file
pattern: configuration stores paths rather than secrets; request preparation
reopens the selected file and reads a bounded value; errors and telemetry name
only the reference, never the secret; and rotation affects later requests
without restart. Startup validates the reference shape but does not read the
file, so an unavailable or unreadable watch credential cannot block daemon
startup or its recovery scan. Its repository request fails closed during
preparation instead. No credential value is persisted in a cursor, event,
dispatch record, session parameter, error, or log.

**Implemented behavior.** An optional `repository_watch.webhook` table enables
one plain local HTTP listener for the watch subsystem. Its bind address is fully
configurable and defaults to `127.0.0.1:3333`; its absolute local request path
is required and configurable, and must be one literal request path: routing
metacharacters are rejected in configuration rather than reaching a router that
would read them as a capture or panic on them. Each webhook-enabled repository
supplies one positive GitHub hook ID and one absolute secret-file path, either
both or neither. Hook IDs are unique, and webhook secret paths cannot alias any
polling, session-tool, or other webhook credential path under the same lexical,
symlink, and Unix file-identity checks. A webhook secret is a repository-watch
credential like the polling token, so daemon startup applies the same boundary
to it: a secret path that equals or aliases the session GitHub credential fails
closed. A listener without an enabled repository, or an enabled repository
without a listener, fails configuration. The daemon binds the configured address
and verifies requests but knows nothing about tunnels or exposure providers. The
reference deployment exposes public path `/github/webhooks` through Tailscale
Funnel `--set-path`, which strips that prefix; its configured local path is
therefore `/`. The reference secret file is
`/home/wkg/.config/signalbox/github-webhook-secret`. Public reachability and its
availability belong to deployment, not to the daemon.

## Poll transport and differ

**Implemented behavior.** Version one uses conditional-request polling with one
independent task per configured repository and the repository's configured
interval. Each resource request sends `If-None-Match` only when the repository
task's process-local cache holds that resource's ETag and typed accepted state.
A production task sends only to the fixed `https://api.github.com` origin. REST
paths are anchored to the configured base repository, and GraphQL variables name
that same repository. The client requires TLS 1.2 or newer and disables
redirects, environment proxies, and automatic retries so credential-bearing
requests cannot be redirected or silently replayed outside their repository
attempt. A resource-level `304 Not Modified` reuses only that resource's
accepted state and does not skip the remaining fetches; GitHub does not count a
conditional `304` against the primary rate limit. Cache keys are bounded,
non-secret resource/page identifiers rather than query strings. The cache starts
empty on every daemon start, so the first poll and the first poll after restart
perform one complete unconditional fetch. When a current poll replaces a
resource at the cache's entry bound, admission evicts an untouched stale entry
before enforcing that bound; replacement within the bound cannot wedge later
polls. Admission is an accelerator and never a precondition for an observation:
a resource that does not fit the entry or retention bound is shed, not committed
to the cache, and the poll continues, so that resource is refetched
unconditionally on the next poll. Retention is bounded separately from, and
lower than, what one poll may transfer, because retention is per watched
repository and multiplies by the configured repository count; shedding is what
keeps the lower retention bound from capping the transfer bound, since every
resource already fetched in the current poll is touched and therefore not
evictable. REST continuation follows GitHub's `Link` relation for the next page,
not response cardinality: a full terminal page at the 100-page bound completes
the projection, while a next relation beyond that bound fails the poll. Because
a `304` can omit changed pagination metadata, a cached full terminal page
conservatively probes one bounded successor; the cap page is reread
unconditionally so that probe never manufactures page 101. A failed, rejected,
partial, or unparseable poll submits no persistence candidate. The
per-repository interval is measured start to start, so a cadence does not drift
by the duration of its own attempt, and attempts never overlap. An attempt that
reaches or exceeds the interval cannot hold that cadence, and starts a fresh
interval from its own completion rather than following immediately: a poll
deadline left in the past would win every scheduling decision the repository
task makes and starve the durable webhook drain. A webhook wake serializes with
the same repository task and does not reset the full-poll deadline; a failed or
unavailable webhook endpoint therefore loses acceleration, not reconciliation.

**Implemented behavior.** One attempt fetches up to eight open pull requests
concurrently. The fetch sequence within a single pull request stays ordered, and
the fetched pull requests are ordered by number before comparison, so
concurrency cannot reorder a baseline. A single attempt may transfer up to 768
MiB of response bytes; what one poller retains between attempts is bounded
separately and lower, because retention is per watched repository and therefore
multiplies by the configured repository count.

**Implemented behavior.** An attempt may reuse the committed detail and settled
check baseline for an open pull request, but only when every one of the
following holds: the open pull request listing reports both an `updated_at`
identical to the one recorded when that baseline was fetched and a head SHA
identical to the committed pull-request context; the recorded fetch reached the
durable cursor, so an attempt that failed before committing cannot authorize
reuse of the stale baseline it never replaced; that fetch observed every check
suite and check run in a terminal state and a known mergeable state, because
neither a check completion nor the provider's background mergeability
calculation moves `updated_at`; and the pull request has been reused fewer than
four consecutive attempts, which bounds how long another check or detail fact
that never moves either listing member can go unobserved. Reviews and threads
are re-fetched on every attempt and replace their prior projections before
comparison, so a delayed detail/check refresh cannot absorb or defer review
dispatch signals. Reactions are likewise re-fetched on every attempt because a
reaction does not move `updated_at` at all; with no configured signal reviewer
there is nothing a reaction can trigger, so the poller issues no reaction
request. A pull request absent from the open listing is never reused. Cached
resources survive the same bounded number of untouched attempts, so reuse does
not discard the validators that keep the following full fetch conditional. The
freshness record is process-local, like the conditional-request cache, so a
restarted daemon re-fetches every pull request.

**Implemented behavior.** Check-suite and check-run requests explicitly select
all attempts and follow bounded result pages. Check runs are enumerated through
each suite returned by the paginated commit suite inventory rather than the
provider's commit check-run search, whose 1,000-suite cap cannot represent a
complete baseline. Every completed provider identity returned by that projection
enters the comparison baseline; the provider's latest-attempt default cannot
silently discard a completion between polls.

**Implemented behavior.** Daemon shutdown wins a race with a repository task's
clean exit. Once shutdown is observable, the supervisor drains every watch task
and reports a clean stop; a task that exits cleanly before shutdown remains a
runtime lifecycle defect.

**Implemented behavior.** The versioned durable cursor retains the complete
normalized repository state, exact signal-reviewer set, and the last positive
occurrence sequence for each recurring source-independent event stream. The
frontier is canonical by its 32-byte stream identities, rejects duplicates and
zero sequences, and admits at most 1,000,000 streams. That ceiling is where one
repository's identity state, rather than its event history, becomes the dominant
cost of watching it: each entry costs a 32-byte stream identity and an 8-byte
sequence, so the limit bounds one frontier near 40 MB. Exceeding it fails the
comparison, because the alternative is reusing an occurrence number and minting
a content identity that collides with an already-durable one. Sequence
exhaustion fails the comparison rather than wrapping. Provider-keyed immutable
facts use sequence one without occupying frontier space. A fact counts as
immutable only when the differ suppresses re-emission on members its stream key
already names, so completed check runs are not among them: their conclusion can
change under an unchanged run identity and completion generation, and they
advance a frontier sequence like any recurring stream. The cursor does not
retain resource keys, ETags, accepted transport responses, raw provider
payloads, or credentials. A per-repository atomic commit accepts an expected
generation, one complete cursor candidate, and its ordered event-occurrence
batch. It serializes competing commits, appends the cursor and every event
together, rolls back the whole batch on failure, reports a stale generation as
conflict, and recognizes only an exact candidate-and-occurrence replay. An
unchanged candidate with no events does not advance the cursor; an unchanged
candidate carrying events is rejected.

A commit coalesces an occurrence whose content identity is already durable for
that repository under the same content, writing the cursor without a second row
for it. A provider entity that leaves the observation and returns re-derives
exactly that occurrence, with a fresh candidate identity but an equal content
identity, and without coalescing the duplicate would abort the whole
cursor-and-event transaction and leave the cursor at the entity-absent
generation, so every later poll would repeat the same failure. Content equality
excludes the random event identity, exactly as the digest does. An occurrence
whose content identity is durable under different content is not coalesced: it
is written, and the durable unique constraint rejects it. Replay detection
compares against the batch the replayed generation would have stored, so a
coalesced commit is still recognized as its own replay. The relational event
table admits an event row only in the database transaction that inserts its
referenced cursor generation, preventing later maintenance or future writers
from changing an already-committed batch.

**Implemented behavior.** `RepoWatchEventContentIdentityV1` is the exact shared
content identity for a normalized event occurrence. It is a 32-byte SHA-256
digest whose length-framed input begins with
`signalbox/repo-watch/event-content-identity/v1`, then includes the repository,
event version, canonical target and complete event payload, a separately
domain-separated source-independent stream identity, and the stream's positive
occurrence sequence. The stream identity is closed by event kind. Recurring PR
lifecycle, mergeability, head, label, thread, branch-advance, and reaction
streams name the PR and their kind-specific label, thread, branch, or reaction
members. Check runs are recurring too, naming their provider run identity and
completion generation: a completed run edited back to an earlier conclusion
restates that conclusion's facts exactly, so only an advancing occurrence
sequence keeps the restored event's identity distinct from the first, and
without it the commit would coalesce the restored conclusion away rather than
announce it. Immutable check-suite facts name their provider identity and
completion generation; reviews name their provider review identity; workflow
facts name branch, workflow identity, run identity, and attempt. The normalized
review observation has no submitted-time member, so version one assumes the
provider review identity alone uniquely identifies that immutable submission.
The random `RepoWatchEventId` is deliberately excluded from the digest.

**Implemented behavior.** A later equal fact on a recurring stream advances its
sequence and therefore has a different content identity. Equal normalized facts
derived from an equal cursor frontier have the same content identity even when
their candidate UUIDs differ. Persistence rejects duplicate UUID or content
identity members within one batch, and the relational store uniquely constrains
`(content_identity_version, content_identity)` across batches. Exact replay
compares the cursor candidate, UUID-bearing event values, and content
identities.

**Implemented behavior.** The version-one cursor reader remains compatible with
the earlier version-one workflow record that lacked a workflow-definition ID. It
admits only the otherwise-canonical legacy shape, uses the retained run ID as
the definition-identity sentinel, suppresses the same completed run attempt by
branch, run ID, and attempt number, and writes the complete current shape on the
next successful commit. A legacy cursor therefore cannot permanently block its
repository.

**Implemented behavior.** The content-identity migration rewrites every durable
cursor to storage version two with an empty occurrence frontier, then all later
poll commits carry and advance that frontier. Event rows recorded before it
cannot be reconstructed as the content occurrences the differ would emit,
because their durable shape lacks every provider identity the differ uses and
the frontier reset discards the sequence state their identities derive from.
Dispatch rows reference those events under `ON DELETE RESTRICT`, so they are
carried rather than discarded, and their identity is derived under a hash domain
reserved for the migration itself and disjoint from the differ's: a carried row
can never claim an identity a producer would also derive, and never matches one.

The carry completes across two migrations, because the first was applied before
its shape was settled and an applied migration is immutable. `202608150001`
marked those rows content-identity version zero and admitted both versions;
`202608170003` moves them to version one and narrows the durable constraint to
version one alone. Exactly one content-identity version is readable once both
have run. The durable constraint and the decoder admit version one alone, so no
earlier event shape survives for a reader to accept.

**Implemented behavior.** A pure differ compares consecutive canonical
per-pull-request state, branch heads, and completed branch-workflow identities
(`workflow_id`, `run_id`, `run_attempt`), producing only the closed version-one
event vocabulary below in deterministic order. The cursor retains provider
identity and completion generation for completed check suites and check runs,
plus provider identities for reviews, threads, and both the workflow definition
and branch-workflow run attempt. The poller uses the provider's `updated_at`
value as a completed check suite's completion generation and the provider's
`completed_at` value as a completed check run's: the provider defines
`updated_at` on a check suite only, while a check run carries `started_at` and
`completed_at`. A completed run whose payload carries no `completed_at` fails
the poll as an invalid response. A rerequested suite therefore emits its later
completion even when its provider identity and conclusion are unchanged, and a
rerequested run emits when the provider gives it a new identity, a different
completion time, or a different conclusion, so a conclusion edited under one
completion time stays observable. Workflows that share a display name remain
distinct, renaming a workflow cannot re-emit its already observed run attempt,
and a new attempt under an unchanged run ID does emit. The display name remains
the rule-visible event payload. A provider fact retained in the consecutive
comparison baseline is not re-emitted. Rules receive only events: they cannot
inspect normalized snapshots or rerun the differ. Why: transport independence
requires both polling and a later authenticated webhook receiver to feed the
same durable facts.

**Implemented behavior.** Polling fetches repository state, not rule inputs. The
branch-workflow projection retains the latest completed run identity and
conclusion for every workflow on every extant branch in the watched repository;
the transport scans each workflow's result pages once and collects every branch
match from that scan. When the newest watched-repository run for a branch and
workflow is queued or in progress, the projection continues scanning for the
latest completed candidate and retains whichever of that candidate and its prior
completed baseline is later. It therefore observes a completion not yet present
in the cursor without regressing to an older completion while the active run
remains unfinished. Per-page validators remain only in the repository task's
process-local cache. A same-named branch on a fork does not enter this
projection: the poller accepts a run only when its provider head-repository
identity equals the configured watched repository and continues through bounded
result pages past foreign or absent head repositories.

## Durable event vocabulary

**Implemented behavior.** Each event value is an immutable, version-one fact
with its own UUID identity, repository, tagged pull-request or branch target,
and closed payload. Pull-request targets carry the positive PR number, current
40-hex head SHA, head repository, base and head branches, title, body, canonical
sorted duplicate-free complete label set, draft state, and the author when
GitHub supplies one. This is normalized event context, not a raw API object. The
only branch-target event in version one is `BranchWorkflowRunCompleted`; its
payload supplies the branch, workflow, and conclusion. Construction rejects a
pull-request event when the payload's current head, base branch, or label
transition contradicts that event's complete current pull-request context. A
`HeadChanged` payload whose previous and current SHAs are equal is invalid.
Label names admit up to 50 Unicode scalar values, including their full UTF-8
representation.

**Implemented behavior.** GitHub can return a null head repository after a
tracked pull request's fork is deleted. For that same previously observed pull
request, the poller retains the prior canonical head-repository identity while
accepting the new lifecycle and other current fields; a new pull request with no
current or prior head-repository identity still fails closed.

**Implemented behavior.** Accepted events append in observation order as durable
facts and are never updated, deleted, or truncated. The relational storage row
fixes the event version to one, records the content-identity version and 32-byte
digest, records `poll` as the only presently implemented producer, closes both
target and payload discriminators, retains complete PR context, and rejects
incoherent payload columns. Reads decode every field into the closed domain
event and fail closed when a durable cursor or event row is malformed or
noncanonical. Bounded keyset pages expose repository event history in
cursor-generation and event-ordinal order.

**Implemented behavior.** The closed version-one event payloads are:

- `PullRequestOpened`
- `PullRequestClosed`
- `PullRequestMerged`
- `HeadChanged { previous, current }`
- `MergeableStateChanged { current }`, where current is `mergeable`,
  `conflicting`, or `unknown`
- `ChecksCompleted { outcome }`, where outcome is `success` or `failure`;
  completed `success`, `neutral`, and `skipped` suites normalize to `success`
- `CheckRunCompleted { name, conclusion }`
- `BranchWorkflowRunCompleted { branch, workflow, conclusion }`, a branch-level
  event rather than a PR event, including when the watched branch is `main`
- `ReviewSubmitted { reviewer, state, commit }`
- `ThreadOpened { thread }`
- `ThreadResolved { thread }`
- `Labeled { label }`
- `Unlabeled { label }`
- `BaseAdvanced { branch }`
- `ReactionChanged { subject, reactor, content, change }`, where subject is
  `PullRequestBody`, `IssueComment { id }`, or `ReviewComment { id }` and change
  is `added` or `removed`

**Implemented behavior.** The `ReviewState` payload admits only `approved`,
`changes_requested`, and `commented`; no dismissal payload or separate event
kind is constructible.

**Implemented behavior.** The differ emits `ReviewSubmitted` only for a newly
submitted review. A later GitHub dismissal is not a version-one fact and emits
no event. When GitHub returns a null author for a historical review whose
account was deleted, normalization reuses the prior reviewer only for the same
provider review identity; a new identity-less review is omitted.

**Implemented behavior.** Reaction ingestion includes only reactions by a login
in the configured signal-reviewer list. Reactions from every other actor are
excluded while normalizing state, and a reaction whose deleted actor has no
current login identity is never added. When any current reaction for one subject
lacks actor identity, normalization conservatively carries forward the prior
retained reactions for that subject only when their reactors remain in the
current signal-reviewer set, so identity loss cannot manufacture removals and a
filter change cannot preserve an excluded reactor. Why: reviewer signals are
actionable facts; the full ambient emoji stream is neither a rule input nor
retained noise.

**Implemented behavior.** The cursor binds its filtered reaction projection to
the exact canonical signal-reviewer login set. When that set changes, the next
complete poll replaces only the reaction comparison baseline without emitting
`ReactionChanged`; every other state transition remains eligible for its normal
event. Why: comparing projections formed under different filters would
manufacture additions or removals from historical reactions.

**Implemented behavior.** A first observation emits `PullRequestOpened` and the
current `MergeableStateChanged` fact for each open pull request, then
establishes its comparison baseline. This lets an already-conflicting pull
request reach the first live rule without waiting for a later round trip through
another state. Later observations emit a fact exactly when its represented state
changes. Closing by merge emits `PullRequestMerged`, not both merged and closed.
Check-suite completion emits the aggregate success/failure event, completed
individual runs emit their named conclusion events, and branch workflow
completions are compared outside PR state. A base branch head change emits
`BaseAdvanced { branch }` for each open PR based on that branch. Repeated
identical observations emit nothing.

## Structured rules

**Implemented behavior.** Configuration encodes rules as versioned TOML structs,
not a string DSL. Fields within one rule are conjunctive and distinct rules are
disjunctive. Omitting every target field means everything; requiring labels or
supplying regex fields narrows only that rule. There is no global targeting
switch.

**Implemented behavior.** The version-one matcher value has exactly these
fields:

- event kinds as an any-of list without payloads encoded into kind names;
- repository and base branch exact values;
- anchored head-branch, title, and body regular expressions;
- label lists named `any_of`, `all_of`, and `none_of`;
- optional exact draft and author values;
- a mergeable-state `any_of` list; and
- a conclusion `any_of` list.

**Implemented behavior.** The last two fields are the ratified payload
qualifiers. They do not split event kinds by payload. For `ChecksCompleted`,
`success` and `failure` map to the same conclusion values used by the qualifier.
A supplied payload qualifier is false for an event kind to which it does not
apply. Expressiveness grows only by adding versioned fields.

**Implemented behavior.** A branch event cannot satisfy pull-request-only base,
head, title, body, label, draft, author, or mergeable-state fields. Repository,
event-kind, and conclusion fields can apply to either context shape where their
payload exists. An exact-author field is false when GitHub supplies no current
pull-request author.

**Implemented behavior.** Rule validation derives required context shapes from
the event kinds that can satisfy all supplied fields, including the two payload
qualifiers, rather than from the event-kind list alone. Rule identities are at
most 128 bytes and admit only ASCII letters, digits, `.`, `-`, and `_`.

**Implemented behavior.** Regex patterns are nonempty, explicitly anchored by
`^` and `$`, and at most 1,024 UTF-8 bytes. Anchoring applies to the complete
expression, so an alternative cannot escape the whole-candidate boundary.
Construction uses Rust's linear-time `regex` crate; backreferences and
look-around are therefore not admitted and no backtracking engine is present.
The crate's ordinary Unicode properties and case folding remain available, and
an invalid pattern reports the regex compiler's safe diagnostic. Exact branch
fields admit Git's complete `refs/heads/<name>` grammar while storing the name
without that prefix.

## Actions and dispatch context

**Implemented behavior.** Each rule value carries a nonempty ordered list of
tagged action variants. Version one ships exactly one configured variant,
`dispatch_session { template }`; no unused action variant is reserved.

**Implemented behavior.** When a fact matches, every configured action produces
one emitted `dispatch_session { template, params }` action in list order, where
`params` is the exact injected tagged context for that event. When an occupied
match joins an outstanding delivery obligation, the eventual action uses the
latest joined event and adds the current-state delivery member defined below; it
never emits each joined event as a separate action.

**Implemented behavior.** Dispatch context is the ratified tagged union:

- `PullRequestContext { repo, number, head_sha, event }`; or
- `BranchContext { repo, branch, workflow, conclusion, event }`.

**Implemented behavior.** The embedded event is the complete triggering durable
fact, not reconstructed API state. A pull-request event always produces the
first shape and `BranchWorkflowRunCompleted` always produces the second.

**Implemented behavior.** A fresh dispatch carries no `delivery` member. A
dispatch settling an occupied-refusal obligation adds
`delivery { mode = "owed_current_state", obligation_id, matched_event_count, first_event_id, latest_event_id, current }`.
Its embedded event is the latest matched fact joined to that obligation, while
`current` is projected from the durable cursor read for dispatch. Pull-request
current state carries `type`, `present`, and, when present, the complete current
target, lifecycle, mergeability, completed check suites and runs, retained
reviews, review threads, and configured-reviewer reactions. An absent pull
request retains its number and `present = false`. Branch current state carries
the branch, its optional current head, the triggering workflow name, and that
workflow's latest completed runs. The count and boundary identities summarize
collapse; intermediate facts are not replayed into the session.

**Implemented behavior.** A session-template context declaration requires a
nonempty set containing pull-request context, branch context, or both. Rule
validation rejects any attainable event shape not accepted by an action's
declared template. An empty event-kind list without narrowing fields can match
both shapes and therefore requires both. An unknown template or missing context
declaration is also a validation failure; diagnostics safely name the affected
template.

**Implemented behavior.** Configuration completes rule and template-context
validation before polling starts. Dispatch therefore cannot discover a shape
mismatch at runtime.

**Implemented behavior.** Each admitted action creates a fresh session from the
complete resolved template copy and submits the tagged context as its first
accepted JSON text input through the existing `StartWhenNoActiveTurn` path in
the same durable transaction. No dispatched session can become visible without
that accepted input, its queued turn, and the dispatch-to-turn audit link. The
input selects the session's version-one defaults and its template-selected
model. The JSON object carries `type = "pull_request"` or `type = "branch"`, the
fields of that tagged context, and an `event` object containing version, event
identity, repository, complete normalized target, kind, and payload. A durable
delivery intent records the reserved submit-command, accepted-input, turn, and
cancellation candidates beside the applied link. Equal recovery reuses the
complete committed batch. A lost post-commit scheduler nudge remains recoverable
by the ordinary eligibility sweep.

**Implemented behavior.** The same transaction also commissions that session's
goal, so no dispatched session is durably visible without a statement of the
authority it was dispatched under. The statement is synthesized from the
dispatching rule, the resolved template, and that action's typed parameters, and
states only those facts: rule, template, and either the pull request with its
head and base branches or the branch with its workflow and conclusion, each in
its repository. A pull request's head branch is qualified by the repository
holding it, which is the fork rather than the watched repository when the pull
request comes from one, so a consumer cannot read a fork's branch as though it
were the watched repository's. Every one of these repository-supplied
identifiers is delimited where the statement renders it, under the rule stated
in [goal mode](goal-mode.md). These identifiers are named in the statement only;
the injected tagged context is unchanged, and it already carries them inside its
embedded event. It is composed by the dispatch rather than declared by the
session, because only an already-attached goal admits a model declaration, so a
session created without one has no transition available to it. Commissioning
records the tagged-context turn described above as that generation's own first
goal turn rather than scheduling one of its own, so a dispatched session commits
exactly one queued turn. Scheduling a separate goal turn instead would run the
template against the statement alone before the triggering event arrived,
because a turn's acceptance position is also its execution order. Pursuit also
holds the batch's singleton until the goal reaches a terminal state, which is
the release rule stated below rather than a new one.

**Implemented behavior.** Every dispatched session carries its statement from
the moment it is visible, and none arrives later. Commissioning happens inside
the dispatch transaction and a recorded evaluation replays from its committed
batch without re-entering that transaction, so no surface backfills a goal onto
a session dispatched without one and none is owed: a database predating
commissioning is not a supported input, under the pre-alpha compatibility rule
that [AGENTS.md](../../AGENTS.md) states.

**Implemented behavior.** The dispatched work turn is the goal's own turn. Its
accepted input belongs to the submit command that delivered the tagged context,
and that same input is recorded as the commissioned generation's first goal
turn, so one dispatched event queues one turn and runs its template once. Every
later turn in that session is an ordinary goal continuation scheduled from it,
and the generation is readable from the turn doing the dispatched work, so a
supersession while that turn is parked cannot broaden the authority a consumer
reads for it. What this requires of the durable goal rules — a goal turn whose
accepted input carries the command that accepted it, and which therefore does
not restate its statement — is stated in [goal mode](goal-mode.md).

**Committed unimplemented functionality.** No present session-creation or
input-submission surface identifies repository watch as a purpose-specific actor
or creation cause. Version one therefore uses the current user-initiated,
no-ancestry creation interface and user-attributed input interface. A committed
follow-up will add purpose-specific durable repository-watch provenance linked
to `RepoWatchDispatchId`; compatibility requires it to preserve dispatch,
session, context, and input identities rather than recreate or reinterpret them.
This paragraph constrains only that future adoption.

## Deduplication, concurrency, and audit

**Implemented behavior.** Every rule independently selects `singleton_per` from
`pull_request` (the default), `stack`, `rule`, or `repo`, plus a nonnegative
cooldown. Pull-request scope keys by repository and PR number. Stack scope keys
by repository and the base-branch chaining component containing the PR: an open
PR is another open PR's parent only when the prospective parent's head
repository equals the prospective child's event repository (its base repository)
and its head branch equals the child's base branch. Rule scope keys only by rule
identity and version; repository scope adds repository. Branch events cannot
satisfy pull-request or stack scope and make such a rule invalid rather than
silently changing its key. The component identity is the lowest-numbered root,
where a root is a component member without an open parent. A rootless component
formed by a cycle uses its lowest-numbered member. The ordinary single-root case
therefore remains the bottom open PR's number in the watched repository, while
independent PRs do not share a singleton even when forks reuse the same
head-branch name or both target the same destination branch.

**Implemented behavior.** One event/rule match admits its complete ordered
action list as one singleton batch. Admission, creation of every dispatched
session, and every audit record commit in one durable transaction; failure rolls
back the whole batch. Each record links the triggering event, rule identity and
version, singleton key, action ordinal, session-template provenance, and newly
created session. The action ordinal distinguishes sibling sessions without
letting the first action suppress later actions from the same match. An occupied
singleton refuses another match and atomically opens one durable delivery
obligation for that singleton. Further matching facts join its latest-event
projection and increment its count, including a match racing with release, so
one singleton has at most one outstanding obligation. Their individual terminal
evaluations remain append-only audit facts. The batch releases the singleton at
the transition that makes every dispatched turn terminal or runtime-irrelevant,
leaves no live runtime-relevant turn for its session, and leaves no pursuing
goal. A goal-ending recheck is deferred to its transaction boundary so an active
turn's stop cascade is visible. A blocked or user-stopped dispatch session, and
an achieved session whose delivered state is no longer the pull request's latest
durable head, opens the same latest-state obligation before release; sibling
terminations and matching events collapse into that one obligation without
regressing its latest event. A batch delivers its originating event when
admission dispatched that event, and the target's collapsed current state when
admission settled an obligation by replaying a still-matching earlier event.
Achievement is terminal exactly when that delivered state is known and is still
the pull request's latest durable head, so the successor that carried the newest
head seals instead of owing another batch after every cooldown. A batch admitted
before the delivered state was recorded has none, and achievement cannot seal
it: reading its originating event as the delivered state would seal without
delivery whenever a head returns to an earlier value. A branch target records no
durable revision, only a workflow conclusion, so achievement is its own seal
there. A batch owes at most one requeue, at its own release, and the terminal
event deciding it is the one ending the generation the dispatch commissioned; a
later goal generation its session accepts terminates without reopening one,
including while a sibling action still holds the batch. Termination takes the
singleton advisory key that admission takes, and locks the rule activation row
that recording a deactivation shares, so a match racing a termination joins its
obligation and a racing deactivation cannot miss it. Termination does not take
the repository key: it runs inside the transaction ending the goal, which holds
that session row, and lifecycle-cutoff processing takes the repository key
before waiting on the same row, so the reverse order would deadlock a goal pass
against a cutoff attempt. The obligation becomes eligible only after release and
the same cooldown that would suppress a fresh successor; cooldown suppression
without an existing obligation does not create one. Eligibility settles the
obligation and creates its one current-state batch atomically. Equal recovery
cannot create a second session for the same admitted action or obligation. A
session whose current goal is pursuing remains nonterminal for singleton
ownership across the gap between a completed goal turn and its durably queued
continuation. Goal blocking, achievement, or user stop rechecks release after
pursuit ends. The append-only dispatch records identify the sessions responsible
for the PR; no mutable assignment flag replaces them.

**Implemented behavior.** A pull-request close or merge durably records one
lifecycle cutoff. When that lifecycle remains terminal, repository watch applies
the ordinary parent-only stop to each generation-one goal it commissioned for
the pull request; it cannot stop descendants, a later user-authored generation,
or an unrelated session. A later open event makes an earlier unprocessed cutoff
a recorded reopen instead. Dispatch admission rechecks the latest durable
lifecycle under the repository lock. A terminal cutoff settles every outstanding
obligation for that pull request immediately, without waiting for singleton or
cooldown readiness; the admission recheck is the race-closing backstop. Either
path settles stale nonterminal work as `target_closed` without creating a
session. A rule that matches the `PullRequestClosed` or `PullRequestMerged`
event itself remains dispatch-eligible, and a non-converged termination of that
dispatch still owes its requeue while its own cutoff remains the latest one; the
terminal event is the cutoff fact, not work made stale by that fact. Corruption
in one commissioned goal rolls back that goal's stop to a savepoint but does not
roll back the cutoff: the terminal event remains durably dispositioned, healthy
commissioned goals are stopped, and later cutoffs remain eligible for
processing.

**Implemented behavior.** Held singleton batches are directly observable in the
`repo_watch_held_dispatch_slot` projection. Each row identifies the repository,
pull request, rule, singleton key, sessions, and held-since time; states each
release clause independently; and names every failing clause in `blockers`.

**Implemented behavior.** Outstanding obligations are directly observable in the
`repo_watch_outstanding_dispatch_obligation` projection. Each row identifies the
repository, rule, singleton and pull request or stack root, first and latest
matched events, collapsed count and timestamps, any occupying dispatch and its
sessions, cooldown eligibility, and present readiness. Rule deactivation settles
an obligation without dispatch rather than leaving permanently owed work for
semantics that are no longer configured; terminal-target settlement likewise
records why the obligation no longer remains owed.

**Implemented behavior.** A newly configured rule activates immediately after
the repository's current durable event tail, before its task polls, and consumes
later events in cursor and event-ordinal order. Activation and each terminal
evaluation outcome are append-only. Restart resumes the oldest unevaluated fact
and the oldest eligible obligation for that rule version; it neither
redispatches an evaluated fact nor treats pre-activation history as a new live
signal. An obligation is a separate collapsed delivery identity, not a request
to reevaluate its occupied facts. Reconciliation records an append-only
deactivation when a configured identity or its repository disappears. Guarded
daemon startup reconciles the complete repository set before any watch task
starts, including the empty set when the repository-watch section is absent; the
absent section still starts no watch runtime or polling task. Configuration
reconciliation and evaluation are serialized per repository: an evaluation
already committed may replay, but an already-loaded event cannot create a
dispatch after deactivation commits. Activation stores a digest of the complete
versioned matcher, ordered action list, singleton scope, and cooldown; changing
any of those semantics while retaining an active identity is a permanent
configuration failure. A deactivated rule identity and version cannot be
configured again; either kind of replacement uses a new identity so no events
can be evaluated under semantics different from the activation that admitted
them.

**Committed unimplemented functionality.** The structured-rule dispatch surface
converges onto the program substrate by replacing each rule with a subscription
whose action is a built-in dispatch program. This page owns that ingress
cutover. Shadowing is validation only and never owns delivery. Cutover commits
in one durable transaction at an event frontier after requiring a terminal
evaluation outcome for every old-rule event through that frontier: it records
deactivation of the old rule after the frontier, activation of the replacement
subscription strictly after it, and the mapping from rule identity and version
to the exact program registration. The same transaction transfers every occupied
singleton batch, its responsible sessions, and any recorded cooldown boundary to
substrate-owned dispatch state without recreating sessions or changing
append-only audit identities. Events at or before the frontier remain owned only
by rule evaluation; later events are owned only by subscription matching.
Reconciliation, rule evaluation, event commit, and subscription matching
serialize against this transaction, so a crash or concurrent poll may retry it
but cannot omit or dispatch a boundary event twice or release an occupied
singleton. After this transaction, the mapped rule is a subscription;
subscription identity, delivery, continuation cursor inheritance, and
cancellation follow the [program substrate](program-substrate.md).

## Live merge-forward rule

**Foundation contract.** The live merge-forward rule is
`merge-forward-on-base-advance`. It matches `BaseAdvanced` for pull requests
whose head branch matches `^agent/.+$`, uses pull-request singleton scope, and
dispatches the merge-forward session template configured with the approved cheap
model and pull-request context. Because each `BaseAdvanced` fact targets an open
pull request based on the branch that advanced, the rule dispatches for each
immediate dependent when a stacked parent branch advances and for each matching
main-based pull request when `main` advances. It does not wait for a workflow
conclusion or a `MergeableStateChanged` conflict fact, and a parent's own
`HeadChanged` fact does not dispatch the parent.

## Designed-for version-two poll-cache persistence

**Committed unimplemented functionality.** No present cursor persists HTTP
validators or per-resource/page accepted transport snapshots. Version two is
designed to pair each bounded canonical resource/page key and ETag with a typed,
minimal snapshot sufficient to reconstruct that resource's normalized
contribution and the identities needed for nested fetches after restart. It does
not persist raw provider JSON, credentials, or reactions from actors outside the
configured signal-reviewer set. These snapshots remain transport state: rules
and durable events cannot inspect them. Until this upgrade is built, every
daemon restart deliberately pays one bounded complete repository poll.

## Webhook transport and shadow reconciliation

**Implemented behavior.** The listener accepts only `POST` on its configured
path as plain HTTP. It requires canonical singleton GitHub hook, delivery,
event, content-type, and `X-Hub-Signature-256` headers; rejects content
encodings other than absent or `identity`; and accepts transfer encoding only
when absent or `chunked`. Hook ID selects the repository and its separately
bounded, reread-on-request secret before the body is interpreted. The receiver
collects at most 25 MiB, verifies lowercase `sha256=` HMAC-SHA-256 against the
exact body bytes with constant-time comparison, and only then parses JSON. The
body's canonical repository must equal the selected repository. It admits at
most 64 requests concurrently, retains at most 128 MiB of request bodies across
them, and reads any one body within 30 seconds; a peer that stalls its body
therefore releases its concurrency slot and memory reservation instead of
holding both. The received buffer is released before the delivery is persisted,
so what the budget accounts for is what is actually held across that wait.

The receiver bounds request heads as well as bodies. One request may carry at
most 64 header fields and 32 KiB of aggregate header bytes counted across every
name and value, and a head that exceeds either is refused with `431` before any
credential is read or any body is collected. Below the router, the connection
itself refuses a head that does not fit a 64 KiB read buffer, which is the
memory bound on a head that is still arriving. All are hard safety ceilings.

Nothing the handler bounds begins until whole request headers arrive, so two
further bounds sit before it: the listener holds at most 256 connections at
once, taken at accept time before any router or handler work, and it retires any
connection whose read makes no progress for 15 seconds. The connection budget is
what bounds a peer that keeps dripping bytes, since each byte received restarts
that deadline. The daemon serves HTTP/1 directly for this reason: the head
ceilings sit on the connection builder, below any router.

Each hook admits at most 3,000 deliveries in each 60-second window, charged only
once a delivery has proved the shared secret. Nothing is charged before
verification: a budget keyed on the hook a request claims is a lever the
attacker holds and GitHub does not, and spending it with forged signatures would
reject the deliveries it exists to protect. Unauthenticated cost is bounded by
resources instead. These are hard safety ceilings, not configuration knobs. The
listener does not grant GitHub-originated data process-protocol authority,
session authority, or polling credentials.

**Implemented behavior.** A verified delivery is durably admitted before the
listener returns `202 Accepted`. `repo_watch_webhook_delivery` keeps the unique
`(hook_id, delivery_id)` tombstone, body digest, repository, bounded event and
action names, receipt sequence, and receipt time; `repo_watch_webhook_payload`
keeps the exact bytes. An equal replay returns the same success without new
work. Reuse of that identity with a different digest returns conflict and cannot
replace the first body. Every new admission and equal durable replay publishes a
coalescing in-memory wake after commit; the listener returns success only while
that repository's drain task remains available to receive it. The wake is an
accelerator over durable state, not a work inventory: the repository task drains
durable pending deliveries on startup, when woken, and as part of every full
poll for which no retry is already owed, and a wake arriving during a drain
remains observable for a follow-up attempt. One pending page is bounded by both
its row count and the exact payload bytes it may hold, and it reads those bodies
one at a time, so a backlog of near-limit deliveries cannot retain far more than
admission itself is allowed to; the oldest delivery is always read, so one body
at the admission ceiling still drains, and every later body is discarded rather
than allowed to overshoot, so a page retains no more than that ceiling. One
drain visits a bounded number of pending pages and then re-arms that same wake,
so a sustained stream is accelerated without holding the worker past an overdue
full poll. A terminal commit whose result is lost in transit is resolved by
re-recording the same request, which reports the row already terminal or records
it now, rather than leaving a durable disposition the shadow never accounted
for. A delivery whose processing fails is deferred for the rest of that drain
rather than failing it, so one persistently unprocessable receipt cannot pin the
head of the queue and starve every later one; the attempt still reports the
first such failure. A signature-valid delivery whose event or action is outside
the mapped set, including a broadly subscribed `workflow_job`, is still
acknowledged successfully and is cheaply logged and recorded as ignored rather
than treated as an intake failure.

**Implemented behavior.** A drain page attempts every loaded delivery even when
one delivery fails. Each failure is logged at warning level with the delivery
identity and a closed cause, and the drain itself emits an error-level record
carrying the first such cause, whichever attempt performed it — a startup drain,
a wake, a retry, or a full poll. A delivery that fails before its terminal
disposition is recorded remains pending, and its successful page peers still
reach terminal state: a targeted refresh the provider will not serve is one such
failure, because that query runs before anything is recorded. A delivery whose
disposition is already durable when a later step fails — the targeted commit, or
the dispatch work that follows it — is terminal and is not loaded again. The
repository task schedules a new drain attempt after five seconds without waiting
for a full poll, another delivery, or a restart. Consecutive failures double
that delay to a five-minute ceiling and a success returns it to five seconds, so
a delivery that cannot be projected costs bounded repeated work rather than a
fixed five-second loop. Only the drain advances that delay: an attempt whose
drain succeeded and whose dispatch work then failed returns it to five seconds,
and one that failed before reaching the drain arms a retry at the current delay
without advancing it, so unrelated dispatch failures cannot grow the delay,
suppress admission wakes, or make polls omit their drain steps while projection
is healthy. Such a retry always lands in the future: leaving an owed deadline in
the past would select the same attempt again immediately, because the retry
precedes the poll. A poll's own drain runs at most once per attempt — the step
after the poll is skipped when the step before it failed — so work already known
to be failing waits for that delay rather than repeating inside one attempt. A
failed full poll schedules that retry only when none is already owed, an
admission wake is suppressed while one is, and a full poll omits both of its
drain steps while one is, so neither a rapidly failing poll nor an authenticated
replay stream can defer or bypass the backoff; a suppressed wake coalesces and
is observed by the attempt that follows the retry. A poll taken during a backoff
window can therefore advance the cursor past a pending delivery's transition,
which that delivery then records as duplicate state — accepted as a bounded
shadow-mode fidelity loss against an unbounded repetition of work already
failing. An overdue retry is taken ahead of an overdue poll, and a full poll
that outlasts its own interval schedules the next one a whole interval from
completion; without both, a poll deadline that is always already elapsed would
win every scheduling decision and starve durable webhook work for as long as
polling kept failing. An independent per-repository observer checks durable
pending work every thirty seconds, reading delivery identity and receipt time
only and never the admitted body. That cadence is anchored, so a slow inspection
does not push the next one out by its own duration, and the inspection is
bounded at ten seconds so a connection pool exhausted by wedged repositories
produces a closed timeout cause rather than silence; once the oldest delivery
has remained undispositioned for one minute it emits an error-level stall signal
with the repository, delivery identity, receipt sequence, pending age, and
closed stall cause. Because the observer is not the serialized drain task, a
task wedged in polling, projection, disposition, or dispatch cannot silence that
signal, and the observer's own inspection is cancelled by shutdown so an
unresponsive database cannot hold daemon termination.

**Implemented behavior.** Shadow mode never inserts a webhook-produced row into
`repo_watch_event` and never mutates the cursor from a payload-derived patch.
Instead, the repository's single serialized worker applies the closed guarded
patch to the latest cursor in memory and runs the same
`derive_repo_watch_events` differ with the same
`RepoWatchEventContentIdentityV1` frontier used by polling. Occurrences of the
families a delivery cannot observe — computed mergeability and aggregate check
rollups — are not projected from a delivery at all, since a payload supplies
neither and projecting one would invent a webhook-only row for a value only
polling can produce. That baseline is cumulative and belongs to the repository
task rather than to one drain: a projection that does not mutate the durable
cursor still advances the observation and frontier the next delivery compares
against, whether that delivery arrives in the same batch, in a later wake, or
after another delivery was deferred. A delivery advances it only once its own
terminal disposition is durable, so a failure between deriving and recording
leaves the accumulated shadow exactly as it was.

Only a full poll replaces that baseline, because only a full poll is the
complete reconciliation sweep, and only once nothing is still pending. The poll
does not perform that handover itself: the worker cannot read the pending queue
atomically with an admission committing on the listener, so a delivery admitted
while the poll was fetching could otherwise be applied to a cursor that already
contains its transition and be recorded as a duplicate. The poll marks the
baseline superseded instead, and the first drain that finds its page empty
performs the replacement, deciding both without an await between them. A
targeted query reconciles just the pull requests it names, so its commit is left
to the cursor and the shadow is kept; the accepted cost is that what a targeted
query learns reaches the shadow at the next full poll rather than immediately.
Pending deliveries are drained before a full poll as well as after it. That
drain failing is reported and not propagated: acceleration is not allowed to
cancel the reconciliation sweep, so one delivery whose targeted request keeps
failing cannot abort every scheduled poll. A poll that observes the same
transition as an already-admitted delivery cannot advance the cursor past it and
leave the delivery applying to state that already contains it. A delivery's
targeted provider queries run before anything is recorded, so a transient
provider failure leaves it pending and retryable rather than terminal with a
query that never ran; its terminal disposition is then recorded before the
resulting cursor commit, so a failure between those two cannot make a retry
re-derive projections against a cursor that has already moved past them. On
daemon restart the baseline is re-seeded from the durable cursor, which is the
same complete reconciliation a full poll performs. The divergence a re-seeding
leaves is accepted rather than removed: a delivery projected against a freshly
seeded baseline records `cross_drain_shadow_gap` on its projections, so the gap
is explained in the parity view instead of being carried by a durable shadow
cursor. `repo_watch_webhook_projection` records each resulting version-one
content identity and event kind, and the cause of any divergence the producing
delivery already knows, while `repo_watch_webhook_disposition` atomically
records projected, duplicate-state, superseded, ignored, or quarantined terminal
disposition. Shadow mode reserves no committed disposition and no resulting
cursor generation: the schema refuses both, so the durable shape a later write
mode would need is left to the ruling that authorizes it. The
`repo_watch_webhook_parity` view joins those identities to version-one
poll-produced `repo_watch_event` rows since that repository's first shadow
receipt and reports `matched`, `webhook_only`, `poll_only`, or
`not_directly_mapped`, each divergent row alongside a `cause` drawn from one
closed vocabulary: `compressed_transition`, `context_drift`, `poll_only_family`,
and `cross_drain_shadow_gap`. A delivery records the cause it knows beside its
own projection; `poll_only_family` is derived instead, because the event
families polling produces and webhooks are not designed to reproduce —
mergeability changes, aggregate check rollups, and reaction changes — have no
delivery to carry it. Event projections intentionally carry no uniqueness
constraint because separate deliveries may represent one content occurrence.
Terminal exact payload bytes remain for seven days, after which maintenance may
delete only the payload; delivery tombstones, digests, projections, and
dispositions remain append-only.

**Implemented behavior.** The mapped set is pull-request open, reopen, close,
synchronize, label, unlabel, edit, draft conversion, and ready-for-review;
submitted pull-request review; resolved or unresolved review thread; completed
check run; completed check suite; completed workflow run; branch push, create,
advance, and delete as represented by GitHub's `push` payload; and ping. Review
dismissal and ping are mapped no-change. Tag pushes, the separate `create` and
`delete` event families, foreign-repository workflow heads, and every other
signature-valid event or action are ignored successfully. Guards make stale
head, lifecycle, branch, workflow-attempt, and immutable-provider facts
superseded or duplicate rather than allowing regression. A rerequested check run
replaces the retained completion only when its provider completion generation is
no older, so a delayed original completion is superseded instead of regressing
the baseline; an equal generation still replaces, which is how a conclusion edit
arrives. A workflow completion whose branch head is already gone is superseded,
because polling projects workflow runs only for the heads it queries and could
never reproduce it. A delivered run adopts the workflow name retained state
already carries for that workflow identity, since polling names a run from the
workflows endpoint's current name and the occurrence identity hashes it. A
completed check run rerequested under the same provider identity carries a new
completion generation or conclusion, which the differ treats as a new observable
completion, so the retained run is replaced under the same head guard. GitHub
represents `pull_request.head.repo` as null once a tracked fork is deleted; like
the poll normalizer, the mapper models that field as optional and application
reuses the retained canonical head repository. An opened or reopened delivery
whose pull request has no canonical baseline applies its complete delivered
context rather than projecting only its hydration query, so the occurrence the
following targeted poll also produces is matched instead of reported as
poll-only.

**Implemented behavior.** Payloads do not authoritatively supply GitHub's
computed mergeability or complete check rollups. A mapped delivery that needs a
missing pull-request baseline, current mergeability, or a check rollup records a
targeted-query projection and immediately reuses the repository poller's
credential, client, conditional cache, normalization, and request bounds to
fetch only the affected pull requests. Those observations commit through the
ordinary poll producer and dispatch path. Full polling continues unchanged as
the slow complete reconciliation sweep and remains authoritative for missed
deliveries, reactions, and every provider fact outside the mapped set. Poll
frequency does not drop in shadow mode; any later write mode or slower cadence
requires a separately reviewed ruling after parity over a real workday.

**Committed unimplemented functionality.** The rollout gate is no *unexplained*
divergence, not no divergence: it is zero `repo_watch_webhook_parity` rows whose
status is `webhook_only` or `poll_only` and whose cause is null, measured over a
real workday. Divergence that names a closed cause is understood and does not
hold the gate. Reaching zero uncaused rows is the remaining rollout work, since
the runtime today records `cross_drain_shadow_gap` and derives
`poll_only_family`, while `compressed_transition` and `context_drift` are
available to record and not yet emitted.

## Open edges

**Deferred or undecided work.** No repository-watch design question remains open
for the commissioned version-one stack. Additional transports, event kinds,
payload qualifiers, matcher fields, actions, and singleton scopes require later
ratified extensions.

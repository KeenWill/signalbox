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
(`agent/repo-watch-dispatch`). The polling and differ behavior below is verified
against this PR (`agent/repo-watch-review-reliability`). The provider members
the poller adopts as check-suite and check-run completion generations are
verified against PR #541 (`fix/check-run-updated-at`). The goal a dispatch
commissions with its session, and the binding of the dispatched work turn to
that goal's generation, are verified against this PR
(`agent/commission-binding`). Exact-object correlation between session GitHub
writes and watcher events is verified against this PR
(`agent/repo-watch-self-cause`). Pull-request lifecycle dispatch cutoff and
commission withdrawal are verified against this PR
(`agent/repo-watch-lifecycle-cutoff`).

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
by the duration of its own attempt; attempts never overlap, and an attempt that
reaches or exceeds the interval is followed immediately by the next. Version one
has no webhook fallback and no speculative second polling transport.

**Implemented behavior.** One attempt fetches up to eight open pull requests
concurrently. The fetch sequence within a single pull request stays ordered, and
the fetched pull requests are ordered by number before comparison, so
concurrency cannot reorder a baseline. A single attempt may transfer up to 512
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

**Implemented behavior.** The versioned durable cursor retains only the complete
normalized repository state and exact signal-reviewer set needed for comparison.
It does not retain resource keys, ETags, accepted transport responses, raw
provider payloads, or credentials. A per-repository atomic commit accepts an
expected generation, one complete cursor candidate, and its ordered event batch.
It serializes competing commits, appends the cursor and every event together,
rolls back the whole batch on failure, reports a stale generation as conflict,
and recognizes only an exact candidate-and-event replay. An unchanged candidate
with no events does not advance the cursor; an unchanged candidate carrying
events is rejected. The relational event table admits an event row only in the
database transaction that inserts its referenced cursor generation, preventing
later maintenance or future writers from changing an already-committed batch.

**Implemented behavior.** The version-one cursor reader remains compatible with
the earlier version-one workflow record that lacked a workflow-definition ID. It
admits only the otherwise-canonical legacy shape, uses the retained run ID as
the definition-identity sentinel, suppresses the same completed run attempt by
branch, run ID, and attempt number, and writes the complete current shape on the
next successful commit. A legacy cursor therefore cannot permanently block its
repository.

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
fixes the event version to one, closes both target and payload discriminators,
retains complete PR context, and rejects incoherent payload columns. Reads
decode every field into the closed domain event and fail closed when a durable
cursor or event row is malformed or noncanonical. Bounded keyset pages expose
repository event history in cursor-generation and event-ordinal order.

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
- `ReviewSubmitted { id, reviewer, state, commit }`, where `id` is GitHub's
  immutable numeric review identity
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
`params` is the exact injected tagged context for that event.

**Implemented behavior.** Dispatch context is the ratified tagged union:

- `PullRequestContext { repo, number, head_sha, event }`; or
- `BranchContext { repo, branch, workflow, conclusion, event }`.

**Implemented behavior.** The embedded event is the complete triggering durable
fact, not reconstructed API state. A pull-request event always produces the
first shape and `BranchWorkflowRunCompleted` always produces the second.

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
singleton refuses another match. The batch releases it at the terminal
transition that makes every dispatched session in that batch terminal; cooldown
is measured from that recorded transition rather than from later watcher work
and suppresses a successor until its interval has elapsed. Equal recovery cannot
create a second session for the same admitted action. A session whose current
goal is pursuing remains nonterminal for singleton ownership across the gap
between a completed goal turn and its durably queued continuation. Goal
blocking, achievement, or user stop rechecks release after pursuit ends. The
append-only dispatch records identify the sessions responsible for the PR; no
mutable assignment flag replaces them.

**Implemented behavior.** A newly configured rule activates immediately after
the repository's current durable event tail, before its task polls, and consumes
later events in cursor and event-ordinal order. Activation and each terminal
evaluation outcome are append-only. Restart resumes the oldest unevaluated fact
for that rule version; it neither redispatches an evaluated fact nor treats
pre-activation history as a new live signal. Reconciliation records an
append-only deactivation when a configured identity or its repository
disappears. Guarded daemon startup reconciles the complete repository set before
any watch task starts, including the empty set when the repository-watch section
is absent; the absent section still starts no watch runtime or polling task.
Configuration reconciliation and evaluation are serialized per repository: an
evaluation already committed may replay, but an already-loaded event cannot
create a dispatch after deactivation commits. Activation stores a digest of the
complete versioned matcher, ordered action list, singleton scope, and cooldown;
changing any of those semantics while retaining an active identity is a
permanent configuration failure. A deactivated rule identity and version cannot
be configured again; either kind of replacement uses a new identity so no events
can be evaluated under semantics different from the activation that admitted
them.

**Implemented behavior.** Completed session mutations record the exact GitHub
review, comment, or review-thread identity acknowledged by the provider. A
repository-watch cursor that observes a matching review write, thread reply, or
thread resolution durably links every resulting event to the creating tool
attempt. For mutable thread resolutions, receipt ordering is compared with the
database-clock instant immediately after the provider snapshot is observed, not
with the start of the whole poll. A receipt completed during the poll but before
that instant can therefore cause the state contained in the snapshot, while a
receipt completed after the snapshot cannot suppress an earlier user resolution.
Each receipt records its first matching cursor once. A published review's
immutable receipt remains directly correlatable to each exact child thread
without recurring per-generation observation rows, so the review and a delayed
inline-thread event carry the same cause once each. An unchanged poll still
consumes a mutable thread receipt when it is the first cursor containing that
exact thread, whether or not the requested transition remains visible, so a
later user transition remains dispatchable.

Evaluation defers an event without recording an outcome while a potentially
matching GitHub mutation is still in flight or commit-ambiguous. If provider
visibility preceded persistence of the successful mutation result, evaluation
after completion reconciles the exact review or thread identity to an event
recorded between the tool attempt and its receipt. The retried matching rule
then records `self_caused` and creates no dispatch batch or session. Correlation
uses provider object identity, never author login: a distinct user-created
review remains eligible even when the session token and user share one login.

**Implemented behavior.** Pull-request dispatch admission reads the latest
durable lifecycle under the repository transaction lock. A matching event whose
target is currently closed or merged records `target_closed` and creates no
batch or session, including when evaluation loaded the event before the closing
poll committed. Branch events are unchanged, and absent pull-request lifecycle
evidence fails closed as durable corruption rather than assuming an open target.

**Implemented behavior.** A rule-independent lifecycle consumer runs before
dispatch evaluation at startup and immediately after every poll commit. Each
durable close or merge is processed exactly once; a historical closure whose
latest lifecycle is reopened records a no-op. Otherwise it composes a
parent-only stop for every still-active generation-one goal commissioned by
repository watch for that repository and pull request. It does not stop an
operator session, descendants, an already-terminal goal, or a later
user-authored generation. Cutoff completion and each applied stop are
append-only audit records. Stopping a queued commission makes its delivery turn
non-runtime-relevant and releases the dispatch singleton in the same
transaction, while a running turn may finish but cannot schedule a successor.

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

## First live rule

**Foundation contract.** The first deployed rule is `merge-forward-on-conflict`.
It matches `MergeableStateChanged` with
`mergeable_state.any_of = ["conflicting"]`, uses pull-request singleton scope,
and dispatches the merge-forward session template configured with the approved
cheap model and pull-request context. The rule does not match transitions back
to `mergeable` or `unknown`.

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

## Designed-for version-two webhook transport

**Committed unimplemented functionality.** No present surface accepts repository
webhooks. Version two is designed to use a separate network ingress that admits
GitHub deliveries, verifies `X-Hub-Signature-256` HMAC over the exact request
bytes before parsing, bounds body and header sizes, rejects missing or malformed
delivery identity, deduplicates delivery identities, and maps accepted payloads
into the identical versioned event vocabulary. It has its own listener,
authentication, admission rules, rate limits, telemetry, and credential
material. It is never multiplexed onto or tunneled through the local
process-protocol socket.

**Committed unimplemented functionality.** A future webhook receiver does not
grant GitHub-originated data process-protocol authority, session authority, or
watch credentials. Repository-specific secrets select the admitted repository
before event derivation; a valid signature for one repository cannot submit an
event for another. Replay, parser differential, oversized-body, timing-safe
signature comparison, secret rotation, proxy trust, and denial-of-service tests
are required before that ingress can ship. These requirements constrain
compatibility only; version one provides no listener or webhook configuration.

## Open edges

**Deferred or undecided work.** No repository-watch design question remains open
for the commissioned version-one stack. Additional transports, event kinds,
payload qualifiers, matcher fields, actions, and singleton scopes require later
ratified extensions.

# Repository watch and event dispatch

**Foundation contract.** Repository watch is a credentialed external-ingress
boundary from its first operation: the daemon holds a distinct credential-file
reference for each configured repository, reads secret bytes only for that
repository's request, and never gives a dispatched session the watch credential.
Per-repository tokens carry the least GitHub scope needed to read the configured
signals and dismiss an eligible stale pull-request review. This is the C0
confused-deputy boundary: a credential for one repository cannot authorize a
request to another repository. A repository without a credential-file reference
is invalid configuration and is not watched; a repository absent from the list
is not watched; an absent repository-watch section means that the subsystem does
not start. Dispatched sessions retain the approval posture of their named
session templates, without authority inherited from the watcher.

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
(`agent/dispatch-requeue-on-invalidation`). Safe rule revision admission and
configuration diagnostics are verified against PR #863
(`agent/repo-watch-rule-robustness`). Bounded dispatch-start leases, priority
nudges, expiry retirement, and nudge outcome telemetry are verified against this
PR (`agent/dispatch-start-lease`). Exact-head convergence assessment and cutoff
are verified against PR #832 (`agent/dispatch-autonomy-convergence`). The
dispatch attempt budget, the delay between attempts, the parked state, and both
ways out of it are verified against PR #980 (`agent/dispatch-retry-budget`). The
source-independent event occurrence identity, its durable frontier, the
commit-time coalescing of a restated occurrence, and the storage migration are
verified against PR #870 (`agent/repo-watch-content-identity`). The
authenticated webhook intake, its ingress ceilings, shadow projection, parity
view and causes, and targeted refresh behavior are verified against this PR
(`agent/repo-watch-webhook-receiver`). The projection coverage enumeration,
pull-request issue-comment behavior, per-page hydration coalescing, and
workflow-run branch symmetry below are verified against PR #891
(`agent/webhook-event-mapping`). Webhook drain liveness and stall reporting are
verified against PR #896 (`agent/webhook-projection-drain`); the drain attempt
deadline is verified against this PR
(`agent/daemon-live-webhook-drain-deadline`), and the enclosing webhook-attempt
deadline is verified against this PR
(`agent/daemon-live-webhook-attempt-deadline`). The provider-wide page backoff
is verified against this PR (`agent/daemon-live-webhook-provider-backoff`).
Webhook preemption of slow complete reconciliation is verified against PR #926
(`agent/webhook-projection-preemption-review`). The finite cutoff and dispatch
reconciliation quanta ahead of and after a webhook drain are verified against
this PR (`agent/daemon-live-bounded-repo-reconciliation`). Repeatable preemption
while durable drain pages remain is verified against this PR
(`agent/daemon-live-repeatable-webhook-preemption`). The approval-judge dispatch
fence and unattended escalation release described below are verified against
this PR (`agent/headless-approval-escalation`). The operator-commissioned
dispatch fence and its unattended-escalation coverage are verified against this
PR (`agent/commissioned-dispatch-fence`); its attended escalation park is
verified against this PR (`agent/daemon-live-headless-approval-park`). External
commissioned-session obligation blocking and blocker replacement are verified
against this PR (`agent/daemon-convergence-sweep`). Conservative stale
blocking-review dismissal is verified against this PR
(`agent/dispatch-autonomy-review-clearance`).

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
polling begins. A rule revision is a positive integer within signed 64-bit
range. Changing the revision does not select a different matcher grammar: the
section and its rule shape remain version one, while the revision distinguishes
successive semantics under one stable operator-assigned rule identity.

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
`/etc/signalbox/github-webhook-secret`. Public reachability and its availability
belong to deployment, not to the daemon.

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
the same repository task, but may preempt the read-only provider sweep of an
in-flight complete poll so admitted durable work cannot wait behind that slow
sweep. Rule activation, dispatch, webhook projection, and cursor commit remain
outside that cancellation region. The task joins the cancelled poll's spawned
fetches, invalidates its partial pull-request freshness, drains webhook work,
and returns the still-due complete poll through a fresh interruptible scheduler
pass. Each bounded drain page re-arms that pass while durable remainder exists,
so a long provider sweep cannot become uninterruptible after the first wake;
once a page observes no remainder, it leaves no continuation wake and the
complete poll proceeds. A drain retry in backoff suppresses admission preemption
until its deadline, and that retry deadline can itself interrupt the provider
sweep. The original cycle start remains the cadence anchor. Each leading or
trailing lifecycle-cutoff phase and rule-or-obligation dispatch phase settles at
most 16 durable records before returning to the webhook-aware scheduler.
Reaching that ceiling re-arms the repository wake, so durable remainder is
revisited without letting a sustained reconciliation backlog indefinitely delay
webhook work.

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
restarted daemon re-fetches every pull request at its next complete poll. A
restart that finds a durable cursor schedules that poll at the configured
cadence instead of immediately, while startup still drains durable webhook work
before scheduling it. A first-ever watch with no cursor polls immediately to
establish its baseline. This keeps operational restarts from multiplying the
provider request budget independently of the configured cadence. Warm-restart
poll scheduling is verified against this PR
(`agent/daemon-live-warm-start-poll-cadence`).

**Implemented behavior.** Check-suite and check-run requests explicitly select
all attempts and follow bounded result pages. The complete paginated suite
inventory proves whether the provider's commit check-run search can be
exhaustive: a nonempty inventory at no more than its 1,000-suite ceiling uses
one paginated commit search in place of the request-per-suite fanout, while an
empty inventory needs no run query and a larger one enumerates each suite
individually. Every completed provider identity returned by that projection
enters the comparison baseline; the provider's latest-attempt default cannot
silently discard a completion between polls. This bounded request optimization
is verified against this PR (`agent/daemon-live-github-rest-quota`).

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
event version, canonical target and the identifying members of the event
payload, a separately domain-separated source-independent stream identity, and
the stream's positive occurrence sequence. Identifying is narrower than
complete: the exclusions below are part of the contract, and a second producer
deriving this identity excludes exactly the same members, because hashing either
one derives a different identity for the same fact and defeats the
cross-producer coalescing this identity exists to enable. The stream identity is
closed by event kind. Recurring PR lifecycle, mergeability, head, label, thread,
branch-advance, and reaction streams name the PR and their kind-specific label,
thread, branch, or reaction members. Check runs are recurring too, naming their
provider run identity and completion generation: a completed run edited back to
an earlier conclusion restates that conclusion's facts exactly, so only an
advancing occurrence sequence keeps the restored event's identity distinct from
the first, and without it the commit would coalesce the restored conclusion away
rather than announce it. Immutable check-suite facts name their provider
identity and completion generation; reviews name their provider review identity;
workflow facts name branch, workflow identity, run identity, and attempt. The
normalized review observation has no submitted-time member, so version one
assumes the provider review identity alone uniquely identifies that immutable
submission. Two payload members are excluded from the digest, and only these
two. The random `RepoWatchEventId` is excluded because a re-derivation of one
occurrence mints a fresh candidate. The workflow display name is excluded
because it is rule-visible payload rather than an identifying member: the differ
suppresses a re-observed run attempt on members the stream identity already
names, and a provider can rename a workflow under all of them, so hashing the
name would mint a new identity for a run that leaves the observation and returns
after a rename. Both remain in the event payload that rules read.

**Implemented behavior.** A later equal fact on a recurring stream advances its
sequence and therefore has a different content identity. Equal normalized facts
derived from an equal cursor frontier have the same content identity even when
their candidate UUIDs differ. Persistence rejects duplicate UUID or content
identity members within one batch, and the relational store uniquely constrains
`(content_identity_version, content_identity)` across batches. Exact replay
compares the cursor candidate and accounts for every requested occurrence. If
the replayed generation stored the occurrence, the stored event's whole
UUID-bearing value and content identity are compared. If that generation
coalesced the occurrence, it must be durable in an earlier generation under the
same content identity and identified content. A coalesced occurrence's own
candidate UUID is neither persisted nor compared, because the fact it restates
is durable under the UUID of the occurrence that first recorded it, so a request
whose occurrences are all coalesced replays on candidate and content identity
alone.

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
later turn of that generation is an ordinary goal continuation scheduled from
it, and the generation is readable from the turn doing the dispatched work, so a
supersession while that turn is parked cannot broaden the authority a consumer
reads for it. What this requires of the durable goal rules — a goal turn whose
accepted input carries the command that accepted it, and which therefore does
not restate its statement — is stated in [goal mode](goal-mode.md).

**Implemented behavior.** The dispatch action is also the immutable authority
source for an approval judge invoked by the generation that dispatch
commissioned. A pull-request dispatch supplies its repository, pull-request
number, exact head commit, head repository and branch, and base branch; a branch
dispatch supplies its repository and branch. These values are read from the
append-only dispatch event and action, not inferred from the synthesized goal or
refreshed provider state. A judged turn recorded in any other generation of that
session — an unrelated successor goal the session later accepted — resolves no
dispatch authority at all, as does a turn no generation recorded: the dispatch
described neither, and judging them against its repository, head, and base
values would both apply a fence to work it never named and route their
escalations down the unattended path below rather than to the user whose goal it
is. The tool approval contract governs their quoted rendering and the judge's
decision use in [tool loop](tool-loop.md#approval-policy-and-decision-sources).

**Implemented behavior.** An operator-commissioned dispatch supplies the same
immutable authority for a session no rule dispatched. The `commission_session`
process request — its framing is owned by
[process protocol](process-protocol.md), its identity and durable-command
mechanics by [identity and commands](identity-and-commands.md) — commits, in one
durable transaction, a session created from a daemon-held template, the
append-only `commissioned_dispatch` fence row naming that session, the caller's
context as the session's first accepted input through the existing
`StartWhenNoActiveTurn` path, and the goal commissioned from the caller's
statement, which adopts that input's reserved turn as the generation's own first
turn. The fence carries exactly the dispatch-fence shapes above: a pull-request
fence names the repository, pull-request number, exact head commit, head
repository and branch, and base branch; a branch fence names the repository and
branch. The approval judge resolves either append-only source under the same
generation-one binding, renders both through one authority rendering, and
refuses a session recording both as corruption. The commission's durable command
identity binds its template, fence, statement, and the digest of its initial
content: an equal retry replays the committed session and fence — resolved from
the durable record before any template resolution, so replay survives template
configuration drift — and the same identity naming different intent, an ordinary
session creation included, is refused as conflicting reuse in both directions.

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
singleton, or an independently commissioned live session owning the same pull
request, refuses another match and atomically opens one durable delivery
obligation for that singleton. The obligation records exactly one blocker: the
occupying repository-watch dispatch or the external commissioned session. If an
previous blocker terminates but another session is live at redispatch admission,
the same obligation atomically replaces its blocker and remains non-ready.
Further matching facts join its latest-event projection and increment its count,
including a match racing with release, so one singleton has at most one
outstanding obligation. Their individual terminal evaluations remain append-only
audit facts. The batch releases the singleton at the transition that makes every
dispatched turn terminal or runtime-irrelevant, leaves no live runtime-relevant
turn for its session, and leaves no pursuing goal. A goal-ending recheck is
deferred to its transaction boundary so an active turn's stop cascade is
visible. A blocked or user-stopped dispatch session, and an achieved session
whose delivered state is no longer the pull request's latest durable head, opens
the same latest-state obligation before release; sibling terminations and
matching events collapse into that one obligation without regressing its latest
event. A batch delivers its originating event when admission dispatched that
event, and the target's collapsed current state when admission settled an
obligation by replaying a still-matching earlier event. Achievement is terminal
exactly when that delivered state is known and is still the pull request's
latest durable head, so the successor that carried the newest head seals instead
of owing another batch after every cooldown. A batch admitted before the
delivered state was recorded has none, and achievement cannot seal it: reading
its originating event as the delivered state would seal without delivery
whenever a head returns to an earlier value. A branch target records no durable
revision, only a workflow conclusion, so achievement is its own seal there. A
batch owes at most one requeue, at its own release, and the terminal event
deciding it is the one ending the generation the dispatch commissioned; a later
goal generation its session accepts terminates without reopening one, including
while a sibling action still holds the batch. Termination takes the singleton
advisory key that admission takes, and locks the rule activation row that
recording a deactivation shares, so a match racing a termination joins its
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

**Implemented behavior.** Every dispatched action admits an immutable durable
start lease in the same transaction as its session and initial turn. The
production ceiling is five minutes and is enforced both by the stored
timestamp-shape constraint and by a code path that may lower but never raise the
limit; it is not deployment configuration. Any durable `model_call` row for the
session is first-call evidence and ends the start obligation without rewriting
the lease. Repository reconciliation loads the oldest bounded set of unexpired
leases without that evidence and reissues priority nudges, including after
restart. Equal pending nudges coalesce by session. Repo-watch records every
coalesced, capacity-dropped, or closed-source outcome with a closed cause code
instead of discarding it.

**Implemented behavior.** An expired lease without model-call evidence is
retired under the repository and session locks. The transaction rechecks call
evidence after taking the session and scheduler locks, then applies the ordinary
parent-only stop when the commissioned generation-one goal is still current and
appends an immutable lease-expiration record. A successor generation is never
stopped for its predecessor's lease; its expiration record carries no goal
command identity, recording that retirement occurred without a stop. The
existing deferred goal-termination authority creates or joins the latest-state
dispatch obligation before releasing every now-releasable singleton batch;
sibling actions retain a multi-action batch until its ordinary release predicate
holds. The obligation therefore survives capacity loss and becomes eligible for
normal current-state redispatch rather than leaving the pull request assigned to
a session that never started (INV-069). If model-call evidence wins the race,
expiry changes no lifecycle state.

**Implemented behavior.** Each obligation lineage carries a durable count of
consecutive dispatches that ended without meeting it. Any requeue increments the
count on the successor it owes, whatever ended the dispatch; the count records
which batch it already includes, so the second and later actions of one batch
add nothing, because one batch is one attempt however many of its sessions
terminate. That record is also what makes a release taken while a sibling is
still running survive that sibling's own termination. A dispatch that converges
owes no successor and leaves the lineage no count at all. Redispatching a
counted lineage waits out a delay that starts at ten minutes after the first
failed attempt and doubles per further consecutive failure to a one-hour
ceiling. Six consecutive failed attempts park the obligation in the transaction
that counts the exhausting attempt: it is excluded from dispatch, stamped with
the time it parked, and readable in the `repo_watch_parked_dispatch_obligation`
projection alongside its count, pull request, and the head it stalled on. That
stalled state is held still for as long as the obligation is parked: a collapsed
singleton advances its latest-event projection on any match, including one from
another pull request, and the release condition is decided against the state the
lineage stalled on rather than against whatever matched last. A lineage whose
pull request has already moved past that state parks all the same, and is then
released from that park at once: the parking stamp and its journal transition
are written, the same progress release that any event would take appends a
second transition, and the count comes back. The exhausting attempt may have run
while a new head or review activity arrived, reaching its evaluation before
there was a park to release, and nothing restates that fact afterwards, so it is
read from the durable record at parking rather than waited for. Going through
the park rather than refusing it is what records the fact as spent, so it buys
one budget and not another at every later exhaustion, and the pair is what an
operator reads in the journal. The delay is measured from the release of the
whole batch, not from the first of its actions to fail: a batch holds its
singleton until every action is terminal, so a clock started at the first
termination would run out while the batch still occupied the slot. The attempt
budget is a schema constant, so parking, the readiness projection, and the
dispatch loader cannot disagree about it; the two delay bounds are compiled into
the daemon and may only be lowered, never raised.

**Implemented behavior.** Two things return a parked obligation to dispatch. An
operator calls `repo_watch_release_parked_dispatch_obligation` with their
identity, which restores the whole budget: an operator asking for another
attempt is asking for the allowance a lineage that never failed would have.
Otherwise the pull request the obligation stalled on must produce a fact that is
new about it — an event carrying a head other than the one it stalled on, or
review activity against it. Whether the rule that parked the obligation also
matches that fact is beside the point, and every event is tested against every
park as it is evaluated: a rule watching one narrow signal would otherwise stay
parked on an obsolete head however far the pull request moved. Progress must
also follow the state the lineage stalled on, and must follow every fact about
that same pull request which the lineage has already spent, counted across the
successor obligations it opens as it settles and requeues: several facts can
follow one stalled state, parking spends the newest of them, and the older ones
stay unevaluated by any rule running behind its siblings. That ordering holds
only within one pull request, because event position numbers a single
repository's stream and a rule-scoped lineage spans repositories; a fact already
spent anywhere in the lineage is refused by identity regardless. A single
repository event is evaluated once per active rule, so without that ordering an
older event replayed by a lagging rule, or a newer one seen again after a second
park, would hand back a budget the pull request never earned. Rule, repository,
and stack singletons collapse many pull requests onto one obligation, so the
fact must name that same pull request: a neighbour's head differs from the
stalled one almost always, and would otherwise restore the budget on every
unrelated match. A branch target carries no head and no review activity at all,
so an obligation stalled on one is released only by an operator. Matching events
that are neither, such as a recomputed mergeable state or a label change, join
the obligation's latest-state projection without restoring anything, so churn
against an unchanged pull request buys no further attempts. Content identity
keeps a restated observation from recording a second durable event, but it does
not bound how often one durable event reaches this test: an event is tested once
per active rule, and both evaluation paths run the test before checking whether
that rule's evaluation of it was already recorded. The spent-event journal is
what makes those repeated tests safe, and is a required guard rather than an
optimization. Every park and every release appends a journal row naming the
count at the transition and, for a release, its operator or the event that
caused it; both releases are schema-owned, so the journal's vocabulary is
spelled only where the constraint closing it lives. Readiness in
`repo_watch_outstanding_dispatch_obligation` excludes a parked obligation and,
independently, one whose count has reached the budget, so no ordering of parking
against that read reports an exhausted obligation as ready. It does not model
the delay between attempts, which the dispatch loader applies against bounds no
projection can see.

**Implemented behavior.** A completed approval-judge escalation judged under
dispatch authority, which the rule above binds to the generation that dispatch
commissioned, is an execution-failure terminal transition rather than an
attended approval wait. An escalation in any other generation of the same
session resolves no such authority and stays the attended wait. It fails the
active turn and blocks the commissioned goal while that goal's authority still
stands — a generation stopped, achieved, or superseded during the provider
round-trip is not blocked, because the authority the block would record has
already ended — and therefore enters the same latest-state obligation and
singleton-release path described above. Once release commits, the replacement
obligation — where the requeue rules above still owe one, which a deactivated
rule or a later close or merge withdraws — becomes eligible under the ordinary
cooldown, the failed-attempt delay above, and the attempt budget that parks a
lineage which keeps failing, and can then create a fresh dispatch; the ended
session does not remain load-bearing occupancy either way. That requeue is a
counted attempt like any other, so a dispatched lineage whose work keeps
escalating parks on that budget rather than redispatching without end. Two turns
are not terminalized this way, and each parks for a user exactly as it would in
a session no dispatch created. A turn a steer still names is attended by
definition, so its escalation parks for the user who steered, leaving the turn
active and its batch held; terminalizing it would strand that steer against the
rule that no turn becomes terminal while pending steering names it, and
reclassifying the steer into a queued successor would start fresh work in a
session whose dispatch is being released for redispatch. A repository-watch
session that has already recorded an escalation parks too, and for a reason that
reads off the record rather than off the batch: [goal mode](goal-mode.md)
exempts its block from automatic resumption, so only a person can put that
session back into flight. A second escalation there is therefore work an
operator resumed, whether or not the batch has released — a sibling action still
pursuing keeps the release absent while the resumption is just as attended — and
it waits for them. Standing authority is the last word on it: a goal that ended
while this judge was in flight leaves stale work, so its escalation is
terminalized rather than parked for a user who will never come, and
terminalizing it owes no second redispatch that the requeue rules above would
not already withhold. A turn no escalation preceded is the dispatched work
itself, including one an ordinary execution failure had automatically resumed,
and takes the unattended path. The block the escalation writes claims no release
— a batch a sibling action still pursues releases only when that sibling ends —
and states that no automatic resumption is scheduled, which
[goal mode](goal-mode.md) admits as its one exception and which is why that need
text names the operator repair itself. Whether the batch released is likewise a
fact about the batch: it is recorded in the release row and the escalation audit
view, not reported as this completion's own effect, because a sibling settling
later would otherwise change the answer an identical replay receives.
`repo_watch_headless_approval_escalation_audit` joins the append-only escalation
cause and rationale to its dispatch release and replacement obligation,
including whether and when that obligation settled. The terminal attempt,
failure transcript entry, and terminal frontier it recorded are all durable
evidence of that transition, so a replayed completion offering any other one of
them is reported as a mismatched replay rather than answered with the recorded
outcome — the same treatment a differing recommendation, rationale, usage, or
continuation attempt already receives.

**Implemented behavior.** An operator-commissioned dispatch has an attending
operator and no independent redispatch path. With its recorded fence resolved, a
completed escalate-to-human in the commissioned generation therefore leaves the
active turn and exact request in `awaiting_tool_approval`. The completed judge
call, recommendation, rationale, and usage are the durable typed record of the
bounded automatic decision; no approval decision is invented. The operator can
approve or deny that request through the ordinary command surface without a
failed turn, a goal retry, or a second judge call.

**Implemented behavior.** A pull-request close or merge durably records one
lifecycle cutoff. When that lifecycle remains terminal, repository watch applies
the ordinary parent-only stop to each generation-one goal it commissioned for
the pull request; it cannot stop descendants, a later user-authored generation,
or an unrelated session. A later open event makes an earlier unprocessed cutoff
a recorded reopen instead. Dispatch admission rechecks the latest durable
lifecycle under the repository lock. A terminal cutoff settles every outstanding
obligation for that pull request immediately, without waiting for singleton or
cooldown readiness, and reaches a parked obligation through the target it
stalled on as well as through its latest-event projection, since a collapsed
singleton lets that projection follow another pull request; an obligation
stalled on the cutoff event itself is preserved whatever has matched since,
because it owes the close automation and an operator release is what lets it
run; the admission recheck is the race-closing backstop. Either path settles
stale nonterminal work as `target_closed` without creating a session. Cutoff
processing settles after it has stopped the goals it commissioned, so it takes
session rows before obligation rows and cannot close a lock cycle against a
dispatch session terminating into the same obligation, and it takes those
obligation rows in the order the progress-release scan takes them, which runs
outside the repository lock. A rule that matches the `PullRequestClosed` or
`PullRequestMerged` event itself remains dispatch-eligible, and a non-converged
termination of that dispatch still owes its requeue while its own cutoff remains
the latest one; the terminal event is the cutoff fact, not work made stale by
that fact. Corruption in one commissioned goal rolls back that goal's stop to a
savepoint but does not roll back the cutoff: the terminal event remains durably
dispositioned, healthy commissioned goals are stopped, and later cutoffs remain
eligible for processing.

**Implemented behavior.** Every completed poll atomically commits its cursor,
events, and durable convergence evidence for each pull request at the exact head
and base revision in that cursor. Evidence identical to that identity's latest
assessment is an idempotent replay; changed evidence appends a new assessment.
The assessment follows the repository's operational status rule: every review
thread must be resolved, without filtering by author or outdated state; at least
one gating check must exist and every gating check on the exact current commit
must be green; the pull request must be settled with known `mergeable`
mergeability; and the aggregate review decision must not be `changes_requested`.
The filtered gating-check inventory is settled only after the same inventory is
observed in two consecutive committed polls for the unchanged exact head, so a
fast check cannot permanently seal the head before a later workflow registers.
Check runs are green only when completed with `success`, `skipped`, or
`neutral`, and status contexts are green only at `success`. Pending, incomplete,
missing-conclusion, and other terminal results are not green. Check names
containing `report only`, `CodeRabbit`, `codecov/project`, or `codecov/patch`,
compared case-insensitively, are non-gating. The GraphQL check-rollup and
review-thread connections are read through every bounded page. The head, check,
and aggregate-review evidence is read before the thread inventory, matching the
operational reference's ordering so a review thread opened between those reads
cannot be hidden by an earlier thread snapshot. The branch-head snapshot is read
before pull-request hydration and supplies the assessed base revision committed
in the cursor. The rollup's commit, head, and base-branch evidence must agree
with the REST pull-request projection and cursor generation or the poll fails
without recording an assessment; independently sampled REST and GraphQL
mergeability values need not be equal while GitHub settles them.

**Implemented behavior.** A passing assessment for a pull request based on
`main` is `merge_ready`. A passing assessment based on another branch is
`internally_converged`, not merge-ready; both classifications end autonomous
work on that exact head. Every assessment is append-only evidence. An
append-only cursor-generation identity advances the current projection when an
A→B→A return reuses A's unchanged evidence, while an exact replay superseded by
a later generation cannot append evidence or advance that identity.
`repo_watch_current_pull_request_convergence` exposes the current identity's
evidence, derived verdict, and any exact-head seal. The first passing assessment
also creates one monotonic seal for the repository, pull request, exact head
SHA, and exact base revision. Later checks or reviews on the same sealed
identity remain visible as newer assessment evidence but cannot reopen dispatch,
so a session does not revisit threads it already resolved on that unchanged
identity. A different head SHA or base revision has no inherited seal and is
assessed and dispatched afresh; convergence therefore terminates unchanged-head
review cycles without treating a new revision as already finished.

**Implemented behavior.** Repository watch records one convergence cutoff only
when a seal's head and base revision are the latest assessed identity. Stale
seals remain pending and become eligible if their identity becomes current
again. Each transition that makes a sealed identity current records its own
cutoff application, so work commissioned while another identity was current is
also stopped when the sealed identity returns. The cutoff applies the ordinary
parent-only stop to every generation-one goal repository watch commissioned for
the pull request, with the same provenance limits as a lifecycle cutoff.
Dispatch admission rechecks the seal under the repository lock: a stale match or
collapsed obligation settles as `target_converged` only when its head is the
latest assessed identity and that identity's head and base revision are sealed.
An older identity cannot stop current work.

**Implemented behavior.** `CHANGES_REQUESTED` gates merging, never dispatching:
repository watch continues delivering matching findings while that aggregate
decision remains. It may dismiss a blocking review only when GitHub reports it
among the pull request's latest opinionated `CHANGES_REQUESTED` reviews and its
associated commit differs from the exact current head. The current convergence
evidence must otherwise pass: zero unresolved threads, at least one gating check
and zero non-green ones, a settled head, and nonconflicting mergeability. An
unsettled head has not finished registering and completing its exact-head
checks, so its empty non-green list is the absence of evidence rather than
evidence of a green head; requiring settlement keeps a dismissal from racing
checks that have yet to report. A head carrying no gating check at all presents
that same empty non-green list, which is why the reference convergence rule
counts it blocked and why clearance refuses it: the dismissed review would be
the only gate that head ever had. Both the in-memory candidate rule and the
durable eligibility query enforce that count, the settled head, and
mergeability, so neither admits an intent the other would refuse. The durable
query proves each term against the recorded assessment rather than against the
watcher that raised the candidate, because the assessment it reads is whichever
watcher recorded one last: a newer assessment appended for the unchanged cursor
while this watcher reconciled must carry the predicate itself, or the intent
would claim the review was the head's only blocker while the evidence it names
records another. Settlement is recorded only for a mergeability GitHub has
decided, so the query admits `mergeable` alone and refuses nothing the in-memory
rule admits. Every effective blocking review must target a superseded head; one
current-head blocker prevents every dismissal for that assessment. A
current-head review is never dismissed automatically. Why: a new review is live
judgment, while a stale aggregate decision whose complete thread inventory is
resolved is forge state that alone prevents an otherwise finished head from
converging. The following ordinary poll observes the dismissal and may then seal
convergence; dismissal itself does not stop dispatch.

**Implemented behavior.** Before sending GitHub's review-dismissal mutation, the
daemon appends a unique intent naming the assessment, repository, pull request,
exact current and reviewed heads, review node, reviewer, fixed reason kind, and
the exact human-readable dismissal message. That message identifies the review
node, reviewer, superseded head, exact current head, and the resolved threads
and green other gates that justify clearance. It then re-reads the pull request
and proves the whole predicate again against live evidence, including
settlement: the gating-check inventory this re-read observes must be the one the
committed poll that raised the candidate recorded for the same head and update
stamp. A check that appeared in between leaves the head unsettled and the review
undismissed until a later poll sees the inventory hold still. It appends the
provider's terminal result separately. Replaying equal evidence reuses the
intent rather than creating or sending concurrent duplicate work. After an
ambiguous process failure, a later poll observes the named review directly: an
already dismissed review completes the audit, a newer pull-request head
supersedes the intent, and a review decision cleared by another actor is
recorded as cleared elsewhere. A still-blocking intent is retried only when a
current poll again proves the full dismissal predicate. Recovery renews its
ownership token immediately before observing each intent, because a deeply
paginated batch can outlive the claim lease; an intent whose lease another
watcher has since taken is left to that watcher, and a lease lost between the
renewal and the terminal write likewise leaves that one intent to its new
claimant rather than abandoning the rest of the batch. The pending-intent
projection makes every unsettled external action directly observable. The next
poll observes the dismissal through the ordinary review and convergence
projections; no synthetic approval is created and no fresh review is requested.

**Implemented behavior.** Held singleton batches are directly observable in the
`repo_watch_held_dispatch_slot` projection. Each row identifies the repository,
the origin fact its dispatch was admitted from, rule, singleton key, sessions,
and held-since time; states each release clause independently; and names every
failing clause in `blockers`. That origin is a pull request or a workflow branch
and never both: a batch admitted from a branch workflow-run fact carries
`workflow_branch` and a null `pull_request_number`, and every other batch
carries `pull_request_number` and a null `workflow_branch`.

**Implemented behavior.** Outstanding obligations are directly observable in the
`repo_watch_outstanding_dispatch_obligation` projection. Each row identifies the
repository, rule, singleton and pull request or stack root, first and latest
matched events, collapsed count and timestamps, any occupying dispatch and its
sessions or external blocking session, cooldown eligibility, present readiness,
failed-attempt count, and parking stamp. Rule deactivation settles an obligation
without dispatch rather than leaving permanently owed work for semantics that
are no longer configured; terminal-target settlement likewise records why the
obligation no longer remains owed.

**Implemented behavior.** A newly configured rule activates immediately after
the repository's current durable event tail, before its task polls, and consumes
later events in cursor and event-ordinal order. Activation and each terminal
evaluation outcome are append-only. Restart resumes the oldest unevaluated fact
and the oldest eligible obligation for that rule version; it neither
redispatches an evaluated fact nor treats pre-activation history as a new live
signal. An obligation is a separate collapsed delivery identity, not a request
to reevaluate its occupied facts. Reconciliation records an append-only
deactivation when a configured identity or its repository disappears. Guarded
daemon startup admits the complete repository set in two phases, including the
empty set when the repository-watch section is absent; the absent section still
starts no watch runtime or polling task. It first validates the whole set in one
transaction it discards, in the Configuration phase before either local socket
binds, so every refusal is reported there against untouched history. It then
commits the deactivations and activations in one transaction after every
remaining fallible startup step succeeds. Before admission commits, startup
drains both pending lifecycle cutoffs and eligible convergence cutoffs before
any watch task starts. A refusal anywhere in the set, and any startup failure
before that commit, therefore leaves no deactivation and no activation behind: a
configuration that never started consumes no revision, and restoring the
previous configuration is admitted rather than refused as reuse. A lost commit
response is resolved by rereading the durable active set rather than assuming an
outcome. That reread commits nothing, so it cannot itself become ambiguous, and
it answers the only question the outcome turned on: either the active set
already equals the configured admission, so the commit won and startup proceeds,
or it does not, so the commit never landed, no revision was consumed, and
startup fails against untouched history with the previous configuration still
admissible. Startup does not attempt the admission a second time in that failing
case; the next start admits the same configuration from that untouched history,
and only an unreachable database defeats the reread. Configuration
reconciliation and evaluation are serialized per repository: an evaluation
already committed may replay, but an already-loaded event cannot create a
dispatch after deactivation commits. Activation stores a digest of the complete
versioned matcher, ordered action list, singleton scope, and cooldown, plus
content-free fingerprints labeled with the exact configuration fields they
represent. Changing any of those semantics while retaining the same rule ID and
revision fails in the Configuration phase before either local socket binds. The
diagnostic names the rule and changed field and directs the operator to
increment `version`; when multiple fields changed, it names the first changed
TOML field in canonical fingerprint order. It never first appears as a
repository-task runtime death. An activation recorded before field fingerprints
existed cannot produce them from its aggregate digest, so the one-time migration
introducing fingerprints retires every such activation. No active activation
lacks fingerprints and the daemon carries no path for that shape; a missing
fingerprint under any non-deactivated activation is storage corruption, checked
before reconciliation compares that activation against configuration, retires it
as an unconfigured rule, or retires it because its whole repository left
configuration. Retiring an activation retires its `(rule ID, revision)` pair, so
the first boot after that migration refuses every configured rule at its
recorded revision as identity reuse, including every rule whose semantics did
not change, and fails in the Configuration phase before either local socket
binds. The operator increments `version` once for each configured rule on that
first upgraded boot; no fingerprint backfill can stand in for the bump, because
the retained aggregate digest does not carry the per-field digests the new
revision records.

**Implemented behavior.** A higher revision under the same rule ID is a
replacement. Reconciliation appends deactivation of the active old revision and
activation of the configured new revision after the current event tail. The old
activation, deactivation, evaluations, dispatches, and sessions remain joined by
the same rule ID and their original revisions, while only later events are
eligible for the replacement. A deactivated `(rule ID, revision)` pair cannot be
configured again, and a revision below the highest revision ever recorded for
that rule ID in that repository is refused, so only a higher revision replaces
the active rule. Rule identity is per repository throughout: activation,
deactivation, fingerprints, and evaluation are keyed by repository, so the same
rule ID first configured in a newly watched repository starts its own lineage at
any revision instead of inheriting another repository's history. A fresh rule ID
remains an admitted replacement path, but a revision bump is the ordinary way to
preserve stable identity and history.

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
and durable events cannot inspect them. Until this upgrade is built, the first
complete poll after a daemon restart re-fetches every pull request; the warm
restart schedule above keeps that cost on the configured poll cadence.

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

Each hook admits at most 3,000 deliveries in any rolling 60-second window,
charged only once a delivery has proved the shared secret. Admissions are
counted in ten-second buckets and attributed to the bucket they land in, so a
burst is counted where it happened: a window that simply reset would admit a
full allowance on either side of its boundary, and smoothing an assumed even
distribution would still under-count a burst arriving at a window edge. One
bucket more than the window spans is kept and counted whole, so the bucket
straddling the edge is never dropped while part of it is still inside the
trailing minute. Both choices err toward refusing rather than admitting. Nothing
is charged before verification: a budget keyed on the hook a request claims is a
lever the attacker holds and GitHub does not, and spending it with forged
signatures would reject the deliveries it exists to protect. Unauthenticated
cost is bounded by resources instead. These are hard safety ceilings, not
configuration knobs. The listener does not grant GitHub-originated data
process-protocol authority, session authority, or polling credentials.

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
full poll. Every drain call also has a sixty-second outer deadline spanning its
provider and database work. Expiry cancels that attempt, leaves unfinished
deliveries pending, invalidates partial provider freshness, emits the closed
`webhook_projection_drain_timed_out` cause, and, unless only post-terminal
dispatch work expired, enters the same bounded projection backoff as another
retryable drain failure. Post-terminal dispatch expiry instead arms its fixed
dispatch follow-up. The serialized task is therefore returned to its scheduler
after bounded child cleanup even when an inner operation never returns.
Unfinished child fetches remain in the poller's shared set, which a later
attempt must drain before it can spawn new work. A deadline reached by the
pre-poll drain stops that poll before its provider sweep can advance the durable
cursor past the still-pending delivery. A targeted completion already started by
the cancelled drain retains its exact terminal request and cursor write. It
records the disposition and projections as the durable recovery handoff before
attempting the cursor write, and the next drain settles that completion and its
shadow outcome before subsequent drain work. Cancellation discards the shadow
only when it races a projected terminal write whose durability is unknown. The
task retains that delivery identity and blocks cursor-advancing polls until the
disposition is definitively observed or replayed. If an earlier delivery had
already failed before a later operation reached the deadline, the drain emits
that earlier closed cause at error level before reporting the timeout,
preserving the first-failure guarantee for an error-only telemetry sink. The
enclosing webhook attempt has a seventy-second deadline so activation, lifecycle
cutoffs, and dispatch reconciliation surrounding the drain cannot hold that task
indefinitely either. Its cancellation drains child provider fetches for at most
five seconds, invalidates partial freshness, emits the closed
`webhook_attempt_timed_out` cause, and enters the same retry backoff. A cleanup
that exceeds its own bound emits `webhook_cancelled_fetch_drain_timed_out`
instead of preventing that retry from being scheduled. A terminal commit whose
result is lost in transit is resolved by reading whether the row is already
terminal, which cannot itself be ambiguous: if it is, the delivery counts as
recorded and the shadow advances; if it is not, the record is re-attempted a
bounded number of times before the delivery is left pending for the next drain.
If every settling read is itself unavailable, the shadow is discarded rather
than trusted, because a disposition may have landed without being reflected in
that baseline. A durable disposition the shadow never accounted for is what this
avoids. A delivery whose target-specific processing fails is deferred for the
rest of that drain rather than failing it, so one persistently unprocessable
receipt cannot pin the head of the queue and starve every later one; the attempt
still reports the first such failure. Credential, transport, provider-throttle,
and provider-outage failures stop the current page instead: they prove later
targeted requests cannot make independent progress, so issuing one for every
loaded peer would amplify the same outage. Those receipts remain durably pending
for the bounded retry backoff. A signature-valid delivery whose event or action
is outside the mapped set, including a broadly subscribed `workflow_job`, is
still acknowledged successfully and is cheaply logged and recorded as ignored
rather than treated as an intake failure. A targeted projection records its
terminal disposition and exact projections as the durable recovery handoff
before its cursor write. If that cursor write conflicts with an intervening full
poll, the delivery remains terminal and the in-memory shadow is handed over to
the competing durable cursor before later pending receipts are projected. A
webhook-enabled shadow wake may also preempt the read-only provider sweep of an
in-flight complete poll, without resetting that poll's deadline, so the durable
delivery drains before bounded reconciliation resumes. After the delivery's
bounded page drains, the still-due poll returns through the same interruptible
scheduler path rather than entering an uninterruptible resumed sweep.

**Implemented behavior.** A drain page attempts every loaded delivery after a
target-specific or persistence failure, but stops at the first repository-wide
provider failure. Each failure is logged at warning level with the delivery
identity and a closed cause, and the drain itself emits an error-level record
carrying the first such cause, whichever attempt performed it — a startup drain,
a wake, a retry, or a full poll. A delivery that fails before its terminal
disposition is recorded remains pending, and its successful page peers still
reach terminal state when the failure is isolatable: a targeted refresh the
provider will not serve is one such failure, because that query runs before
anything is recorded. A targeted commit runs before the disposition is recorded,
so its failure leaves the delivery pending too. Once a targeted refresh's exact
projections and disposition are durable, a later cursor-write failure does not
reopen the delivery; the durable cursor becomes the next shadow baseline. A
delivery whose disposition is already durable when a later step fails — the
dispatch work that follows it — is terminal and is not loaded again; that
failure carries the same delivery identity and closed cause at warning level,
recorded where it happens because the delivery never reaches the drain page's
deferral record. The repository task schedules a new drain attempt after five
seconds without waiting for a full poll, another delivery, or a restart.
Consecutive failures double that delay to a five-minute ceiling and a success
returns it to five seconds, so a delivery that cannot be projected costs bounded
repeated work rather than a fixed five-second loop. Only the drain advances that
delay: an attempt whose drain succeeded and whose dispatch work then failed
returns it to five seconds and keeps a retry armed there, because that work runs
only from a later attempt and the delivery that would have woken one is already
terminal. That follow-up is distinct from projection backoff: admission wakes
remain enabled and full polls keep both drain steps while it is owed. A full
poll whose drains succeeded and whose trailing cutoff or dispatch work then
failed arms the same follow-up rather than waiting for the next poll, because
that work runs over what the drain committed and no delivery is left pending to
wake it. An attempt that failed before reaching the drain arms a retry at the
current delay without advancing it, so unrelated dispatch failures cannot grow
the projection delay. Taking a retry spends its deadline, so an attempt that
then fails before its drain arms a fresh one rather than selecting itself again
immediately, which the retry's priority over polling would otherwise cause. That
fresh deadline keeps the kind the spent one had: the attempt that failed before
its drain says nothing about projection, so a follow-up whose trailing work
failed again stays a follow-up rather than becoming backoff that suppresses
admission wakes and poll drains while projection is healthy. A deadline that
expired while some other attempt ran is left expired, so the next pass takes it
at once rather than having it pushed out again by a poll that keeps failing
slowly. A poll whose pre-poll drain failed does not repeat it: the step after
the poll is skipped, so work already known to be failing waits for that delay
rather than repeating inside one attempt. A pre-poll drain that succeeded is
still followed by the post-poll one, which is what catches deliveries admitted
while the poll was running. A full poll that fails before its drains have run
schedules that retry only when none is already owed, an admission wake is
suppressed while one is, and a full poll omits both of its drain steps while one
is, so neither a rapidly failing poll nor an authenticated replay stream can
defer or bypass the backoff; a suppressed wake coalesces and is observed by the
attempt that follows the retry. A poll taken during a backoff window therefore
commits a cursor that a delivery still pending does not reflect. The shadow
baseline that delivery seeded is marked superseded but retained, because
replacement waits for an empty pending page, so its retry projects against that
baseline rather than reseeding from the advanced cursor — the accepted cost is
that divergence, taken against an unbounded repetition of work already failing.
An overdue retry is taken ahead of an overdue poll, and a full poll that
outlasts its own interval schedules the next one a whole interval from
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
targeted provider queries complete before anything is recorded, so a transient
provider failure leaves the delivery pending. Once those queries succeed, the
exact projections and terminal disposition form the durable recovery handoff
before the cursor write. A later cursor failure does not reopen the delivery;
the in-memory shadow is discarded so subsequent work reloads the durable cursor
baseline, while a cursor conflict hands ownership to the intervening poll. On
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
Terminal exact payload bytes remain for seven days; after each successful full
poll, at most once per day and starting with the first poll after boot, the
daemon deletes only the expired payload bytes. Delivery tombstones, digests,
projections, and dispositions remain append-only.

**Implemented behavior.** Projection coverage is closed by delivery family and
action:

| GitHub delivery                                                                          | Shadow projection                                                                                                                                                                                        |
| ---------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `pull_request: opened`, `reopened`                                                       | Guarded complete context plus pull-request hydration; may derive `PullRequestOpened`                                                                                                                     |
| `pull_request: closed`                                                                   | Guarded lifecycle context; may derive `PullRequestClosed` or `PullRequestMerged`                                                                                                                         |
| `pull_request: synchronize`                                                              | Guarded context plus mergeability and check-rollup queries; may derive `HeadChanged` and poll-derived rollup facts                                                                                       |
| `pull_request: labeled`, `unlabeled`, `edited`, `converted_to_draft`, `ready_for_review` | Guarded complete context; label actions may derive `Labeled` or `Unlabeled`                                                                                                                              |
| `issue_comment: created`, `edited`, `deleted` on a pull request                          | Pull-request hydration through the polling normalizer; may derive only facts polling observes, including reaction changes                                                                                |
| `pull_request_review: submitted`                                                         | Guarded provider-review union; may derive `ReviewSubmitted`                                                                                                                                              |
| `pull_request_review: dismissed`                                                         | Mapped no-change because version one has no dismissal fact                                                                                                                                               |
| `pull_request_review_thread: resolved`, `unresolved`                                     | Guarded thread state; may derive `ThreadResolved` or `ThreadOpened`                                                                                                                                      |
| `check_run: completed`                                                                   | An unambiguous watched pull request gets a guarded provider-run union and check-rollup query; otherwise only a head-SHA check-rollup query; may derive `CheckRunCompleted` and poll-derived rollup facts |
| `check_suite: completed`                                                                 | Head-SHA check-rollup query; only its poll-normalized result may derive `ChecksCompleted`                                                                                                                |
| `workflow_run: completed` for the watched repository                                     | Guarded workflow/run/attempt state when the run's head branch is still in the observed branch set; may derive `BranchWorkflowRunCompleted`                                                               |
| `push` on `refs/heads/*`                                                                 | Guarded branch create, advance, or delete; an advance may derive `BaseAdvanced` for affected open pull requests                                                                                          |
| `ping`                                                                                   | Mapped endpoint-health no-change                                                                                                                                                                         |

Ordinary issue comments, other actions in those families, tag pushes, the
separate `create` and `delete` event families, foreign-repository workflow
heads, completed workflow runs whose payload head repository or head branch is
absent, workflow runs whose head branch is absent from the observed branch set,
and every other signature-valid event are ignored successfully. That last one
holds the two producers to the same fact set: polling admits a workflow run only
for a branch it currently observes, so a run on a deleted branch is a fact
polling can never produce, and projecting it would leave a webhook-only parity
row nothing can ever match and, under a later write mode, a dispatch target that
no longer exists. It is ignored rather than turned into a targeted query for the
same reason — there is nothing to reconcile toward, so the delivery is terminal
`ignored` and records no projection. The branch set that decides this is the one
every earlier delivery has already been applied to, and deliveries drain in
receipt order, so a branch the stream itself announced carries into every later
run on it; a branch the stream never announced, because it was created outside
the mapped set or before intake began, leaves its runs ignored until the next
complete poll, and the poll-only row that follows is an accurate report that the
webhook stream could not have projected them. The workflow-run generation guard
is unchanged for a branch that is still present. Guards otherwise make stale
head, lifecycle, branch, workflow-attempt, and immutable-provider facts
superseded or duplicate rather than allowing regression. A rerequested check run
replaces the retained completion only when its provider completion generation is
no older, so a delayed original completion is superseded instead of regressing
the baseline; an equal generation still replaces, which is how a conclusion edit
arrives. A delivered run adopts the workflow name retained state already carries
for that workflow identity. The occurrence identity deliberately excludes that
mutable display name, so this is not what keeps the two sources matching; it
keeps the shadow observation equal to the one polling stores, so a rename cannot
make an otherwise duplicate delivery look like a changed fact. A completed check
run rerequested under the same provider identity carries a new completion
generation or conclusion, which the differ treats as a new observable
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
ordinary poll producer and dispatch path. Whole-pull-request hydrations coalesce
per pull request across one drained page of pending deliveries: the whole page
is durably admitted before it is read, so one hydration already observes every
delivery on it, and repeating the hydration would only re-read the same state at
the shared credential's expense. Anyone who may comment on a watched pull
request would otherwise pace that hydration — detail, check suites, check runs,
reviews, threads, and one request per comment for its reactions — with repeated
comment deliveries. Coalescing is scoped to the page and never to a whole drain,
because a later page may carry deliveries admitted after the earlier hydration
ran. Head-guarded mergeability and check-rollup queries name a specific commit
and do not coalesce against a hydration. Only a hydration that reached the
provider suppresses a later one. A refresh that fails before its fetch or before
its commit leaves its delivery pending rather than terminal, so the page's
remaining deliveries reissue it; and a hydration requested beside a head-guarded
query is never recorded, because the merged request carries that guard and a
superseded head discards the fetched state while the query still reports
success. A refresh whose cursor commit loses its generation race is likewise
never recorded: its delivery stays terminal, because its disposition and exact
projections are already durable, but the fetch never became cursor state, so the
page's remaining deliveries still owe that hydration. The same lost race clears
the fetch's process-local freshness, which no later generation may then vouch
for. A delivery whose hydration the page already issued records no
targeted-query projection of its own, on the same rule that only a query the
poller actually made is recorded. Coalescing therefore bounds bursts and not
pacing: a delivery admitted after a hydration reports state that hydration could
not have observed, so it is refreshed however slowly such deliveries arrive.
Bounding a paced stream would require a minimum interval between a pull
request's refreshes, trading both freshness and the fidelity of the parity
measurement shadow mode exists to produce; that trade is not taken while poll
frequency is unchanged and the complete sweep remains authoritative. Full
polling continues unchanged as the slow complete reconciliation sweep and
remains authoritative for missed deliveries, reactions, and every provider fact
outside the mapped set. Poll frequency does not drop in shadow mode; any later
write mode or slower cadence requires a separately reviewed ruling after parity
over a real workday.

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

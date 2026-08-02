# Repository watch and event dispatch

**Foundation contract.** Repository watch is a credentialed external-ingress
boundary from its first operation: the daemon holds a distinct credential-file
reference for each configured repository, reads secret bytes only for that
repository's request, and never gives a dispatched session the watch credential.
Per-repository tokens carry the least GitHub scope needed to read the configured
signals. This is the C0 confused-deputy boundary: a credential for one
repository cannot authorize a request to another repository. A repository
without a credential file is invalid configuration and is not watched; a
repository absent from the list is not watched; an absent repository-watch
section means that the subsystem does not start. Dispatched sessions retain the
approval posture of their named session templates, without authority inherited
from the watcher.

This bottom specification diff owns the four-pull-request repository-watch
stack. The version-one domain vocabulary and validation shapes were verified
against PR #430 (`agent/repo-watch-spec`). Persistence, polling, and rule
dispatch become implemented only in the child pull requests named by their
verification references.

## Configuration and credential boundary

**Foundation contract.** Repository watch has one optional, versioned TOML
section. It contains a list of repositories, a list of signal-reviewer logins,
and versioned structured rules. Each repository entry names exactly one
`namespace/name` repository, a positive polling interval, and its own credential
file. Duplicate repositories, unreadable credential files, unknown keys,
unsupported versions, zero intervals, malformed values, and invalid rules fail
configuration before any watch task starts. Other daemon GitHub credentials do
not substitute for a missing repository-watch credential. Both repository-slug
segments are nonempty ASCII letters, digits, dots, hyphens, or underscores;
neither segment is `.` or `..`.

**Foundation contract.** Credential files follow the house credential-file
pattern: configuration stores paths rather than secrets; request preparation
reopens the selected file and reads a bounded value; errors and telemetry name
only the reference, never the secret; and rotation affects later requests
without restart. No credential value is persisted in a cursor, event, dispatch
record, session parameter, error, or log.

## Poll transport and differ

**Foundation contract.** Version one uses conditional-request polling with one
independent task per configured repository and the repository's configured
interval. The durable cursor retains the last accepted ETag and the complete
normalized repository state needed for comparison. A request sends
`If-None-Match` when an ETag exists. `304 Not Modified` advances no state and
emits no event; GitHub does not count a conditional `304` against the primary
rate limit. A successful changed response is normalized and committed with its
new cursor and derived events atomically. A failed, rejected, partial, or
unparseable response changes neither cursor nor event history. The next request
occurs after the per-repository interval; version one has no webhook fallback
and no speculative second polling transport.

**Foundation contract.** Polling fetches repository state, not rule inputs. A
pure differ compares consecutive normalized per-pull-request state and the
configured branch-workflow state, producing only the closed version-one event
vocabulary below. Rules never inspect GitHub API objects or normalized snapshots
and never rerun the differ. Why: transport independence requires both polling
and a later authenticated webhook receiver to feed the same durable facts.

## Durable event vocabulary

**Implemented behavior and foundation contract.** Each event is an immutable,
version-one fact with its own UUID identity, repository, tagged pull-request or
branch target, and closed payload. Pull-request targets carry the positive PR
number, current 40-hex head SHA, head repository, base and head branches, title,
body, complete label set, draft state, and the author when GitHub supplies one.
This is normalized event context, not a raw API object. The only branch-target
event in version one is `BranchWorkflowRunCompleted`; its payload supplies the
branch, workflow, and conclusion. Events append in accepted observation order
and are never updated or deleted. Construction rejects a pull-request event when
the payload's current head, base branch, or label transition contradicts that
event's complete current pull-request context. A `HeadChanged` payload whose
previous and current SHAs are equal is invalid. Label names admit up to 50
Unicode scalar values, including their full UTF-8 representation.

**Implemented behavior and foundation contract.** The closed version-one event
payloads are:

- `PullRequestOpened`
- `PullRequestClosed`
- `PullRequestMerged`
- `HeadChanged { previous, current }`
- `MergeableStateChanged { current }`, where current is `mergeable`,
  `conflicting`, or `unknown`
- `ChecksCompleted { outcome }`, where outcome is `success` or `failure`
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

**Foundation contract.** Reaction ingestion includes only reactions by a login
in the configured signal-reviewer list. Reactions from every other actor are
excluded while normalizing state, so they cannot create durable
`ReactionChanged` events. Why: reviewer signals are actionable facts; the full
ambient emoji stream is neither a rule input nor retained noise.

**Foundation contract.** A first observation emits `PullRequestOpened` for each
open pull request and establishes its comparison baseline. Later observations
emit a fact exactly when its represented state changes. Closing by merge emits
`PullRequestMerged`, not both merged and closed. Check-suite completion emits
the aggregate success/failure event, completed individual runs emit their named
conclusion events, and branch workflow completions are compared outside PR
state. A base branch head change emits `BaseAdvanced { branch }` for each open
PR based on that branch. Repeated identical observations emit nothing.

## Structured rules

**Implemented behavior and foundation contract.** Rules are versioned TOML
structs, not a string DSL. Fields within one rule are conjunctive and distinct
rules are disjunctive. Omitting every target field means everything; requiring
labels or supplying regex fields narrows only that rule. There is no global
targeting switch.

**Implemented behavior and foundation contract.** Version one has exactly these
matcher fields:

- event kinds, matched any-of without encoding payloads into kind names;
- repository and base branch exact matches;
- anchored head-branch, title, and body regular expressions;
- labels `any_of`, `all_of`, and `none_of`;
- exact draft and author values;
- mergeable-state `any_of`, applicable to `MergeableStateChanged`; and
- conclusion `any_of`, applicable to `ChecksCompleted`, `CheckRunCompleted`, and
  `BranchWorkflowRunCompleted`.

**Implemented behavior and foundation contract.** The last two fields are the
ratified payload qualifiers. They do not split event kinds by payload. For
`ChecksCompleted`, `success` and `failure` map to the same conclusion values
used by the qualifier. A supplied payload qualifier is false for an event kind
to which it does not apply. Expressiveness grows only by adding versioned
fields.

**Implemented behavior and foundation contract.** A branch event cannot satisfy
pull-request-only base, head, title, body, label, draft, author, or
mergeable-state fields. Repository, event-kind, and conclusion fields can apply
to either context shape where their payload exists. An exact-author field is
false when GitHub supplies no current pull-request author. Rule validation
derives accepted context shapes from the event kinds that can satisfy all
supplied fields, rather than from the event-kind list alone.

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

**Implemented behavior and foundation contract.** Each rule carries a nonempty
ordered list of tagged action variants. Version one ships exactly one configured
variant, `dispatch_session { template }`. When a fact matches, that
configuration produces the emitted action
`dispatch_session { template, params }`, where `params` is the exact injected
tagged context for that event. No unused action variant is reserved.

**Implemented behavior and foundation contract.** Dispatch context is the
ratified tagged union:

- `PullRequestContext { repo, number, head_sha, event }`; or
- `BranchContext { repo, branch, workflow, conclusion, event }`.

The embedded event is the complete triggering durable fact, not reconstructed
API state. A pull-request event always produces the first shape and
`BranchWorkflowRunCompleted` always produces the second.

**Implemented behavior and foundation contract.** Each session-template entry
declares a nonempty set containing pull-request context, branch context, or
both. Rule configuration is invalid when any event kind the rule can match
produces a shape rejected by any action's template. An empty event-kind list
without narrowing fields can match both shapes and therefore requires both. An
unknown template or missing context declaration is also a validation failure.
Validation completes before polling starts; dispatch never discovers a shape
mismatch at runtime.

## Deduplication, concurrency, and audit

**Foundation contract.** Every rule independently selects `singleton_per` from
`pull_request` (the default), `stack`, `rule`, or `repo`, plus a nonnegative
cooldown. Pull-request scope keys by repository and PR number. Stack scope keys
by repository and the base-branch chaining component containing the PR: an open
PR whose head branch is another open PR's base branch is its parent in the
component. Rule scope keys only by rule identity and version; repository scope
adds repository. Branch events cannot satisfy pull-request or stack scope and
make such a rule invalid rather than silently changing its key.

**Foundation contract.** Dispatch admission and its audit record are one durable
transaction. The record links the triggering event, rule identity and version,
singleton key, action ordinal, session-template provenance, and newly created
session. An occupied singleton refuses another dispatch. A terminal dispatched
session releases the singleton, while cooldown suppresses a successor until its
recorded interval has elapsed. Equal recovery cannot create a second session for
the same admitted action. The append-only dispatch record identifies the session
responsible for the PR; no mutable assignment flag replaces it.

## First live rule

**Foundation contract.** The first deployed rule is `merge-forward-on-conflict`.
It matches `MergeableStateChanged` with
`mergeable_state.any_of = ["conflicting"]`, uses pull-request singleton scope,
and dispatches the merge-forward session template configured with the approved
cheap model and pull-request context. The rule does not match transitions back
to `mergeable` or `unknown`.

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

# Repository watch and event dispatch

Repository watch turns GitHub activity on configured repositories into durable
events, matches them against configured rules, and dispatches sessions to act on
them.

## Overview

Repository watch is a credentialed external-ingress boundary. Each configured
repository has its own credential-file reference, read only for that
repository's request and never given to a dispatched session. Only configured
repositories are watched, and without a repository-watch section the subsystem
does not start. The configuration section (`RepositoryWatchConfiguration`) and
the example TOML own the shape of what an operator writes.

Two transports feed one fact store. Polling sends conditional requests from one
independent task per configured repository at that repository's interval; the
conditional-request cache starts empty on every daemon start, so the first poll
after a restart is one complete unconditional fetch except for resources a
preceding startup drain's targeted refresh warmed. A webhook listener accepts
only `POST` on its configured path as plain HTTP, and a repository that enables
it runs in shadow or primary mode. Shadow projects a delivery against an
in-memory baseline and records parity rows only; primary applies it to the
durable cursor and writes webhook-produced events.

Each repository has one versioned durable cursor (`RepoWatchCursor`). It retains
the normalized repository state the next comparison needs, the exact
signal-reviewer set, and the last positive occurrence sequence of every
recurring event stream. A merged pull request leaves the ordinary observation
and keeps only a compact baseline of the members the differ needs to recognize
post-merge changes. A per-repository atomic commit takes an expected generation,
one complete cursor candidate, and its ordered batch of event occurrences.

A pure differ (`derive_repo_watch_events`) compares consecutive canonical
per-pull-request state, branch heads, and completed branch-workflow identities,
and produces only the closed event vocabulary in deterministic order. Each event
(`RepoWatchEvent`) is an immutable fact with its own UUID, its repository, a
tagged pull-request or branch target, and a closed payload; the vocabulary has
fifteen kinds, and `BranchWorkflowRunCompleted` is the only one with a branch
target. Every occurrence also carries a content identity shared across
producers. Stream identity is closed by event kind: a recurring kind names the
pull request plus its kind-specific label, thread, branch, or reaction member;
an immutable check-suite fact names provider identity and completion generation;
a review names its provider review identity; a workflow fact names branch,
workflow, run, and attempt.

Rules are versioned structures (`RepoWatchMatcherV1`). Fields within one rule
are conjunctive and distinct rules are disjunctive. Omitting every target field
matches everything, narrowing applies only to the rule that supplies it, and
there is no global targeting switch. A supplied payload qualifier is false for
an event kind it does not apply to. A branch event cannot satisfy
pull-request-only fields, while repository, event-kind, and conclusion fields
apply to either context shape. Each rule carries a nonempty ordered list of
tagged actions and independently selects a singleton scope from pull request
(the default), stack, rule, or repository, plus a nonnegative cooldown.

Dispatch context (`DispatchSessionParameters`) is a tagged union of pull-request
context and branch context, each embedding the triggering event. A fresh
dispatch delivers its originating event and carries no delivery member; a
dispatch that settles an occupied-refusal obligation delivers the target's
collapsed current state and adds a delivery member naming the obligation, the
count of matches it collapsed, the boundary event identities, and that state.
One event and rule match admits the rule's complete ordered action list as one
singleton batch. Each dispatch record links the triggering event, rule identity
and version, singleton key, action ordinal, session-template provenance, and the
new session. A durable delivery intent records the reserved submit-command,
accepted-input, turn, and cancellation candidates beside the applied link, so
equal recovery reuses the committed batch. An obligation records exactly one
blocker: the occupying repository-watch dispatch or an external commissioned
session. Every park and release of an obligation appends a journal row naming
the count at the transition and, for a release, its operator or the causing
event, under a schema-owned vocabulary. Readiness in
`repo_watch_outstanding_dispatch_obligation` excludes a parked obligation and,
independently, one whose count has reached the budget.

A dispatch is also an authority source. A pull-request dispatch fixes
repository, number, exact head commit, head repository and branch, and base
branch; a branch dispatch fixes repository and branch. An operator-commissioned
dispatch (`commissioned_dispatch`) supplies the same immutable authority for a
session no rule dispatched. When an approval judge escalates under dispatch
authority, the block it writes claims no release and states that no automatic
resumption is scheduled, one of the two exceptions [goal mode](goal-mode.md)
admits.

Every completed poll also records convergence evidence for each pull request at
the exact head and base revision. A passing assessment for a pull request based
on `main` is `merge_ready` and one based on another branch is
`internally_converged`; both end autonomous work on that exact head.

Rule activation stores a digest of the complete versioned matcher, ordered
actions, singleton scope, and cooldown, plus content-free per-field fingerprints
labeled with the fields they represent, so reconciliation can tell a changed
rule from a re-read one.

Webhook parity is measured in the database. `repo_watch_webhook_projection`
records each derived content identity, event kind, and known divergence cause;
`repo_watch_webhook_disposition` records one of six closed terminal
dispositions, and a primary delivery records `committed` in place of
`projected`. `repo_watch_webhook_parity` joins projections to poll-produced rows
between a repository's first shadow receipt and its promotion and reports
`matched`, `webhook_only`, `poll_only`, or `not_directly_mapped`. Projection
coverage is closed by delivery family and action, mapping each admitted GitHub
delivery to the guarded state it applies and the events it may derive.

Repository-watch operations are readable through a typed read-only application
port (`repo_watch_operations`) backed by the durable cursor, event, evaluation,
dispatch, obligation, webhook, and commissioned records.

The convergence sweep is a periodic pass that runs beside repository watch.
Repository watch owns event-driven dispatch; the sweep supplies liveness for
watched pull requests whose provider events stopped arriving. It owns its
convergence predicate, its fenced commission, its durable retry and park
records, and its configuration throttle. It is opt-in twice:
`[repository_watch.convergence_sweep]` supplies one review-response session
template and the timing policy, and each repository lists its
`convergence_pull_requests`. A census snapshot is converged exactly when no
review thread is unresolved, the status rollup belongs to the current head,
every gating check is green, and mergeability is `mergeable`.

## Design decisions

Dispatched sessions keep the approval posture of their named session templates
and inherit no authority from the watcher. Startup validates the credential
reference's shape without reading the file, so an unreadable watch credential
cannot block daemon startup; its request fails closed instead. The daemon binds
the configured listener address and verifies requests; tunnels, exposure
providers, and public reachability belong to deployment. A production task sends
only to the fixed `https://api.github.com` origin. The client requires TLS 1.2
or newer and disables redirects, environment proxies, and automatic retries, so
a credential-bearing request cannot be redirected or replayed. The listener
grants GitHub-originated data no process-protocol authority, session authority,
or polling credentials, and its ceilings are hard safety limits, not
configuration values.

Cache admission is an accelerator, never a precondition: a resource that does
not fit is shed, the poll continues, and that resource is refetched
unconditionally next poll. The per-repository interval is measured start to
start, so a cadence does not drift by the duration of its own attempt, and
attempts never overlap; an attempt that reaches or exceeds the interval starts
the next poll one interval after it completes, because an already-elapsed
deadline would never sleep and the task's other arms would never run. A webhook
wake serializes with the repository task but may preempt the read-only provider
sweep of an in-flight complete poll; rule activation, dispatch, webhook
projection, and cursor commit stay outside that cancellation region. An attempt
reuses a committed detail and settled check baseline for an open pull request
only when the recorded fetch reached the durable cursor and observed every check
terminal and a known mergeable state, because an uncommitted fetch proves
nothing about a baseline it never replaced and neither a check completion nor a
mergeability calculation moves `updated_at`. Reviews, threads, and reactions are
re-fetched every attempt and replace their prior projections before comparison,
because a reaction does not move `updated_at` and a delayed detail refresh must
not defer a review dispatch signal; with no configured signal reviewer the
poller issues no reaction request. A restart schedules the next complete poll at
what remains of the configured cadence, measured from the durable record of the
last completed sweep written inside that sweep's own commit and never from the
process start, so frequent restarts neither multiply provider requests nor
postpone the sweep; startup still drains durable webhook work first. Every
completed provider identity the check projection returns enters the comparison
baseline, so the provider's latest-attempt default cannot silently discard a
completion between polls. When the newest run for a branch and workflow is
queued or in progress, the branch projection keeps the later of the newest
completed candidate and the prior completed baseline. A run enters the branch
projection only when its head-repository identity equals the watched repository,
so a same-named fork branch never enters it.

GitHub can return a null head repository after a fork is deleted; the poller
retains the prior canonical identity for that pull request, and a new pull
request with none fails closed. When GitHub returns a null author for a
historical review, normalization reuses the prior reviewer only for the same
provider review identity, and a new identity-less review is omitted. When a
current reaction lacks actor identity, normalization carries prior retained
reactions for that subject forward only while their reactors remain in the
signal-reviewer set, so identity loss cannot manufacture removals.

A merged pull request's baseline is compacted so a merge burst cannot make every
later webhook refresh re-transfer full terminal detail, while post-merge label,
check, review, thread, and reaction changes still produce events. A compact
baseline remains while the merged pull request's recurring streams remain in the
frontier, because evicting it alone would make a later refresh look like an
initial observation. No lifecycle releases a stream: a release is valid only for
a subject that provably produces no further occurrence, and a merged pull
request is not one. Exceeding the stream ceiling fails the comparison, and
sequence exhaustion fails rather than wrapping, because reuse would mint a
content identity colliding with a durable one. The frontier is never replaced
with an empty one, because every stream would restart at sequence one and mint
identities a commit coalesces, silently losing those events and their
dispatches. The frontier records the pull request owning each recurring stream
although nothing reads it yet, because a stream identity is a one-way hash no
later migration can invert.

The content identity is a domain-separated SHA-256 digest over the repository,
event version, canonical target, identifying payload members, a separately
domain-separated stream identity, and the stream's positive occurrence sequence.
Exactly two payload members are excluded from the digest: the random event
identity, because a re-derivation mints a fresh candidate, and the workflow
display name, because a provider can rename a workflow under every identifying
member; both stay in the payload rules read. A later equal fact on a recurring
stream advances its sequence and so has a different content identity, while
equal facts derived from an equal frontier have the same identity even when
their candidate UUIDs differ. A fact is immutable only when the differ
suppresses re-emission on members its stream key already names; completed check
runs are recurring, naming run identity and completion generation, so only an
advancing sequence keeps a restored earlier conclusion distinct from the first.
A completed check suite's completion generation is the provider's `updated_at`
and a completed run's is `completed_at`, because the provider defines only those
fields. A rerequested suite emits its later completion even with unchanged
identity and conclusion, and a rerequested run emits on a new identity,
completion time, or conclusion. Workflows sharing a display name stay distinct,
renaming cannot re-emit an observed run attempt, and a new attempt under an
unchanged run identity does emit.

An unchanged candidate with no events does not advance the cursor, and an
unchanged candidate carrying events is rejected. Replay detection compares
against the batch the replayed generation would have stored, so a coalesced
commit is still recognized as its own replay. Each compact baseline records the
signal-reviewer filter that produced its reactions, and the cursor binds its
reaction projection to the exact signal-reviewer set; a changed set replaces
only the reaction baseline without emitting `ReactionChanged`, because comparing
projections formed under different filters would manufacture transitions. A
first observation emits `PullRequestOpened` and the current
`MergeableStateChanged` fact for each open pull request, then establishes its
baseline, so an already-conflicting pull request reaches the first rule at once.
Closing by merge emits `PullRequestMerged`, not both merged and closed. A base
branch head change emits `BaseAdvanced` for each open pull request based on that
branch. `ReviewState` admits only approved, changes requested, and commented;
the differ emits `ReviewSubmitted` only for a newly submitted review, and a
later GitHub dismissal emits no event.

Rules are versioned TOML structures, not a string DSL, and expressiveness grows
only by adding versioned fields. Changing a rule revision does not select a
different matcher grammar; the revision distinguishes successive semantics under
one stable rule identity. Patterns compile with a linear-time regex engine, so
backreferences and look-around are not admitted and no backtracking engine is
present. Rule validation derives the required context shapes from the event
kinds that can satisfy every supplied field, including the payload qualifiers,
not from the kind list alone, and rejects any attainable event shape an action's
declared template does not accept; configuration completes that validation
before polling starts, so dispatch cannot discover a shape mismatch at runtime.
Branch events cannot satisfy pull-request or stack scope and make such a rule
invalid rather than silently changing its key. Stack scope keys by repository
and the base-branch chaining component: a parent's head repository must equal
the child's base repository and its head branch the child's base branch, and the
component identity is its lowest-numbered root, or its lowest-numbered member
for a rootless cycle. Exactly one action variant ships, `dispatch_session` with
a template, and no unused variant is reserved. When a fact matches, every
configured action produces one dispatch action in list order whose parameters
are the exact tagged context for that event; a match that joined an outstanding
obligation eventually emits one action using the latest joined event plus the
delivery member, never one action per joined event. The embedded event is the
complete triggering durable fact, not reconstructed API state, and the matched
count and boundary identities summarize collapse without replaying intermediate
facts into the session.

The goal statement is synthesized from the dispatching rule, the resolved
template, and the typed parameters, and states only the rule, the template, and,
in its repository, the pull request with its head and base branches or the
branch with its workflow and conclusion. A head branch is qualified by the fork
holding it, so a consumer cannot misread it as the watched repository's. Every
repository-supplied identifier the statement renders is quoted, with the
backslash, the quote, and every line terminator escaped, so an identifier cannot
forge its closing delimiter or leave its line and two distinct identifiers never
render alike. The statement is composed by the dispatch rather than declared by
the session, because only an already-attached goal admits a model declaration.
Commissioning records the tagged-context turn as the generation's own first goal
turn rather than scheduling one, so a dispatched session commits exactly one
queued turn and runs its template once; that turn's input is the tagged context
the dispatch submitted, not a restatement of the statement. Every later turn of
that generation is an ordinary goal continuation, and the generation is readable
from the dispatched turn, so a supersession cannot broaden the authority read
for it. The approval judge resolves either append-only authority source under
the same generation-one binding, renders both through one rendering, and refuses
a session recording both as corruption. The commission's durable command
identity binds template, fence, statement, and initial content digest, so an
equal retry replays and a different intent under that identity is refused, under
the command protocol in [identity and commands](identity-and-commands.md). The
append-only dispatch records identify the sessions responsible for a pull
request; no mutable assignment flag replaces them.

Further matching facts join an obligation's latest-event projection and
increment its count, including a match racing release, so one singleton has at
most one outstanding obligation. A blocked or user-stopped dispatch session, and
an achieved session whose delivered state is no longer the pull request's latest
durable head, opens a latest-state obligation before release. Sibling
terminations and matching events collapse into it without regressing its latest
event. Achievement is terminal exactly when the delivered state is known and
still the latest durable head, so the successor carrying the newest head seals;
a batch admitted before its delivered state was recorded has none and cannot
seal, because reading its originating event would seal without delivery whenever
a head returns. Termination takes the singleton advisory key admission takes and
locks the rule activation row a deactivation shares, but not the repository key,
because lifecycle-cutoff processing takes that key before waiting on the same
session row and the reverse order would deadlock. An obligation becomes eligible
only after release and the same cooldown that would suppress a fresh successor,
cooldown suppression without an existing obligation creates none, and
eligibility settles the obligation and creates its one current-state batch
atomically. A session whose current goal is pursuing stays nonterminal for
singleton ownership across the gap between a completed goal turn and its durably
queued continuation.

Every dispatched action admits an immutable start lease in the same transaction
as its session and initial turn; its five-minute ceiling is enforced by the
stored constraint and by code that may lower but never raise it, and is not
deployment configuration. Retiring an expired lease applies the ordinary
parent-only stop only when the commissioned generation-one goal is still current
and appends an immutable lease-expiration record; a successor generation is
never stopped for its predecessor's lease, and its record carries no goal
command identity. Deferred goal termination creates or joins the latest-state
obligation before releasing every releasable batch, while sibling actions hold a
multi-action batch to its ordinary predicate, so the obligation survives
capacity loss and is redispatched rather than leaving the pull request assigned
to a session that never started.

Each obligation lineage carries a durable count of consecutive dispatches that
ended without meeting it. Any requeue increments the count on the successor,
whatever ended the dispatch; the count records which batch it already includes,
so siblings add nothing; and a dispatch that converges leaves no count.
Redispatching a counted lineage waits a delay that starts at ten minutes and
doubles per further failure to a one-hour ceiling. Six consecutive failures park
the obligation in the transaction counting the exhausting attempt; a parked
obligation is excluded from dispatch, stamped, and readable with its count, pull
request, and stalled head. The attempt budget is a schema constant so parking,
the readiness projection, and the dispatch loader cannot disagree, and the two
delay bounds are compiled in and may only be lowered. An operator release
through `repo_watch_release_parked_dispatch_obligation` restores the whole
budget. Every event is tested against every park as it is evaluated, whether or
not the rule that parked the obligation matches it, so a rule watching one
narrow signal cannot stay parked on an obsolete head. Progress must follow the
state the lineage stalled on and every fact about that pull request the lineage
already spent across its successor obligations, so a lagging rule replaying an
older event cannot restore the budget for a fact already spent. Rule,
repository, and stack singletons collapse many pull requests onto one
obligation, so a releasing fact must name the same pull request the lineage
stalled on. A branch target carries no head and no review activity, so an
obligation stalled on one is released only by an operator.

The escalation fails the active turn and blocks the commissioned goal only while
that goal's authority still stands, then enters the latest-state obligation and
release path; its requeue is a counted attempt, so a lineage whose work keeps
escalating parks on the budget. Two turns are not terminalized this way and park
for a user exactly as in a session no dispatch created: a turn a steer still
names is attended by definition, and a session that already recorded an
escalation parks too, because goal mode exempts its block from automatic
resumption. Standing authority takes precedence: a goal that ended while the
judge was in flight leaves stale work, so its escalation is terminalized rather
than parked. A turn no escalation preceded is the dispatched work itself,
including one an ordinary execution failure automatically resumed, and takes the
unattended path. The terminal attempt, failure transcript entry, and terminal
frontier are durable evidence of the transition, so a replayed completion
offering any other one is reported as a mismatched replay. An
operator-commissioned dispatch has an attending operator and no independent
redispatch path, so its completed escalation leaves the turn and request
awaiting tool approval, as does a sweep-commissioned dispatch whose authority
still stands; the completed judge call, recommendation, rationale, and usage are
the durable record, and no approval decision is invented.

A pull-request close or merge records one lifecycle cutoff, a later open event
makes an earlier unprocessed cutoff a recorded reopen, and dispatch admission
rechecks the latest durable lifecycle under the repository lock. A terminal
cutoff settles every outstanding obligation for that pull request immediately,
without waiting for singleton or cooldown readiness, except that an obligation
stalled on the cutoff event itself is preserved, because it owes the close
automation and only an operator release lets it run. Either settlement path
records stale nonterminal work as `target_closed` without creating a session. A
rule matching `PullRequestClosed` or `PullRequestMerged` stays
dispatch-eligible, and a non-converged termination of that dispatch still owes
its requeue while its own cutoff remains the latest. Corruption in one
commissioned goal rolls back that goal's stop to a savepoint without rolling
back the cutoff, so healthy goals stop and later cutoffs stay eligible.

Every completed poll commits its cursor, events, and convergence evidence in one
transaction; evidence identical to the latest assessment for the same head and
base revision is an idempotent replay, and changed evidence appends a new
assessment. The gating-check inventory is settled only after the same inventory
is observed in two consecutive committed polls for the unchanged head, so a fast
check cannot seal the head before a later workflow registers. Check runs are
green only when completed with success, skipped, or neutral, status contexts
only at success, and pending or missing-conclusion results are not green. Check
names containing `report only`, `CodeRabbit`, `codecov/project`, or
`codecov/patch`, compared case-insensitively, are non-gating. Head, check, and
aggregate-review evidence is read before the thread inventory, so a thread
opened between those reads cannot be hidden by an earlier snapshot. The rollup's
commit, head, and base-branch evidence must agree with the REST projection and
cursor generation, or the poll fails without recording an assessment. An
append-only cursor-generation identity advances the current projection when an
A-B-A return reuses A's unchanged evidence, while a superseded exact replay
cannot advance it. The first passing assessment creates one monotonic seal for
repository, pull request, exact head, and exact base revision; later checks or
reviews on a sealed identity stay visible but cannot reopen dispatch, so a
session does not revisit threads it already resolved, and a different head or
base inherits no seal. A convergence cutoff is recorded only when a seal's head
and base are the latest assessed identity, stale seals stay pending until their
identity is current again, and admission rechecks the seal under the repository
lock, settling a stale match as `target_converged` only for the latest assessed
and sealed identity.

`CHANGES_REQUESTED` gates merging, never dispatching: repository watch keeps
delivering matching findings while that aggregate decision remains. A blocking
review may be dismissed only when GitHub reports it among the latest opinionated
`CHANGES_REQUESTED` reviews, its commit differs from the exact current head, and
the current evidence otherwise passes with zero unresolved threads, at least one
gating check and zero non-green ones, a settled head, and nonconflicting
mergeability. Both the in-memory candidate rule and the durable eligibility
query enforce the gating count, the settled head, and mergeability, so neither
admits an intent the other would refuse. Before sending the dismissal mutation,
the daemon appends a unique intent naming assessment, repository, pull request,
both heads, review node, reviewer, reason kind, and the exact message, then
re-reads the pull request and proves the whole predicate again against live
evidence, requiring the gating inventory the committed poll recorded for the
same head and stamp. Replaying equal evidence reuses the intent, and a
still-blocking intent is retried only when a current poll again proves the full
predicate. After an ambiguous failure a later poll observes the review directly:
an already dismissed review completes the audit, a newer head supersedes the
intent, and another actor's clearance is recorded as cleared elsewhere. The
following poll observes the dismissal through the ordinary review and
convergence projections and may then seal; no synthetic approval is created, no
fresh review is requested, and dismissal itself does not stop dispatch.

A newly configured rule activates immediately after the repository's current
durable event tail, before its task polls, and consumes later events in cursor
and event-ordinal order. Restart resumes the oldest unevaluated fact and the
oldest eligible obligation for that rule version, redispatching no evaluated
fact and treating no pre-activation history as live. Reconciliation records an
append-only deactivation when a configured identity or its repository disappears
from configuration, and deactivation settles an obligation without dispatch
rather than leaving permanently owed work; terminal-target settlement records
why the obligation is no longer owed. Guarded startup admits the complete
repository set, the empty set included, in two phases: it first validates the
whole set in one transaction it discards, in the Configuration phase before
either local socket binds, then commits the deactivations and activations in one
transaction after every remaining fallible startup step succeeds. A refusal
anywhere in the set, or any startup failure before that commit, leaves no
deactivation or activation, so restoring the previous configuration is admitted
rather than refused as reuse. A lost commit response is resolved by rereading
the durable active set, which commits nothing and so cannot itself become
ambiguous. Reconciliation and evaluation serialize per repository, so an
already-loaded event cannot create a dispatch after deactivation commits, though
a committed evaluation may replay. Changing a rule's semantics while keeping the
same rule identity and revision fails in the Configuration phase. A higher
revision under the same rule identity is a replacement and the ordinary way to
preserve stable identity and history: reconciliation appends deactivation of the
old revision and activation of the new one after the current event tail; a fresh
rule identity remains an admitted replacement path. A deactivated
identity-and-revision pair cannot be configured again, a revision below the
highest ever recorded for that identity in that repository is refused, and rule
identity is per repository, so the same identity first configured in a newly
watched repository starts its own lineage.

Everything the listener does before a delivery is durably admitted is identical
in both webhook modes. The body's canonical repository must equal the repository
the hook identity selected. Each hook admits at most 3,000 deliveries in any
rolling 60-second window, charged only once a delivery has proved the shared
secret, because a budget keyed on a claimed hook is attacker-controlled and
spending it on forged signatures would reject real deliveries. An equal replay
returns the same success without new work, and reuse of that identity with a
different digest returns conflict and cannot replace the first body. Every new
admission and equal replay publishes a coalescing in-memory wake after commit,
and the listener returns success only while that repository's drain task can
receive it. The repository task drains pending deliveries at startup, when
woken, and before and after every full poll that owes no retry, and schedules a
new drain attempt after five seconds, doubling to a five-minute ceiling on
consecutive failures and returning to five seconds on success. The drain
deadline spans provider and database work; expiry cancels the attempt, leaves
deliveries pending, invalidates partial freshness, emits a closed timeout cause,
and enters projection backoff. A deadline reached by the pre-poll drain stops
that poll before its provider sweep can advance the cursor past the
still-pending delivery, and a poll that observes the same transition as an
admitted delivery cannot advance the cursor past it. When no earlier delivery on
the page failed projection, expiry during the dispatch work after a delivery's
terminal record leaves no projection pending; that attempt is rescheduled at
five seconds without backoff and does not stop a pre-poll drain's poll. A
targeted completion started by a cancelled drain retains its exact terminal
request and cursor write, recording disposition and projections as the durable
recovery handoff before the cursor write. If every settling read is unavailable,
the shadow is discarded rather than trusted, because a disposition may have
landed without being reflected in that baseline. A delivery whose
target-specific processing fails is deferred for the rest of that drain rather
than failing it, and the attempt still reports the first such failure;
credential, transport, provider-throttle, and provider-outage failures stop the
current page, because they prove later targeted requests cannot make independent
progress. A failing pre-poll drain is reported and not propagated, so
acceleration never cancels the reconciliation sweep.

A signature-valid delivery outside the mapped set, including ordinary issue
comments, other actions in mapped families, tag pushes, create and delete
families, foreign workflow heads, and a broadly subscribed `workflow_job`, is
acknowledged and recorded as ignored rather than treated as an intake failure. A
delivery's targeted provider queries complete before anything is recorded, so a
transient provider failure leaves the delivery pending; once its projections and
disposition are durable, a later cursor-write failure does not reopen it and the
durable cursor becomes the next shadow baseline. On any targeted-completion
failure, unpublished provider freshness is invalidated, so a later unrelated
commit cannot authorize reuse of state that never reached that cursor. A mapped
delivery needing a missing baseline, current mergeability, or a check rollup
records a targeted-query projection and reuses the poller's credential, client,
cache, normalization, and bounds. Within one drain page, a whole-pull-request
hydration an earlier delivery landed with no head-guarded refresh alongside it
is not repeated, so the later delivery records no query for it; mergeability and
check-rollup refreshes always issue. Guards classify stale head, lifecycle,
branch, workflow-attempt, and immutable-provider facts as superseded or
duplicate rather than allowing a regression. Event projections carry no
uniqueness constraint, because separate deliveries may represent one content
occurrence. Terminal payload bytes remain seven days; after a successful full
poll, at most once per day starting with the first poll after boot, expired
bytes are deleted.

The repository's single serialized worker applies the closed guarded patch to
the latest cursor in memory and runs the same differ and content-identity
frontier polling uses; computed mergeability and aggregate check rollups are
never projected from a delivery. The shadow baseline is cumulative and belongs
to the repository task; a delivery advances it only once its own terminal
disposition is durable, and only a full poll, once nothing is pending, replaces
it, because only a full poll is the complete reconciliation sweep. Primary mode
reloads the durable cursor per delivery so the loaded generation is the expected
generation the optimistic commit needs; a patch that duplicates state, is
superseded, or names a fact outside the observable set records the same
disposition as in shadow mode and writes nothing, and a patch that applies is
compared against the loaded cursor in one differ pass whose occurrences are the
batch the commit writes. A primary delivery records no event projection, because
its own commit is the durable row and projecting it too would leave a permanent
`webhook_only` row nothing can match. The parity view's poll side is bounded by
the repository's promotion, its first committed disposition, rather than by its
first webhook-produced row, and a disposition records what a delivery did rather
than what mode was configured, so no mode record exists for a reverted
configuration to strand. Dispatch processing follows a landed primary commit,
because under primary mode every applied delivery is a cursor advance rules may
act on. Parity is measurable in shadow mode as zero parity rows whose status is
`webhook_only` or `poll_only` and whose cause is null.

Automation convergence is not provider mergeability or checks: a current-head
seal requires the latest dispatch released, its goal achieved, and its delivered
head current; an achieved release against an older head is a stale seal, and
held, queued, non-converged, and unattempted states stay distinct.

The convergence sweep observes GitHub and commissions work; it never merges a
pull request, replies to or resolves a review thread, or mutates Git. No rule
discovers or enrolls every open pull request. The sweep interval bounds how long
unconverged work stays undiscovered while limiting provider request volume, and
the per-pull-request cool-off gives a commissioned session time to make durable
progress before another dispatch. A failed census fetch, commission, template
resolution, or sweep-state read appends a typed failure event and advances the
count of consecutive failures of that kind; retries back off exponentially
within configured bounds, the fifth consecutive failure parks the target for an
operator, and a successful observation resets the count. A storage outage that
also prevents the failure record leaves only a log, and the target retries at
the next census. After cool-off, repeated sweeps with an unchanged head, an
unchanged unresolved-thread count, and no recorded model call park the target at
once as `no_model_activity` with the operator need `inspect_inactive_session`.
The sweep is a shallow daemon loop that creates no reusable program primitive,
and it does no prioritization or scheduling beyond the configured list.

## Boundary contracts

The daemon refers to a credential by its non-secret name everywhere except at
the point of use. No credential value, credential file path, or database URL
appears in a log, an error, or a durable record. For a profile whose credential
value the daemon resolves, the daemon redacts that exact value from provider
text before it truncates the text; a delivery that gives the daemon no value
receives credential-shape redaction instead. A credential for one repository
never authorizes a request to another.

A poll that fails, is rejected, or cannot be parsed commits no cursor and no
events. A commit appends the cursor and its events together under an expected
generation, rolls back as a whole on failure, and reports a stale generation as
a conflict. Every producer derives the same identity for the same content, so
the same event from two producers is stored once. The daemon verifies a
webhook's signature against the exact body bytes before it parses the body. The
daemon stores a verified delivery before the listener returns 202. A webhook
wake only prompts the daemon to process deliveries it has already stored; the
stored deliveries, not the wake, are the record of pending work. Shadow mode
writes no webhook-produced event row and never patches the cursor from a
payload; a targeted poll it triggers writes as any poll does.

The per-repository commit serializes competing commits and recognizes only an
exact replay of candidate and occurrences. It coalesces an occurrence whose
content identity is already durable for the repository under the same content,
writing the cursor without a second row; without coalescing, an entity that
leaves the observation and returns would abort the whole transaction and every
later poll would repeat the failure.

A complete or targeted observation derives events against the full provider
state first, then removes each merged pull request from the ordinary cursor
observation.

The identifying payload members are narrower than the complete payload, and a
second producer must exclude exactly the same members or cross-producer
coalescing fails.

A provider fact retained in the consecutive comparison baseline is not
re-emitted. Rules receive only events and cannot inspect normalized snapshots or
rerun the differ, because polling and webhooks must feed the same durable facts.

Accepted events append in observation order as durable facts and are never
updated, deleted, or truncated.

Every row one commit writes records that commit's producer: `poll` for the
complete sweep and every targeted refresh it performs, `webhook` for a row an
authenticated delivery committed under primary mode. A fact a delivery's own
targeted refresh supplied is therefore attributed to that delivery, and a
primary-mode row is never reported as a poll-only divergence.

`ChecksCompleted` carries success or failure, and completed success, neutral,
and skipped suites normalize to success.

Reaction ingestion includes only reactions by a login in the configured
signal-reviewer list, and a reaction whose deleted actor has no current login is
never added.

Each admitted action creates a fresh session from the complete resolved template
copy, submits the tagged context as its first accepted JSON input through
`StartWhenNoActiveTurn`, and commissions the session's goal, all in one
transaction. No dispatched session is visible without its accepted input, its
queued turn, its dispatch-to-turn audit link, and a statement of the authority
it was dispatched under.

An operator commission through `commission_session`, and every sweep dispatch,
commits in one transaction the template session, the append-only
commissioned-dispatch fence, the caller's context as first accepted input, and
the goal adopting that turn.

The dispatch action is the immutable authority source for an approval judge
invoked by the generation that dispatch commissioned. Its values are read from
the append-only dispatch event and action, never inferred from the synthesized
goal or refreshed provider state. A judged turn recorded in another generation
of that session, or in no generation, resolves no dispatch authority. The quoted
rendering and the use of the judge's decision follow [tool loop](tool-loop.md).

An occupied singleton, or an independently commissioned live session owning the
same pull request, refuses another match and atomically opens one durable
delivery obligation. A batch releases its singleton at the transition that makes
every dispatched turn terminal or runtime-irrelevant, leaves no live
runtime-relevant turn, and leaves no pursuing goal. A batch owes at most one
requeue, at its own release, decided by the terminal event ending the generation
the dispatch commissioned; a later generation reopens none. Equal recovery
cannot create a second session for the same admitted action or obligation.

Any durable model-call row for the session is first-call evidence and ends the
start obligation without rewriting the lease.

A parked obligation returns to dispatch only by operator release or by a new
fact about the pull request it stalled on: an event carrying a head other than
the stalled one, or review activity against it.

A completed approval-judge escalation judged under dispatch authority is an
execution-failure terminal transition rather than an attended approval wait. The
turn keeps awaiting approval while a steer names it, while operator- or
sweep-commissioned authority still stands, and while authority still stands for
a repository-watch dispatch whose session already recorded an escalation.

While a pull request's lifecycle remains terminal, repository watch applies the
parent-only stop to each generation-one goal it commissioned for that pull
request and to nothing else.

A converged assessment requires every review thread resolved, at least one
gating check with every gating check green, settled `mergeable` mergeability,
and no `changes_requested` decision.

Every effective blocking review must target a superseded head; one current-head
blocker prevents every dismissal for that assessment, and a current-head review
is never dismissed automatically.

The webhook receiver collects at most 25 MiB of body, compares the HMAC in
constant time, and parses nothing before verification succeeds.

Full polling is the slow complete reconciliation sweep and is authoritative for
missed deliveries, reactions, and every fact outside the mapped set.

Readiness is read from the durable outstanding-obligation view rather than
recomputed, so the operator read cannot report an obligation ready that
admission would refuse.

A malformed, closed, missing, partial, oversized, or provider-refused sweep
census is a facts-fetch failure, never evidence of convergence.

Contracts this page relies on but does not own: the command claim and replay
protocol and the provenance-only rule in
[identity and commands](identity-and-commands.md); the lock order, the
no-transaction-across-I/O rule, and typed corruption on an undecodable row in
[persistence protocol](persistence-protocol.md); ownership-owes-resumption and
its repository-watch exception in [goal mode](goal-mode.md); approval and
dispatch from the frozen proposal snapshot in [tool loop](tool-loop.md); request
framing in [process protocol](process-protocol.md).

## Planned

- A purpose-specific creation cause and actor for repository-watch dispatch;
  dispatch uses the user-initiated creation and input interfaces:
  [design](../design/repo-watch.md).
- Durable repository-watch provenance linked to the dispatch identity,
  preserving dispatch, session, context, and input identities:
  [design](../design/repo-watch.md).
- Cutover of the structured-rule dispatch surface onto the program substrate,
  each rule becoming a subscription whose action is a built-in dispatch program:
  [design](../design/repo-watch.md).
- Shadowing during that cutover as validation only, never delivery owner:
  [design](../design/repo-watch.md).
- Persisted HTTP validators and per-resource accepted transport snapshots stored
  beside the cursor, so a restart does not re-fetch every pull request:
  [design](../design/repo-watch.md).
- That persisted cache excludes raw provider JSON, credentials, and reactions
  from actors outside the signal-reviewer set:
  [design](../design/repo-watch.md).

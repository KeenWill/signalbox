# Model-call execution

Model-call execution carries one turn attempt's request to a model provider as
one durable, at-most-once physical call and records what came back.

## Overview

The subsystem is the model-call chain: rendering a context frontier into
provider messages; the staged prepare, authorize-send, and commit-observation
transactions; the tool rounds between calls; classification of the provider's
answer into a physical disposition; and the prohibition on retrying a call.
[credential-availability](credential-availability.md) owns what a
credential-pool selection can end as; this page owns the evidence and cause a
terminal call records and the mechanics of a successor call. Tool requests,
approvals, and attempts belong to [tool-loop](tool-loop.md); turn and attempt
lifecycle to [turn-lifecycle-and-scheduling](turn-lifecycle-and-scheduling.md);
semantic entries and frontiers to
[sessions-and-transcript](sessions-and-transcript.md); storage and the outbox to
[persistence-protocol](persistence-protocol.md); the typed model-runtime layer
and its adapters to [runtime-substrate](runtime-substrate.md); model
configuration and credentials to
[configuration-and-credentials](configuration-and-credentials.md).

A model call is one durable daemon authorization to attempt a provider
interaction. Its record, `CurrentModelCall` in
`crates/domain/src/model_call.rs`, fixes at creation which turn and attempt own
it, the frozen model selection, the provider target pinned for the turn, and the
exact ordered context frontier it consumes. A live call is prepared, in flight,
or cancellation-requested. When it ends, a separate ended record carries one of
five physical dispositions, the provider's reported token usage, and a
usage-provenance discriminator.

`ModelCallExecution` in `crates/domain/src/model_execution.rs` is the aggregate:
one active accepted-input turn in its running phase plus the one call owned by
its current turn attempt. Reconstitution rebuilds it from stored rows and checks
every fact against the others before any transition is authorized.

Rendering first applies the compaction projection to the call's exact frontier:
when summaries exist, the latest summary comes first, every entry after its
through-boundary follows, and the summary is omitted from its later physical
position. The projected order becomes provider-neutral messages, and the runtime
bridge maps those to provider wire messages. The selected summary renders as one
user-role message, a fixed prior-conversation-summary preface followed by the
summary text; its provider-neutral message keeps the producing call and the
summarized range. Attachments render as the bounded textual stubs
[blob-storage](blob-storage.md) defines.

Context compaction produces its summary through a dedicated physical model call
with its own durable prepared, in-flight, and terminal lifecycle, separate from
ordinary calls. A headroom guard runs at two points. Before activating a queued
turn it may spend one automatic compaction. When that compaction fails, or the
request still exceeds the window after it, one transaction fails the queued turn
with no ordinary call prepared. Inside the tool-result continuation transaction
an exceeded bound commits the tool results, prepares no continuation call, and
fails the turn with a headroom record. The guard adds the newest reported input
for the pinned target, a byte allowance for model-visible content that input
does not cover, and the configured output reservation, and compares the sum with
the configured context window. The compaction call's own input budget is its
context window less the output ceiling and the required prompt; when even the
first safe prefix cannot fit that budget, no call is prepared and one
transaction fails the turn as a compaction wall. Automatic compaction targets
the first safe boundary at or beyond half the rendered bytes and falls back to
the latest safe boundary that fits.

Anthropic prospective input counting is the one provider interaction permitted
before activation and before a `model_call` exists. The accepted input, frozen
session epoch, pinned target preview, and credential pin authorize that
stateless estimate; it has no completion semantics and creates no call outcome.
Attachment verification precedes that interaction. Cancellation or transient
attachment loss leaves the turn queued, and any later attempt must render and
count the then-current preview again. A definitive attachment failure atomically
activates and closes the exact prospective Prepared call with that evidence.
Only a successful estimate whose input plus full output reservation is at most
95 percent of the configured context ceiling enters the counted activation
transaction; otherwise the turn compacts. An estimate that returns no validated
count falls through to ordinary uncounted activation. A reserved dispatch-start
leaves an Anthropic turn queued before count or attachment I/O.

Anthropic ordinary calls enable provider-default server-side compaction only for
provider-model identifiers in the closed adapter mapping: the `claude-fable-5`,
`claude-mythos-5`, `claude-mythos-preview`, `claude-opus-5`, `claude-opus-4-6`,
`claude-opus-4-7`, `claude-opus-4-8`, `claude-sonnet-5`, and `claude-sonnet-4-6`
family stems, including numeric release suffixes such as `claude-fable-5-1` and
dated suffixes. Each returned compaction block is an opaque ordered semantic
entry replayed unchanged as Anthropic assistant content when the resolved target
supports that mapping. A later call to an unsupported Anthropic target or
another adapter omits the provider-qualified opaque block from its request
projection and retains the preserved pre-compaction history; this projection
neither removes nor rewrites the durable entry. The block's durable nullable
`content` fact separately classifies its input as replaced or retained for later
headroom accounting on calls that replay it: non-null replaces the
pre-compaction input and null is a replayable no-op. This classification does
not rewrite billing evidence. Anthropic iteration usage remains the sum of every
reported iteration on the call's four usage axes, and an iteration missing
required input or output usage is invalid response material. The
tool-continuation guard likewise excludes replaced pre-compaction input from its
retained-context baseline when the next request replays the block.

`ModelCallExecutionService::execute` in
`crates/application/src/model_execution.rs` runs one linear invocation over five
composed roles: prepare, capability, authorize-send, provider, and
commit-observation, plus an id generator and a per-attempt dispatch gate. The
prepare transaction commits the prepared call with its pinned non-secret
credential reference, the turn-target pin, and a transition outbox event. When
the frozen selection resolves to no target, that transaction creates no call and
fails the attempt and turn as target unavailable. Capability preparation and
provider interaction run outside any transaction and share one call-scoped
cancellation signal that resolves when an authoritative reload finds the call
cancellation-requested or terminal, or finds a logical-terminal proof for the
delegated child turn that owns the call; a retained provider result observed
after that proof is discarded, not committed. The authorize-send transaction
moves the call to in flight. The commit-observation transaction reloads
authority and commits the call disposition, the attempt and turn transitions,
semantic entries, the terminal frontier, and the outbox rows.
`FatalExecutionSupervisor` in `apps/signalboxd` wraps execution and stops the
process when a failure leaves nothing trustworthy to classify live; startup
recovery in `crates/persistence/src/startup.rs` then classifies every retained
call from durable evidence.

The runtime bridge in `crates/model-provider-runtime` maps the runtime's typed
terminal evidence to exactly one disposition: completed text, provider
compaction blocks, and tool-call content to `Completed`, refusal to `Refused`, a
provider error or other proof of non-acceptance to `KnownFailed`, cancellation
before send or confirmed cancellation to `Cancelled`, and loss after possible
acceptance to `Ambiguous`. The requested selection, the pinned resolved target,
and the provider-reported identity are three separate facts, and the bridge is
the one place that relates the third to the second. Exactly one of three
relations holds: exact, alias concretion (the configured spelling followed by a
dated snapshot qualifier), or different lineage.

`apply_terminal_observation` derives one of seven outcomes from fresh state, and
persistence commits the outcome atomically with its outbox rows. Ambiguity parks
the turn in the durable awaiting-recovery phase, carrying that one call as its
wait set and retaining the session slot. When such an unacknowledged ambiguity
also carries an applied-interrupt proof, the turn instead terminalizes as
reconciliation required, with the wait set, an interrupt-requires-reconciliation
marker, and a reconciliation outbox record, and releases the slot.

A `KnownFailed` call whose cause is one of the three availability causes (quota
exhausted, rate limited, or overloaded) and whose pool configures `switch_now`
for that cause may be followed by a successor call: a distinct call on a
successor turn attempt against the next admitted member of the same pool.
`AvailabilitySuccessorModelCallTurn` is the aggregate transition that authorizes
it.

Usage evidence is a projection of terminal physical model calls that never
materializes the transcript; `UsageReader` in `crates/application/src/usage.rs`
is the read port. Each projected row carries the call's target, a bounded
non-secret credential-profile label, its usage provenance and input semantics,
and its token axes; the projection is append-only. Two read forms exist: an
aggregate report grouped by compatibility key, and a newest-first detail page
with a keyset cursor.

## Design decisions

A terminal call record never reopens, because it records what was externally
done, and rewriting it would let later facts silently change that record. The
migration enforces the transition matrix, the immutability of terminal rows and
authorization facts, and the target pin, because the schema backstops the
aggregate against any buggy or racing writer, not just the audited one. The
provider target is pinned as a turn fact before the first call exists and every
call in the turn uses it, so a mutable alias or deployment change cannot enter a
turn as recovery.

A call prepared before the input-semantics pin existed keeps a null pin and its
reported axes exactly; the null, not a rewrite of the axes, keeps a read from
deriving cost from possibly cache-inclusive input. Every writer records usage
provenance as `reported`; `estimated` exists in the closed vocabulary for a
later explicit estimator.

Reconstitution refuses every invalid shape rather than repairing it, because
acting on a partially consistent projection could authorize a second provider
effect against stale authority. Reconstituting a checkpointed prepared call
reloads the call's own stored snapshot, including the steering it consumed,
because checkpointing cannot erase steering that the durable call was prepared
to observe. Every adapter reaching the domain seam rejects cross-wired steering
history itself, even when its storage schema already performed the same
correlation.

The renderer skips every imported entry other than attested text: source events
and content absence are not conversational, imported tool identities do not
exist here, thinking is source-private, and media has no admitted projection.

A required bounded deployment prompt is the dedicated compaction call's system
prompt; the session system prompt is never substituted for it. A uniqueness
violation observed while applying a compaction completion is a decided fact, not
a retryable database failure: the completion fails closed and its in-flight call
is left to startup recovery, because the identities are pinned by then and an
identical retry would fail the same way. The output ceiling and the context
window are operator-declared per catalog selection and never inferred from
provider or model names. The daemon reserves the full configured output ceiling
before each continuation even for an adapter that can only render the ceiling as
advisory context, so such a deployment keeps its intended reply budget rather
than the model's larger capability ceiling.

The headroom guard reads the newest reported input from any terminal ordinary
call since the last compaction, whatever its disposition. Why: a failed or
ambiguous round may still have been accepted, and its reported size keeps a
resumed session from resending a request that already exhausted headroom. An
ordinary call's reported input is the next request's baseline because its
successor resends that prefix; a compaction call's reported input measures
source text no later request carries, so its baseline is the retained summary
output plus the allowance for what it did not summarize. After a nominal
completion the daemon retains the reported usage and the completed observation
even when reported output, or input plus output, exceeds a configured ceiling,
and emits an operator cause instead, because the provider has already accepted
and served the request.

A new prepared call commits in its own transaction and never advances to in
flight in that transaction, so a crash can never produce a provider effect with
nothing durable to classify. The adapter resolves its credential from the call's
pinned reference and returns an opaque one-shot send capability that application
and domain code can only move, so credential escape and capability reuse are
structurally impossible rather than a review convention. A deterministic adapter
defect commits the guarded unsent known-failure closure before raising its fatal
operator signal, so a successfully recorded defect cannot terminate every later
incarnation on the same call. Only failure or ambiguity of that closure leaves
the call prepared for startup to validate and retry. The per-attempt dispatch
gate is held from the authorize-send commit until the runtime first reports that
provider acceptance is possible, which serializes execution passes for that
attempt across the acceptance boundary without serializing interrupt
application.

The chain exclusion that removes the failed member commits in the observation
transaction itself, because a crash between the observation and a later release
could readmit the profile whose failure parked the turn. Identities knowable
only under the lock are minted through application-owned generator closures that
persistence invokes inside the transaction, so the locked pending count moves
into the transaction without moving identity authority into persistence. A
proven daemon-minted identity collision is the only failure retried within one
invocation, with fresh candidates and no repeated credential or provider work,
because a unique-violation rollback is the one failure that guarantees the
transaction had no effect.

Ambiguity parks the turn instead of retrying or substituting, because a lost
acknowledgement cannot prove the provider did not act, and an invented
exactly-once claim could duplicate both an effect and its spend. Refusal never
admits a successor: it is provider judgment about the request, so another
account would refuse the same content and substituting one would only seek a
different answer. Credential resolution failure and credential rejection never
admit a successor: both are deployment misconfiguration, and moving to another
account hides the account that is broken.

A successful call ends its availability chain, and a later tool round starts a
fresh one, so a round that exhausts the pool before calling carries no earlier
round's failure. A successor prepared when a parked wait releases carries the
predecessor call and its non-acceptance proof in its origin, so it is that
failure's authorized successor rather than the start of a fresh chain. Releasing
a wait never readmits the member whose failure parked the turn, because
otherwise a one-member `switch_now` pool configured to park would wake at its
deadline, drop the sole exclusion, and call the same profile again without
bound. Goal disposition keys on whether the observation selected a wait, not on
the pool's configured action, so a park pool whose members are all excluded
blocks like any other failure rather than staying current forever;
[goal-mode](goal-mode.md) owns the disposition.

The identity relation is derived from the configured target's own family, never
from a table of known provider identifiers, so a newly published model needs no
code change. Alias concretion requires a full date shape, compact or hyphenated
and never checked for calendar validity, rather than any trailing segment, so a
version extension of the same family name is not read as a snapshot. An accepted
alias concretion records the identity that served only as operator diagnostics,
not as a durable per-call provenance row. A reported identity reaches operator
diagnostics only after the adapter's credential redaction and a
character-boundary truncation to the configured diagnostic bound;
[runtime-substrate](runtime-substrate.md) owns the redaction. A substitution
fails the adapter stage closed with an operator error, because the durable
substitution provenance it would have to record does not exist; a substituted
call is therefore classified `Ambiguous` by restart rather than `KnownFailed`
live. The runtime's exhaustive provider-error classification is carried verbatim
into the operator cause codes rather than restated, so the adapter taxonomy and
the operator vocabulary cannot drift apart.

Every model-call transaction issues the session-scheduler row lock as its first
statement, so per-session serialization is total and lock-order cycles on one
session are impossible; [persistence-protocol](persistence-protocol.md) owns the
lock order. Tool-result continuation reconstructs its result frontier under the
per-session lock and takes the global ordering guard only before projecting its
outbox event, so long frontier reads do not serialize unrelated model-call
writers while the guard still prevents a credential/allocator cycle.

A failure with retained execution evidence after its one reconciliation pass, an
ambiguous commit outcome, an unwind, or cancellation raises the fatal signal and
the process exits nonzero. Why: startup recovery is the one audited path that
classifies an issued call from durable evidence, and a live process that cannot
construct a trustworthy result must stop rather than improvise. Repeated
same-incarnation reconciliation drains are exercised only by tests.

An aggregate usage read consumes a bounded count of newest matching calls and
returns a bounded count of groups, recording truncation when either bound is
hit, because bounding before grouping prevents an unscoped lifetime query from
imposing work proportional to retained history. Usage result shapes hold their
bounds and internal consistency by construction rather than by adapter
discipline, and the PostgreSQL adapter fails closed on a projection row that
would require an unconstructable result. Oldest-first traversal is not exposed,
because a statement timestamp precedes its commit and a late commit can appear
behind an already emitted cursor. Dollar cost is not stored in the projection;
[configuration-and-credentials](configuration-and-credentials.md) owns its
read-time derivation.

## Boundary contracts

A model call is one recorded attempt. The daemon sends each attempt to the
provider at most once. A retry is a new recorded attempt; no code retries a call
without recording the retry in the database. Before anything has been sent to
the provider, the daemon may prepare an unsent call again. After a known failure
the daemon may start a new attempt with a different credential. It never sends
again with the credential that failed in that chain. A call whose outcome is
unknown is never retried automatically; the turn parks for recovery. A CLI
harness may retry inside itself. Those retries are provider-internal; the daemon
neither observes nor records them and adds no retries of its own. A migration
constraint enforces one call per attempt. A `switch_now` failure with proven
non-acceptance writes a durable chain exclusion for the failed member, and
successor selection and preparation skip excluded members; that selection, not a
constraint, enforces no-reuse. The one-shot send capability, the per-attempt
dispatch gate, the authorize-send commit, and startup parking of an issued call
enforce at-most-once sending. Only the rule that no code retries a call without
recording the retry is unenforced.

The terminal transition stores the input, output, cache-creation, and cache-read
token axes independently; a null axis means the provider did not supply it, and
a present zero stays zero. The prepared checkpoint pins whether reported input
includes the separately reported cache axes, so a later configuration that
routes the target through another adapter cannot change how a stored call's
input is read.

Canonical credential references stay exact and gain no length bound from the
usage read projection. Aggregate reads group only calls that agree on call kind,
target, credential profile, provenance, input semantics, and the presence state
of every token axis; that grouping is the compatibility boundary for cost
derivation. A detail cursor provides deterministic keyset traversal of rows
already visible ahead of it, not a cross-page snapshot.

A tool continuation must prove that the exact stored call frontier includes the
current tool round's complete result evidence. A checkpointed call becomes
resumable only when its extended snapshot is a strict prefix-preserving
extension of the turn's starting snapshot and its membership equals the checked
semantic entries.

Every rendered message keeps its source-qualified semantic-entry reference and
content-authority provenance; role and provenance derive from the entry itself,
never from turn grouping. Skipping an entry changes only what the model sees; it
does not remove, rewrite, summarize, or reorder the semantic entries or their
addressable imported frontier. Delegation messages render through an injected
user transport role that creates no accepted input, no user actor, and no child
transcript access; the structured value retains its model- or tool-authored
spawn provenance. A model-identity change projects as an injected user-role
message naming the newly selected identity and the frozen defaults epoch. The
session system prompt is sourced only from the calling turn's frozen defaults
epoch, and the bridge sets the operation's system prompt from it exactly or
leaves it empty; [sessions-and-transcript](sessions-and-transcript.md) owns the
freeze.

Missing usage fields stay missing and are never invented; classification does
not derive usage from the disposition, content, context, or provider family, and
classification issues no separate counting operation. Historical compaction
calls with unknown cache-inclusion semantics are treated as cache-exclusive, so
the guard may overcount but never omits reported cache axes. A definitive
request-size failure on a frontier the prospective call preserves forces one
automatic compaction when no later accepted call or completed compaction
supersedes it, even without reported usage. Missing usage does not trigger the
tool-result headroom boundary, and inconsistent producing-call evidence fails
closed.

A trustworthy ordinary capability failure commits the prepared-to-known-failed
closure with attempt and turn failure in a separate guarded transaction. Every
durable physical call transition, not only the terminal one, appends its
transition outbox event in the transaction that commits it;
[persistence-protocol](persistence-protocol.md) owns the outbox rule. The
provider port is invoked at most once per invocation, and exactly once only
after the in-flight commit is known. Credential-pool effects derived from an
observation reload the immutable policy identity pinned by that prepared call,
not the session's current credential-history head. Every derived record commits
with the observation's exact correlation in the same all-or-nothing transaction
as the terminal evidence and the disposition it selects. The application owns
all candidate identity minting, and persistence uses or discards candidates but
never mints its own. An ambiguous commit is never resolved by replay; the next
pass rereads authoritative state before any later action.

A physical call completion is never treated alone as proof that the logical turn
completed. A `KnownFailed` call retains only the closed provider-error
classification as its optional cause, never provider prose. A known failure ends
the attempt and fails the turn with a `TurnFailed` entry and a terminal frontier
unless the same observation admits an availability successor. An admitting
observation ends the predecessor attempt as a known failure but appends no
`TurnFailed`, creates no terminal frontier, and reclassifies no pending steering
while a successor or wait keeps the turn active;
[turn-lifecycle-and-scheduling](turn-lifecycle-and-scheduling.md) owns
reclassification at terminal outcomes. A concurrently accepted stop is
serialized by the session-scheduler lock, so one commit can never both
terminalize the turn and authorize a successor. The successor pins the same
resolved target and a different credential reference, so no call changes
identity mid-flight. For each admitted availability cause the adapter must
supply distinct typed evidence that the request was not accepted; classification
as quota, rate limit, or overload alone is insufficient. Without the exact
applied-interrupt proof a physical cancellation is an unstopped known failure,
and a stop-requested attempt whose call ends known-failed still fails and cannot
admit a successor, because the physical result has not proven cancellation.

`Completed` admits only text, provider-compaction, and tool-call parts. A
provider-compaction part must be a complete validated `compaction` object, must
contain no prepared credential, and is retained byte-for-byte for replay while
only a non-text marker crosses the process protocol. Empty text and empty
thinking blocks are dropped, while thinking with text and redacted thinking fail
the adapter stage closed as unsupported material, because no durable semantic
representation exists for either. Tool content and a tool-use finish must agree;
either one without the other is a known failure. The dedicated compaction call
rejects every tool and suppressed-tool part and accepts a summary only from a
completion that ended by end turn or stop sequence, because its completion must
be whole summary text. Classification is an adapter contract consuming the
full-request-send boundary; the daemon never reinterprets SDK errors by
retryability or exception type. The identity relation applies to every identity
the exchange reported, early observations and terminal evidence alike, because
it is timing-sensitive. Different lineage is a substitution: the provider served
a model the daemon never authorized, and it is never collapsed into the alias
case or into an ordinary provider failure. When the Anthropic adapter sees the
server-side fallback block, the response can never complete as the resolved
target's output, whatever the block names; a block naming the configured target
itself classifies as ambiguity rather than substitution, because no durable
marker-only evidence exists to carry a substitution. Every classified outcome
and every fail-closed bridge defect carries a stable sanitized cause code
alongside the shared operator failure class defined in
[runtime-substrate](runtime-substrate.md).

A model-call transaction that both appends an outbox event and locks shared
credential-pool action heads first takes one global transaction-scoped ordering
guard.

A returned infrastructure failure that proves both a non-ambiguous commit
outcome and the absence of retained evidence stays an ordinary per-session
scheduler failure while other sessions continue. Attachment unavailability
during preparation is not a stage failure: it carries no ambiguous durable
effect, returns a nonfatal deferred result, and leaves the scheduler running;
[blob-storage](blob-storage.md) owns the preparation order and failure classes.
A later scheduler pass never treats an issued unclassified call as fresh
authorization. At startup, a durable prepared ordinary call proves no send
authorization existed: recovery leaves the call, attempt, and turn unchanged,
and the ordinary scheduler later retries preparation of that same unsent call. A
durable unstopped in-flight call with no surviving evidence ends `Ambiguous`,
its abandoned attempt ends lost, and the turn parks in the awaiting-recovery
phase. A durable cancellation-requested call reconstructs its applied interrupt,
ends the attempt after-cancellation lost, and terminalizes the turn as
reconciliation required with that call as the ambiguity set. A dedicated
compaction call owns no turn: at startup a prepared one ends `KnownFailed`, an
in-flight one ends `Ambiguous`, its pending compaction command fails, and no
summary is written. Recovery never itself resumes an attempt, redispatches a
call, or assumes a request was or was not sent.

The model-runtime layer imports and redefines no domain identifier type, and a
runtime-generated identity is never authoritative correlation; the correlation
the sealed issued call carries is.

## Planned

- Multipart attachment rendering ([design](../design/model-call-execution.md)).
- Runner-placement rendering and the executable session-tool snapshot
  ([design](../design/model-call-execution.md)).
- Reuse of a successful attachment verification within a turn
  ([design](../design/model-call-execution.md)).
- The process-level exclusion-evidence event
  ([design](../design/model-call-execution.md)).
- The reconstitution check that the pinned policy contains the pinned profile
  with the expected adapter and delivery kind
  ([design](../design/model-call-execution.md)).
- A structured-output contract on the session path
  ([design](../design/model-call-execution.md)).
- Durable provider-target evidence, pending the per-call provenance schema
  decision ([design](../design/model-call-execution.md)).
- Unstopped ambiguity recovery ([design](../design/model-call-execution.md)).
- The workspace-instruction region of the prepared model operation
  ([design](../design/model-call-execution.md)).

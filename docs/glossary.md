# Glossary

This glossary is a terminology index into the living specification under
[docs/spec](spec/README.md). Each term carries one working definition and a link
to the spec page (or section) that owns its full semantics; terms whose designs
are accepted but not yet distilled into any spec page are marked as deferred
design and link to [open questions](open-questions.md).

## Session

A durable, independently browsable conversation with versioned model-selection
defaults, ordered semantic history, operational work, and future archival state.
See [sessions and transcript](spec/sessions-and-transcript.md).

## Accepted input

One user submission made durable with its explicit delivery request and
recoverable disposition before acknowledgement. See
[sessions and transcript](spec/sessions-and-transcript.md).

## UserContent

The caller-supplied immutable content value owned by one accepted input. See
[user content](spec/sessions-and-transcript.md#user-content).

## Durable command identity

One owner-global idempotency identity for a durably handled caller command,
whose claimed canonical payload and terminal result make replay deterministic.
See
[durable command records](spec/identity-and-commands.md#durable-command-records).

## Actor

The typed provenance fact recording which kind of agency initiated a durable
command or recorded transition. See
[actor attribution](spec/identity-and-commands.md#actor-attribution).

## Turn

One durable logical request for Signalbox to produce a conversational outcome
from one typed origin under one frozen effective configuration. See
[turn states and the active slot](spec/turn-lifecycle-and-scheduling.md#turns-states-and-the-single-active-slot).

## Turn attempt

One exclusive physical orchestration tenure that advances an active running turn
until it ends or yields to a durable wait. See
[turn attempts](spec/turn-lifecycle-and-scheduling.md#turn-attempts).

## Model call

One durable hub authorization to attempt a physical interaction with a model
provider against one exact resolved target and context frontier. See
[call records and lifecycle](spec/model-call-execution.md#call-records-and-lifecycle).

## Outcome-authoritative provider call

The sole model call eligible to determine one provider interaction's completion,
refusal, failure, or cancellation; authority transfer is deferred design. See
[model-call open edges](spec/model-call-execution.md#open-edges).

## Provider-target mismatch invalidation

A typed value recorded when trusted mismatch evidence is first learned after the
outcome-authoritative call completed but before its turn terminalized, stopping
further semantic effects without rewriting the call. Deferred design; see
[model-call open edges](spec/model-call-execution.md#open-edges).

## Tool request

A logical request for one named tool operation with normalized arguments, policy
state, and eventual logical outcome. Deferred design; see
[tool safety](open-questions.md#tool-safety).

## Tool attempt

One physical effort by a hub-local or runner-local executor to perform a tool
request. Deferred design; see
[tool safety](open-questions.md#tool-safety).

## Creation cause

The typed reason a session exists, of which owner-initiated is the first
implementable value. See
[creation provenance](spec/sessions-and-transcript.md#session-identity-and-creation-provenance).

## Transcript ancestry

The source frontier from which a session's initial semantic conversation context
was derived, or an explicit absence of such a source. See
[creation provenance](spec/sessions-and-transcript.md#session-identity-and-creation-provenance).

## TranscriptFrontier

The purpose-specific domain boundary identifying an immutable point in a source
session's semantic transcript history, referenced by transcript ancestry. See
[creation provenance](spec/sessions-and-transcript.md#session-identity-and-creation-provenance).

## Input delivery policy

The explicit instruction for handling user input relative to authoritative
session state: start, interrupt, next safe point, or after current turn. See
[occupied-slot input handling](spec/turn-lifecycle-and-scheduling.md#occupied-slot-input-handling).

## Applied interrupt proof

A causal value constructible only from the committed applied result of
`SubmitInput::Interrupt` for one exact predecessor turn, the sole baseline
authority for turn cancellation. See
[turn states and the active slot](spec/turn-lifecycle-and-scheduling.md#turns-states-and-the-single-active-slot).

## Runner

An outbound-connected process that declares capabilities and execution-boundary
properties, then performs selected runner-local tool attempts under one
deployment identity. Deferred design; see
[scheduling and runners](open-questions.md#scheduling-and-runners).

## Runner property evidence

The declared, configured, verified, and effective evidence distinctions used
when selecting and explaining a runner. Deferred design; see
[scheduling and runners](open-questions.md#scheduling-and-runners).

## Execution boundary

The actual identity and isolation properties of a runner deployment, such as OS
user, container, sandbox, and filesystem scope. Deferred design; see
[scheduling and runners](open-questions.md#scheduling-and-runners).

## Tool policy

Hub-owned evaluation that determines whether a specific logical tool request is
allowed, denied, or requires confirmation, plus any placement or constraint
decision. Deferred design; see
[tool safety](open-questions.md#tool-safety).

## Approval

A recorded human decision permitting or denying one exact logical tool request
as presented to the user. Deferred design; see
[tool safety](open-questions.md#tool-safety).

## Executor placement

The selected location for a physical tool attempt: a hub-local executor or a
runner-local executor on an identified runner. Deferred design; see
[scheduling and runners](open-questions.md#scheduling-and-runners).

## Known failure

A terminal physical outcome backed by adequate evidence that the intended effect
did not complete, or completed with a specific reported failure. See
[observation classification](spec/model-call-execution.md#provider-observation-classification).

## Ambiguous outcome

A physical outcome where available evidence cannot establish whether an external
effect occurred. See
[observation classification](spec/model-call-execution.md#provider-observation-classification).

## Reconciliation marker

The immutable payload of `TurnDisposition::ReconciliationRequired`, naming the
exact still-unacknowledged ambiguous operations and one typed stop reason. See
[turn states and the active slot](spec/turn-lifecycle-and-scheduling.md#turns-states-and-the-single-active-slot).

## Semantic transcript entry

One immutable identified semantic-history fact owned by a source session. See
[semantic transcript entries](spec/sessions-and-transcript.md#semantic-transcript-entries).

## Context frontier

A session-owned immutable snapshot resolving to the exact ordered-distinct
semantic-entry references consumed by one model call or fixed at turn start. See
[context frontier snapshots](spec/turn-lifecycle-and-scheduling.md#context-frontier-snapshots).

## Queue order

The durable total ordering of accepted-input-origin work derived from immutable
acceptance positions and typed priority relations. See
[eligibility derivation](spec/turn-lifecycle-and-scheduling.md#eligibility-derivation).

## Starting lineage

The immutable first-in-session or exact immediate-predecessor relation fixed for
an accepted-input-origin turn when it becomes eligible. See
[eligibility derivation](spec/turn-lifecycle-and-scheduling.md#eligibility-derivation).

## Session acceptance tail

The completeness witness carried by an evidence-bearing active-turn
reconstitution: one gap-free session-scoped interval of accepted inputs. See
[evidence-bearing reconstitution](spec/turn-lifecycle-and-scheduling.md#evidence-bearing-reconstitution).

## Session configuration defaults

A mutable-by-version session-level model-selection value used to resolve
configuration requests for future origin input. See
[session defaults](spec/sessions-and-transcript.md#session-defaults-and-replacement).

## Effective configuration

The complete immutable semantic configuration governing one turn, frozen from
the request and session defaults when its origin input is accepted. See
[model-selection validation](spec/configuration-and-credentials.md#model-selection-validation).

## Dispatch generation

A per-attempt monotonic ordinal identifying which scheduler dispatch may report
for a physical attempt, realized today as the dispatch gate. See
[staged execution](spec/model-call-execution.md#staged-execution).

## Transactional outbox

The append-only event rows written inside the transactions that commit
client-observable state, the sole path from a commit to an update event. See
[transactional outbox](spec/persistence-protocol.md#transactional-outbox).

## Update event

One durable-transition fact delivered to subscribers, produced only from the
outbox rows its committing transaction appended. See
[transactional outbox](spec/persistence-protocol.md#transactional-outbox).

## Subscription cursor

The opaque resumption token each durable update event advances, derived from the
outbox's monotonic commit-ordered sequence. See
[transactional outbox](spec/persistence-protocol.md#transactional-outbox).

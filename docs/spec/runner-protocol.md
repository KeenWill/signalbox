# Runner protocol and placement

This page specifies the implemented runner-protocol domain foundation as
verified against the implementing stack through PR #260
(`agent/runner-protocol-domain`); its durable Postgres representation and
restart-recovery authority were verified through PR #267
(`agent/runner-persistence`). It owns logical runner enrollment,
daemon-authoritative catalog validation, runner leases, session placement and
affinity, credential-profile grants, and workspace requirements. The tool
registry's common declarations remain owned by [tool loop](tool-loop.md);
session transcript and frontier mechanics remain owned by
[sessions and transcript](sessions-and-transcript.md); physical tool attempts
remain owned by [tool loop](tool-loop.md). Invariant tags cite
[the invariant test index](../invariants.md).

The executable runner build-out is a foundation proposal at the bottom of its
implementing stack. The domain and Postgres foundation above remain the verified
surface until the child pull requests named by that stack land with this diff.
Together they add the local runner wire, application orchestration, the
`signalbox-runner` binary, workstation tools, bubblewrap execution, workspaces,
and the offline proof specified below. Remote transport and dynamic policy stay
under [Open edges](#open-edges).

## Version-one executable boundary

Version one runs one `signalbox-runner` on the same Ubuntu host and under the
same effective user as `signalboxd`. Its closed runner tool families are:

- workspace file read, write, and exact edit;
- Git clone, fetch, branch creation, commit, and push;
- serial shell execution; and
- serial build/test execution.

The tool loop remains serial: the daemon offers at most one live lease for a
session, the runner executes at most one dispatch at a time, and one result
reaches a durable terminal attempt before the next dispatch. Version one has no
Mac runner, remote transport, concurrent execution, or MCP locus. The additive
wire types do not encode a same-host assumption, but no unused remote mechanism
or negotiation surface is designed.

## Local transport and connection protocol

`signalboxd` binds a dedicated runner Unix domain stream socket, distinct from
the client socket. `signalbox-runner` dials that socket and never listens. The
daemon applies the process socket's owner-private canonical parent, trusted
ancestry, exact `0600` node mode, sidecar exclusive `flock`, rename-resistant
path identity, effective-user peer check, and identity-safe cleanup discipline.
The runner verifies the connected socket's pinned effective-user ownership,
mode, and path identity before sending enrollment. The local effective user is
the version-one trust boundary; the opaque authentication-reference identity is
correlated with the stored enrollment but is not treated as a secret.

Runner-wire version one is newline-delimited UTF-8 JSON with a required
`version` and closed tagged message vocabulary. Each complete line, including
its newline, is at most 8 MiB. Unknown versions, message kinds, fields, enum
tokens, noncanonical identities, nonpositive generations, oversized frames, and
correlation mismatches fail closed. The runner supplies no policy declaration:
its first-enrollment message carries one stable request identity and one
availability-only advertisement, while resume carries the daemon-issued
enrollment, runner, and authentication-reference identities. The daemon
validates and commits enrollment and registration before returning their exact
identities and registration revision.

One connection then carries this serial state machine:

1. The daemon sends `lease_offer` with the complete lease correlation and
   immutable dispatch payload. The runner admits the exact tool, sandbox
   profile, credential profile, and workspace before replying `lease_claim`.
2. The daemon commits the exact claim before sending `lease_claimed`. Receipt of
   that acknowledgement is the execution capability held by the runner. Before
   accepting `dispatch`, the runner fsyncs the complete claimed correlation and
   `waiting_dispatch` phase below its private state root. An offer or sent claim
   without the acknowledgement never authorizes execution.
3. The daemon sends `dispatch`; the runner executes only when both the claimed
   capability and the dispatch have the same complete correlation. It fsyncs
   `dispatch_received` before acknowledging the frame internally and
   `execution_may_have_started` immediately before invoking the executor.
4. The runner retains one terminal success, known-failure, or ambiguous evidence
   envelope and resends it until the daemon durably commits the matching
   attempt/lease transition and replies `result_recorded`. Only then may the
   runner discard the evidence.

The closed version-one frame vocabulary is:

| Direction       | Frame                     | Required checked payload and effect                                                                                                                                                                                           |
| --------------- | ------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| runner → daemon | `enroll`                  | Enrollment-request id and complete advertisement. Admitted only as pristine enrollment or one provisioning-only pending replacement candidate.                                                                                |
| daemon → runner | `enrolled`                | Request id, issued enrollment/runner/authentication ids, registration revision, and accepted advertisement digest. Durable registration precedes send.                                                                        |
| runner → daemon | `resume`                  | Request id, all issued identities, complete current advertisement, prior registration revision, and bounded reconnect inventory.                                                                                              |
| daemon → runner | `resumed`                 | Current registration revision and one closed directive per inventoried item: resend, await, discard-as-recorded, or fail stale.                                                                                               |
| daemon → runner | `replacement_pending`     | Request id, issued candidate identities, provisioning-only registration revision, and advertisement digest. Heartbeat, startup reconciliation, and an owner-command-bound workspace operation are admissible; leases are not. |
| runner → daemon | `advertise`               | Complete replacement advertisement under the current identities and registration revision.                                                                                                                                    |
| daemon → runner | `registered`              | Exact new registration revision and advertisement digest after durable registration.                                                                                                                                          |
| daemon → runner | `heartbeat`               | Positive connection-epoch-local sequence and last accepted peer sequence.                                                                                                                                                     |
| runner → daemon | `heartbeat_ack`           | Same challenge sequence, monotonic runner sequence, and exact optional lease/workspace phase.                                                                                                                                 |
| runner → daemon | `workspace_leak_page`     | Registration revision, report digest, positive page, prior-page digest, final-page flag, and at most 64 sorted typed leak facts. Spool until acknowledged.                                                                    |
| daemon → runner | `workspace_leak_recorded` | Exact report digest, page, and page digest after durable page admission; the final acknowledgement publishes the complete startup report.                                                                                     |
| daemon → runner | `workspace_provision`     | Single-use provisioning authorization, session, placement revision, runner/registration, repository key, sandbox profile, and optional credential-profile name.                                                               |
| runner → daemon | `workspace_ready`         | Same authorization correlation plus complete provisioned-workspace manifest identity and bounded content digest. Spool until acknowledged.                                                                                    |
| daemon → runner | `workspace_recorded`      | Exact authorization and manifest correlation after durable receipt admission.                                                                                                                                                 |
| daemon → runner | `workspace_release`       | Exact retired session placement revision and workspace-manifest identity.                                                                                                                                                     |
| runner → daemon | `workspace_released`      | Same release correlation after manifest transition and symlink-safe removal.                                                                                                                                                  |
| daemon → runner | `lease_offer`             | Complete immutable lease and tool dispatch correlation, selected profile/grant, normalized arguments, and result bounds.                                                                                                      |
| runner → daemon | `lease_claim`             | Exact offered correlation after local tool/profile/credential/workspace admission.                                                                                                                                            |
| daemon → runner | `lease_claimed`           | Exact claimed correlation after durable claim commit; it is one half of execution capability.                                                                                                                                 |
| daemon → runner | `dispatch`                | Exact claimed correlation and immutable payload; receipt with `lease_claimed` authorizes one execution.                                                                                                                       |
| runner → daemon | `result`                  | Exact claimed correlation and one bounded success, known-failure, or ambiguous evidence envelope. Spool until acknowledged.                                                                                                   |
| daemon → runner | `result_recorded`         | Exact result correlation after atomic lease/attempt result commit.                                                                                                                                                            |
| either          | `shutdown`                | Current connection epoch and closed reason `daemon_shutdown` or `runner_shutdown`; creates no loss proof by itself.                                                                                                           |
| daemon → runner | `rejected`                | Offending frame kind, available correlation, and one closed code; no arbitrary peer text. Fatal codes close the connection.                                                                                                   |

An advertisement contains at most 16 capability classes, 256 tools, 64
credential-profile names, and 64 repository keys; names are sorted and unique.
Workspace and sandbox inventories are their closed vocabularies. A reconnect
inventory contains at most one lease, one unacknowledged terminal result, one
workspace operation, and one unacknowledged leak-report page because execution
and report delivery are serial. Digests are lowercase 64-character SHA-256 hex
over the canonical checked representation; they detect replay disagreement and
confer no authority. `rejected` codes are `unsupported_version`,
`malformed_frame`, `enrollment_conflict`, `enrollment_revoked`,
`registration_rejected`, `stale_connection`, `correlation_mismatch`,
`policy_rejected`, `workspace_conflict`, `runner_lost`, `unavailable`, or
`shutting_down`. Any frame outside the state shown by the table, a duplicate
with unequal canonical payload, or an acknowledgement without its durable
predecessor is fatal and advances nothing. Equal replay returns or resends the
same recorded answer.

After `enrolled`, `replacement_pending`, or `resumed`, startup reconciliation
sends one leak report before any workspace provision or lease offer is
admissible; heartbeats remain admissible throughout. A report is named by the
lowercase SHA-256 digest of its complete canonical sorted fact sequence and has
pages numbered from one. Each nonfinal page carries exactly 64 facts and each
final page carries zero through 64; even an empty report sends final page one.
Facts are sorted by their bounded repository-relative locator and contain only
`unknown_manifest`, `retired_present`, `manifest_conflict`, `cleanup_failed`, or
`unreconciled`, the locator, lowercase SHA-256 manifest-or-entry digest, and
nullable session and placement revision when independently parseable. Each page
carries the prior page digest, null only on page one, so equal replay is exact
and omission or reordering fails closed. The runner journals the report and
current page until `workspace_leak_recorded`. The daemon durably stages pages by
report and page, then atomically replaces the runner-status leak snapshot only
when it acknowledges the final page; an interrupted newer report never erases
the prior complete one. Reconnect inventory names the exact retained page and
resumes at its durable acknowledgement boundary.

Every message after `enrolled` or `replacement_pending` carries the exact active
or pending connection registration revision. Lease messages additionally carry
lease id, lease-lineage generation, runner, session, tool request, physical tool
attempt, issuing turn attempt, and tool dispatch generation. An acknowledgement
for another revision or correlation is stale evidence and cannot advance either
side (INV-021, INV-043).

The daemon sends a heartbeat challenge every five seconds. The runner replies
with its monotonically increasing heartbeat sequence and exact outstanding lease
phase. One missed acknowledgement fences new offers while the connection is
suspect. Three consecutive misses, fifteen seconds after the last accepted
acknowledgement, durably mark the connection lost. A reconnect before that
deadline resumes continuity and, if a `suspect` owner event was emitted, emits
the matching `connected` recovery event before offers resume; after `RunnerLost`
commits, the old identity cannot revive or clear it.

Reconnect repeats resume and advertisement, then exchanges a bounded inventory
containing at most the one serial outstanding lease, its fsynced local phase,
and retained terminal evidence. `waiting_dispatch`, `dispatch_received`, and
`execution_may_have_started` retain the complete lease and dispatch correlation.
The first two prove only that the journaled executor invocation had not started;
the last carries ordinary effect-class ambiguity. Canonical durable state
decides whether the daemon resends a claim acknowledgement, dispatch, or result
acknowledgement; advertisement and connection memory never recreate authority.
When no claim acknowledgement was issued, the daemon may commit the exact
no-execution proof before re-leasing. A complete reconnect inventory that omits
a daemon-recorded claimed lease cannot strand or repeat it: the daemon durably
marks that lease lost and applies its effect-class ambiguity law. A claimed
lease reported without a terminal envelope likewise follows its fsynced phase.
Rejected duplicate or stale frames are retained only as bounded operator
classification, never as domain evidence.

## Identity, enrollment, and registration

`RunnerId`, `RunnerEnrollmentId`, `RunnerAuthenticationId`, and `RunnerLeaseId`
are distinct UUID-backed domain identities. A runner identity is issued by one
logical enrollment and is not derived from hardware, a hostname, a network
address, or any other machine fingerprint. Persistent and short-lived runners
use the same identity law. A newly enrolled ephemeral runner receives a new
identity; reconnecting under an existing active enrollment retains the existing
one.

The runner creates one random enrollment-request identity on first startup and
atomically journals it below its private state root before connecting. The
daemon accepts `enroll` only from the checked same-user local socket, only while
no other active version-one enrollment exists, or as the one pending successor
after that enrollment connection has durably become lost, and only when every
advertised capability class is in its fixed allowed set. A pristine enrollment
atomically issues and persists fresh enrollment, runner, and
authentication-reference identities with the first active registration. A
successor request atomically issues the same identity shapes plus one checked
`PendingRunnerEnrollment` record and pending registration revision. Neither is
an active `RunnerEnrollment` or `ValidatedRunnerRegistration`; together they
carry provisioning-only authority: it admits heartbeat, startup leak
reconciliation, and a workspace operation bound to one already-claimed owner
replacement command, but never registration mutation, grant creation, lease
offer, claim, or dispatch. At most one pending request exists. Exact replay of
either request identity returns the same issued identities and registration
result; a third request conflicts rather than replacing either authority. The
runner atomically journals the returned receipt before treating enrollment or
pending replacement as complete.

For a pinned repository-backed loss, `replace_lost_runner` first durably claims
the owner command and its complete request, then creates one single-use
provisioning authorization naming that command and pending registration. The
runner provisions and spools `workspace_ready` under that limited authority.
Only a later transaction can activate the pending enrollment: it rechecks the
lost predecessor and connected candidate, consumes the exact workspace receipt,
revokes the predecessor, constructs the active enrollment and validated
registration from the exact pending facts, and installs the successor placement,
grant, semantic boundary, and terminal command result atomically. Pre-pin
replacement needs no workspace and performs that promotion in its single
terminal transaction. A provisioning rejection or candidate loss records the
typed terminal command rejection, retires only that command staging workspace
through the normal release/trash path, and leaves the candidate pending for an
explicit later command. Process exit after command claim is recoverable: startup
resumes the one nonterminal replacement command from its durable provisioning
authorization and receipt rather than inventing a second claim. No database
transaction remains open across runner I/O.

Later connections send `resume` with the request identity and all three issued
identities. Stored mismatch, revocation, or a second live connection for the
same registration fails closed. A cleanly superseded connection epoch becomes
stale before the new one may dispatch. No application credential, bearer token,
or proof exchange exists in the same-host version; remote authentication remains
a separate open decision.

Why: the daemon issues and durably owns logical enrollment authority while the
runner retains a stable idempotency fact for crash recovery. Treating the opaque
authentication-reference id as a secret would provide no authentication against
another process running as the same trusted user and would falsely imply a
remote-ready handshake.

One `RunnerEnrollment` binds the enrollment identity, runner identity, opaque
authentication-reference identity, and owner-allowed capability classes. The
authentication reference identifies daemon-resident enrollment policy; it is not
an authentication secret. Enrollment is either active or revoked. Revocation is
terminal and makes later registration invalid. Complete reconstitution rejects
mismatched enrollment, runner, authentication, allowed class inventory, optional
last issued registration revision, or lifecycle state rather than repairing it.
Durable revocation commits first and then flips the exact caller-held
enrollment-shared active fence, so an existing validated registration becomes
non-current for later leases, reconciliation, runner replacement, or grant
replacement. A failed durable revocation leaves that caller-held fence active. A
lease offer rechecks the active enrollment and its exact enrollment, runner, and
authentication-reference correlations; a lease already offered is unaffected.
The Postgres admission trigger locks the current enrollment and placement heads
before accepting even a direct lease-row insert, so concurrent revocation,
runner loss, or runner replacement wins before the stale offer can commit. Both
the current and audit allowed-class inventories reject row mutation and
statement-level truncation, including cascading truncation.

A registration carries availability claims only:

- the runner's advertised capability classes;
- tool names;
- credential-profile and repository-key names; and
- workspace and sandbox-profile capabilities.

It carries no permission default, effect class, placement declaration, approval
posture, credential path, or credential value. Registration validates classes,
tools, workspace capabilities, and sandbox profiles against enrollment and the
daemon catalog. Credential-profile names are checked, duplicate-free
availability facts from strict runner configuration; the owner selects one exact
advertised name, while daemon-owned profile and override policy decide approval
independently of that name. A disallowed catalog claim or malformed name rejects
the complete registration. A valid registration retains the exact advertised
subset and attaches daemon-authoritative tool and sandbox declarations.
Preparing a registration also takes the enrollment-shared exclusive preparation
fence: a second preparation while one is outstanding fails typed, and the fence
releases when the prepared registration commits or is abandoned. The persistence
adapter stages this checked registration, commits its complete durable rows and
current head, and only then advances the enrollment-owned current registration
revision; the held fence means no concurrent registration can advance that
revision between staging and this post-commit advance, so a successful durable
write is never reported as a failed registration, while a failed durable write
leaves the prior registration current. Enrollment persistence admits only a
pristine active enrollment that has issued no registration; inserting one whose
caller-held authority already advanced would reload with no issued revision and
disagree with canonical storage on every later registration. Complete enrollment
reconstitution restores the optional last issued revision from independently
matching stored facts; the next successful registration advances it instead of
reusing a prior revision. A persistence load supplied with caller-held
enrollment authority compares that authority's current registration revision
with the independently loaded canonical enrollment before binding any historical
registration to its revision fence. Retained copies of every prior registration
become stale and cannot authorize a later lease, reconciliation, or
grant-bearing placement transition. Omitting a formerly advertised capability
removes its availability from the new registration, but never changes its
daemon-side policy. A pinned session never inherits additions from
re-registration. If a new registration omits a runner-required capability in
that session's pinned snapshot, no later lease is authorized; an explicit
registration-reconciliation transition marks the placement `RunnerLost` without
rewriting its snapshot. Omitting a combined-locus tool disables runner dispatch
for that tool but retains placement so daemon fallback remains admissible. Why:
re-registration can narrow current availability without downgrading a
confirmation requirement, widening authorization, or silently changing
established affinity (INV-042, INV-044).

## Advertised catalogs and daemon authority

Capability-class names are exact, bounded catalog keys. Credential-profile and
repository-key names use the same checked syntax but are runner-configured
availability keys. Construction rejects empty values, U+0000, values longer than
64 UTF-8 bytes, and bytes outside ASCII letters, digits, dot, underscore, and
hyphen. A name must begin with an ASCII letter or digit. Workspace capability is
closed vocabulary; the implemented arm is `WorktreePerSession`.

The version-one allowed capability-class set has exactly one member,
`workstation-v1`. The compiled workstation registry places every one of its ten
runner-only declarations in that class, and `signalbox-runner` advertises it
exactly. Runner configuration cannot add, remove, or rename a capability class.
Exact-runner session selectors remain admissible; a class selector uses the
literal `workstation-v1`.

Sandbox profile is also closed vocabulary: `WorkspaceRestricted`
(`workspace-restricted` on configuration and wire surfaces) and `Ambient`
(`ambient`). Both must be explicitly advertised before a session can select
them. The daemon owns each profile's fixed meaning and approval defaults; an
advertisement asserts availability only (INV-042).

One `RunnerCatalog` domain value contains allowed capability classes, complete
runner-tool declarations, allowed workspace capabilities, and the two fixed
sandbox-profile definitions. The persistence adapter owns this catalog
independently of stored registration rows. Registration reconstitution compares
every stored class, tool declaration, workspace capability, and sandbox profile
with that trusted catalog and rejects any difference; stored declarations cannot
bootstrap their own authority. Duplicate names or an internally inconsistent
placement declaration rejects the complete catalog. Credential and repository
names remain exact availability inventories recorded by that registration and
acquire no policy by being stored.

The version-one daemon composition constructs this value once from the compiled
workstation registry, the exact `workstation-v1` class, and fixed profile
definitions; it has no runner-catalog file or reload path. The runner
independently derives its advertised class and tool names from the executors
compiled into that same binary and its credential/profile names from checked
runner configuration. Exact registration validation detects any disagreement.
Dynamic catalog revisioning and reload remain deferred rather than being
approximated by process-local mutation.

Each `RunnerToolDeclaration` contains:

- the existing checked `ToolName`;
- one checked `RunnerToolModelDefinition`, containing a nonempty bounded
  model-facing description and a canonical JSON-object argument schema;
- one required `ToolPermissionDefault`;
- one required `RunnerToolEffectClass`; and
- one nonempty `ToolAdmissibleLoci` value.

The declaration permission remains model-definition and daemon-locus
compatibility metadata. It never authorizes a runner attempt: runner approval is
derived only from the pinned sandbox profile and exact override, then
snapshotted into the grant. A combined-locus declaration must still match the
daemon-local default so fallback cannot change its advertised definition
silently.

`ToolAdmissibleLoci` is closed typed vocabulary:

- `DaemonOnly`;
- `RunnerOnly { selector }`; or
- `DaemonOrRunner { selector }`.

A runner selector is either one exact `RunnerId` or one `RunnerCapabilityClass`.
When both loci are admissible, the domain retains daemon-local admissibility if
the attached runner does not currently advertise the tool, while runner lease
creation fails `ToolUnavailable`. It does not transfer the consumed runner
authorization or credential-profile grant to daemon execution. The application
orchestration selects the locus before authorization; a change to daemon
fallback discards runner-pair authority and resolves the daemon-local tool
policy without the runner-resident profile. Placement is immutable declaration
metadata, not a per-call choice supplied by a runner or model. An MCP locus is
not part of the vocabulary.

`RunnerToolDeclaration` is the one daemon-authoritative runner-dispatch
declaration. Every runner-advertisable tool therefore has model-facing
description and schema authority even when daemon execution is inadmissible. The
current daemon-local application `ToolDefinition` is a compatibility
representation, not a second source of policy. The application adapter compiles
argument validation from the runner declaration's exact schema and rejects a
shared name unless model-facing definition and permission are equal and the
local effect maps exactly (`EffectFree` to `Pure`, `ExternalEffect` to
`SideEffecting`). `Idempotent` has no current daemon-local projection, so a tool
with that effect cannot include the daemon locus until the representations are
consolidated.

Advertisement validation never synthesizes a declaration for an unknown tool.
Unknown tools, capability classes, workspace capabilities, and sandbox profiles,
or malformed credential-profile names reject the complete advertisement. A
daemon-only tool or a runner tool whose declared identity-or-class selector the
advertisement does not satisfy also rejects the complete advertisement. The
resulting `ValidatedRunnerRegistration` exposes only exact advertised
availability paired with daemon-owned policy. A runner can therefore neither
self-widen its tool surface nor replace confirmation with automatic approval
(INV-042).

## Effect classes and runner leases

Every tool declaration has exactly one effect class and there is no default:

- `Pure` performs no externally visible state change and is idempotent;
- `Idempotent` may change state but is safe to repeat; or
- `SideEffecting` may have changed state and is not known safe to repeat.

An undeclared runner tool never receives a lease because advertisement
validation rejects it. Where a later boundary must classify an untrusted
declaration before validation, it treats the tool as `SideEffecting`; that
fail-closed adapter behavior is not a fourth domain effect class.

A `RunnerLease` binds one lease identity, exact tool name, complete authorized
physical-attempt dispatch correlation, session, runner, effect class, and
positive lease-lineage generation. Lease creation is not a free constructor: it
consumes one `RunnerToolAttemptAuthorization`, which binds the approved request
and its exact tool name to the tool loop's `AuthorizedToolAttempt`. Only
`ToolBatch::authorize_runner_attempt` and `ToolBatch::resume_runner_attempt`
publicly produce that pairing: each selects the batch's canonical immutable
request and approval together with its physical-attempt authority.
`RunnerToolAttemptAuthorization` has no public raw-parts constructor. The
underlying attempt exists only after the automatic or owner decision authorizes
that exact attempt, and neither authority nor the resulting lease is cloneable.
Every checked `ToolBatch` carries a durable per-attempt inventory of runner
authority already issued. Its in-memory clones share the exact atomic guard for
each physical attempt. The persistence loader derives the active batch's
consumed inventory from exact current physical attempts already bound by durable
runner lease generations, and complete reconstitution restores every consumed
guard from that inventory. A stored retryable claimed loss leaves its source
attempt in flight, so a reloaded batch still carries the exact live source the
checked claimed replacement transition requires; the predecessor leaves the
current-attempt view, and enters the batch's restored retired-identity
inventory, only once the atomic replacement commit retires it to terminal
history. A reloaded batch therefore keeps rejecting retired attempt-identity
reuse in the domain rather than at the retained row's key. Atomic runner
authorization marks that exact attempt issued in the batch; a later clone or
reconstitution from the updated facts cannot mint a second runner lease
capability. Current active enrollment, pinned placement, its exact validated
registration, and any selected active credential grant jointly authorize every
offer after the first. The initial offer instead creates that pinned placement,
any selected grant, and generation-one lease in one checked transition from
`Unpinned`; it does not require those products to exist beforehand. The request,
attempt, session, and two-way crash class must match the selected tool,
placement, and declaration-derived effect class (`Pure` to `EffectFree`;
`Idempotent` or `SideEffecting` to `ExternalEffect`). Revoked enrollment, lost
placement, or a mismatched runner, request, tool, attempt, effect, profile, or
grant cannot create a lease.

When a credential profile is selected, the lease also retains the exact
immutable `CredentialDispatchAuthorization`: session, runner, profile, grant
revision, tool, and resolved pair posture. Grant replacement or revocation
therefore cannot erase which snapshot authorized an already offered lease.

A lease begins `Offered` at lease-lineage generation one. Only the exact lease,
runner, tool, authorized physical-attempt correlation, and lineage generation
may claim it, producing `Claimed`; only that same correlation may complete it.
Completion is terminal. A stale or cross-wired correlation cannot advance the
aggregate. Complete reconstitution accepts only the closed state shapes and
exact correlations, and requires the opaque validated registration whose
daemon-owned declaration independently confirms the stored runner, tool, and
three-way effect class.

`LostUnclaimed` requires an opaque durable no-execution proof bound to the exact
lease correlation; complete loss reconstitution requires the same proof. The
proof exposes no public raw-parts constructor, so an offered lease and its
public correlation cannot mint this authority. The persistence adapter may
reconstitute it only by comparing the complete stored proof correlation with the
independently loaded lease correlation through the checked reconstitution input.
The local transport is the independently authoritative producer. Its claim
transaction serializes against the current connection/loss head and commits the
exact claim before acknowledging it; only that acknowledgement can enable
execution. When loss wins first, the fenced connection epoch plus the durable
absence of a claim proves that no execution capability was issued, and the same
transaction records `LostUnclaimed` with its exact proof. When claim wins first,
the durable lease is `Claimed` even if acknowledgement delivery is uncertain and
loss follows execution-possible law. Mere absence of a frame in process memory
is never proof. The Postgres representation commits the proof atomically with
the lost-unclaimed event and requires it before a successor generation can
consume that retry path. Every retryable loss admission — lost-unclaimed, whose
proof-backed retry reissues the never-executed attempt for every effect class,
and claimed pure or idempotent loss — reads its source attempt under a row lock
and requires it to still be in flight, so a concurrent terminal attempt update
serializes with the loss instead of racing past that live-source check. Claimed
retry instead requires a durable record binding the complete lost lease
correlation to the exact fresh physical-attempt dispatch. That record is an
idempotent reservation: if a process exits before the replacement attempt is
stored, recovery loads the exact reserved dispatch and may replay only that
replacement. One transaction retires the in-flight source attempt to its
effect-correct terminal history and commits the fresh replacement attempt with
its successor lease generation, and the Postgres representation rejects a
reserved replacement attempt committed without that successor generation, so the
durable claimed-retry states are exactly the loss over its still-in-flight
source — with or without the replayable reservation — and the complete consumed
retry whose successor lease is already offered; a crash can strand neither a
consumed one-shot preparation fence without its successor lease nor a retired
source without its replacement, and a different replacement cannot overwrite the
reservation. Without the proof, losing even an `Offered` lease conservatively
follows the execution-possible law: pure or idempotent work requires a fresh
physical attempt, while side-effecting work requires crash classification.

With that proof, loss before claim permits every effect class to be re-leased at
the checked successor lease-lineage generation. Loss after claim follows the
required retry law:

- `Pure` and `Idempotent` produce typed re-lease authority at the checked
  successor generation. After claim, that authority consumes the owning checked
  `ToolBatch`, retires the prior in-flight attempt to its effect-correct
  terminal history, installs and authorizes a fresh physical `ToolAttemptId`,
  retains every retired attempt identity in the updated batch and its complete
  reconstitution facts, and returns both attempt records. Before claim, the
  authority instead consumes the owning checked batch, verifies its complete
  original dispatch, and reissues the never-executed attempt through a fresh
  single-use durable runner-issuance guard. Only the private evidence produced
  by the applicable batch transition can authorize the re-lease. Preparing
  either retry consumes the loss authority's one-shot preparation fence, and
  complete reconstitution restores its durable consumed state. For an unclaimed
  retry, the predecessor guard moves one-way from issued to retired while the
  returned batch installs a distinct successor guard; retained batch copies
  cannot reopen the predecessor, and retry-marked authority is rejected by the
  ordinary offer transition; and
- `SideEffecting` produces typed crash-classification authority whose physical
  attempt is derived from the opaque lost lease and never produces re-lease
  authority.

Generation exhaustion, reuse of any current or retired attempt identity for
either claimed replacement or ordinary preparation, and a standalone
same-request authorization for claimed retry all fail closed. A claimed
replacement must retire the complete original session, turn, issuing turn
attempt, request, physical attempt, dispatch generation, and effect fence. Its
private evidence retains the complete source lease correlation and complete
replacement dispatch; the re-lease transition compares both without truncating
them to shared request or attempt identities. `RunnerLeaseLoss` has sealed
construction, so only `RunnerLease::lose` or `RunnerLease::reconstitute_loss`
over an already-lost checked projection can produce retry or
crash-classification authority. Re-leasing continues one logical tool request
and lease lineage. Its successor `RunnerGeneration` is distinct from the fresh
physical attempt's `ToolDispatchGeneration`, which starts at `first()` under the
tool-loop law. Every repeated physical execution therefore has its own attempt
identity and record as required by INV-004. Side-effecting loss composes with
the existing physical-attempt ambiguity machinery; this domain slice does not
duplicate or overwrite that attempt's outcome (INV-004, INV-025, INV-026,
INV-043).

The lease aggregate contains no channel handle or process-local connection
state. A reconnecting registration cannot recreate, complete, or discard a lease
from an advertisement. Why: the held streaming channel is transport, not lease
or claim authority. The application repository commits claim, terminal result,
and lease state together; the wire adapter projects only from those durable
facts during reconnect.

## Session placement and affinity

`SessionRunnerPlacement` starts with one request that is immutable between
explicit replacement transitions:

- a `RunnerSelector`, targeting a capability class or exact runner identity;
- `WorkingDirectorySelection`, either runner default or one exact bounded
  working-directory value;
- an optional `CredentialProfileName`;
- `WorkspaceRequirement`, either none or a repository worktree;
- one exact `RunnerSandboxProfile`, defaulted to `WorkspaceRestricted` only at
  the owner/client construction boundary and always explicit in the domain; and
- one bounded map of exact tool names to
  `RunnerToolPermissionOverride::{Auto, Confirm}`.

The override map has at most 64 entries, rejects duplicate or undeclared tool
names as one invalid request, and is copied into the durable placement snapshot.
It is session policy rather than runner advertisement: a runner cannot add,
remove, or reinterpret it.

The working-directory value is exact nonempty UTF-8, excludes U+0000, and is at
most 4,096 bytes. The domain does not apply host-platform path parsing. A
repository-worktree requirement carries one exact repository key with the same
nonempty, U+0000-free, at-most-4,096-byte contract.

Before execution, placement is `Unpinned`. Mere attachment does not pin it. A
placement without a repository requirement can pin in the first atomic dispatch
transaction. A repository placement first requires one checked, single-use
`WorkspaceProvisioningAuthorization`. The daemon derives it only after
validating the candidate runner and registration; it binds the session,
placement revision, runner, registration revision, repository key, sandbox
profile, and optional credential profile. It authorizes only acquisition of that
repository and no model-selected tool. The runner rejects an unknown credential
profile before accepting the authorization and returns one
`ProvisionedWorkspace` receipt whose manifest facts match every correlation.

The first dispatch transaction atomically consumes the workspace authorization
and receipt when present, consumes the exact tool-attempt authorization,
validates the placement request against the same current registration, installs
`Pinned` state, creates any initial credential grant, and stores the offered
lease. A crash can therefore leave either retryable provisioning evidence or the
complete pin/grant/lease boundary, never an in-flight tool attempt without its
lease. The pinned state contains the runner, selected working directory,
credential-profile selection, tool inventory, runner-required tool inventory,
provisioned workspace, sandbox profile, and exact permission overrides. Ordinary
attachment and lease creation accept only that exact runner and current grant.
Re-registration or reconnect changes none of these facts, and there is no
automatic migration or class-based rescheduling (INV-044, INV-045).

For `RepositoryWorktree`, the provisioned workspace's working directory is the
selected execution directory. Attachment and reconstitution reject a provisioned
directory that differs from either the recorded selected directory or an exact
requested directory.

Re-registration never mutates this snapshot. Additions remain unavailable to the
pinned session until an explicit replacement. Omission of the pinned selector
class, credential profile, runner-only tool, or workspace capability makes
dispatch validation fail and the checked reconciliation transition converts the
placement to `RunnerLost`. Omission of a combined-locus tool makes only runner
dispatch for that tool unavailable: reconciliation retains the pinned placement
and daemon-local fallback remains admissible. An availability-equivalent
registration leaves the placement unchanged.

Runner loss is explicit state, not implicit reassignment. Marking a pinned
runner lost retains the prior placement and disables future lease creation.
Marking an exact-identity request lost before pin retains that request and
records `RunnerLostBeforePin { runner }`, disabling eligibility and initial pin.
An owner-directed pinned replacement supplies and installs a new complete
placement request, validated registration, working directory, credential-profile
selection, tool inventory, and provisioned workspace. It advances a positive
placement revision and returns one `RunnerPlacementChange` value carrying the
complete before-and-after placement requests and pinned facts needed for the
frontier-extending injected message. When credential grant authority changes,
the same result also carries complete before-and-after grant reconstitution
facts, including the prior narrowed tool inventory and successor inventory. A
pre-pin replacement instead returns a checked `RunnerPrePinReplacement` carrying
the lost exact identity and before-and-after requests; the successor is ordinary
`Unpinned`, and no pinned facts or semantic placement change exist.

Every replacement runner must differ from the lost runner and be currently
registered on a live connection. Pinned replacement provisions a fresh workspace
at the successor revision; pre-pin replacement provisions nothing until eventual
initial dispatch. Reconnect of the lost identity cannot consume either
replacement transition or clear a lost state. Safe retry authority exists only
for a pinned lost runner and can be consumed only as part of its owner
replacement; it never causes automatic dispatch.

Reconstitution accepts a complete public raw-facts input and rejects ordinary
`Unpinned` above revision one unless append-only history proves an exact
lost-before-pin owner replacement into that revision. It rejects
`RunnerLostBeforePin` unless the request selector is the retained exact runner.
It also rejects a pinned or pinned-loss state that does not match its current
request and validated capabilities, and any stored credential-grant lineage
whose revision is newer than the placement revision. A profileless placement
with retained lineage additionally requires the exact terminally revoked grant
tombstone for that session, runner, and revision; an omitted, active, or
cross-wired tombstone fails closed. Durable replacement-history verification is
enforced by the persistence projection described below. Pinned or pinned-loss
reconstitution validates against the exact registration snapshot that produced
the pin and rejects any stored tool or runner-required-tool inventory that
differs from that checked result. A current narrowed re-registration is
reconciled separately and is not substituted for that historical snapshot. This
domain aggregate accepts every positive placement revision because each is
reachable through checked successor transitions.

The store retains append-only created, pinned, runner-lost-before-pin,
pre-pin-replaced, runner-lost, runner-replaced, abandoned, and profile-replaced
records behind one current pointer. A profile-replaced record carries the pinned
registration snapshot forward even though the replacement was validated against
the enrollment-owned current revision, so an availability-equivalent
re-registration cannot make profile replacement undurable. Relational transition
checks require contiguous event history, exact revision succession, unchanged
affinity facts at runner loss, profile-only changes for profile replacement, and
each stored tool's runner-required flag to match its declaration's runner-only
or combined locus. Storing either replacement event revalidates, under the
enrollment row lock in the committing transaction, that the supplied
registration's enrollment remains active and that its revision remains the
enrollment-owned current registration, so a replacement prepared before a
concurrent revocation or re-registration is rejected instead of installed as
stale authority. Every appended record advances the current-placement head in
the same transaction. Reconstitution reads the current record with its exact
validated registration and tool inventory. The loaded persistence wrapper
retains that historical registration and its durable revision so a caller can
reconcile against newer availability and persist `RunnerLost` without
reconstructing or guessing the pinned evidence.

Runner loss is an application-visible typed session state. A pinned placement
becomes `RunnerLost`; an unpinned placement whose exact-identity selector names
the lost runner becomes `RunnerLostBeforePin { runner }`. An unpinned
capability-class request has selected no runner and is unaffected. Once durable
loss commits, only two owner commands can leave either lost state. For pinned
loss, `replace` names a different live runner or atomically activates the one
pending replacement enrollment, then commits the checked successor placement,
grant lineage, semantic `RunnerPlacementChanged` transcript entry, and next
context frontier atomically. The entry is reference-only and contains no
credential value or unbounded runner output. Replacing `RunnerLostBeforePin`
updates the exact selector and returns to `Unpinned` at the successor revision
without fabricating a semantic boundary, workspace, grant, or lease. `abandon`
requires the exact current lost placement and an empty active-turn slot, then
installs terminal `RunnerAbandoned` placement state. An active turn must first
finish the existing stop, approval-decision, or reconciliation flow; abandonment
has no cancellation proof and cannot end a turn. An idle or queued-only session
fabricates no turn or frontier and later exposes only daemon-executable tools.
It creates no successor turn and never rewrites an issued side effect as known.
Neither command can target an ordinary unpinned, live, stale, or
already-replaced placement (INV-026, INV-029, INV-037, INV-044).

## Sandbox profiles and approval

The sandbox profile is an immutable placement fact and appears in every session,
lease, dispatch, result, evidence, transcript, and owner-inspection projection.
A client that omits it receives `WorkspaceRestricted` at the client construction
boundary; the domain and wire always carry the selected value. Changing profiles
requires the same explicit replacement frontier as changing runners.

For `WorkspaceRestricted`, the runner launches every executable tool as a fresh
bubblewrap process. It unshares user, mount, PID, IPC, UTS, cgroup, and network
namespaces; drops capabilities; clears inherited environment; mounts fresh
`/proc`, `/dev`, `/tmp`, and runtime directories; binds only the exact session
repository read-write; and binds configured toolchain and cache allowlist paths
read-only. Repository provisioning uses the same profile before publication: it
binds only the authorized empty staging repository at the fixed guest workspace
path, uses a per-provisioning broker socket, injects only the selected helper
credential, verifies the resulting clone, and atomically publishes it. The
runner refuses restricted registration when the installed bubblewrap cannot
prove the required namespace and bind behavior. File tools use
descriptor-relative traversal beneath the repository and refuse symlinks, magic
links, device nodes, sockets, and path escape. Writes replace a sibling
temporary file atomically. Shell and build/test tools receive no host path that
was not bound into their namespace.

The restricted network namespace has no host interface. A namespace-local shim
connects through one per-dispatch Unix socket to a runner-owned HTTPS broker.
The broker accepts only `CONNECT` to port 443, checks the requested hostname
before resolution, pins the resolved destination for that connection, parses the
TLS ClientHello, and requires its SNI to equal the admitted hostname. Version
one admits the `github.com` and `crates.io` hostname suffixes and the exact
`api.anthropic.com` hostname; runner configuration may remove entries but cannot
add another. CONNECT authorities are canonical ASCII DNS names: lowercase, no
trailing dot, no empty label, and no IP literal. A suffix match is
label-boundary exact: the host equals the base or ends in `.` plus the base, so
`notgithub.com` never matches `github.com`. Resolution rejects unspecified,
loopback, private, link-local, multicast, and otherwise nonpublic destinations
before pinning. Direct IP destinations, plaintext forwarding, other ports, DNS
rebinding, and missing or mismatched SNI fail closed. The broker proves a TLS
tunnel to the checked host; it does not claim visibility into the encrypted
application protocol.

For `Ambient`, the runner still uses one labeled bubblewrap supervisor but binds
the invoking user filesystem and shares host networking. It therefore provides
process supervision, not confinement, and every surface says `ambient`; no
surface calls it sandboxed. Full user powers include read access to every
same-user-readable path, including ungranted runner credential files and daemon
model-provider credential files if their paths are discoverable. The rule that
model-provider keys never reach the runner means the daemon has no runner-wire
or environment-injection path for them; it is not a filesystem-confidentiality
claim under `ambient`. Explicit profile selection accepts that exposure. A
runner has one global execution permit, so even different sessions cannot
execute tools concurrently in version one.

Runner-tool approval resolves in this order after locus selection and exact
argument validation:

1. an exact per-tool session override supplies `Auto` or `Confirm`;
2. otherwise `WorkspaceRestricted` supplies `Auto` for every version-one tool;
3. otherwise `Ambient` supplies `Auto` for `Pure` and `Confirm` for `Idempotent`
   or `SideEffecting`.

The frozen dangerous blanket remains daemon-local authority and cannot authorize
runner dispatch. A required confirmation is bound to the exact physical tool
attempt through the existing decision flow. Profile or override policy cannot
make a tool available, change its effect class, alter its arguments, or move its
locus. The resulting decision provenance and selected profile are retained with
the authorization, lease, and terminal evidence.

## Credential profiles and approval

A credential profile has two deliberately separate representations:

- the runner holds the profile credential value, provisioned out of band; and
- the daemon holds only its checked name, selection, policy, grant, and audit
  facts.

No credential-value control field exists in the runner-protocol domain.
Advertisements, registrations, placement, grants, leases, replacement changes,
and reconstitution inputs can carry only `CredentialProfileName`. Model-provider
credentials never enter runner configuration, wire state, or the injected
execution environment (INV-035). Explicit `ambient` may still read same-user
files through its full filesystem powers and carries no contrary confidentiality
guarantee.

For version one, the daemon compiles each exact tool/profile posture in a grant
from the selected sandbox profile and session override: `Auto` becomes
`Automatic`, while `Confirm` becomes `SessionPolicy`. The grant snapshot is the
durable effective posture; the runner advertisement never supplies it. An absent
pair, stale grant, or posture whose decision provenance does not match fails
closed. Profile policy cannot make an undeclared tool available and cannot alter
its effect class or admissible loci.

Before claiming a lease, the runner resolves the granted profile name against
its checked configuration. An unknown name rejects admission. For each dispatch
it opens the configured credential path without following symlinks, requires a
regular effective-user-owned file with exact `0600` mode, reads a bounded value,
and removes trailing `\n` and `\r`. The runner injects the value only under the
configured environment name inside the execution namespace; it is absent from
argv, wire state, manifests, and logs. Exact-value output redaction limits
accidental echo but is not a claim that arbitrary model-controlled execution
cannot misuse a credential within its scope.

Session creation records the requested profile as a placement axis. The pinning
transition snapshots the selected profile and validated advertised tool set into
one `CredentialProfileGrant`, because only then are the exact runner and
availability known. The grant binds the session, runner, profile, and positive
grant revision. A runner that did not advertise the profile cannot receive the
grant. Lease creation requires the current active grant, the same pinned runner
and profile, a tool present in the snapshot, and consumption of the exact
`RunnerToolAttemptAuthorization` produced after approval resolution and bound to
that tool. The grant records the exact tool/profile posture without issuing a
reusable standalone dispatch token.

Grant replacement is forward-only. It requires the supplied validated
registration to remain the enrollment-owned current revision, checks the current
grant revision, and installs one complete later snapshot. A retained stale
registration cannot mint a successor grant after later policy has been attached.
The result carries a `CredentialProfileChange` with the before-and-after profile
and tool inventories for later frontier injection. Runner replacement applies
the same current-registration gate, consumes the exact last-grant runner and
revision carried by the pinned placement, and creates a checked successor
revision. A profileless replacement carries both that placement evidence and the
lineage forward as a new terminal tombstone; omitting the tombstone is therefore
structurally rejected, and restoring a previously selected profile cannot
recreate revision one. Every prior revoked revision remains terminal. Every
successor binds its predecessor through the immediately prior placement's exact
runner and grant revision. Each independent lineage also carries the immutable
event ordinal of its revision-one placement through grant, audit, placement, and
lease references, so repeated equal runner and revision numbers within one
session cannot cross-wire provenance. Revocation is also forward-only and gates
later lease creation. A lease already offered is already dispatched and
completes or crash-classifies normally; revocation neither rewrites nor cancels
it. A revoked grant revision cannot become active again. Complete reconstitution
accepts a complete public raw-facts input, checks an independently authoritative
expected session, and rejects foreign runner facts, a profile absent from the
validated registration, or a tool set wider than the advertisement. Grants
created by initial pin or runner replacement contain the complete validated
registration tool inventory; explicit profile replacement may select a checked
subset. The store retains normalized grant snapshots and append-only issued,
replaced, and revoked audit events. Grant relations contain only profile names,
tool names, pair approval posture, and typed audit correlations: there is no
credential-value or generic payload column. A stored grant preserves an explicit
profile approval exactly; only a genuinely absent policy pair may use the
session-policy fallback. Truncation of immutable grant audit evidence is
rejected. Lease insertion joins the current unrevoked grant and exact
tool/profile pair atomically with dispatch authorization. Durable admission
requires provenance matching the effective profile/override posture: `Automatic`
only for `Auto`, and an exact owner confirmation for `SessionPolicy`. The
daemon-local dangerous blanket is never accepted for runner insertion, including
a direct lease-row insert (INV-035, INV-045).

## Workspace provisioning and recovery

`WorkspaceRequirement::RepositoryWorktree` is satisfiable only when the selected
validated registration advertises `WorkspaceCapability::WorktreePerSession` and
the repository key resolves in checked runner configuration to a credential-free
HTTPS clone URL. The provisioned workspace binds the session, placement
revision, runner, repository key, exact clone URL identity, sandbox profile, and
working directory. Its cleanup owner is structurally the runner that provisioned
it; no daemon-cleanup alternative is constructible. Replacement always uses the
successor placement revision and cannot carry the prior workspace forward.

The runner opens one effective-user-owned real `0700` root without following its
final component, pins and retains its directory identity, and holds a
process-wide exclusive lock through that dirfd. Every state, staging, session,
and trash traversal is descriptor-relative beneath it with no symlink or magic
link traversal. A complete workspace lives at
`sessions/<canonical-session-uuid>/<placement-revision>/repo`, including its own
`.git`; no shared Git directory, linked worktree administration, user home path,
or credential-bearing remote URL is used. Provisioning creates a sibling staging
directory, performs the clone through the restricted profile and selected PAT,
writes a versioned `0600` non-secret manifest in the non-mounted placement
parent, fsyncs the manifest and containing directories, and atomically renames
the prepared placement directory before returning `ProvisionedWorkspace`. Exact
replay returns the matching ready receipt; conflicting facts fail closed.

The manifest records lifecycle, session, placement revision, runner, repository
key, the lowercase SHA-256 digest of the configuration-validated canonical clone
URL, credential-profile name, sandbox profile, relative repository path, and the
bounded commit or branch facts needed for recovery. The canonical URL is
credential-free, but its digest is sufficient identity and avoids repeating the
operator configuration value. Recovery resolves the repository key again and
requires the current canonical URL digest to equal the protected manifest value;
a changed mapping is `manifest_conflict` and can never reinterpret an existing
clone. The writable repository `.git/config` is not authority. The manifest
records no credential path or value. The same runner state root durably spools
one unacknowledged terminal result per the serial wire protocol.

A daemon release is accepted only for an exact retired placement revision —
either superseded by replacement or terminal `RunnerAbandoned` — after no live
lease or unacknowledged result remains. The session itself need not be terminal
and may continue on its successor placement or with daemon-only tools. The
runner marks release in the manifest, atomically renames the placement below
`trash/`, fsyncs, and then deletes it by descriptor-relative traversal that
unlinks symlinks instead of following them. Startup resumes deletion only for
manifest-proven trash and may remove staging whose manifest proves it was never
published. It reconciles every ready or active manifest with the daemon before
execution and reports every unknown, retired-but-present, conflicting, or
otherwise unreconciled workspace as a typed leak. It never silently deletes a
reported leak. This startup report is visible even when no session can be
resumed.

## Open edges

- Remote runner transport, authentication, compatibility negotiation, and
  multi-host identity remain in
  [Protocols and persistence](../open-questions.md#protocols-and-persistence)
  and
  [Identity, credentials, and resource governance](../open-questions.md#identity-credentials-and-resource-governance).
- More than one active runner, automatic scheduling, load balancing, and MCP
  placement remain in
  [Scheduling and runners](../open-questions.md#scheduling-and-runners).
- Dynamic sandbox policy, catalog file parsing and reload, catalog revision
  rebinding, and concurrent execution remain in
  [Tool safety](../open-questions.md#tool-safety).
- General result-egress policy beyond exact injected-value redaction, including
  detection of transformed credential disclosure, remains in
  [Identity, credentials, and resource governance](../open-questions.md#identity-credentials-and-resource-governance).

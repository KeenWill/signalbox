# Runner protocol and placement

The runner protocol enrolls `signalbox-runner` processes with `signalboxd`,
records what each runner offers, and binds a session to at most one runner, at
most one credential profile, and one sandbox profile.

## Overview

The daemon and the runner are two processes on one host under one effective
user. `signalboxd` binds a Unix domain stream socket reserved for runners,
distinct from the client socket; `signalbox-runner` dials it and never listens.
On first startup the runner creates one enrollment-request identity, journals it
below its private state root, and sends `enroll`. The daemon issues the runner's
identities and its first registration, and the runner journals the receipt.
Every later connection sends `resume` under those identities. After the
handshake the runner advertises its availability and answers heartbeat
challenges. The daemon records each connection under a durable connection epoch,
marks a runner suspect and then lost when heartbeats stop, and propagates the
loss to the sessions pinned to that runner. The runner recovery commands also
have durable request, receipt, and daemon handlers. The local catalog admits the
`echo` capability class, the `ambient` profile, and the existing daemon `echo`
tool as a combined-locus pure declaration with the identical model definition
and permission default. The runner advertises configured availability for those
entries, credential profiles, and anonymous repositories. Credential-bound
repositories remain configured locally but are not advertised. An ambient runner
also advertises `worktree_per_session`. It executes `echo` in a plain child
process under `ambient` and retains the claimed phase and terminal result in a
versioned, fsynced, atomically published private journal. An initialized state
root without that journal fails startup. Resume reconciles the retained lease
phase and terminal result against durable daemon state. Provisioning and release
resume the exact retained workspace operation. Startup reconciles leak pages
before execution. Sandbox supervision is listed under Planned.

The daemon admits authenticated runner recovery after migrations and before the
generic startup scan or blob checks. Ordinary enrollment waits until the process
socket is bound and startup enables scheduling; sequencing is owned by
[turn lifecycle and scheduling](turn-lifecycle-and-scheduling.md).

The three process-wire creation commands retain optional runner placement and
initialize its unpinned status in the creation transaction. Creation validates
the request against the active registration without pinning or issuing a grant
or lease.

The domain lives in `crates/domain/src/runner/` and the wire vocabulary in
`crates/runner-wire`. A `RunnerEnrollment` binds the daemon-issued runner,
enrollment, and authentication-reference identities to the capability classes
the daemon allows. A `ValidatedRunnerRegistration` is one revision of a runner's
advertisement checked against that enrollment and the daemon's `RunnerCatalog`,
paired with the daemon's policy for every tool and profile it admits. The
advertisement carries availability only: capability classes, tool names,
credential-profile names with their repository entries, and workspace and
sandbox-profile capabilities, and an optional runner-reported absolute default
directory retained with that registration. The advertisement digest includes the
optional directory after the repository inventory. Each tool a runner may
advertise has one daemon-authoritative `RunnerToolDeclaration` giving its
model-facing definition, its effect class, and its admissible loci. Every
declaration has exactly one effect class, pure, idempotent, or side-effecting,
with no default; the class decides what a lost or repeated execution may do. A
tool's admissible loci are daemon only, runner only, or either, and a runner
locus names one runner identity or one capability class.

A `SessionRunnerPlacement` is the session's placement aggregate: one
`SessionRunnerPlacementRequest`, the placement revision, and the lifecycle
state. The request states the runner selector and the session's choices on the
four axes, workspace, repository, credentials, and sandbox. The aggregate starts
unpinned; the first dispatch pins it to one runner, snapshots the registration
it was validated against, and creates a `CredentialProfileGrant` when a profile
was selected. Pinning records the tool names the validated registration admits
and their runner-only subset. A `RunnerLease` is the domain record of one tool
attempt offered to that runner; its offer, claim, and completion drive the local
wire exchange. Loss and retry transitions are domain code. A placement changes
only by explicit transition: replacing a lost runner and replacing the pinned
credential profile each advance its revision. When a pinned runner is lost, the
placement enters a lost state that only two user commands leave: replace, which
installs a successor placement, and abandon, which retires the placement.

## Design decisions

One runner runs on the same host and under the same effective user as
`signalboxd`. That user is the trust boundary, so the authentication-reference
identity is correlated with the stored enrollment and is not a secret.

Positive placement revisions, the `RunnerPlacementChanged` boundary, the runner
event family, and the placement fields of session-creation records stay
compatible with a relocation that no loss caused. Why: a user-directed move of a
healthy session will consume them.

No path migrates a session, promotes a successor runner, or reschedules work
without a user command. A fresh enrollment is active at once; only a successor
after loss waits, and it waits on a user command, never on a daemon decision.

The daemon issues and owns logical enrollment authority; the runner keeps only a
stable idempotency fact for crash recovery.

The terminal result envelope on the wire is the projection of `ToolAttemptEnd`,
not a second terminal-state model.

Digests detect replay disagreement and confer no authority.

The daemon-local `ToolDefinition` is a compatibility representation, not a
second source of policy.

Without a durable no-execution proof, losing even an offered lease follows the
execution-possible law: pure or idempotent work needs a fresh physical attempt,
and side-effecting work needs crash classification. Why: the absence of a frame
in process memory is never proof that nothing ran. An offered lease lost with
durable no-execution proof permits a checked successor generation for every
effect class while retaining the unexecuted attempt.

The rule that model-provider keys never reach the runner means the daemon has no
runner-wire or environment-injection path for them; it is not a
filesystem-confidentiality claim under `ambient`.

The workspace manifest carries the digest of the canonical clone URL rather than
the URL. Why: the URL is credential-free, but its digest is sufficient identity
and avoids repeating the operator's configuration value.

## Workspace publication

The runner accepts an ambient `workspace_provision` only after resolving its
repository and optional credential profile against local configuration. Unknown
profiles reject before the operation is journaled. Anonymous acquisition runs
Git as a plain child process. Credentialed acquisition is unavailable.

Repository workspaces live at
`sessions/<canonical-session-uuid>/<placement-revision>/repo`, with their own
`.git` directory and no shared object store or credential-bearing remote URL.
Repository-free private roots use the sibling `work` path. A sibling staging
placement holds a versioned `0600` manifest outside the writable root;
publication fsyncs the files and directories and atomically renames the
placement. The manifest advances from `staging` to `ready`, then to `active`
upon an exact `workspace_recorded` acknowledgement. A repository records a
commit, a branch and commit, or an `unborn_branch` with its name and no commit
revision.

An in-flight release keeps the same worker after a transport loss. The runner
reaps it and journals its outcome before the next resume handshake.

The runner retains the complete provisioning request and ready receipt in its
private journal. Restart recomputes the fixed path and authenticates the root
and manifest, preserving session files. A cleanup guard removes unpublished
repository staging on preparation failure or cancellation. Equal replay retains
the manifest identity and ready receipt. A changed repository mapping fails as
`manifest_conflict`. Reconnect admits only the exact stored daemon authorization
under the unchanged registration, and the runner resends its authenticated
receipt until acknowledgement activates the manifest and clears the pending
operation. The journal retains acknowledged ready facts and the execution
directory identity until release is reconciled. Startup authenticates these
facts against the configuration and filesystem before reconnecting; a changed
mapping or replaced directory fails as `manifest_conflict`. The daemon
serializes provisioning, release, and lease delivery on each connection until
the corresponding durable outcome is acknowledged. Expected acquisition refusals
are journaled as `operation_failed` and retained through heartbeat and reconnect
until acknowledged; the runner keeps serving.

## Workspace release and leak reporting

The daemon issues cleanup for a retired manifest only to its connected owner,
after its leases and results settle. Plain-directory placements have no manifest
and receive no cleanup. Connection loss retires an outstanding release as
unowned and retains a leak diagnostic. Loss that settles a live lease also
retains its retired manifest as a `retired_present` leak. On reconnect,
`fail_stale` clears that exact journaled release; ownership never transfers to a
successor.

The runner fsyncs `release_accepted` before marking the manifest `releasing`,
renaming the placement below `trash/`, and deleting it through directory
descriptors without following symlinks. Restart continues accepted deletion even
when its manifest is gone. Completion is journaled before `workspace_released`
and retained until the exact acknowledgement. Failed cleanup retains
`workspace_cleanup_failed` until durable acknowledgement and exposes the
surviving workspace as a `cleanup_failed` leak. Release diagnostics retain the
manifest digest, so a startup report of the same workspace matches the retained
leak. A readable releasing manifest, whether still in place or below `trash/`,
retains its canonical workspace locator, ready-manifest digest, and session
correlation in the startup report, matching the stored cleanup diagnostic.
Pending and completed releases are reconciled by startup reporting. When a trash
manifest is already deleted, its canonical trash name identifies the retained
release and its existing diagnostic.

Before accepting new provisioning or executing tools, startup reports ready and
active manifests and unknown entries in bounded, digest-correlated pages.
Provisioning waits until every startup page is acknowledged. The daemon
reconciles exact retained ready facts and stores unresolved diagnostics before
acknowledging each page. The final page verifies strict ordering and the
complete digest over all retained facts. Reports remain visible without a
resumable session and authorize no deletion.

## Boundary contracts

Every runner-scoped fact is recorded per runner, never per deployment: identity,
enrollment, registration revision, connection and loss state, advertisement, and
workspace root.

Any frame outside the connection's current state, a duplicate frame with an
unequal canonical payload, or an acknowledgement without its durable predecessor
is fatal and advances nothing.

A runner identity is issued by one logical enrollment and is never derived from
hardware, a hostname, a network address, or any machine fingerprint. Persistent
and short-lived runners follow the same rule: a newly enrolled ephemeral runner
receives a new identity, and a runner reconnecting under an active enrollment
keeps its existing one. The runner creates one random enrollment-request
identity on first startup and journals it atomically below its private state
root before it connects, so replaying `enroll` after a crash is safe. The daemon
admits a pristine active enrollment while no other active enrollment exists, or
one pending successor after that enrollment is durably lost. Equal enrollment
replay retains the exact receipt. Pending enrollment permits heartbeat and a
command-bound workspace operation, but cannot mutate registration or authorize
grants, leases, claims, or dispatch.

Durable revocation commits first and then flips the caller-held active fence
that the enrollment shares; a failed durable revocation leaves that fence
active. A lease offer rechecks the active enrollment and its exact enrollment,
runner, and authentication-reference correlations. A lease already offered is
already dispatched: it completes or crash-classifies normally, and revocation
neither rewrites nor cancels it.

An advertisement carries no permission default, effect class, placement
declaration, approval posture, credential path, or credential value, and
advertising a name confers no execution authority. A validated registration
pairs that availability with daemon-owned policy: the complete declaration of
each admitted tool and the approval policy of each admitted credential profile.
Registration validates every class, tool, workspace capability, and sandbox
profile against the enrollment and the daemon catalog; one disallowed claim, one
malformed name, one daemon-only tool, or one runner tool whose identity-or-class
selector the advertisement does not satisfy rejects the complete registration.
Capability-class, credential-profile, and repository-key names share one checked
syntax, but classes are catalog keys while the other two are availability keys
from runner configuration. Credential-profile names are duplicate-free; the user
selects one advertised name, and daemon-owned policy decides approval
independently of the name.

Omitting a formerly advertised capability removes its availability from the new
registration and never changes daemon-side policy. Omitting a combined-locus
tool, one admissible on both loci that the attached runner does not advertise,
disables runner dispatch for that tool and keeps the placement: the domain keeps
daemon-local admissibility while runner lease creation fails `ToolUnavailable`.
Daemon fallback transfers neither the consumed runner authorization nor the
credential-profile grant to daemon execution.

An unpinned placement permits daemon-local `echo` execution under its daemon
permission default, independently of runner permission overrides or credential
profile selection. That execution leaves the placement unpinned and creates no
runner grant or lease.

`RunnerToolDeclaration` is the one daemon-authoritative runner-dispatch
declaration, so every runner-advertisable tool has model-facing description and
schema authority even when daemon execution is inadmissible. Its permission
default is model-definition and daemon-locus compatibility metadata and never
authorizes a runner attempt. A combined-locus declaration must equal the
daemon-local default, so fallback cannot silently change the definition the
model saw. Placement is immutable declaration metadata, never a per-call choice
supplied by a runner or a model. A boundary that must classify an untrusted
declaration before validation treats the tool as side-effecting; that
fail-closed adapter behavior is not a fourth effect class.
[Tool loop](tool-loop.md) owns the executor boundary and the mapping between
runner effect classes and the daemon catalog.

The current active enrollment, the pinned placement, its exact validated
registration, and any selected active credential grant jointly authorize every
lease offer after the first. The lease aggregate holds no channel handle or
process-local connection state, so a reconnecting registration cannot recreate,
complete, or discard a lease from an advertisement. Re-leasing continues one
logical tool request and one lease lineage; its successor `RunnerGeneration` is
distinct from the fresh physical attempt's `ToolDispatchGeneration`, which
starts at first. A lease claim or completion requires the exact lease, runner,
tool, tool-attempt dispatch correlation, lease generation, offer-time
registration revision, placement revision, working directory, and sandbox
profile. The offer retains the canonical tool arguments independently of the
pin's registration snapshot. A runner generation with no representable successor
fails closed as generation exhaustion.

The local `echo` path atomically stores the initial pin, optional grant, and
first offered lease under the connected runner's current registration. It uses
the requested runner approval posture; `Confirm` requires a human decision even
under daemon blanket approval. The pure child resolves no credential path and
injects no credential value.

The local tool loop is serial: at most one live lease per session, one runner
dispatch at a time, and one durable terminal attempt before the next dispatch. A
runner holds one global execution permit, so tools of different sessions never
execute concurrently on it. A combined-locus tool runs on the session's attached
runner when that runner advertises it and otherwise runs on the daemon. An
unpinned placement that the connected runner cannot satisfy, or a placement lost
before its first pin, retains daemon fallback. Caller cancellation retains the
global dispatch permit until the durable lease completes or becomes lost.
Closing the incarnation's database pool ends its local dispatch waiters. A lost
lease releases the global dispatch permit; the tool loop accepts the recovery
wait only after rereading the exact lost lease and its issuing turn attempt's
durable yield.

The daemon sends `lease_offer` with the complete lease correlation and the
immutable dispatch payload. The runner admits the exact tool, sandbox profile,
and optional credential profile, then replies `lease_claim`. The daemon commits
the claim before it sends `lease_claimed`, and receipt of that acknowledgement
is the runner's execution capability; an offer or a sent claim without it
authorizes nothing. Before accepting `dispatch` the runner fsyncs the complete
claimed correlation and the `waiting_dispatch` phase below its private state
root. The runner executes only when the claimed capability and the dispatch
carry the same complete correlation. It fsyncs `dispatch_received` before
acknowledging the frame internally and `execution_may_have_started` immediately
before it invokes the executor. The runner retains one terminal evidence
envelope and resends it until the daemon commits the matching attempt and lease
transition and replies `result_recorded`, then discards it. Local runner
shutdown waits for admitted execution and its result acknowledgement before
sending the shutdown frame. Either shutdown direction checks for offered or
claimed leases under the enrollment lock. With unsettled execution it records
connection loss and closes the transport without a clean shutdown frame;
otherwise it records clean shutdown. Connection loss propagates to the lease and
runner recovery wait.

Resume authenticates the enrollment receipt before changing registration or
recording retained terminal evidence. Canonical lease state determines its exact
inventory directives. An intact claimed lease in `waiting_dispatch` or
`dispatch_received` permits replay of the claim acknowledgement and unchanged
dispatch; neither phase permits replay after committed connection loss. A claim
reported as `execution_may_have_started` without a retained result, or omitted
from inventory, is durably lost under the effect-class ambiguity law, including
when inventory names a different historical lease. An equal recorded result is
acknowledged; an unequal duplicate is fatal. The runner discards its journal
entry only on the exact recorded or stale directive and never invokes a started
lease again. Fresh offers wait until reconnect establishes the resumed
connection; prior physical connections cannot issue authority after a new epoch.

Workspace, repository, credentials, and sandbox are independent axes of one
session: a choice on any axis constrains no other, and no axis is inferred from
another. Workspace, credentials, and sandbox are each a stated choice at
creation; a repository is named only inside a worktree requirement. A request
that selects no workspace or no credential profile states that absence
explicitly and receives absence, never a daemon- or runner-selected substitute,
because a silently inferred credential is an authorization the user never
granted. A placement with no repository performs no repository operation; with a
named profile it still creates the grant for the session's other admitted
dispatches. A profileless placement creates no grant, resolves no configured
path, and injects no value. A repository-worktree requirement is satisfiable
only when the selected registration advertises the per-session worktree
capability and the repository key resolves in checked runner configuration to a
credential-free HTTPS clone URL. Domain placement admission requires a matching
runner selector, advertised support for the requested sandbox and any selected
credential profile, and daemon-catalog admission of every overridden tool.

Ordinary attachment and lease creation accept only the exact pinned runner and
the current grant; re-registration and reconnect change none of the pinned
facts. Pinned reconstitution validates against the exact registration snapshot
that produced the pin, and a current narrowed re-registration is reconciled
separately, never substituted for that snapshot. Lease reconstitution rejects as
corrupt any disagreement in the independently recorded correlation, session,
registration-declared effect class, credential authorization, or state.

The generic snapshot writer stores neither runner replacement event and never
installs loss. Each replacement runs in its own command-authorized transaction
that revalidates, under the runner lock order, that the supplied registration's
enrollment is active, its connection live, and its revision current. The
heartbeat-loss transaction records lost for the exact current connection epoch,
so a stale epoch cannot write after that commit, and the connection-loss
propagation transaction consumes the pinned evidence when it persists the lost
state. [Persistence protocol](persistence-protocol.md) owns the lock order, the
rule that no transaction stays open across runner I/O, and the validation of
runner transition events against the placement revision they name. When a prior
credential grant exists, domain runner replacement consumes it and creates its
checked successor revision in the same transition. Replacing a pinned runner or
its credential profile returns exact before-and-after placement and grant change
facts. An active or revoked prior grant yields a checked successor that is
active with a selected profile or a revoked tombstone without one, while its
consumed revision remains terminal. The domain replaces an active grant on a
pinned runner with an active successor or terminally revokes it.

Reconnect of the lost identity cannot consume either replacement transition or
clear a lost state. Safe retry authority exists only for a pinned lost runner,
is consumed only as part of its user replacement, and never causes automatic
dispatch. Abandonment has no cancellation proof and cannot end a turn; an active
turn first finishes its stop, approval-decision, or reconciliation flow. An idle
or queued-only session that abandons its runner fabricates no turn or frontier,
creates no successor turn, afterwards exposes only daemon-executable tools, and
never rewrites an issued side effect as known. Replace and abandon follow the
command claim protocol that [identity and commands](identity-and-commands.md)
owns.

The sandbox profile is an immutable placement fact and appears in every session,
lease, dispatch, result, evidence, transcript, and user-inspection projection.
Changing it requires the same explicit replacement frontier as changing runners.
Every surface names the unconfined profile `ambient`, and no surface calls it
sandboxed. Profile or override policy cannot make a tool available, change its
effect class, alter its arguments, or move its locus. The domain rejects a
placement whose selected registration does not advertise its sandbox profile. A
placement override naming a tool absent from the validated registration's daemon
catalog rejects the request as `ToolUndeclared`.

No credential-value field exists in the runner-protocol domain; advertisements,
registrations, placements, grants, leases, changes, and reconstitution inputs
carry only `CredentialProfileName`. Model-provider credentials never enter
runner configuration, wire state, or the injected execution environment. The
workspace manifest records no credential path or value, and a writable
repository's `.git/config` is never authority.
[Configuration and credentials](configuration-and-credentials.md) owns the
credential reference and value split and the runner credential lifecycle;
[the Git authority threat model](git-authority-threat-model.md) owns the
transport threat narrative.

[Sessions and transcript](sessions-and-transcript.md) owns the session's dotted
placement path, which is not a runner placement fact.

`promote_pending_runner` names the pending enrollment request and atomically
revokes its lost predecessor and promotes its exact enrollment and registration;
it changes no session placement. The daemon delivers the promoted `enrolled`
receipt on the candidate connection before releasing its provisioning slot or
delivering a lease offer; the runner fsyncs its exact promotion and equal replay
changes nothing. `replace_lost_runner` names a lost session and an optional
checkout revision, promotes a connected pending successor when needed, and
installs the successor placement and grant lineage. Successor selection follows
the enrollment chain to its current pending or active descendant. Pre-pin
replacement provisions nothing and returns to unpinned at the next revision.
Replacement behind an active model call or tool batch remains staged until its
observation or complete-result boundary. Registration-triggered loss permits
replacement on the same runner after its current registration satisfies the
retained request; other loss sources require a different runner.

Pinned replacement requiring a repository or private root retains a single-use
command authorization and an exactly correlated `workspace_ready` receipt,
including its absolute working directory. The daemon acknowledges a durably
retained receipt even while installation waits. Provisioning retains a
repository key and checkout recovery facts together or neither; mismatched
repository and recovery facts are rejected before staging. Installation consumes
that receipt, promotes the pending candidate, installs the placement and grant,
appends the reference-only placement boundary, and records the terminal result
atomically after any authorized in-flight call reaches its observation boundary
and, for a tool batch, after all results are appended. Provisioning refusal or
candidate loss records a typed terminal rejection and leaves the candidate
pending. A terminal delegated runtime does not count as an active turn for
recovery commands. Replacement stays staged while an explicit compaction call is
nonterminal; its observation commit wakes installation from the resulting
frontier. Replacement rejects a candidate lacking the requested sandbox or
repository workspace capability before staging provisioning. Ambient
default-directory replacement requires the successor registration's reported
directory. A rejected command's ready workspace, including a correlated receipt
arriving after abandonment, is released only through its exact manifest
correlation on the candidate's retained connection epoch. Suspicion retains that
cleanup authority; loss does not transfer it. Release acknowledgement uses the
same current-epoch fence as release dispatch.

## Planned

- General operation-failure evidence over the wire:
  [runner protocol design](../design/runner-protocol.md).
- Several runners enrolled with one daemon at once:
  [runner protocol design](../design/runner-protocol.md).
- User-directed relocation of a healthy session, `move_healthy_session`:
  [runner protocol design](../design/runner-protocol.md).
- Initial repository-placement provisioning admission and credentialed or
  restricted repository acquisition:
  [runner protocol design](../design/runner-protocol.md).
- The restricted sandbox and ambient supervision under bubblewrap, with confined
  file tools: [runner protocol design](../design/runner-protocol.md).
- The restricted-namespace HTTPS egress broker:
  [runner protocol design](../design/runner-protocol.md).
- Forced Git configuration and the canonical repository binding check:
  [runner protocol design](../design/runner-protocol.md).

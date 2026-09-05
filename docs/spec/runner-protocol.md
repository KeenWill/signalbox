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
loss to the sessions pinned to that runner. This registration slice and the
`signalbox-runner` binary are the whole of what is built. The runner advertises
only the credential-profile names and repository entries its strict
configuration carries, and no capability class, tool, workspace capability, or
sandbox profile; the daemon catalog that would admit such claims is empty, and
the runner sends an empty reconnect inventory and executes nothing. Leases,
dispatch, workspaces, sandboxes, and recovery are listed under Planned.

The domain lives in `crates/domain/src/runner.rs` and the wire vocabulary in
`crates/runner-wire`. A `RunnerEnrollment` binds the daemon-issued runner,
enrollment, and authentication-reference identities to the capability classes
the daemon allows. A `ValidatedRunnerRegistration` is one revision of a runner's
advertisement checked against that enrollment and the daemon's `RunnerCatalog`,
paired with the daemon's policy for every tool and profile it admits. The
advertisement carries availability only: capability classes, tool names,
credential-profile names with their repository entries, and workspace and
sandbox-profile capabilities. Each tool a runner may advertise has one
daemon-authoritative `RunnerToolDeclaration` giving its model-facing definition,
its effect class, and its admissible loci. Every declaration has exactly one
effect class, pure, idempotent, or side-effecting, with no default; the class
decides what a lost or repeated execution may do. A tool's admissible loci are
daemon only, runner only, or either, and a runner locus names one runner
identity or one capability class.

A `SessionRunnerPlacement` is the session's placement aggregate: one
`SessionRunnerPlacementRequest`, the placement revision, and the lifecycle
state. The request states the runner selector and the session's choices on the
four axes, workspace, repository, credentials, and sandbox. The aggregate starts
unpinned; the first dispatch pins it to one runner, snapshots the registration
it was validated against, and creates a `CredentialProfileGrant` when a profile
was selected. A `RunnerLease` is the domain record of one tool attempt offered
to that runner; its offer, claim, loss, and retry transitions are domain code,
and the wire dispatch that would drive them is listed under Planned. A placement
changes only by explicit transition: replacing a lost runner and replacing the
pinned credential profile each advance its revision. When a pinned runner is
lost, the placement enters a lost state that only two user commands leave:
replace, which installs a successor placement, and abandon, which retires the
placement.

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

The daemon retains a runner-authored failure detail verbatim as operator
evidence, exposes it through runner inspection, and never parses or branches on
it. Why: a runner can add detail codes without a daemon change.

The daemon-local `ToolDefinition` is a compatibility representation, not a
second source of policy.

Without a durable no-execution proof, losing even an offered lease follows the
execution-possible law: pure or idempotent work needs a fresh physical attempt,
and side-effecting work needs crash classification. Why: the absence of a frame
in process memory is never proof that nothing ran.

The rule that model-provider keys never reach the runner means the daemon has no
runner-wire or environment-injection path for them; it is not a
filesystem-confidentiality claim under `ambient`.

The workspace manifest carries the digest of the canonical clone URL rather than
the URL. Why: the URL is credential-free, but its digest is sufficient identity
and avoids repeating the operator's configuration value.

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
admits `enroll` only as a pristine enrollment while no other active enrollment
exists.

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
starts at first.

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
credential-free HTTPS clone URL.

Ordinary attachment and lease creation accept only the exact pinned runner and
the current grant; re-registration and reconnect change none of the pinned
facts. Pinned reconstitution validates against the exact registration snapshot
that produced the pin, and a current narrowed re-registration is reconciled
separately, never substituted for that snapshot.

The generic snapshot writer stores neither runner replacement event and never
installs loss. Each replacement runs in its own command-authorized transaction
that revalidates, under the runner lock order, that the supplied registration's
enrollment is active, its connection live, and its revision current. The
heartbeat-loss transaction records lost for the exact current connection epoch,
so a stale epoch cannot write after that commit, and the connection-loss
propagation transaction consumes the pinned evidence when it persists the lost
state. [Persistence protocol](persistence-protocol.md) owns the lock order, the
rule that no transaction stays open across runner I/O, and the validation of
runner transition events against the placement revision they name.

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
effect class, alter its arguments, or move its locus.

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

## Planned

- Lease offer, claim, dispatch, result and failure spooling, and
  reconnect-inventory reconciliation over the wire, under one global execution
  permit per runner: [runner protocol design](../design/runner-protocol.md).
- Several runners enrolled with one daemon at once:
  [runner protocol design](../design/runner-protocol.md).
- Pending successor enrollment, promotion, and the replacement command:
  [runner protocol design](../design/runner-protocol.md).
- Same-runner replacement after registration-triggered loss:
  [runner protocol design](../design/runner-protocol.md).
- User-directed relocation of a healthy session, `move_healthy_session`:
  [runner protocol design](../design/runner-protocol.md).
- Workspace provisioning, private writable roots, the workspace manifest
  lifecycle, and root re-adoption on runner restart:
  [runner protocol design](../design/runner-protocol.md).
- Workspace release and startup leak reconciliation:
  [runner protocol design](../design/runner-protocol.md).
- The restricted sandbox and ambient supervision under bubblewrap, with confined
  file tools: [runner protocol design](../design/runner-protocol.md).
- The restricted-namespace HTTPS egress broker:
  [runner protocol design](../design/runner-protocol.md).
- Forced Git configuration and the canonical repository binding check:
  [runner protocol design](../design/runner-protocol.md).

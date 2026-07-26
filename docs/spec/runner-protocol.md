# Runner protocol and placement

This page specifies the implemented runner-protocol domain foundation as
verified against the implementing stack through PR #260
(`agent/runner-protocol-domain`). It owns logical runner enrollment,
daemon-authoritative catalog validation, runner leases, session placement and
affinity, credential-profile grants, and workspace requirements. The tool
registry's common declarations remain owned by [tool loop](tool-loop.md);
session transcript and frontier mechanics remain owned by
[sessions and transcript](sessions-and-transcript.md); physical tool attempts
remain owned by [tool loop](tool-loop.md). Invariant tags cite
[the invariant catalog](../invariants.md).

The verified surface in this stack is domain-only. There is no runner binary,
store adapter, transport message, streaming connection, authentication
handshake, or network code. Those implementation edges are listed under
[Open edges](#open-edges).

## Identity, enrollment, and registration

`RunnerId`, `RunnerEnrollmentId`, `RunnerAuthenticationId`, and `RunnerLeaseId`
are distinct UUID-backed domain identities. A runner identity is issued by one
logical enrollment and is not derived from hardware, a hostname, a network
address, or any other machine fingerprint. Persistent and short-lived runners
use the same identity law. A newly enrolled ephemeral runner receives a new
identity; reconnecting under an existing active enrollment retains the existing
one.

One `RunnerEnrollment` binds the enrollment identity, runner identity, opaque
authentication-reference identity, and owner-allowed capability classes. The
authentication reference identifies daemon-resident enrollment policy; it is not
an authentication secret. Enrollment is either active or revoked. Revocation is
terminal and makes later registration invalid. Complete reconstitution rejects
mismatched enrollment, runner, authentication, allowed class inventory, or
lifecycle state rather than repairing it. Revocation also makes an existing
validated registration unable to authorize a later lease. A lease offer rechecks
the active enrollment and its exact enrollment, runner, and
authentication-reference correlations; a lease already offered is unaffected.

A registration carries availability claims only:

- the runner's advertised capability classes;
- tool names;
- credential-profile names; and
- workspace capabilities.

It carries no permission default, effect class, placement declaration, approval
posture, or credential value. Registration validates the advertisement against
both the enrollment's allowed capability classes and the daemon-side catalog. An
unknown or disallowed claim rejects the complete registration. A valid
registration retains the exact advertised subset and attaches the
daemon-authoritative declarations. Omitting a formerly advertised capability
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

Capability-class and credential-profile names are exact, bounded catalog keys.
Construction rejects empty values, U+0000, values longer than 64 UTF-8 bytes,
and bytes outside ASCII letters, digits, dot, underscore, and hyphen. A name
must begin with an ASCII letter or digit. Workspace capability is closed
vocabulary; the implemented arm is `WorktreePerSession`.

The owner-editable catalog is validated into one `RunnerCatalog` domain value.
It contains allowed capability classes, complete runner-tool declarations,
credential-profile policies, and allowed workspace capabilities. Duplicate
names, a credential policy naming an undeclared tool, or an internally
inconsistent placement declaration rejects the complete catalog.
Configuration-file parsing and replacement are later application work; the
domain value is independent of TOML.

Each `RunnerToolDeclaration` contains:

- the existing checked `ToolName`;
- one checked `RunnerToolModelDefinition`, containing a nonempty bounded
  model-facing description and a canonical JSON-object argument schema;
- one required `ToolPermissionDefault`;
- one required `RunnerToolEffectClass`; and
- one nonempty `ToolAdmissibleLoci` value.

`ToolAdmissibleLoci` is closed typed vocabulary:

- `DaemonOnly`;
- `RunnerOnly { selector }`; or
- `DaemonOrRunner { selector }`.

A runner selector is either one exact `RunnerId` or one `RunnerCapabilityClass`.
When both loci are admissible, the domain retains daemon-local admissibility if
the attached runner does not currently advertise the tool, while runner lease
creation fails `ToolUnavailable`. It does not transfer the consumed runner
authorization or credential-profile grant to daemon execution. Later application
orchestration must select the locus before authorization; a change to daemon
fallback discards runner-pair authority and resolves the daemon-local tool
policy without the runner-resident profile. Placement is immutable declaration
metadata, not a per-call choice supplied by a runner or model. An MCP locus is
not part of the vocabulary.

`RunnerToolDeclaration` is the one daemon-authoritative runner-dispatch
declaration. Every runner-advertisable tool therefore has model-facing
description and schema authority even when daemon execution is inadmissible. The
current daemon-local application `ToolDefinition` is a compatibility
representation, not a second source of policy. A later application adapter must
compile argument validation from the runner declaration's exact schema and
reject a shared name unless model-facing definition and permission are equal and
the local effect maps exactly (`EffectFree` to `Pure`, `ExternalEffect` to
`SideEffecting`). `Idempotent` has no current daemon-local projection, so a tool
with that effect cannot include the daemon locus until the representations are
consolidated.

Advertisement validation never synthesizes a declaration for an unknown tool.
Unknown tools, credential profiles, capability classes, and workspace
capabilities reject the complete advertisement. A daemon-only tool or a runner
tool whose declared identity-or-class selector the advertisement does not
satisfy also rejects the complete advertisement. The resulting
`ValidatedRunnerRegistration` exposes only exact advertised availability paired
with daemon-owned policy. A runner can therefore neither self-widen its tool
surface nor replace confirmation with automatic approval (INV-042).

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
and its exact tool name to the tool loop's `AuthorizedToolAttempt`. The latter
exists only after the automatic or owner decision authorizes that exact attempt,
and neither authority nor the resulting lease is cloneable. Current active
enrollment, pinned placement, its exact validated registration, and any selected
active credential grant jointly authorize every offer after the first. The
initial offer instead creates that pinned placement, any selected grant, and
generation-one lease in one checked transition from `Unpinned`; it does not
require those products to exist beforehand. The request, attempt, session, and
two-way crash class must match the selected tool, placement, and
declaration-derived effect class (`Pure` to `EffectFree`; `Idempotent` or
`SideEffecting` to `ExternalEffect`). Revoked enrollment, lost placement, or a
mismatched runner, request, tool, attempt, effect, profile, or grant cannot
create a lease.

When a credential profile is selected, the lease also retains the exact
immutable `CredentialDispatchAuthorization`: session, runner, profile, grant
revision, tool, and resolved pair posture. Grant replacement or revocation
therefore cannot erase which snapshot authorized an already offered lease.

A lease begins `Offered` at lease-lineage generation one. Only the exact lease,
runner, tool, authorized physical-attempt correlation, and lineage generation
may claim it, producing `Claimed`; only that same correlation may complete it.
Completion is terminal. A stale or cross-wired correlation cannot advance the
aggregate. Complete reconstitution accepts only the closed state shapes and
exact correlations.

`LostUnclaimed` means authoritative proof that no execution capability was
issued, not merely absence of a claim frame after an offer was sent. A future
transport must durably commit the exact claim and acknowledge it before the
runner may execute. Channel loss after delivery but before that acknowledgement
cannot be interpreted as proof either way by transport alone.

With that proof, loss before claim permits every effect class to be re-leased at
the checked successor lease-lineage generation. Loss after claim follows the
required retry law:

- `Pure` and `Idempotent` produce typed re-lease authority at the checked
  successor generation; after claim that authority consumes the owning checked
  `ToolBatch`, retires the prior in-flight attempt to its effect-correct
  terminal history, installs and authorizes a fresh physical `ToolAttemptId`,
  retains every retired attempt identity in the updated batch, and returns both
  attempt records; only the private replacement evidence produced by that batch
  transition can authorize the claimed re-lease, while authority lost before
  claim retains the never-executed attempt identity; and
- `SideEffecting` produces typed crash-classification authority whose physical
  attempt is derived from the opaque lost lease and never produces re-lease
  authority.

Generation exhaustion, reuse of any current or retired attempt identity, and a
standalone same-request authorization for claimed retry all fail closed.
`RunnerLeaseLoss` has sealed construction, so only `RunnerLease::lose` can
produce retry or crash-classification authority. Re-leasing continues one
logical tool request and lease lineage. Its successor `RunnerGeneration` is
distinct from the fresh physical attempt's `ToolDispatchGeneration`, which
starts at `first()` under the tool-loop law. Every repeated physical execution
therefore has its own attempt identity and record as required by INV-004.
Side-effecting loss composes with the existing physical-attempt ambiguity
machinery; this domain slice does not duplicate or overwrite that attempt's
outcome (INV-004, INV-025, INV-026, INV-043).

The lease aggregate contains no channel handle or process-local connection
state. A reconnecting registration cannot recreate, complete, or discard a lease
from an advertisement. Why: the held streaming channel is transport, not lease
or claim authority. Store mapping, the single runner-initiated outbound stream,
reconnect resynchronization, and exact wire correlations are later stacks.

## Session placement and affinity

`SessionRunnerPlacement` starts with one request that is immutable between
explicit replacement transitions:

- a `RunnerSelector`, targeting a capability class or exact runner identity;
- `WorkingDirectorySelection`, either runner default or one exact bounded
  working-directory value;
- an optional `CredentialProfileName`; and
- `WorkspaceRequirement`, either none or a repository worktree.

The working-directory value is exact nonempty UTF-8, excludes U+0000, and is at
most 4,096 bytes. The domain does not apply host-platform path parsing. A
repository-worktree requirement carries one exact repository key with the same
nonempty, U+0000-free, at-most-4,096-byte contract.

Before execution, placement is `Unpinned`. Mere attachment does not pin it. The
first authorized runner lease atomically validates the request against that
runner's exact validated registration and produces `Pinned` state, any requested
initial credential grant, and the initial offered lease together. Selector,
credential-profile availability, workspace capability, and the tool-bound
authorized-attempt correlation must all match. The pinned state contains the
runner, selected working directory, credential-profile selection, tool
inventory, runner-required tool inventory, and any provisioned workspace. When a
profile was requested, that same transition constructs its initial grant from
the now-exact runner and registration; session creation cannot construct a
runner-bound grant while class-targeted placement is still unpinned. Once
pinned, ordinary attachment and lease creation accept only that exact runner and
require the current grant when a profile is selected. There is no automatic
migration or class-based rescheduling to a different runner (INV-044).

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

Runner loss is explicit state, not implicit reassignment. Marking the pinned
runner lost retains the prior placement and disables future lease creation. An
owner-directed replacement supplies and installs a new complete placement
request, validated registration, working directory, credential-profile
selection, tool inventory, and provisioned workspace. Exact-runner placement
therefore changes identity only through this explicit replacement. It advances a
positive placement revision and returns one `RunnerPlacementChange` value
carrying the complete before-and-after placement requests and pinned facts
needed for a later frontier-extending injected message. Reconstitution accepts a
complete public raw-facts input and rejects an unpinned revision other than one,
a pinned or lost state that does not match its current request and validated
capabilities. Durable replacement-history verification belongs to the later
persistence projection. Pinned or lost reconstitution validates against the
exact registration snapshot that produced the pin and rejects any stored tool or
runner-required-tool inventory that differs from that checked result. A current
narrowed re-registration is reconciled separately and is not substituted for
that historical snapshot. This domain aggregate accepts every positive revision
because each is reachable through checked successor transitions.

This stack proves that replacement must be explicit and produces the typed
change facts. Application orchestration that appends the corresponding semantic
message and context frontier is a later edge.

## Credential profiles and approval

A credential profile has two deliberately separate representations:

- the runner holds the profile's credential value, provisioned out of band; and
- the daemon holds only its checked name, selection, policy, grant, and audit
  facts.

No credential-value control field exists in the runner-protocol domain.
Advertisements, registrations, placement, grants, leases, replacement changes,
and reconstitution inputs can carry only `CredentialProfileName`. Arbitrary
runner tool result or error text is not proof that a tool did not echo a secret;
result-egress controls are a later security boundary (INV-035).

One daemon catalog policy declares approval posture for exact
`(ToolName, CredentialProfileName)` pairs. The closed posture is `Automatic` or
`SessionPolicy`. For runner dispatch under a selected profile, the frozen
dangerous blanket remains first. Otherwise `Automatic` authorizes the exact pair
without a judge, while `SessionPolicy` and an absent pair require confirmation.
The registry's tool-only default applies only when no credential profile is
selected; it cannot override a pair-level `SessionPolicy`. Profile policy cannot
make an undeclared tool available and cannot alter its effect class or
admissible loci.

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

Grant replacement is forward-only. It checks the current revision and installs
one complete later snapshot, returning a `CredentialProfileChange` with the
before-and-after profile and tool inventories for later frontier injection.
Runner replacement consumes the exact last-grant runner and revision carried by
the pinned placement and creates a checked successor revision. A profileless
replacement carries both that placement evidence and the lineage forward as a
new terminal tombstone; omitting the tombstone is therefore structurally
rejected, and restoring a previously selected profile cannot recreate revision
one. Every prior revoked revision remains terminal. Revocation is also
forward-only and gates later lease creation. A lease already offered is already
dispatched and completes or crash-classifies normally; revocation neither
rewrites nor cancels it. A revoked grant revision cannot become active again.
Complete reconstitution accepts a complete public raw-facts input, checks an
independently authoritative expected session and rejects foreign runner facts, a
profile absent from the validated registration, or a tool set wider than the
advertisement. Durable revision history and atomic store dispatch gating remain
persistence work (INV-045).

## Workspace provisioning

`WorkspaceRequirement::RepositoryWorktree` is satisfiable only when the selected
validated registration advertises `WorkspaceCapability::WorktreePerSession`. A
provisioned workspace binds the session, runner, repository key, and exact
working directory. Its cleanup owner is structurally the runner that provisioned
it; no daemon-cleanup alternative is constructible. Replacement must supply a
new provisioned workspace when the requirement remains active and cannot
silently carry a prior runner's workspace forward.

This stack validates and records the capability, requirement, ownership, and
placement correlations. Filesystem provisioning and cleanup execution belong to
the later runner workspace stack.

## Open edges

- Runner transport, authentication exchange, durable registration and lease
  storage, durable-claim acknowledgement before runner execution, reconnect
  recovery, compatibility, and result envelopes are recorded in
  [Protocols and persistence](../open-questions.md#protocols-and-persistence)
  and
  [Identity, credentials, and resource governance](../open-questions.md#identity-credentials-and-resource-governance).
- Frontier injection, runner-loss recovery beyond explicit replacement, and
  lease/affinity orchestration are recorded in
  [Scheduling and runners](../open-questions.md#scheduling-and-runners).
- MCP placement is recorded in
  [Scheduling and runners](../open-questions.md#scheduling-and-runners).
- Credential-scoped runner classes are recorded in
  [Identity, credentials, and resource governance](../open-questions.md#identity-credentials-and-resource-governance).
- Catalog file parsing, reload, revision pinning, and safe rebinding are
  recorded in [Tool safety](../open-questions.md#tool-safety).
- Application orchestration that selects runner or daemon locus before
  authorization, resolves credential-pair posture through the existing
  tool-decision path, discards runner-pair authority before daemon fallback,
  compiles runner argument schemas into executable validators, and projects from
  current daemon-local tool definitions is recorded in
  [Tool safety](../open-questions.md#tool-safety).
- Runner result-egress policy, including whether and how arbitrary tool output
  is screened for credential disclosure, is recorded in
  [Identity, credentials, and resource governance](../open-questions.md#identity-credentials-and-resource-governance).
- Workspace filesystem provisioning, cleanup recovery, and containment claims
  are recorded in [Tool safety](../open-questions.md#tool-safety).

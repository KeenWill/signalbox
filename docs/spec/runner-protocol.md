# Runner protocol and placement

This page specifies the implemented runner-protocol domain foundation as
verified against the implementing stack rooted at PR #259
(`agent/runner-protocol`). It owns logical runner enrollment,
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
lifecycle state rather than repairing it.

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
daemon-side policy. Why: re-registration can narrow current availability without
downgrading a confirmation requirement or widening authorization (INV-042).

## Advertised catalogs and daemon authority

Capability-class and credential-profile names are exact, bounded catalog keys.
Construction rejects empty values, U+0000, values longer than 64 UTF-8 bytes,
and bytes outside ASCII letters, digits, dot, underscore, and hyphen. A name
must begin with an ASCII letter or digit. Workspace capability is closed
vocabulary; the implemented arm is `WorktreePerSession`.

The owner-editable catalog is validated into one `RunnerCatalog` domain value.
It contains complete runner-tool declarations, credential-profile policies, and
allowed workspace capabilities. Duplicate names, a credential policy naming an
undeclared tool, or an internally inconsistent placement declaration rejects the
complete catalog. Configuration-file parsing and replacement are later
application work; the domain value is independent of TOML.

Each `RunnerToolDeclaration` contains:

- the existing checked `ToolName`;
- one required `ToolPermissionDefault`;
- one required `RunnerToolEffectClass`; and
- one nonempty `ToolAdmissibleLoci` value.

`ToolAdmissibleLoci` is closed typed vocabulary:

- `DaemonOnly`;
- `RunnerOnly { selector }`; or
- `DaemonOrRunner { selector }`.

A runner selector is either one exact `RunnerId` or one `RunnerCapabilityClass`.
When both loci are admissible, dispatch selects the session's attached runner
when that runner satisfies the selector and currently advertises the tool;
otherwise daemon-local execution is admissible. Placement is immutable
declaration metadata, not a per-call choice supplied by a runner or model. An
MCP locus is not part of the vocabulary.

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

A `RunnerLease` binds one lease identity, exact tool attempt, session, runner,
effect class, and positive dispatch generation. A lease begins `Offered`. Only
the bound runner and generation may claim it, producing `Claimed`; only that
same correlation may complete it. Completion is terminal. A stale runner,
generation, tool attempt, or lease identity cannot advance the aggregate.
Complete reconstitution accepts only the closed state shapes and exact
correlations.

Loss before claim proves that no runner effect was authorized, so every effect
class may be re-leased at the checked successor generation. Loss after claim
follows the required retry law:

- `Pure` and `Idempotent` produce typed re-lease authority for the same physical
  attempt at the checked successor generation; and
- `SideEffecting` produces typed crash-classification authority naming the exact
  physical attempt and never produces re-lease authority.

Generation exhaustion fails closed. Re-leasing is a continuation of one physical
attempt under a new runner-dispatch fence, not a new logical tool request.
Side-effecting loss composes with the existing physical-attempt ambiguity
machinery; this domain slice does not duplicate or overwrite that attempt's
outcome (INV-025, INV-026, INV-043).

The lease aggregate contains no channel handle or process-local connection
state. A reconnecting registration cannot recreate, complete, or discard a lease
from an advertisement. Why: the held streaming channel is transport, not lease
or claim authority. Store mapping, the single runner-initiated outbound stream,
reconnect resynchronization, and exact wire correlations are later stacks.

## Session placement and affinity

`SessionRunnerPlacement` starts with one immutable request:

- a `RunnerSelector`, targeting a capability class or exact runner identity;
- `WorkingDirectorySelection`, either runner default or one exact bounded
  working-directory value;
- an optional `CredentialProfileName`; and
- `WorkspaceRequirement`, either none or a repository worktree.

The working-directory value is exact nonempty UTF-8, excludes U+0000, and is at
most 4,096 bytes. The domain does not apply host-platform path parsing. A
repository-worktree requirement carries one exact bounded repository key.

Before execution, placement is `Unpinned`. Attaching the first runner validates
the request against that runner's exact validated registration: selector,
credential-profile availability, and workspace capability must all match. The
transition produces `Pinned` state containing the runner, selected working
directory, credential-profile grant, tool inventory, and any provisioned
workspace. Once pinned, ordinary attachment accepts only that exact runner.
There is no automatic migration or class-based rescheduling to a different
runner (INV-044).

Runner loss is explicit state, not implicit reassignment. Marking the pinned
runner lost retains the prior placement and disables future lease creation. An
owner-directed replacement supplies a new validated registration, working
directory, credential-profile selection, tool inventory, and provisioned
workspace. It advances a positive placement revision and returns one
`RunnerPlacementChange` value carrying the complete before-and-after placement
facts needed for a later frontier-extending injected message. Reconstitution
rejects an unpinned revision other than one, a pinned or lost state that does
not match its request and validated capabilities, or replacement history whose
revision and runner facts disagree.

This stack proves that replacement must be explicit and produces the typed
change facts. Application orchestration that appends the corresponding semantic
message and context frontier is a later edge.

## Credential profiles and approval

A credential profile has two deliberately separate representations:

- the runner holds the profile's credential value, provisioned out of band; and
- the daemon holds only its checked name, selection, policy, grant, and audit
  facts.

No credential-value type exists in the runner-protocol domain. Advertisements,
registrations, placement, grants, leases, replacement changes, and
reconstitution inputs can carry only `CredentialProfileName` (INV-035).

One daemon catalog policy declares approval posture for exact
`(ToolName, CredentialProfileName)` pairs. The closed posture is `Automatic` or
`SessionPolicy`. `Automatic` records scoped policy approval without an approval
judge; `SessionPolicy` continues through confirmation or the session's existing
dangerous blanket. An absent pair also selects `SessionPolicy`. Profile policy
cannot make an undeclared tool available and cannot alter its effect class or
admissible loci.

Session creation snapshots the selected profile and validated advertised tool
set into one `CredentialProfileGrant`. The grant binds the session, runner,
profile, and positive grant revision. A runner that did not advertise the
profile cannot receive the grant. Dispatch authorization requires the current
active grant, the same runner and profile, and a tool present in the snapshot.
Every authorization resolves approval from the exact tool/profile pair.

Grant replacement is forward-only. It checks the current revision and installs
one complete later snapshot, returning a `CredentialProfileChange` with the
before-and-after profile and tool inventories for later frontier injection.
Revocation is also forward-only and gates future dispatch authorization.
Authorization already captured by a claimed lease remains valid for that
in-flight attempt; revocation neither rewrites nor cancels it. A revoked grant
cannot become active again. Complete reconstitution rejects foreign session or
runner facts, a profile absent from the validated registration, a tool set wider
than the advertisement, or a revoked projection with active-dispatch authority
(INV-045).

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
  storage, reconnect recovery, compatibility, and result envelopes are recorded
  in [Protocols and persistence](../open-questions.md#protocols-and-persistence)
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
- Workspace filesystem provisioning, cleanup recovery, and containment claims
  are recorded in [Tool safety](../open-questions.md#tool-safety).

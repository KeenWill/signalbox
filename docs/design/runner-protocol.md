# Runner protocol design

This design is not built; it extends
[runner protocol and placement](../spec/runner-protocol.md) with the lease and
dispatch machine, successor enrollment and replacement, healthy-session
relocation, workspaces, sandboxes, the egress broker, and forced Git
configuration.

## Goal

A runner executes tools for the sessions pinned to it under one serial lease
protocol whose every step is journaled on both sides, so a crash on either side
resumes from durable state and never repeats a side effect unknowingly. A lost
runner is replaced or promoted only by a user command, and a session whose
runner stopped advertising a required capability recovers on that same runner.
Each placement has exactly one writable root that the runner can re-identify
after a restart; a manifest-backed root is released when the placement retires
and reported as a leak when it cannot be released. Restricted tools run inside a
namespace with no host interface, reach the network only through a
hostname-checked HTTPS broker, and run Git only under configuration the runner
forces and a canonical-URL check the model cannot defeat.

## Design

### Lease and dispatch

The tool loop is serial: at most one live lease per session, one runner dispatch
at a time, and one durable terminal attempt before the next dispatch. A runner
holds one global execution permit, so tools of different sessions never execute
concurrently on it. A combined-locus tool runs on the session's attached runner
when that runner advertises it and otherwise runs on the daemon.

The daemon sends `lease_offer` with the complete lease correlation and the
immutable dispatch payload. The runner admits the exact tool, sandbox profile,
credential profile, and workspace, then replies `lease_claim`. The daemon
commits the claim before it sends `lease_claimed`, and receipt of that
acknowledgement is the runner's execution capability; an offer or a sent claim
without it authorizes nothing. Before accepting `dispatch` the runner fsyncs the
complete claimed correlation and the `waiting_dispatch` phase below its private
state root. The runner executes only when the claimed capability and the
dispatch carry the same complete correlation. It fsyncs `dispatch_received`
before acknowledging the frame internally and `execution_may_have_started`
immediately before it invokes the executor. The runner retains one terminal
evidence envelope and resends it until the daemon commits the matching attempt
and lease transition and replies `result_recorded`, then discards it.

The phases `waiting_dispatch` and `dispatch_received` prove only that the
journaled executor invocation had not started; `execution_may_have_started`
carries ordinary effect-class ambiguity. On reconnect the runner sends a bounded
inventory of at most one lease with its fsynced phase, one retained result, one
workspace operation, one operation failure, and one leak page. Canonical durable
state decides whether the daemon resends a claim acknowledgement, a dispatch, or
a result acknowledgement; advertisement and connection memory never recreate
authority. A reconnect inventory that omits a daemon-recorded claimed lease
cannot strand or repeat it: the daemon marks that lease lost and applies its
effect-class ambiguity law. A claimed lease reported without a terminal envelope
follows its fsynced phase.

The runner spools `workspace_leak_page`, `workspace_ready`,
`workspace_released`, `result`, and `operation_failed` until each is
acknowledged. A runner that cannot perform an admitted operation reports it with
`operation_failed` rather than sending nothing. A failure the daemon has durably
recorded resolves the corresponding provisioning, release, or lease authority as
refused, and neither side waits on it further.

### Successor enrollment, promotion, and replacement

Several runners are enrolled with one daemon at once.

After durable predecessor loss, one successor `enroll` may be admitted as a
provisioning-only pending replacement candidate. It receives the same identity
shapes plus a pending enrollment and pending registration revision; at most one
pending request exists, and equal replay returns its exact original receipt.
Pending authority admits heartbeat, startup leak reconciliation, and one
user-command-bound workspace operation, and never registration mutation, grant
creation, lease offer, claim, or dispatch.

`promote_pending_runner` is the user command for the fact that this daemon's
active runner is durably gone, or, with several runners, that one of this
daemon's active runners is gone and a successor for it is pending. It requires
the recorded active enrollment's connection to be durably lost and the pending
candidate to be connected, then revokes the predecessor and constructs the
active enrollment and validated registration from the exact pending facts in one
transaction. It provisions no workspace, consumes no receipt, touches no session
placement, creates no lease, and fabricates no turn or frontier. A session
pinned to the promoted predecessor stays lost until its own user replacement
runs.

For a pinned repository-backed loss, `replace_lost_runner` first durably claims
the user command and its complete request, then creates one single-use
provisioning authorization naming that command and the pending registration. The
runner provisions and spools `workspace_ready` under that limited authority. A
later transaction activates the pending enrollment: it rechecks the lost
predecessor and the connected candidate, consumes the exact workspace receipt,
revokes the predecessor, constructs the active enrollment and validated
registration from the pending facts, and installs the successor placement, grant
lineage, semantic `RunnerPlacementChanged` transcript entry, next context
frontier, and terminal command result atomically. The transcript entry is
reference-only and contains no credential value or unbounded runner output.
Pinned replacement provisions a fresh workspace at the successor revision when
the successor request requires one; pre-pin replacement provisions nothing until
initial dispatch and performs the promotion in its single terminal transaction.
A provisioning rejection or candidate loss records the typed terminal command
rejection, retires only the command's staging workspace through the release
path, and leaves the candidate pending for a later command. Process exit after
the command claim is recoverable: startup resumes the one nonterminal
replacement command from its durable authorization and receipt rather than
claiming again.

Re-registration triggers a loss with its own recovery: when a live runner stops
advertising a capability that a pinned placement requires, reconciliation marks
the placement lost while the connection and enrollment stay healthy. For that
loss source only, the replacement command may name the same runner identity;
every other loss source keeps the different-runner requirement. A checked
re-enrollment against the current connection revalidates the exact enrollment,
runner, and authentication-reference correlations, requires the current
registration to advertise every capability the successor placement request
needs, and installs the successor placement, grant lineage, and semantic
boundary exactly as a different-runner replacement does.

### Healthy-session relocation

`move_healthy_session` is the user command that re-places a healthy session on a
different runner; its same-runner form changes only the working directory. It
consumes positive placement revisions, the `RunnerPlacementChanged` boundary,
the runner event family, and the placement fields of session-creation records,
and adds no other contract. Its injected placement event never claims that
relocation deleted prior files.

### Workspaces

A session with no worktree has no session repository; it reaches one only by
cloning into its writable root under a runner-configured repository key and that
key's optional credential profile. A plain-directory workspace is no workspace
requirement paired with an exact working-directory selection: the runner
provisions nothing, never creates, renames, or deletes that directory, and
retiring the placement releases nothing.

A repository placement requires one checked single-use provisioning
authorization that binds the session, placement revision, runner, registration
revision, repository key, sandbox profile, and optional credential profile. It
authorizes only acquisition of that repository and no model-selected tool. The
runner accepts it only when the repository entry's optional profile equals the
authorization's optional profile: both absent authorize an anonymous clone, both
equal authorize the grant, and anything else is `credential_unavailable`. That
same equality governs every Git operation, whether the entry was reached through
the workspace manifest, the provisioning authorization, or a checked `git_clone`
argument. The runner rejects an unknown credential profile before accepting the
authorization and returns one `ProvisionedWorkspace` receipt whose manifest
facts match every correlation.

A complete workspace lives at
`sessions/<canonical-session-uuid>/<placement-revision>/repo` with its own
`.git`; no shared Git directory, linked-worktree administration, home path, or
credential-bearing remote URL is used. Provisioning creates a sibling staging
directory, clones under the restricted profile, writes a versioned `0600`
non-secret manifest in the non-mounted placement parent, fsyncs, and atomically
renames the placement directory before returning the receipt. Exact provisioning
replay returns the matching ready receipt, and conflicting facts fail closed. A
clone of an empty repository is an ordinary success whose manifest records the
unborn branch.

Exactly one writable root exists per placement: the provisioned repository, the
exact selected working directory, or a private root at the sibling path
`sessions/<canonical-session-uuid>/<placement-revision>/work` that the runner
creates on first use with the same manifest and no repository key, clone-URL
digest, or credential-profile name. Confinement is defined over that root, which
need not be a repository. Each root is identified by durable facts, a manifest,
the placement value that names the directory, or the private root's
deterministic path and manifest, never by process memory. A restarted runner
recomputes the private-root path from placement facts, authenticates it against
the manifest, and re-adopts the root with whatever the session wrote into it; it
never substitutes a fresh empty directory for one holding session files.

The manifest lifecycle is `staging`, `ready`, `active`, then `releasing`.
Transitions advance only in that order, equal replay retains the same value, and
deletion is represented by absence rather than a fifth token. Recovery resolves
the repository key again and requires the current canonical URL digest to equal
the manifest value; a changed mapping is `manifest_conflict`, never a
reinterpretation of an existing clone.

Runners are not cleanup authorities. Only the runner that provisioned a
workspace can delete it; a replaced, revoked, or dead runner leaves its
workspace on disk. No cleanup authority resumes for a retired identity, and no
mechanism transfers ownership of an existing clone to a successor. The leak
report is the whole response to a workspace left on disk.

### Release and leak reconciliation

The daemon enqueues a release only for an exact retired placement revision,
superseded by replacement or terminal abandonment, after no live lease or
unacknowledged result remains; the session itself may continue on its successor
placement or with daemon-only tools. A retired plain-directory placement has no
manifest and therefore no release: the daemon enqueues nothing, the runner never
renames or deletes it, and it is not reported as a leak. Reachability is a
second, independent precondition: a release is enqueued only while the runner
holding the workspace is still connected. Retirement whose predecessor
connection is already lost enqueues no release, and losing a connection that
still owed one retires that release as unowned; either way the workspace becomes
a recorded leak.

The runner fsyncs a `release_accepted` journal entry carrying the complete
release correlation before it does anything irreversible. Only then does it mark
release in the manifest, atomically rename the placement below `trash/`, fsync,
and delete it by descriptor-relative traversal that unlinks symlinks instead of
following them. It advances the entry to `release_completed` and resends
`workspace_released` until the daemon replies `workspace_release_recorded`,
which frees the journaled release and the runner's single workspace-operation
slot. A crash resumes from the journal rather than from a manifest the runner
may already have deleted: `release_accepted` resumes the deletion and then
reports, and `release_completed` resends the correlation. A release whose rename
or deletion keeps failing reports `workspace_cleanup_failed`; its
acknowledgement retires the release journal with the failure, and the surviving
placement is reported as a `cleanup_failed` leak.

Startup reconciles every ready or active manifest with the daemon before any
execution and reports every unknown, retired-but-present, conflicting, or
otherwise unreconciled workspace as a typed leak. The runner never silently
deletes a reported leak, and the startup report is visible even when no session
can be resumed.

### Sandbox profiles

For the restricted profile the runner launches every executable tool as a fresh
bubblewrap process that unshares the user, mount, PID, IPC, UTS, cgroup, and
network namespaces, drops capabilities, clears the inherited environment, mounts
fresh `/proc`, `/dev`, `/tmp`, and runtime directories, binds only the writable
root read-write, and binds configured toolchain and cache paths read-only. The
runner refuses restricted registration when the installed bubblewrap cannot
prove that namespace and bind behavior. File tools use descriptor-relative
traversal beneath the writable root and refuse symlinks, magic links, device
nodes, sockets, and path escape.

For `ambient` the runner uses one labeled bubblewrap supervisor but binds the
invoking user's filesystem and shares host networking, so it supervises without
confining. Its full user powers include read access to every same-user-readable
path, including ungranted runner credential files and daemon model-provider
credential files when their paths are discoverable. Explicit profile selection
accepts that exposure.

### Egress broker

The restricted network namespace has no host interface. A namespace-local shim
connects through one per-dispatch Unix socket to a runner-owned HTTPS broker.
The broker accepts only `CONNECT` to port 443, checks the requested hostname
before resolution, pins the resolved destination for that connection, parses the
TLS ClientHello, and requires its SNI to equal the admitted hostname. CONNECT
authorities are canonical ASCII DNS names, lowercase, with no trailing dot, no
empty label, and no IP literal, and a suffix match is label-boundary exact.
Resolution rejects unspecified, loopback, private, link-local, multicast, and
otherwise nonpublic destinations before pinning. Direct IP destinations,
plaintext forwarding, other ports, DNS rebinding, and missing or mismatched SNI
fail closed. The broker proves a TLS tunnel to the checked host and claims
nothing about the encrypted application protocol.

### Forced Git configuration and the canonical binding

Every Git invocation, in provisioning and in every Git tool, runs with its
effective configuration forced by the runner rather than validated afterwards.
The runner points `GIT_CONFIG_SYSTEM` and `GIT_CONFIG_GLOBAL` at `/dev/null`,
passes `protocol.allow=never`, `protocol.https.allow=always`, and
`protocol.ext.allow=never` on the command line, and disables repository-local
hooks. The same command line empties the accumulated credential-helper list
before installing the one helper that invocation should use, so the effective
helper set is exactly what the runner installed. The three helper cases are the
fixed runner-owned helper for a granted profile, the per-provisioning broker
helper, and no helper at all for an invocation that reaches no remote;
[configuration and credentials](../spec/configuration-and-credentials.md) owns
the helper and its forced `credential.useHttpPath`. Command-line configuration
takes precedence over model-writable repository configuration, so no repository
setting can move the transport off HTTPS or substitute an executable for a
fetch.

The `insteadOf` rewrite table cannot be emptied the way protocol and helper keys
are: it is an unbounded keyspace, so command-line configuration can add entries
but never subtract them. The canonical repository binding therefore has its own
check, independent of the transport configuration. Every invocation that reaches
a remote first selects exactly one repository entry: provisioning uses its
authorization's entry, an existing-worktree tool uses the key recorded in the
workspace manifest, and `git_clone` uses its checked argument. The invocation
then resolves the complete effective-URL sequence Git will use and requires
every member, byte for byte, to equal the canonical URL of the selected entry.
The runner asks Git for that resolution under exactly the forced configuration,
working directory, and repository selection the guarded invocation will use, so
each answer is post-rewrite. A literal URL resolves through
`ls-remote --get-url`; a named fetch enumerates `remote get-url --all`; a named
push enumerates `remote get-url --push --all`. An empty result, a count other
than one, or an unequal member fails before network use, and extra fetch or push
URLs are rejected, including a repeated canonical push URL. The check holds
because the resolve and use invocations are adjacent under the runner's one
global execution permit with repository hooks disabled, so no model-authored
code runs between them. The check binds the URL and not the bytes: fetching from
the right repository is no claim about what that repository serves.
[The Git authority threat model](../spec/git-authority-threat-model.md) owns the
attack narrative.

## Compatibility constraints

This design constrains sandbox, approval, workspace, credential, and generic
execution behavior, and its per-tool compatibility constraints are binding on
present code.

The frame vocabulary, correlations, phases, and inventory shapes in
`crates/runner-wire` stay compatible with the lease and dispatch machine above;
a change to them is a change to this design.

Positive placement revisions, the `RunnerPlacementChanged` boundary, the runner
event family, and the placement fields of session-creation records stay
compatible with a relocation that no loss caused.

The canonical binding check depends on the adjacency of its resolve and use
invocations, which the one global execution permit and disabled repository hooks
provide; code that admits concurrent tool execution on one runner or enables
repository hooks is a change to this design.

The gate that admits `enroll` only while no other active enrollment exists is a
development boundary; nothing built forecloses several runners enrolled at once.

The domain gate in `replace_lost_runner` that refuses a same-runner successor,
and the test that pins that refusal after registration-triggered loss,
contradict the same-runner recovery above and flip when the replacement command
is built.

Every transaction this design adds takes runner locks in the order
[persistence protocol](../spec/persistence-protocol.md) fixes and holds no
transaction open across runner I/O.

## Acceptance criteria

A session holds at most one live lease, a runner executes at most one dispatch
at a time, every lease phase is journaled before the step it names, and a
retained result is resent until `result_recorded`. After a runner or daemon
crash, reconnect reconciliation reaches the same durable state as an
uninterrupted exchange, and no lease is stranded or repeated.

A second runner enrolls while the first stays active, and every runner-scoped
fact stays per runner. A pending successor enrolls after durable loss, admits
only heartbeat, leak reconciliation, and one command-bound workspace operation,
and becomes active only through `promote_pending_runner` or
`replace_lost_runner`. A pinned session lost to re-registration is replaced onto
the same runner, without abandonment, once that runner advertises the required
capability again.

`move_healthy_session` relocates a healthy session or changes its working
directory with a `RunnerPlacementChanged` entry and no loss.

Every placement has exactly one writable root that a restarted runner re-adopts.
Provisioned workspaces live at the fixed path with a manifest, release deletes
only retired, reachable, manifest-named workspaces through the journal, and
every unreconciled workspace appears as a typed leak in the startup report.

Restricted tools run in a bubblewrap namespace with one writable root and reach
the network only through the broker; every surface names the unconfined profile
`ambient`.

Every Git invocation runs under the forced configuration, and every
remote-reaching invocation fails before network use unless Git's effective URL
sequence is exactly one canonical URL.

# Runner protocol design

This design is not built; it extends
[runner protocol and placement](../spec/runner-protocol.md) with concurrent
enrollment and active-turn replacement, healthy-session relocation, workspaces,
sandboxes, the egress broker, and forced Git configuration.

## Goal

A runner executes tools for the sessions pinned to it under one serial lease
protocol whose every step is journaled on both sides, so a crash on either side
resumes from durable state and never repeats a side effect unknowingly.
Restricted tools run inside a namespace with no host interface, reach the
network only through a hostname-checked HTTPS broker, and run Git only under
configuration the runner forces and a canonical-URL check the model cannot
defeat.

## Design

### Successor enrollment, promotion, and replacement

Several runners are enrolled with one daemon at once.

Pre-continuation takeover retains the pending relocation instead of appending
the entry or advancing the frontier. Continuation or batch terminalization
appends that entry exactly once after all batch results and before the next
model call or terminal marker.

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

[Workspace publication](../spec/runner-protocol.md#workspace-publication) owns
the ambient anonymous clone, protected manifest, and restart re-adoption.
Repository-free private-root acquisition is not built. Restricted acquisition
clones inside the restricted profile. Each placement has exactly one writable
root, whether a repository, selected plain directory, or private root.
Confinement is defined over that root.

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
fact stays per runner. A pending successor reconciles leaks without becoming
active.

`move_healthy_session` relocates a healthy session or changes its working
directory with a `RunnerPlacementChanged` entry and no loss.

Every placement has exactly one writable root that a restarted runner re-adopts.
Restricted tools run in a bubblewrap namespace with one writable root and reach
the network only through the broker; every surface names the unconfined profile
`ambient`.

Every Git invocation runs under the forced configuration, and every
remote-reaching invocation fails before network use unless Git's effective URL
sequence is exactly one canonical URL.

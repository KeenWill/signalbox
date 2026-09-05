# Git authority threat model design

This design is not built; it extends
[git-authority-threat-model.md](../spec/git-authority-threat-model.md) with
workspace minting, push by remote name, and workspace relocation.

## Goal

A person registers a workspace and mints push destinations scoped to it. The
daemon records the roots it derives. A push names a remote, and the daemon
resolves that name against the durable record. A moved workspace keeps its
identity and its grants.

## Design

Storage. The `workspace`, `configured_git_remote_mint`,
`configured_git_remote_withdrawal`, and `configured_git_remote_live` tables
exist. `workspace_id` is the primary key, `root_path` is unique, and a live
destination is keyed by workspace and remote name. Operator-registered minting
is the only tier that widens what Signalbox may push from, so an
operator-registered row carries the `register_workspace` command that created
it; a daemon-derived row carries no command.

Minting. Registration through the client resolves the root path once, following
symbolic links and removing `.` and `..` components, and stores the canonical
bytes. The store judges canonical form as bytes and cannot see the filesystem,
so a canonical spelling whose components are symbolic links is admitted; the
minting boundary is the only place that resolves them. Spellings that
canonicalization collapses are one key; two bind-mount paths to one directory
canonicalize to different bytes and stay distinct keys. At most one live
destination exists per workspace and name. The daemon inserts a daemon-derived
row for each per-session root its derivation materializes
([configuration-and-credentials.md](../spec/configuration-and-credentials.md));
those rows are bookkeeping, and no path reads them to decide a binding.

Withdrawal. Retiring a destination inserts a withdrawal that retires exactly one
mint and frees its name. A withdrawal and the replacement mint may commit in one
transaction.

Push by name. `GitPushArguments` gains the remote name beside the branch, and no
caller supplies a URL. The executor resolves the name to the live mint for the
session's workspace and fails with a typed error when none stands. A destination
stays `https` only, and the transport compiles no SSH support, so the store and
the transport refuse the same set.

Relocation. A relocation is a durable fact that binds an existing workspace
identity to a new canonical root; the identity and its grants stand. Registering
a durably relocated directory resolves to that identity instead of minting a new
one.

## Compatibility constraints

Until the resolver lands, the push executor is constructed with one validated
`ConfiguredGitRemote`, `GitPushArguments` carries a branch and nothing else, and
no caller path accepts a URL.

No present surface mints a workspace record, and nothing reads the workspace
tables to decide which roots the daemon may open.

`WorkspaceRootPath` admits canonical bytes only and performs no normalization;
no comparison-time normalization is added anywhere.

`WorkspaceOrigin` enumerates both variants without a wildcard, so a further tier
cannot default to carrying no human act.

Sessions never mint a workspace or a destination.

The identity generators for `WorkspaceId`, `GitRemoteMintId`, and
`GitRemoteWithdrawalId` land with the store and the operator verbs
([identity-and-commands.md](../spec/identity-and-commands.md)).

## Acceptance criteria

Registering a workspace through the client stores the canonical root once,
records the registering command, and refuses a second spelling that
canonicalization collapses onto an existing key.

A push carrying a remote name resolves to the one live mint for that name in the
session's workspace, or fails with a typed error; a URL from any caller is
rejected.

Withdrawing a mint retires exactly one destination and frees its name; a
withdrawal and a replacement mint commit together.

Every per-session derived root has a daemon-derived row, and no read path
consults those rows for a binding.

Relocating a workspace records the new canonical root under the existing
identity, and its minted destinations keep resolving.

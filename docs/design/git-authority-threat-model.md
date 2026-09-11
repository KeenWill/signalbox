# Git authority threat model design

This design is not built; it extends
[git-authority-threat-model.md](../spec/git-authority-threat-model.md) with
daemon-derived workspace records, push by remote name, and workspace relocation.

## Goal

The daemon records the roots it derives. A push names a remote, and the daemon
resolves that name against the durable record. A moved workspace keeps its
identity and its grants.

## Design

The daemon inserts a daemon-derived row for each per-session root its derivation
materializes
([configuration-and-credentials.md](../spec/configuration-and-credentials.md));
those rows are bookkeeping, and no path reads them to decide a binding.

A withdrawal and the replacement mint may commit in one transaction.

Push by name. `GitPushArguments` gains the remote name beside the branch, and no
caller supplies a URL. The executor resolves the name to the live mint for the
session's workspace and fails with a typed error when none stands. The store and
transport use the same destination type for HTTPS and SSH.

Relocation. A relocation is a durable fact that binds an existing workspace
identity to a new canonical root; the identity and its grants stand. Registering
a durably relocated directory resolves to that identity instead of minting a new
one.

## Compatibility constraints

Until the resolver lands, the push executor is constructed with one validated
`ConfiguredGitRemote`, `GitPushArguments` carries a branch and nothing else, and
no caller path accepts a URL.

Nothing reads the workspace tables to decide which roots the daemon may open.

`WorkspaceRootPath` admits canonical bytes only and performs no normalization;
no comparison-time normalization is added anywhere.

`WorkspaceOrigin` enumerates both variants without a wildcard, so a further tier
cannot default to carrying no human act.

Sessions never mint a workspace or a destination.

## Acceptance criteria

A push carrying a remote name resolves to the one live mint for that name in the
session's workspace, or fails with a typed error; a URL from any caller is
rejected.

A withdrawal and a replacement mint commit together.

Every per-session derived root has a daemon-derived row, and no read path
consults those rows for a binding.

Relocating a workspace records the new canonical root under the existing
identity, and its minted destinations keep resolving.

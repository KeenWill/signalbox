# Git authority threat model

The Git authority lets a session read and write one pinned repository through
typed operations that cannot choose which repository they touch or where a push
goes.

## Map

The Git tool family is a suite of typed operations constructed over one
workspace root; `LocalGitTools` is that suite. The deployment injects the root
when it constructs a suite, and the daemon composes one suite per root it
serves; [configuration-and-credentials.md](configuration-and-credentials.md)
states how a session's root is derived. A suite reaches a session through the
tool loop ([tool-loop.md](tool-loop.md)), which owns approval and dispatch.

Repository paths and repository data are untrusted input. The trusted computing
base is the local process, the kernel, the filesystem, the cryptographic hash
implementations, and the typed Git library.

A suite has two layers. The authority layer opens the live repository
administration tree through pinned directory descriptors and captures
configuration, references, lock state, and object data into private snapshots;
the typed Git library, `git2`, works only on those snapshots.

Pushing is a separate surface with its own authority. A push names a branch; its
destination is a remote a person minted, scoped to a durable workspace record.
The grant is keyed by the record's identity, not its path.

## Decisions

`git2` is trusted for typed Git semantics only after authority capture: parsing
the configuration snapshot, reading and writing private index snapshots, reading
objects from the captured object database, and hashing or decoding objects. It
never discovers a repository or opens the live administration tree by an ambient
or caller-selected path. The suite never searches the current directory,
ancestors, environment, home directory, or process-global Git state for a
repository.

Live administration reads are implemented in the authority layer rather than
through a path-based repository API, because such an API cannot express the
descriptor-pinned, no-follow, bounded-read contract. A review request to move a
live read back to a path-based library, absent a stated invariant violation, is
dispositioned as an accepted residual citing this page.

No operation accepts a command line, shell fragment, executable, repository
path, or remote destination, and the implementation never spawns a Git binary.

Minting a destination is a human act; pushing to a minted destination is an
approval-gated agent act. A session cannot mint a workspace or a destination;
session-facing minting, if ever admitted, is a posture-gated tool decided
separately.

Workspace roots are globally unique by canonical spelling, and the key carries
no runner or location dimension.

Three Git behaviors bind any Git transport Signalbox runs. A repository-local
`credential.helper` value beginning with `!` is a shell snippet Git executes, so
a transport that leaves the helper list in place lets an auto-approved fetch run
model-authored code. `pushurl` is multi-valued, Git pushes to every value, and
`remote get-url --push` returns only the first, so checking one push URL proves
nothing about the rest. A canonical URL repeated in that list makes Git invoke
the destination twice, so the repetition is rejected: the second invocation
could report a known failure after the first had already changed external state.

Signalbox does not isolate a repository from every process that can write it:
another same-authority or privileged process can mutate repository data after
the final validation or after an operation returns. Descriptor pinning does not
sandbox a hostile same-UID process, stop writes through pre-existing hard links
or open descriptors, or survive a compromised kernel or library.

Bounded scans and bounded content limit Signalbox's own work and do not
guarantee repository availability. Unsupported layouts and formats, exhausted
bounds, allocation failure, and host I/O failure are rejected, and the tool does
not repair a corrupt repository.

Repository semantics outside the direct main-worktree subset are unsupported,
not partially trusted. Linked worktrees, discovery, alternate object databases,
replacement-object configuration, and other rejected extension surfaces need a
separate user-approved contract before support.

Remote authentication, transport security, server-side authorization, and remote
repository behavior are not properties of the local authority;
[web-egress-threat-model.md](web-egress-threat-model.md) and
[configuration-and-credentials.md](configuration-and-credentials.md) own egress
and credential scope.

The contracts below are the security acceptance boundary. A finding that
demonstrates a violation is must-fix however many review waves have passed, and
a newly found gap in a mechanism enforcing one of them is in-scope hardening
fixed in the current pull request's disposition commit. A finding that violates
no stated contract, names no in-scope enforcement gap, and contradicts no
implemented contract is an accepted residual resolved without code change. That
classification never covers a reproducing violation or a defect this subsystem
introduced.

## Contracts

The Git family operates only on a direct main worktree whose `.git` directory is
immediately inside the root its suite was constructed with. The root is
construction input and never a per-call argument, so a local operation cannot
select another repository. Composing several suites does not weaken this: each
suite is a separate construction, and no suite can reach another's root.

Every admitted Git action is a fixed typed operation with a compiled argument
schema and a typed result or failure. Text fields such as a commit message are
data: bounded, recorded verbatim, and never interpreted as configuration or
instructions.

Live administration reads open relative to pinned directory descriptors with
`O_NOFOLLOW`, bounded reads, identity snapshots, and path revalidation.

`gix-validate` owns reference-name grammar and `gix-hash` owns object-ID
grammar; Signalbox adds the declared-format, `refs/` namespace, and UTF-8
representability checks. The pinned repository configuration selects SHA-1 or
SHA-256 once.

Mutable `HEAD` and reference values are pinned for the duration of an operation,
not frozen for the suite's lifetime. A completed typed publication is the
baseline the next operation observes, and an operation guard still rejects a
concurrent transition.

At its validation points the suite fails closed on observed concurrent
replacement or in-place change and preserves entries it no longer owns.

An external write names a branch and a minted remote, never a destination; the
endpoint is durable authority recorded ahead of the call. The push requires
explicit approval that no policy overrides; [tool-loop.md](tool-loop.md) owns
the approval mechanism.

Operator-registered minting is the only tier that widens what Signalbox may push
from, so an operator-registered workspace record carries the durable command
that registered it. A daemon-derived workspace row records what the per-session
derivation produced; nothing reads it to decide which roots the daemon may open.

Grants are scoped by workspace identity, so a grant does not survive a move of
the directory and must be minted again under the new workspace.

## Not built

- Push by remote name, resolved against the durable record
  ([design](../design/git-authority-threat-model.md)).
- Workspace minting, with the root canonicalized once at minting so later scope
  comparisons are between identities
  ([design](../design/git-authority-threat-model.md)).
- Workspace registration, recording the canonical root a person resolved at the
  time ([design](../design/git-authority-threat-model.md)).
- Relocation as a durable fact that preserves workspace identity
  ([design](../design/git-authority-threat-model.md)).

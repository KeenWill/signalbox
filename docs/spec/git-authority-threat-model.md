# Git authority threat model

The Git authority lets a session read and write one pinned repository through
typed operations that cannot choose which repository they touch or where a push
goes.

## Overview

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
the typed Git library, `git2`, works only on those snapshots. All operations
capture objects on demand into a private database and revalidate their source
bindings before returning; unrelated historical object contents are not copied.
Packed delta traversal retains layer offsets and decodes one layer at a time;
open descriptors do not grow with delta depth. Pack and index files open on
demand against captured identities; idle pairs retain no descriptors. Blob
decoding, delta reconstruction, worktree reads, checkout, and object publication
use temporary files and fixed-size I/O buffers.

Pushing is a separate surface with its own authority. A push names a branch; its
destination is a remote the deployment configured, never one the caller chose. A
minted destination is scoped to a durable workspace record, and its grant is
keyed by the record's identity, not its path.

A mutation reports ambiguous evidence when publication fails and rollback does
not establish restoration of its reference, reflog, index, or worktree changes.
Repository validation failure after index publication is ambiguous. Failures
before publication retain their known-failure classification.

## Design decisions

`git2` is trusted for typed Git semantics only after authority capture: parsing
the configuration snapshot, reading and writing private index snapshots, reading
objects from the captured object database, and hashing or decoding objects. It
never discovers a repository or opens the live administration tree by an ambient
or caller-selected path. The suite never searches the current directory,
ancestors, environment, home directory, or process-global Git state for a
repository.

Live administration reads are implemented in the authority layer rather than
through a path-based repository API, because such an API cannot express the
descriptor-pinned, no-follow, bounded-read contract.

No operation accepts a command line, shell fragment, executable, repository
path, or remote destination. Local operations never spawn a Git binary. The
daemon push transport runs Git from the pinned checkout root with a private
administration directory and captured object database, excluding checkout
configuration, credential helpers, hooks, and redirects. Push credentials and
the destination come from the watched repository's deployment configuration; the
retained dispatch head fences the branch.

For dispatched pushes, object capture follows the branch's commits after the
retained dispatch head. Merge paths stop at ancestors shared with that head. The
fence and shared-ancestor boundary commits retain their tree metadata for
negotiation; earlier commits and unchanged boundary blobs are omitted. Selected
packed objects and their delta dependencies retain the content bounds.
Pack-index lookups do not count unrelated entries against the push-range
inspection bound. The transport disables delta compression against omitted
historical blobs. Object-ID collections compare the object format and its active
identifier bytes.

Push layout validation, reference resolution, and object capture run off the
async worker with a 300-second preparation deadline; expiry returns
`PreDispatchInfrastructure` without invoking the transport.

Minting a destination is a human act, and a session cannot mint a workspace or a
destination; pushing to a minted destination is an approval-gated agent act.

Workspace, mint, and withdrawal rows are append-only; retiring a destination
records a withdrawal rather than editing or deleting the mint. The live
destination table is derived from those facts, so a mint stands in it until its
withdrawal is recorded.

A push destination is `https` only; the durable mint and the configured remote
judge a URL by one type, so both refuse the same set.

Workspace roots are globally unique by canonical spelling, and the key carries
no runner or location dimension. Why: the single-runner rule means no two
machines present the same root; scoping roots per runner belongs to
[runner-protocol.md](runner-protocol.md).

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

Blob preparation and range reads trust ingest-verified catalog evidence and
filesystem length and inode checks. A same-UID writer changing blob bytes in
place after ingest is an accepted residual; explicit operator reads verify the
complete blob's SHA-256.

Scans and result text remain bounded; worktree, staging, and object-database
content have no aggregate byte ceiling. Commits, trees, and tags retain a 1 MiB
decoded metadata bound before libgit2 parsing; blob content streams without that
structural bound. Patches preview bounded content prefixes with truncation
markers, and status identifies renames by exact object identity. Worktree
streams pin one descriptor and revalidate its identity around each page; object
publication streams each batch into one pack and index pair. Skipping an
already-present object requires decoding and hashing its live content; a loose
pathname or pack-index hit alone is insufficient. Checkout retains the clean
path identity and revalidates the opened file and path before truncating or
removing it; removal quarantines and revalidates the full file snapshot, and new
files are created exclusively. Copied files retain descriptor ownership and pass
pathname, identity, and target-blob hash checks; captured checkout content must
match the target before index publication. Merge verification retains bounded
previews and uses file-backed comparison scratch data with linear-space
divide-and-conquer line matching; disjoint replacements use a linear scan. A
fixed budget bounds frontier visits and matching line comparisons across each
streamed file diff; exhaustion compares the whole-object transition instead of
line effects. Private-pack writes and merge comparison check preparation
deadlines between fixed-size pages; rename similarity streams fixed-size
signatures cached by object ID for each comparison pass. Unsupported layouts and
formats, exhausted bounds, allocation failure, and host I/O failure are
rejected, and the tool does not repair a corrupt repository. The configured
`max_git_object_bytes` limit (`"none"` for unbounded) applies to the objects an
operation reads, including packed delta bases, intermediate results, and delta
instructions, not to unrelated objects retained in its history.

Repository semantics outside the direct main-worktree subset are unsupported,
not partially trusted. Linked worktrees, discovery, alternate object databases,
replacement-object configuration, and other rejected extension surfaces need a
separate user-approved contract before support.

Remote authentication, transport security, server-side authorization, and remote
repository behavior are not properties of the local authority;
[web-egress-threat-model.md](web-egress-threat-model.md) and
[configuration-and-credentials.md](configuration-and-credentials.md) own egress
and credential scope.

The contracts below are the security acceptance boundary. A demonstrated
violation of one, or a gap in a mechanism enforcing one, is a defect. A finding
that violates no contract and contradicts no implemented contract is an accepted
residual.

## Boundary contracts

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

An external write names a branch and nothing else. Its destination is the one
validated remote the deployment configured when it constructed the push
executor, never a caller argument. The push requires explicit approval that no
policy overrides; [tool-loop.md](tool-loop.md) owns the approval mechanism.

Grants are scoped by workspace identity, so a grant does not survive an
unrecorded move of the directory and must be minted again under the new
workspace.

Operator workspace registration canonicalizes the root once in the daemon
filesystem and stores its unique spelling with the registering command.
Workspace comparisons use the resulting identity. Git remote minting records one
HTTPS destination per workspace and name; withdrawal retires exactly one mint
and frees its name. Neither operation changes which roots the daemon may open.

## Planned

- Push by remote name, with the endpoint resolved against the durable minted
  record ahead of the call ([design](../design/git-authority-threat-model.md)).
- Relocation as a durable fact that preserves workspace identity
  ([design](../design/git-authority-threat-model.md)).

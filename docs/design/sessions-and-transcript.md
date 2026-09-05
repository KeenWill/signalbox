# Sessions and the transcript design

This document describes committed work that is not built; it extends
[sessions-and-transcript](../spec/sessions-and-transcript.md) and is deleted
when the work lands.

## Goal

Ten capabilities extend the session and transcript subsystem. Instruction-aware
defaults replacement keeps a session's model selection compatible with its
admitted workspace instructions. Program creation causes let registered programs
create sessions under the [program substrate](../spec/program-substrate.md). The
browser follow route is used only by the open workspace. The timeline reports
referenced blob facts from a durable relation. Search producers publish
attachment and derived-text classes through the projection-writer port. A
relocation boundary entry records every session move in the transcript.
Delegation result sealing consumes a durable reconstituted terminal result. A
spawned child defaults into its parent's directory. A static eligible-failure
producer terminalizes a turn at eligibility, and a wait-transition failure
producer terminalizes a turn whose predecessor model call already issued.

## Design

Instruction-aware replacement. A replacement for a session with a nonempty
admitted instruction set rejects its proposed model selection unless every
target the current configuration can select from that direct selection or alias
has a typed system-instruction transport and capacity for the complete retained
workspace-instruction region. The check runs before the successor epoch commits
and under the same serialization the epoch commits under, so an admission or
activation occurs wholly before or after it and cannot invalidate the evidence
checked. Rejection is typed and leaves the current defaults and the admitted set
unchanged. Acceptance-time validation in
[configuration-and-credentials](../spec/configuration-and-credentials.md)
repeats the same check for each later origin after resolving its alias against
the then-current catalog, so replacement-time validation never stands in for it
after an alias retarget or a daemon restart. The rows that serialization takes,
and their order, belong to the lock protocol in
[persistence-protocol](../spec/persistence-protocol.md).

Program creation causes. The creation-cause vocabulary gains workflow and eval
variants for sessions created by registered programs. Each names the creating
program run, and the eval variant also names the trial identity the
[evaluation system](../spec/eval-system.md) defines. Both are constructible only
by the program substrate's host-side session capability and join the stored
closed-discriminator convention beside the three present spellings.

Follow route. Only the open workspace subscribes to a session's follow stream;
no other browser surface holds a follow subscription for the selected workspace.

Timeline blob relation. A durable timeline-to-blob relation supplies the
referenced blob count and byte length reported by the session summary read. The
two remain separate facts, and a nonzero byte length describes a reference, not
materialized bytes.

Search projection-writer port. Producers that own attachment filenames,
attachment media metadata, and derived text artifacts publish those classes
through the typed projection-writer port, so a read returns them with a reveal
address like every other class. A producer adopting the port publishes only text
its durable contract explicitly supplies, and only after its own source exists.

Relocation boundary entry. One entry kind references the complete checked
successor placement record at a relocation boundary, and the referenced record
is the authority for whether the runner or the working directory moved. Every
transaction for a pinned loss replacement or a user-directed move of a healthy
session or of its working directory appends one such entry after the latest
authoritative semantic frontier, or establishes a one-entry root when no
frontier exists, and advances a session placement-frontier pointer with the
placement revision. Active continuation and the next eligible origin both extend
that exact boundary before any execution on the successor placement. A
same-revision, missing-record, non-prefix, cross-session, or second placement
boundary fails closed. When the installing command runs while an authorized
model call is in flight, the boundary is appended only after that call's
observation commits, so the call's own entries precede it and the prefix-only
law holds. The entry copies no runner advertisement, workspace path, credential
fact, or tool output; the placement record remains its content authority, and
the checked placement transactions are its only producers. The provider
projection resolves the record to a rendered placement event; that rendering is
planned on [model-call-execution](../spec/model-call-execution.md).

Delegation terminal-result reconstitution. A durable reconstitution surface
yields one sealed projection of an ended call and its turn. Result sealing
consumes that projection and never accepts parallel raw identities or semantic
entries as proof of a terminal outcome.

Parent-directory default. A spawned child is placed in its parent's directory. A
pathless parent yields a pathless child, and a child of a parent in the root
directory carries the parent's acknowledged global-read root rather than
deriving a new one-segment path. The derived placement carries only the path and
does not copy the parent's complete placement. The session-placement surface
implements it.

Static eligible failure. A turn that fails at eligibility, before any attempt
exists, is terminalized by one transaction that commits its origin entries and
its failed marker together and emits the turn-failed update event atomically,
under the same marker-uniqueness and turn-state agreement rules as the four
present producers. A delegation-wake turn commits every coalesced delivery as an
origin entry, in recipient delivery sequence. A delegated child's first turn
keeps the `DelegatedTask` entry the spawn transaction already committed, so its
failure transaction appends only the marker.

Wait-transition failure. A turn whose frozen credential pool is exhausted when
its durable wait releases, and whose predecessor model call already issued, is
terminalized by one transaction that consumes the wait, opens and ends a fresh
call-free attempt, appends the failed marker, and emits the turn-failed update
event atomically. The fresh attempt's wait-release continuation origin, not that
marker, names the predecessor call, its cause, and its non-acceptance proof; the
earlier model-call known-failure closure committed without terminalizing and
cannot serve a transition happening now. Where the chain has issued no call, the
same transaction consumes the wait and opens and ends a fresh call-free attempt,
and the pre-call exhaustion producer appends its marker. On both paths that
transaction also reclassifies any steering still pending on the source turn as a
queued successor. The release and exhaustion conditions belong to
[credential-availability](../spec/credential-availability.md).

## Compatibility constraints

Creation-cause readers do not assume the present vocabulary is final, and the
stored discriminator's decode surface stays extensible without reinterpreting
existing spellings. No present replacement path performs the
instruction-capacity check, because no present surface admits a bundle; a
replacement path added now must be able to run inside the serialization the
admitted set will use. The session summary read keeps referenced blob count and
byte length as separate fields, both zero. No present producer calls the
projection-writer port; a producer adopting it publishes only text its durable
contract supplies. No present writer emits a relocation entry, a
placement-frontier pointer, or an entry for a queued turn; the semantic payload
set stays closed until a migration widens it. No present surface exposes
terminal-result reconstitution, and no new delegation path may seal a result
from raw identities. No present delegation or placement surface derives the
parent-directory default. The static eligible-failure and wait-transition
failure paths have no producer, and every present failed-turn producer keeps
emitting the turn-failed event atomically with the marker.

## Acceptance criteria

A replacement against a session with a nonempty admitted set and a selection
lacking transport or capacity is rejected with a typed rejection that leaves
defaults and admitted set unchanged, a compatible selection succeeds, and a
concurrent admission is ordered wholly before or after the replacement. A
program-created session stores a workflow or eval cause naming its program run
and, for eval, its trial; the three present spellings decode unchanged, and no
surface outside the host-side capability can construct the new causes. A browser
with no open workspace holds no follow subscription, and exactly one while a
workspace is open. The session summary read reports referenced blob count and
byte length equal to the relation's totals, nonzero for a session whose timeline
references blobs, and no detail read fetches blob bytes. Attachment filename,
media metadata, and derived-text classes appear in search results with reveal
addresses only after their source committed. Every pinned replacement and every
user-directed move appends exactly one relocation entry, the next model call
reads a frontier containing it, each fail-closed case is rejected, and the entry
carries no runner, workspace, credential, or tool content. Replacing a runner
lost before the session pinned it appends no entry and returns the placement to
unpinned at the successor revision. Delegation result sealing reads one sealed
projection and has no raw-identity path. A spawned child sits in its parent's
directory, is pathless when its parent is, and copies no other placement axis. A
turn that fails at eligibility carries its origin entries, one for an accepted
input, every coalesced delivery in sequence for a delegation wake, and the
checked delegated-task entry for a delegated child's first turn, and one failed
marker committed together with a turn-failed event and no attempt row. A turn
released from a wait with an exhausted pool and an already-issued predecessor
call carries one failed marker committed with a fresh call-free ended attempt, a
turn-failed event, and no terminal model call.

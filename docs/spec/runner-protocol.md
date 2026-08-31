# Runner protocol and placement

The user-vocabulary surface on this page was re-verified through PR #378
(`agent/user-vocabulary`).

This page specifies the implemented runner-protocol domain foundation as
verified against the implementing stack through PR #260
(`agent/runner-protocol-domain`); its durable Postgres representation and
restart-recovery authority were verified through PR #267
(`agent/runner-persistence`). The durable connection/loss-head offer and claim
fences are re-verified through this PR (`agent/runner-loss-epoch`). Sandbox,
repository-entry, permission-override, manifest-recovery, structural wire, and
persistence-adapter contracts were re-verified through PR #350
(`agent/runner-wire-protocol`). The wire lease correlation's complete execution
placement facts are re-verified against the parent slice
(`agent/runner-lease-execution-correlation`); its domain and durable
reconstitution fences are re-verified through this PR
(`agent/runner-lease-domain-correlation`). Existing-pin attempt-and-offer
atomicity is re-verified through this PR
(`agent/runner-pinned-dispatch-transaction`). Durable lease-claim admission is
re-verified through this PR (`agent/runner-lease-claim-transaction`). The atomic
claimed-lease and physical-attempt result boundary is re-verified through this
PR (`agent/runner-lease-result-transaction`). The corrected reconstitution
mismatch contract was re-verified through PR #322 (`agent/docs-discipline`;
pinned and pinned-loss request mismatches). The placement loss-source, pre-pin
replacement and abandonment state shapes, and append-only reconstitution-history
contract are re-verified through this PR (`agent/runner-placement-loss-domain`).
It owns logical runner enrollment, daemon-authoritative catalog validation,
runner leases, the independent session-composition axes, session placement and
affinity, credential-profile grants, and workspace requirements. The tool
registry's common declarations remain owned by [tool loop](tool-loop.md);
session transcript and frontier mechanics remain owned by
[sessions and transcript](sessions-and-transcript.md); physical tool attempts
remain owned by [tool loop](tool-loop.md). Invariant tags cite
[the invariant test index](../invariants.md).

The typed `ReplaceLostRunner`, `RunnerReplacementTarget`, and
`AbandonLostRunner` domain command payloads are verified against this PR
(`agent/runner-replacement-domain-contract`). The closed abandonment result and
rejection payloads, its application transaction boundary, and its atomic
PostgreSQL command transaction are verified against this PR
(`agent/runner-abandonment-transaction`). Its process-protocol adapter is
verified against this PR (`agent/runner-abandonment-process`). The closed
pre-pin replacement result and rejection payloads, application transaction
boundary, and atomic PostgreSQL transaction for a different exact live runner
are verified against this PR (`agent/runner-pre-pin-replacement`).
Pending-enrollment activation inside that pre-pin replacement transaction is
verified against this PR (`agent/runner-pending-pre-pin-replacement`). The
checked same-runner registration-recovery domain transition is verified against
this PR (`agent/runner-same-runner-domain`). The checked repository-workspace
provisioning authorization for a distinct successor or the narrow same-runner
recovery is verified against this PR
(`agent/runner-replacement-workspace-authorization-domain`). The application
coordination boundary that supplies one fresh authorization identity to the
atomic staging transaction is verified against this PR
(`agent/runner-replacement-provisioning-application`).

The persistence adapter consumes the checked same-runner transition only while
staging a pinned repository workspace. **Committed unimplemented
functionality.** No current adapter completes pinned replacement or handles the
replacement process request. Future adapters must preserve the closed
constraints below for those slices.

Pending enrollment admission was verified against the parent slice
(`agent/runner-pending-successor-promotion`); its deployment-scoped activation
transaction is verified against this PR
(`agent/runner-pending-successor-activation`), and its process request is
verified against this PR (`agent/runner-pending-successor-process`).

The connection-loss persistence transaction was verified against this PR
(`agent/runner-loss-session-transaction`). Daemon paging of its durable cursors
and startup resumption were verified against this PR
(`agent/runner-loss-daemon-propagation`). Registration-triggered placement
reconciliation, including exact durable cause authentication and restartable
daemon paging, was verified against this PR
(`agent/runner-registration-reconciliation`). Pending-successor admission after
exact durable predecessor loss was verified against this PR
(`agent/runner-pending-successor-promotion`).

Exact-address routing from a daemon operation producer to the established
socket-owning connection task is verified against this PR
(`agent/runner-connection-broker`).

The registration-only executable slice is verified through PR #376
(`agent/runner-daemon`). It adds the dedicated local listener, durable
idempotent enrollment receipts, exact resume and replacement-advertisement
registration, the `signalbox-runner` binary, explicit credential/repository
availability, and heartbeat liveness exchange with durable connection epochs,
shutdown, suspect, and loss facts. Its fatal stale-shutdown close, complete
rejection correlations, and lifecycle observability are re-verified through PR
#382 (`agent/runner-honesty`). Recovery inventory, workspaces, leases,
execution, and model calls remain unimplemented as labeled below. Remote
transport and dynamic policy stay under [Open edges](#open-edges).

The additive persisted `AlwaysConfirm` declaration vocabulary is verified
through PR #366 (`agent/exec-tools`). The runner workstation-registry
reclassification is verified through this PR (`agent/daemon-wiring`).

## Version-one executable boundary

Version one runs one `signalbox-runner` on the same Ubuntu host and under the
same effective user as `signalboxd`.

No present runner surface provides workspace, Git, shell, build, test, or model
execution. A future runner-side workstation registry and executor are
unimplemented and undecided. The committed unimplemented runner foundation below
continues to constrain sandbox, approval, workspace, credential, and generic
execution behavior. Its existing per-tool compatibility constraints remain
binding; the remaining registry inventory, any additional tool names,
unconstrained argument details, and execution deadlines are undecided. That
registry work is recorded under
[Scheduling and runners](../open-questions.md#scheduling-and-runners).

The tool loop remains serial: the daemon offers at most one live lease for a
session, the runner executes at most one dispatch at a time, and one result
reaches a durable terminal attempt before the next dispatch. Version one has no
Mac runner, remote transport, concurrent execution, or MCP locus. The additive
wire types do not encode a same-host assumption, but no unused remote mechanism
or negotiation surface is designed.

### The singleton-runner rule is temporary

The single-runner rule is a short-term development boundary, not a design
commitment. Several runners enrolled with one daemon simultaneously is required
functionality on a medium-term horizon at the latest, and nothing may be built
that forecloses it. The single active enrollment, and the rejection of a second
healthy `enroll` below, are version-one artifacts that lift when multi-runner
enrollment lands; they are not a statement that a deployment has one runner.
Every runner-scoped fact — identity, enrollment, registration revision,
connection and loss state, advertisement, workspace root — is therefore already
per runner rather than per deployment, and a contract that reads "the runner" as
a deployment-wide singleton is a defect to be repaired rather than a rule to be
preserved. Why: an agent that mistakes this boundary for a decision attaches
deployment-scoped meaning to runner-scoped facts, and every such attachment has
to be found and undone before a second runner can connect.

### Committed functionality beyond version one

Nothing in this section describes implemented behaviour. Version one has no
`move_healthy_session` request, no client may send one, and no daemon or runner
surface answers one, exactly as no second simultaneous enrollment exists above.
Every statement here is a constraint on future change rather than a capability
present today, and that constraint is the only force it carries: the version-one
mechanisms it names may not be altered in a way that forecloses these
commitments. What remains genuinely undecided about them — workspace portability
between runners, and any automatic-placement policy — stays in
[open questions](../open-questions.md#scheduling-and-runners), which remains the
one home for undecided design; a commitment whose implementation is deferred is
not an undecided question.

User-directed relocation of a healthy session is committed functionality that
version one does not implement. `move_healthy_session` is the user command that
re-places a healthy session on a different runner; its same-runner form changes
only the working directory. Version one's sole placement-change producer is loss
replacement ([session placement and affinity](#session-placement-and-affinity)),
so no implementation exists here. The mechanisms the command will consume —
positive placement revisions, the `RunnerPlacementChanged` semantic boundary,
the runner event family, and the placement fields carried by session-creation
records — are specified so that adding it changes no other contract, and every
later change to those mechanisms must remain compatible with a relocation that
no loss caused. Its model-facing consequence distinguishes retired placement
authority from filesystem reachability: the injected placement event never
claims that relocation deleted prior files
([model-call execution](model-call-execution.md#frontier-rendering)).

Successor promotion stays user-initiated in every form. Fresh-install enrollment
is instantly active; only a successor after loss waits, and it waits on a user
command rather than on a daemon decision. No mechanism in this page migrates a
session, promotes a candidate, or reschedules work automatically.

## Local transport and connection protocol

`signalboxd` binds a dedicated runner Unix domain stream socket, distinct from
the client socket. `signalbox-runner` dials that socket and never listens. The
daemon applies the process socket's owner-private canonical parent, trusted
ancestry, exact `0600` node mode, sidecar exclusive `flock`, rename-resistant
path identity, effective-user peer check, and identity-safe cleanup discipline.
Configuration rejects any intersection between the two canonical public socket
paths and their adjacent `.lock` and `.identity` artifacts, including paths
whose parent aliases resolve to the same directory. The runner verifies the
connected socket's pinned effective-user ownership, mode, and path identity
before sending enrollment. The local effective user is the version-one trust
boundary; the opaque authentication-reference identity is correlated with the
stored enrollment but is not treated as a secret.

The daemon admits at most 64 concurrent runner connection tasks and allows ten
seconds for the first complete frame; at the task limit it pauses acceptance,
and an incomplete handshake expires closed. A malformed initial or established
frame receives `malformed_frame`, while an unsupported envelope version receives
`unsupported_version`; both use unavailable correlation and then close. An
orderly shutdown signals and drains admitted tasks for at most five seconds. A
fatal connection task or listener-accept failure does the same before its
failure is propagated. Expiry aborts the remaining tasks and returns typed
drain-timeout evidence that retains any initiating failure; every runtime exit
applies identity-safe listener-path cleanup.

Runner-wire version one is newline-delimited UTF-8 JSON with a required
`version` and closed tagged message vocabulary. Each complete line, including
its newline, is at most 8 MiB. Unknown versions, message kinds, fields, enum
tokens, noncanonical identities, nonpositive generations, oversized frames, and
correlation mismatches fail closed. The runner supplies no policy declaration:
its first-enrollment message carries one stable request identity and one
availability-only advertisement, while resume carries the daemon-issued
enrollment, runner, and authentication-reference identities. The daemon
validates and commits enrollment and registration before returning their exact
identities and registration revision.

The executable connection implements `enroll`/`enrolled`, empty-inventory
`resume`/`resumed`, `advertise`/`registered`, heartbeat challenge and
acknowledgement, and typed `rejected` closure. The daemon commits enrollment or
registration before acknowledging it. The runner fsyncs its request identity and
exact returned receipt before treating either as current, reconnects after
transient transport loss, and never infers an unadvertised capability.

After durable handshake admission, the daemon connection task registers its
exact enrollment, runner, and physical connection epoch with a process-local
outbound broker before writing the handshake receipt. The task writes that
receipt before consuming the broker queue, so an operation cannot precede the
runner's durable identity receipt. The broker admits only the closed
daemon-to-runner operation-frame family, rejects a mismatched runner
correlation, and uses a one-frame handoff queue; durable journals, not this
queue, own retry. Before writing a dequeued frame, the task rechecks that the
exact durable connection epoch is current. Heartbeat deadlines take priority
over outbound queue work. Dropping the socket task retires only that exact
route. No production operation producer currently calls the broker, and inbound
runner operation frames remain unimplemented and fail closed.

**Committed unimplemented functionality.** No present durable producer or runner
surface serves the following lease/dispatch state machine; the outbound broker
above transports caller-constructed closed frames but supplies no authority to
construct one. The structural wire remains compatible with the state machine:

1. The daemon sends `lease_offer` with the complete lease correlation and
   immutable dispatch payload. The runner admits the exact tool, sandbox
   profile, credential profile, and workspace before replying `lease_claim`.
2. The daemon commits the exact claim before sending `lease_claimed`. Receipt of
   that acknowledgement is the execution capability held by the runner. Before
   accepting `dispatch`, the runner fsyncs the complete claimed correlation and
   `waiting_dispatch` phase below its private state root. An offer or sent claim
   without the acknowledgement never authorizes execution.
3. The daemon sends `dispatch`; the runner executes only when both the claimed
   capability and the dispatch have the same complete correlation. It fsyncs
   `dispatch_received` before acknowledging the frame internally and
   `execution_may_have_started` immediately before invoking the executor.
4. The runner retains one terminal success, known-failure, or ambiguous evidence
   envelope and resends it until the daemon durably commits the matching
   attempt/lease transition and replies `result_recorded`. Only then may the
   runner discard the evidence.

The closed version-one frame vocabulary is:

| Direction       | Frame                        | Required checked payload and effect                                                                                                                                                                                       |
| --------------- | ---------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| runner → daemon | `enroll`                     | Enrollment-request id, digest version, and complete advertisement. Admitted only as pristine enrollment or one provisioning-only pending replacement candidate.                                                           |
| daemon → runner | `enrolled`                   | Request id, issued enrollment/runner/authentication ids, registration revision, assigned connection epoch, and accepted advertisement digest. Durable registration and epoch precede send.                                |
| runner → daemon | `resume`                     | Request id, digest version, all issued identities, complete current advertisement, prior registration revision, and bounded reconnect inventory.                                                                          |
| daemon → runner | `resumed`                    | Current registration revision, assigned connection epoch, and one closed directive per inventoried item: resend, await, discard-as-recorded, or fail stale.                                                               |
| daemon → runner | `replacement_pending`        | Request id, issued candidate identities, provisioning-only registration revision, assigned connection epoch, and advertisement digest. Heartbeat and future startup reconciliation are admissible; leases are not.        |
| runner → daemon | `advertise`                  | Complete replacement advertisement under the current identities and registration revision.                                                                                                                                |
| daemon → runner | `registered`                 | Exact new registration revision and advertisement digest after durable registration.                                                                                                                                      |
| daemon → runner | `heartbeat`                  | Positive connection-epoch-local sequence and last accepted peer sequence.                                                                                                                                                 |
| runner → daemon | `heartbeat_ack`              | Same challenge sequence, monotonic runner sequence, and exact optional lease/workspace phase.                                                                                                                             |
| runner → daemon | `workspace_leak_page`        | Registration revision, report digest, positive page, prior-page digest, final-page flag, and at most 64 sorted typed leak facts. Spool until acknowledged.                                                                |
| daemon → runner | `workspace_leak_recorded`    | Exact report digest, page, and page digest after durable page admission; the final acknowledgement publishes the complete startup report.                                                                                 |
| daemon → runner | `workspace_provision`        | Single-use provisioning authorization, session, placement revision, runner/registration, repository key, sandbox profile, and optional credential-profile name.                                                           |
| runner → daemon | `workspace_ready`            | Same authorization correlation plus complete provisioned-workspace manifest identity and bounded content digest. Spool until acknowledged.                                                                                |
| daemon → runner | `workspace_recorded`         | Exact authorization and manifest correlation after durable receipt admission.                                                                                                                                             |
| daemon → runner | `workspace_release`          | Exact retired session placement revision and workspace-manifest identity.                                                                                                                                                 |
| runner → daemon | `workspace_released`         | Same release correlation after manifest transition and symlink-safe removal. Spool until acknowledged.                                                                                                                    |
| daemon → runner | `workspace_release_recorded` | Exact release correlation after durable release admission; this frame, or `operation_failure_recorded` for the same release correlation, frees the runner's journaled release and its workspace-operation slot.           |
| daemon → runner | `lease_offer`                | Complete immutable lease and tool dispatch correlation, selected profile/grant, normalized arguments, and result bounds.                                                                                                  |
| runner → daemon | `lease_claim`                | Exact offered correlation after local tool/profile/credential/workspace admission.                                                                                                                                        |
| daemon → runner | `lease_claimed`              | Exact claimed correlation after durable claim commit; it is one half of execution capability.                                                                                                                             |
| daemon → runner | `dispatch`                   | Exact claimed correlation and immutable payload; receipt with `lease_claimed` authorizes one execution.                                                                                                                   |
| runner → daemon | `result`                     | Exact claimed correlation and one bounded success, known-failure, or ambiguous evidence envelope. Spool until acknowledged.                                                                                               |
| daemon → runner | `result_recorded`            | Exact result correlation after atomic lease/attempt result commit.                                                                                                                                                        |
| runner → daemon | `operation_failed`           | Exact refused-operation correlation, one daemon-actionable failure category, and one bounded runner-specific detail. Spool until acknowledged.                                                                            |
| daemon → runner | `operation_failure_recorded` | Exact failure correlation after durable admission; frees the runner's journaled failure, and for a `workspace_cleanup_failed` failure also retires the journaled release it names and frees the workspace-operation slot. |
| either          | `shutdown`                   | Current connection epoch and closed reason `daemon_shutdown` or `runner_shutdown`; the hub durably records `shutdown`, distinct from `lost`. A stale epoch is refused with exact epoch correlation.                       |
| daemon → runner | `rejected`                   | Offending frame kind, available correlation, and one closed code; no arbitrary peer text. Fatal codes close the connection.                                                                                               |

Every frame is one JSON object with exactly `version`, `kind`, and `payload`
members. `version` is the integer `1`, `kind` is one table token, and `payload`
is one object whose required members are fixed by that kind; none permits an
additional member. Shared correlations are records rather than flattened
prefixes. A lease correlation contains, in this order, registration revision,
lease id, positive lease-lineage generation, runner, positive pinned-placement
revision, the concrete bounded working directory selected for execution, sandbox
profile, tool name, session, turn, tool request, physical tool attempt, issuing
turn attempt, and positive tool dispatch generation. The placement members make
every claim, dispatch, result, acknowledgement, heartbeat phase, and reconnect
directive repeat the same execution locus; a runner never infers them from the
tool name or repository presence. A provisioning correlation contains
authorization id, session, positive placement revision, runner, registration
revision, optional repository key, sandbox profile, and optional
credential-profile name. A release correlation contains session, positive
retired placement revision, runner, and stable workspace-manifest id. A
leak-page correlation contains registration revision, report digest, and
positive page. Repeating a shared correlation in an acknowledgement means
repeating that complete record.

A lease offer's result bounds record has exactly `success_text_bytes` and
`failure_detail_bytes`, both unsigned integers. Version one requires 1,048,576
and 4,096 respectively: a runner rejects any other pair rather than negotiating
bounds. Its terminal result envelope is the closed union:

- `success`, carrying exactly one `text` member whose UTF-8 value contains no
  U+0000 and is within `success_text_bytes`;
- `known_failure`, carrying exact `error_kind` and optional `detail`, where the
  kind is `unknown_tool`, `invalid_arguments`, `execution_failed`,
  `result_too_large`, or `crash_lost`, and a present detail is nonempty,
  POSIX-trimmed, control-free UTF-8 within `failure_detail_bytes`; or
- `ambiguous`, carrying no member beyond its union tag.

The union tag is the `kind` member of the envelope object. A member belonging to
another arm, an absent required member, or an extra member is malformed. These
are the wire projection of `ToolAttemptEnd`, not a second terminal-state model.

A reconnect inventory is one object with exactly five optional members: `lease`,
`result`, `workspace_operation`, `operation_failure`, and `leak_page`. Each
member is absent or carries one item; JSON null is never an absence. The lease
item is the complete lease correlation plus one `phase` token
`waiting_dispatch`, `dispatch_received`, or `execution_may_have_started`. The
result item is that correlation plus the complete terminal envelope. A
workspace-operation item is the closed union `provision` or `release` carrying
its complete correlation and phase: provisioning admits `provisioning` or
`ready_unrecorded`, while release admits `release_accepted` or
`release_completed`. An operation-failure item carries its closed provision,
release, or lease-offer correlation, its admissible failure category, and its
bounded detail object. A leak-page item carries the complete leak-page frame
payload. The count restrictions below apply after this structure is checked.

`resumed` carries a `directives` object with those same five optional members,
and its presence set must equal the received inventory's presence set. Each
present member repeats the inventoried item's complete correlation and carries
exactly one action: `resend`, `await`, `discard_as_recorded`, or `fail_stale`.
The daemon may select an action only after matching durable state; neither the
inventory order nor a missing member can stand in for the correlation.

A heartbeat acknowledgement carries `lease_phase` and `workspace_phase`, each as
an optional member rather than JSON null. A present lease phase is the same
lease item admitted by reconnect. A present workspace phase is the same
workspace-operation item admitted by reconnect, or that operation's exact
correlation with `failure_unrecorded` after an operation failure is journaled.
Thus a heartbeat phase can report progress but cannot create lease, result,
provisioning, release, or failure authority.

`rejected.available_correlation` is a required closed union so partial decode
never guesses what the sender meant. Its arms are `none` with no correlation,
`enrollment` with enrollment-request id, `registration` with registration
revision, `lease` with the complete lease correlation, `provision` with the
complete provisioning correlation, `release` with the complete release
correlation, `leak_page` with the complete leak-page correlation, and
`operation_failure` with the exact refused-operation correlation, and
`connection_epoch` with the hub-assigned epoch targeted by the refused frame.
`none` is admissible only when the frame failed before one complete arm was
available. Every other arm is rejected if any of its required correlation
members was unavailable; fragments are never padded with sentinels or borrowed
from connection memory.

An advertisement contains at most 16 capability classes, 256 tools, 64
credential-profile names, and 64 repository entries; names are sorted and
unique. A repository entry is one exact repository key paired with the exact
optional credential-profile name that key's runner configuration carries, and
entries are sorted and unique by key. Absence advertises anonymous HTTPS
availability rather than an incomplete pair;
[runner credential lifecycle](configuration-and-credentials.md#runner-credential-lifecycle)
owns that configuration meaning. Why: the daemon has to decide before a model
call whether a session can clone anything at all, and independent inventories of
keys and profiles cannot answer that — only the pairing shows whether an entry
is anonymous or requires the profile this session was granted
([tool loop](tool-loop.md#registry-placement-and-effect-metadata) owns the
advertisement condition that reads it). Workspace and sandbox inventories are
their closed vocabularies. A reconnect inventory contains at most one lease, one
unacknowledged terminal result, one workspace operation, one unacknowledged
operation failure, and one unacknowledged leak-report page because execution,
workspace work, and report delivery are each serial. Digests detect replay
disagreement and confer no authority. `rejected` codes are
`unsupported_version`, `unsupported_digest_version`, `malformed_frame`,
`enrollment_conflict`, `enrollment_revoked`, `registration_rejected`,
`stale_connection`, `correlation_mismatch`, `policy_rejected`,
`workspace_conflict`, `runner_lost`, `unavailable`, or `shutting_down`. Any
frame outside the state shown by the table, a duplicate with unequal canonical
payload, or an acknowledgement without its durable predecessor is fatal and
advances nothing. Equal replay returns or resends the same recorded answer.

Digest bytes are pinned rather than left to each implementation. Every digest is
the lowercase 64-character hex SHA-256 of a domain-separated, version-tagged
preimage: the ASCII prefix `sbx-digest-v1:<kind>:` followed by that kind's
canonical field encoding, where `<kind>` is exactly `advertisement`,
`leak-report`, `leak-page`, `workspace-manifest`, or `clone-url`. The canonical
encoding is length-prefixed: each field is its unsigned 64-bit big-endian byte
length followed by its exact bytes, in the field order that kind's definition
fixes, with each sorted inventory in the sort order already required of it and
with no separator, padding, or optional whitespace anywhere. Text is exact UTF-8
with no normalization, case folding, or trimming; an optional field is preceded
by one presence byte so an absent field is distinguishable from a present empty
one. Why: length prefixing is what makes two different field splittings hash
differently, and domain separation is what keeps an advertisement digest from
ever equalling a manifest digest computed over the same bytes.

Every field takes exactly one of these encodings, so no implementation picks a
representation: text is its exact UTF-8 bytes; an identity is its canonical
lowercase hyphenated 36-byte UUID text; a digest is its lowercase 64-byte hex
text; a closed-vocabulary value is its exact wire token; an unsigned integer is
exactly eight bytes big-endian; and a boolean is exactly one byte, `0x00` or
`0x01`. A sorted inventory is its element count as an unsigned integer followed
by each element as its own length-prefixed field, and a nested record is one
length-prefixed field whose bytes are that record's own canonical encoding under
these same rules. A decimal, textual, or other-width integer is therefore never
admissible, and a registration revision has exactly one byte sequence.

Each kind's field sequence is complete and fixed here:

- `advertisement` — capability classes, tool names, workspace capabilities,
  sandbox profiles, and credential-profile names, each as a sorted inventory of
  its exact names or closed tokens, then repository entries as one sorted
  inventory of nested records of repository key followed by optional
  credential-profile name, using the ordinary presence-byte encoding
  ([advertised catalogs and daemon authority](#advertised-catalogs-and-daemon-authority));
- `leak-report` — one inventory holding the report's complete sorted fact
  sequence, each fact a nested record of fact kind, locator, entry digest,
  optional session, then optional placement revision;
- `leak-page` — registration revision, report digest, page number, optional
  prior-page digest, final-page flag, then that page's facts as one inventory of
  those same nested records;
- `workspace-manifest` — lifecycle, stable manifest id, session, placement
  revision, runner, optional repository key, optional canonical-clone-URL
  digest, optional credential-profile name, sandbox profile, relative workspace
  path, then optional recovery. Present recovery is one nested record beginning
  with the `commit` or `branch` token: `commit` is followed by its exact
  revision, while `branch` is followed by its validated ref name and exact
  revision. A revision is exactly 40 or 64 lowercase hexadecimal bytes, and a
  branch name is within the existing 255-byte validated-ref cap. Repository key,
  clone-URL digest, and recovery are present together for a repository worktree
  and absent together for a private root; for a repository worktree the
  credential profile remains independently optional, while a private root
  records none
  ([workspace provisioning and recovery](#workspace-provisioning-and-recovery));
  and
- `clone-url` — the single configuration-validated canonical URL text.

`enroll` and `resume` carry the exact digest version the runner will compute,
and the daemon admits only the version it implements. A mismatch is the fatal
`unsupported_digest_version` rejection naming both versions, so a changed
encoding fails with a stated cause instead of surfacing as a bare hash mismatch
on the first acknowledgement.

A runner that cannot perform an admitted operation reports it rather than
falling silent. `operation_failed` carries the exact correlation of the refused
operation — provisioning authorization, release, or lease offer before
`lease_claim` — and two layers. The first is one closed daemon-actionable
category: `credential_unavailable`, `repository_unavailable`,
`sandbox_unavailable`, `workspace_conflict`, `workspace_cleanup_failed`, or
`lease_admission_refused`. Category admissibility is total with the correlation:
a provisioning authorization admits `credential_unavailable`,
`repository_unavailable`, `sandbox_unavailable`, or `workspace_conflict`; a
release admits only `workspace_cleanup_failed`; and a lease offer admits the
four provisioning categories plus `lease_admission_refused`. Every other pair is
a malformed frame: the runner rejects it while constructing the frame and never
spools it, and the daemon rejects it before durable admission.
`workspace_cleanup_failed` is therefore the release-specific member: a journaled
release whose rename or deletion keeps failing reports it rather than retaining
its journal forever, and `operation_failure_recorded` for that exact release
correlation resolves the release as refused, retiring both journals — the
failure and the release — and freeing the runner's single workspace-operation
slot. It is the one acknowledgement other than `workspace_release_recorded` that
retires a release, and no single release correlation ever draws both, because a
release either completed or was refused. A release whose owning connection
becomes durably lost draws neither and is retired as unowned instead
([workspace provisioning and recovery](#workspace-provisioning-and-recovery)).
Why: `workspace_release_recorded` follows a completed deletion and can never be
produced for a release whose deletion failed, so admitting only that frame as a
release's retirement would leave the reporting runner holding the journal and
its one workspace-operation slot forever — the exact outcome this category
exists to prevent. The undeleted placement then appears in the next startup
report as a `cleanup_failed` leak, which is the version-one response to
unreclaimed disk. Daemon logic keys off that category alone, so the set stays
small and every member names a decision the daemon can make. The second is one
runner-authored detail: a runner-specific code, a message, and a structured
payload, all carried as data. The daemon retains the detail as operator evidence
and exposes it verbatim through user runner inspection; it never parses,
interprets, or branches on it, so a runner may add detail codes freely without a
daemon change.

Extensible is not unbounded, and each member's limit is exact rather than
described as bounded. The complete detail is durable operator evidence held
within 4,096 UTF-8 bytes, the bound this system already uses for a known
failure's durable detail and for an exact runner value, and the three members
are derived from that bound:

- the code uses the same checked-name syntax as every other runner catalog key —
  nonempty, U+0000-free, at most 64 UTF-8 bytes, only ASCII letters, digits,
  dot, underscore, and hyphen, and beginning with an ASCII letter or digit
  ([advertised catalogs and daemon authority](#advertised-catalogs-and-daemon-authority))
  — so a new code needs no new grammar and is safe to retain and display
  verbatim;
- the message is exact nonempty UTF-8, excludes U+0000, and carries at most
  1,024 UTF-8 bytes, retained with no trimming, case folding, or other
  normalization. A runner whose captured text is longer truncates it to that cap
  with a head-and-tail `[signalbox: N bytes omitted]` marker rather than
  emitting a frame it knows is inadmissible; and
- the payload is one JSON object — `{}` when there is nothing structured to add,
  never absent and never JSON null — whose complete serialized form is at most
  2,048 UTF-8 bytes, whose member names use the code's checked-name syntax, and
  whose values are JSON strings under the message's byte bound, unsigned
  integers, booleans, null, or nested objects and arrays holding at most 64
  members or elements each, with at most eight containers on any root-to-value
  path counted exactly as
  [conversation import](conversation-import.md#claude-code-session-jsonl-versions-1-and-2)
  counts container depth.

Those three limits together leave more than a kibibyte of the retained bound for
the detail object's own framing and worst-case JSON escaping. A detail outside
any of them is an oversized or malformed frame and fails closed like any other,
so the runner checks the detail as it constructs it and never spools one it
could not have admitted itself. Why: `operation_failed` is spooled until
acknowledged, so a detail one side considers valid and the other rejects is not
a cosmetic disagreement between implementations — it is resent forever, and the
provisioning, release, or lease offer it was reporting waits permanently for an
`operation_failure_recorded` that can never arrive. Exact limits are what make
two independently implemented sides admit the same set.

A failure the daemon has durably recorded terminates that operation: the
corresponding provisioning, release, or lease authority is resolved as refused,
and neither side waits on it further.

After `enrolled`, `replacement_pending`, or `resumed`, startup reconciliation
sends one leak report before any workspace provision or lease offer is
admissible; heartbeats remain admissible throughout. A report is named by the
lowercase SHA-256 digest of its complete canonical sorted fact sequence and has
pages numbered from one. Each nonfinal page carries exactly 64 facts and each
final page carries zero through 64; even an empty report sends final page one.
Each fact contains only `unknown_manifest`, `retired_present`,
`manifest_conflict`, `cleanup_failed`, or `unreconciled`, a bounded
runner-root-relative locator, a lowercase SHA-256 manifest-or-entry digest, and
nullable session and placement revision when independently parseable. Facts are
strictly increasing by the complete tuple locator, kind, digest, optional
session, optional placement revision. Locator and digest compare as unsigned
lexicographic UTF-8 bytes, with the shorter value first when one is a prefix;
kind order is the closed order just listed; and each optional value compares by
its ordinary canonical presence-byte encoding followed, when present, by its
canonical wire bytes. An exact duplicate tuple is a malformed report: the runner
rejects it before naming, journaling, or spooling the report, and the daemon
rejects any nonempty page after page one whose first fact is not greater than
the prior staged page's last fact, or any page whose later fact is not greater
than its predecessor, without acknowledging that page or publishing the
snapshot. Why: equal tuples have one exclusive keyset cursor, so admitting
duplicates could make a page boundary silently omit retained evidence. The
locator is relative to the runner state root and not to any repository, so a
workspace-free private root, staging, and trash are all nameable. It carries no
absolute host path, matching the projection in
[process protocol](process-protocol.md#client-requests). Each page carries the
prior page digest, null only on page one, so equal replay is exact and omission
or reordering fails closed. The runner journals the report and current page
until `workspace_leak_recorded`. The daemon durably stages pages by report and
page, then atomically replaces the runner-status leak snapshot only when it
acknowledges the final page; an interrupted newer report never erases the prior
complete one. Reconnect inventory names the exact retained page and resumes at
its durable acknowledgement boundary.

Every operation message after `enrolled` or `replacement_pending` carries the
exact active or pending connection registration revision. Lease messages
additionally carry lease id, lease-lineage generation, runner, tool name,
session, turn, tool request, physical tool attempt, issuing turn attempt, and
tool dispatch generation. An acknowledgement for another revision or correlation
is stale evidence and cannot advance either side (INV-042, INV-021, INV-043).

The hub durably assigns a new positive connection epoch before each `enrolled`
or `resumed` acknowledgement. Every post-handshake lifecycle transition names
that exact epoch. When the allocation commit result is ambiguous, the hub
rereads the exact enrollment-and-epoch event and treats only the canonical
`connected`/`established` first event as a committed allocation before replying.
A newer connection fences its predecessors; a shutdown order or other transition
naming a stale epoch receives `stale_connection` with the observed epoch and
cannot apply its requested transition to the fresh epoch. Because
`stale_connection` is fatal, an established stream that sends the stale shutdown
is terminalized as `protocol_failure` and closed.

The daemon sends a heartbeat challenge every five seconds. The registration-only
runner replies with its monotonically increasing heartbeat sequence and no lease
or workspace phase. An acknowledgement must name the exact outstanding
challenge, and a second challenge is not issued while the first remains
unanswered. One missed interval durably records `suspect`; the third consecutive
miss, fifteen seconds after the challenge, records `lost`. An exact late
acknowledgement before the third miss appends `connected` with
`heartbeat_recovered`. Operation phases fail closed because this runtime
advertises and serves no operation provider.

An unannounced transport close or protocol failure durably records `lost`; an
epoch-targeted shutdown from either side durably records `shutdown`. On hub
startup, every prior-process connection left `connected` or `suspect` is marked
`lost`, and every pending loss cursor is propagated to affected sessions, before
the runner listener binds. The append-only event stream retains the epoch,
within-epoch ordinal, closed state, and typed cause, so a dead runner does not
remain observable as healthy after disconnect or restart.

Before closing an established stream for a rejected advertisement, the daemon
records peer, authority, and policy rejection as `protocol_failure`, or durable
availability and corruption rejection as `transport_closed`. An operating-system
read failure is likewise `transport_closed`, never `protocol_failure`. If the
third-miss timeout discovers that its epoch is stale, the daemon returns
`stale_connection` with that epoch before closing instead of inviting the old
runner to resume and fence the successor.

Enrollment revocation terminalizes a still-`connected` or `suspect` epoch as
`lost` with cause `enrollment_revoked` in the same transaction before the
enrollment becomes revoked. The serving task's next lifecycle observation
returns typed `enrollment_revoked` evidence and closes that physical stream. A
terminal connection remains unchanged. Startup reconciliation therefore never
needs to append a lifecycle event to an already revoked enrollment.

The runner retries socket absence, refusal, reset, timeout, transient socket
identity replacement, `unavailable`, and `shutting_down` without weakening any
identity check. It reports the failed stage and uses exponential delays from one
second through a 30-second cap, resetting after a completed enroll or resume.
Policy, correlation, revocation, and other permanent rejections remain fatal. On
a local termination signal, the runner finishes any outbound frame already in
progress, then gives its epoch-qualified shutdown write five seconds from the
next clean frame boundary; expiry exits with typed failure and leaves the hub to
record transport or heartbeat loss rather than presenting the runner as healthy.

**Committed unimplemented functionality.** No present runner sends a nonempty
reconnect inventory. Future execution support repeats resume and advertisement,
then exchanges a bounded inventory containing at most the one serial outstanding
lease, its fsynced local phase, and retained terminal evidence.
`waiting_dispatch`, `dispatch_received`, and `execution_may_have_started` retain
the complete lease and dispatch correlation. The first two prove only that the
journaled executor invocation had not started; the last carries ordinary
effect-class ambiguity. Canonical durable state decides whether the daemon
resends a claim acknowledgement, dispatch, or result acknowledgement;
advertisement and connection memory never recreate authority. When no claim
acknowledgement was issued, the daemon may commit the exact no-execution proof
before re-leasing. A complete reconnect inventory that omits a daemon-recorded
claimed lease cannot strand or repeat it: the daemon durably marks that lease
lost and applies its effect-class ambiguity law. A claimed lease reported
without a terminal envelope likewise follows its fsynced phase. Rejected
duplicate or stale frames are retained only as bounded operator classification,
never as domain evidence.

## Identity, enrollment, and registration

`RunnerId`, `RunnerEnrollmentId`, `RunnerAuthenticationId`, and `RunnerLeaseId`
are distinct UUID-backed domain identities. A runner identity is issued by one
logical enrollment and is not derived from hardware, a hostname, a network
address, or any other machine fingerprint. Persistent and short-lived runners
use the same identity law. A newly enrolled ephemeral runner receives a new
identity; reconnecting under an existing active enrollment retains the existing
one.

The runner creates one random enrollment-request identity on first startup and
atomically journals it below its private state root before connecting. The
daemon accepts `enroll` only from the checked same-user local socket, only while
no other active version-one enrollment exists, and only when every advertised
capability class is in its fixed allowed set. A pristine enrollment atomically
issues and persists fresh enrollment, runner, and authentication-reference
identities with the first active registration. Exact replay of its request
identity returns the same issued identities, registration revision, and
advertisement; another pristine request conflicts rather than replacing active
authority. The runner atomically journals the returned receipt before treating
enrollment as complete.

After durable predecessor loss, the enrollment path may issue the same identity
shapes plus one checked `PendingRunnerEnrollment` and pending registration
revision. The relation retains the exact active predecessor and durable loss
epoch that admitted it. Pending authority admits a physical connection, exact
receipt resume, heartbeat, and startup connection reconciliation, but never
registration mutation, grant creation, lease offer, claim, or dispatch. At most
one pending request may exist in the temporary version-one deployment, and equal
replay returns its original identities, registration, advertisement, and
authority while opening a fresh connection epoch.

A pending enrollment can be consumed by the implemented pre-pin replacement
transaction: the exact selected request must still own the provisioning-only
candidate, that candidate must be connected and advertise every retained
placement axis, and the same commit activates it, revokes its predecessor,
installs the unpinned successor placement, and records the terminal command
result. It performs no workspace operation. **Committed unimplemented
functionality.** No present pending enrollment can perform the future
user-command-bound workspace operation required by a pinned replacement. Pinned
replacement staging is implemented at the application and persistence boundary
for a distinct active runner, an exact pending enrollment, or the
registration-loss-only same-runner exception. No present daemon or runner
command surface invokes it, and no present transaction completes the pinned
replacement.

Loss triggered by re-registration has its own recovery. When a live runner stops
advertising a capability that a pinned placement requires, the
registration-reconciliation transition marks that placement `RunnerLost` while
the connection and enrollment stay healthy, so no successor is pending and no
different live runner exists to name. For that loss source only, the user
replacement command may name the same runner identity: a checked re-enrollment
against its current connection revalidates the exact enrollment, runner, and
authentication-reference correlations, requires the current registration to
advertise every capability the successor placement request needs, and then
installs the successor placement, grant lineage, and semantic boundary exactly
as a different-runner replacement does. Every other loss source keeps the
different-runner requirement, and no provisioning-only successor is admitted for
this loss source in version one. Why: the runner is present and capable at the
moment of recovery, so demanding a second runner would leave the only state that
produced this loss permanently unrecoverable.

The domain transition for that replacement is implemented. It admits the same
runner only when the supplied loss-causing registration actually invalidates the
retained pin, the current registration is either the exact same snapshot at the
loss revision or a genuinely newer registration, both registrations retain the
exact enrollment, runner, and authentication-reference lineage, and the current
registration satisfies the complete successor request. The ordinary replacement
transition still refuses the same runner, so a reconstituted registration-loss
label alone is not recovery authority. The pinned repository-workspace staging
transaction supplies those checked registrations and accepts the same runner
only through the explicit reenrollment target. **Committed unimplemented
functionality.** No daemon adapter yet invokes that transaction or installs the
resulting pinned replacement.

A pending successor may also be promoted with no lost session placement
involved. The implemented `promote_pending_runner` transaction is the
deployment-scoped mutation for explicit user-initiated promotion: it acts on the
fact that this daemon's active runner is durably gone, requires the recorded
active enrollment's connection to be durably lost and the pending candidate to
be connected under its provisioning-only authority, then revokes the predecessor
and constructs the active enrollment and validated registration from the exact
pending facts in one transaction. A predecessor reconnect and later loss does
not invalidate the immutable admission relation; promotion authenticates and
retains that current loss while the relation continues to retain the earlier
loss that admitted the candidate. Its immutable result retains the exact pending
registration, candidate connected event, current predecessor loss, and
pending-to-active audit transition rather than depending on mutable later
connection heads. It provisions no workspace, consumes no workspace receipt,
touches no session placement, creates no lease, and fabricates no turn or
frontier; a session pinned to the predecessor stays `RunnerLost` until its own
user replacement runs. The command generalizes to multi-runner as the fact that
one of this daemon's active runners is durably gone and a successor for it is
pending, and stays user-initiated in both forms. Why: a deployment with no
session, or one whose every placement is an unpinned capability-class request,
offers no placement for a replacement command to target, so without this path
its pending candidate would remain provisioning-only forever.

The process-protocol `promote_pending_runner` request invokes this transaction.
It returns the exact promoted enrollment receipt, a typed durable rejection, or
conflicting command reuse; equal request replay returns the original recorded
result without reinterpreting current runner state.

For a pinned repository-backed loss, `replace_lost_runner` first durably claims
the user command and its complete request, then creates one single-use
provisioning authorization naming that command and selected current
registration. That atomic staging transaction takes the session scheduler,
selected runner authority, and placement in the runner lock order, authenticates
either a distinct live successor or the registration-loss-only same-runner
exception, and returns the original durable stage on equal replay. A
workspace-free placement returns `NotApplicable` without claiming the command,
so its later terminal transaction remains the sole command-claiming
transaction.
The runner provisions and spools `workspace_ready` under that limited authority.
Only a later transaction can activate the pending enrollment: it rechecks the
lost predecessor and connected candidate, consumes the exact workspace receipt,
revokes the predecessor, constructs the active enrollment and validated
registration from the exact pending facts, and installs the successor placement,
grant, semantic boundary, and terminal command result atomically. Pre-pin
replacement needs no workspace and performs that promotion in its single
terminal transaction. A provisioning rejection or candidate loss records the
typed terminal command rejection, retires only that command staging workspace
through the normal release/trash path, and leaves the candidate pending for an
explicit later command. Process exit after command claim is recoverable: startup
resumes the one nonterminal replacement command from its durable provisioning
authorization and receipt rather than inventing a second claim. No database
transaction remains open across runner I/O.

Later connections send `resume` with the request identity, all three issued
identities, prior registration revision, complete advertisement, and an empty
reconnect inventory. Stored identity mismatch, revocation, a future revision, or
a stale revision paired with changed availability fails closed. Equal
availability returns the current durable revision; changed availability at the
current revision atomically appends its immediate successor before reply. No
application credential, bearer token, or proof exchange exists in the same-host
version; remote authentication remains a separate open decision.

A second live connection for the same registration receives a fresh epoch. The
prior connection's later advertisement, heartbeat acknowledgement, shutdown, or
loss transition is stale and cannot advance lifecycle or registration state.

Why: the daemon issues and durably owns logical enrollment authority while the
runner retains a stable idempotency fact for crash recovery. Treating the opaque
authentication-reference id as a secret would provide no authentication against
another process running as the same trusted user and would falsely imply a
remote-ready handshake.

One `RunnerEnrollment` binds the enrollment identity, runner identity, opaque
authentication-reference identity, and daemon-allowed capability classes. The
authentication reference identifies daemon-resident enrollment policy; it is not
an authentication secret. Enrollment is either active or revoked. Revocation is
terminal and makes later registration invalid. Complete reconstitution rejects
mismatched enrollment, runner, authentication, allowed class inventory, optional
last issued registration revision, or lifecycle state rather than repairing it.
Durable revocation commits first and then flips the exact caller-held
enrollment-shared active fence, so an existing validated registration becomes
non-current for later leases, reconciliation, runner replacement, or grant
replacement. A failed durable revocation leaves that caller-held fence active. A
lease offer rechecks the active enrollment and its exact enrollment, runner, and
authentication-reference correlations; a lease already offered is unaffected.
The Postgres admission trigger locks the current enrollment and placement heads
before accepting even a direct lease-row insert, so concurrent revocation,
runner loss, or runner replacement wins before the stale offer can commit. Both
the current and audit allowed-class inventories reject row mutation and
statement-level truncation, including cascading truncation.

A registration carries availability claims only:

- the runner's advertised capability classes;
- tool names;
- credential-profile names, and repository entries pairing each key with the
  optional profile defined by
  [runner credential lifecycle](configuration-and-credentials.md#runner-credential-lifecycle),
  where absence advertises anonymous HTTPS; and
- workspace and sandbox-profile capabilities.

It carries no permission default, effect class, placement declaration, approval
posture, credential path, or credential value. Registration validates classes,
tools, workspace capabilities, and sandbox profiles against enrollment and the
daemon catalog. Credential-profile names are checked, duplicate-free
availability facts from strict runner configuration; the user selects one exact
advertised name, while daemon-owned profile and override policy decide approval
independently of that name. A disallowed catalog claim or malformed name rejects
the complete registration. A valid registration retains the exact advertised
subset and attaches daemon-authoritative tool and sandbox declarations.
Preparing a registration also takes the enrollment-shared exclusive preparation
fence: a second preparation while one is outstanding fails typed, and the fence
releases when the prepared registration commits or is abandoned. The persistence
adapter stages this checked registration, commits its complete durable rows and
current head, and only then advances the enrollment-owned current registration
revision; the held fence means no concurrent registration can advance that
revision between staging and this post-commit advance, so a successful durable
write is never reported as a failed registration, while a failed durable write
leaves the prior registration current. Enrollment persistence admits only a
pristine active enrollment that has issued no registration; inserting one whose
caller-held authority already advanced would reload with no issued revision and
disagree with canonical storage on every later registration. Complete enrollment
reconstitution restores the optional last issued revision from independently
matching stored facts; the next successful registration advances it instead of
reusing a prior revision. A persistence load supplied with caller-held
enrollment authority compares that authority's current registration revision
with the independently loaded canonical enrollment before binding any historical
registration to its revision fence. Retained copies of every prior registration
become stale and cannot authorize a later lease, reconciliation, or
grant-bearing placement transition. Omitting a formerly advertised capability
removes its availability from the new registration, but never changes its
daemon-side policy. A pinned session never inherits additions from
re-registration. If a new registration omits a runner-required capability in
that session's pinned snapshot, no later lease is authorized. Every changed
registration beyond the first creates a durable bounded reconciliation cursor.
Before acknowledging the registration, the daemon pages older pinned sessions in
session-identity order and atomically records either preservation or a
`RunnerLost` placement whose registration loss source names the exact
incompatible registration revision. A current offered or claimed lease and its
active turn move through the same loss and `AwaitingRunnerRecovery` transaction
as connection loss. Cursor and per-session evidence make a crash restartable;
startup completes pending registration cursors before classifying prior-process
connections lost, so an already-recorded registration cause cannot be relabeled
as connection loss. Another changed registration is refused while the current
cursor still has an unobserved pinned session. Omitting a combined-locus tool
records preservation and disables runner dispatch for that tool, while retaining
placement so daemon fallback remains admissible. Why: re-registration can narrow
current availability without downgrading a confirmation requirement, widening
authorization, silently changing established affinity, or losing the recovery
distinction carried by the exact loss source (INV-009, INV-042, INV-044).

## Advertised catalogs and daemon authority

Capability-class names are exact, bounded catalog keys. Credential-profile and
repository-key names use the same checked syntax but are runner-configured
availability keys. Construction rejects empty values, U+0000, values longer than
64 UTF-8 bytes, and bytes outside ASCII letters, digits, dot, underscore, and
hyphen. A name must begin with an ASCII letter or digit. Workspace capability is
closed vocabulary; its defined arm is `WorktreePerSession`.

The registration-only daemon catalog has no allowed capability class, tool,
workspace, or sandbox-profile entry. `signalbox-runner` therefore advertises
each of those inventories as explicitly empty. It advertises only credential
profiles and repository entries read from strict runner configuration. The
daemon currently admits exactly the `github-runner` credential-profile name,
with an empty approval policy, so that name confers no execution authority.

The registration-only catalog remains empty. A future execution catalog,
capability class, tool inventory, and sandbox-profile composition are
unimplemented and undecided under
[Scheduling and runners](../open-questions.md#scheduling-and-runners). The
existing advertisement and validation vocabulary supplies no execution authority
by itself (INV-042).

One `RunnerCatalog` domain value contains allowed capability classes, complete
runner-tool declarations, allowed workspace capabilities, and the two fixed
sandbox-profile definitions. The persistence adapter owns this catalog
independently of stored registration rows. Registration reconstitution compares
every stored class, tool declaration, workspace capability, and sandbox profile
with that trusted catalog and rejects any difference; stored declarations cannot
bootstrap their own authority. Duplicate names or an internally inconsistent
placement declaration rejects the complete catalog. Credential-profile names and
repository entries remain exact availability inventories recorded by that
registration and acquire no policy by being stored: the profile name a
repository entry carries, when present, states which configured credential that
repository requires, never that any session holds it; absence states that the
entry is anonymous. The configuration meaning of presence and absence is owned
by
[runner credential lifecycle](configuration-and-credentials.md#runner-credential-lifecycle).

The registration-only daemon composition constructs one `RunnerCatalog` with
empty class, tool, workspace, and sandbox inventories plus the exact
`github-runner` credential policy, and has no runner-catalog file or reload
path. Exact registration validation rejects any class, tool, workspace, or
sandbox claim and checks each credential-profile claim against that same
catalog. Dynamic catalog revisioning and reload remain deferred rather than
being approximated by process-local mutation.

Future execution support may replace that empty catalog only after its registry,
capability class, profiles, and executor contract are decided. Exact
registration validation will continue to reject any disagreement between the
daemon-authoritative catalog and a runner advertisement.

Each `RunnerToolDeclaration` contains:

- the existing checked `ToolName`;
- one checked `RunnerToolModelDefinition`, containing a nonempty bounded
  model-facing description and a canonical JSON-object argument schema;
- one required `ToolPermissionDefault`, whose additive `always_confirm` storage
  encoding round-trips the daemon-local always-confirm approval declaration;
- one required `RunnerToolEffectClass`; and
- one nonempty `ToolAdmissibleLoci` value.

The declaration permission remains model-definition and daemon-locus
compatibility metadata. It never authorizes a runner attempt: runner approval is
derived only from the pinned sandbox profile and exact override, then
snapshotted into the grant. A combined-locus declaration must still match the
daemon-local default so fallback cannot change its advertised definition
silently.

`ToolAdmissibleLoci` is closed typed vocabulary:

- `DaemonOnly`;
- `RunnerOnly { selector }`; or
- `DaemonOrRunner { selector }`.

A runner selector is either one exact `RunnerId` or one `RunnerCapabilityClass`.
When both loci are admissible, the domain retains daemon-local admissibility if
the attached runner does not currently advertise the tool, while runner lease
creation fails `ToolUnavailable`. It does not transfer the consumed runner
authorization or credential-profile grant to daemon execution. The application
orchestration selects the locus before authorization; a change to daemon
fallback discards runner-pair authority and resolves the daemon-local tool
policy without the runner-resident profile. Placement is immutable declaration
metadata, not a per-call choice supplied by a runner or model. An MCP locus is
not part of the vocabulary.

`RunnerToolDeclaration` is the one daemon-authoritative runner-dispatch
declaration. Every runner-advertisable tool therefore has model-facing
description and schema authority even when daemon execution is inadmissible. The
current daemon-local application `ToolDefinition` is a compatibility
representation, not a second source of policy. The application adapter compiles
argument validation from the runner declaration's exact schema and rejects a
shared name unless model-facing definition and permission are equal and the
local effect maps exactly (`EffectFree` to `Pure`, `ExternalEffect` to
`SideEffecting`). `Idempotent` has no current daemon-local projection, so a tool
with that effect cannot include the daemon locus until the representations are
consolidated.

Advertisement validation never synthesizes a declaration for an unknown tool.
Unknown tools, capability classes, workspace capabilities, and sandbox profiles,
or malformed credential-profile names reject the complete advertisement. A
daemon-only tool or a runner tool whose declared identity-or-class selector the
advertisement does not satisfy also rejects the complete advertisement, as does
a repository entry whose present credential-profile name is absent from the same
advertisement's profile inventory: an entry naming a profile the runner does not
offer could never be granted, while an entry naming none is intentionally usable
without a grant. The resulting `ValidatedRunnerRegistration` exposes only exact
advertised availability paired with daemon-owned policy. A runner can therefore
neither self-widen its tool surface nor replace confirmation with automatic
approval (INV-042).

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
physical-attempt dispatch correlation, session, runner, authorizing registration
revision, pinned placement revision, concrete execution working directory,
sandbox profile, effect class, and positive lease-lineage generation. Claim,
completion, no-execution proof, retry lineage, and durable reconstitution repeat
that complete correlation; none may infer an execution-locus member from the
tool name, repository presence, or current placement. Lease creation is not a
free constructor: it consumes one `RunnerToolAttemptAuthorization`, which binds
the approved request and its exact tool name to the tool loop's
`AuthorizedToolAttempt`. Only `ToolBatch::authorize_runner_attempt` and
`ToolBatch::resume_runner_attempt` publicly produce that pairing: each selects
the batch's canonical immutable request and approval together with its
physical-attempt authority. `RunnerToolAttemptAuthorization` has no public
raw-parts constructor. The underlying attempt exists only after the automatic or
user decision authorizes that exact attempt, and neither authority nor the
resulting lease is cloneable. Every checked `ToolBatch` carries a durable
per-attempt inventory of runner authority already issued. Its in-memory clones
share the exact atomic guard for each physical attempt. The persistence loader
derives the active batch's consumed inventory from exact current physical
attempts already bound by durable runner lease generations, and complete
reconstitution restores every consumed guard from that inventory. A stored
retryable claimed loss leaves its source attempt in flight, so a reloaded batch
still carries the exact live source the checked claimed replacement transition
requires; the predecessor leaves the current-attempt view, and enters the
batch's restored retired-identity inventory, only once the atomic replacement
commit retires it to terminal history. A reloaded batch therefore keeps
rejecting retired attempt-identity reuse in the domain rather than at the
retained row's key. Atomic runner authorization marks that exact attempt issued
in the batch; a later clone or reconstitution from the updated facts cannot mint
a second runner lease capability. Current active enrollment, pinned placement,
its exact validated registration, and any selected active credential grant
jointly authorize every offer after the first. The initial offer instead creates
that pinned placement, any selected grant, and generation-one lease in one
checked transition from `Unpinned`; it does not require those products to exist
beforehand. The request, attempt, session, and two-way crash class must match
the selected tool, placement, and declaration-derived effect class (`Pure` to
`EffectFree`; `Idempotent` or `SideEffecting` to `ExternalEffect`). Revoked
enrollment, lost placement, or a mismatched runner, request, tool, attempt,
effect, profile, or grant cannot create a lease.

When a credential profile is selected, the lease also retains the exact
immutable `CredentialDispatchAuthorization`: session, runner, profile, grant
revision, tool, and resolved pair posture. Grant replacement or revocation
therefore cannot erase which snapshot authorized an already offered lease.

A lease begins `Offered` at lease-lineage generation one. Only the exact lease,
runner, tool, authorized physical-attempt correlation, and lineage generation
may claim it, producing `Claimed`; only that same correlation may complete it.
Completion is terminal. A stale or cross-wired correlation cannot advance the
aggregate. Complete reconstitution accepts only the closed state shapes and
exact correlations, and requires the opaque validated registration whose
daemon-owned declaration independently confirms the stored runner, tool, and
three-way effect class.

`LostUnclaimed` requires an opaque durable no-execution proof bound to the exact
lease correlation; complete loss reconstitution requires the same proof. The
proof exposes no public raw-parts constructor, so an offered lease and its
public correlation cannot mint this authority. The persistence adapter may
reconstitute it only by comparing the complete stored proof correlation with the
independently loaded lease correlation through the checked reconstitution input.
The durable claim transaction is the independently authoritative producer. It
serializes against the current connection/loss and registration heads, commits
the exact claim, and returns the canonical claimed lease before acknowledgement;
the generic projection writer cannot originate a claim. The durable result
transaction reloads that exact claimed lease and a result-only authority from
the canonical active tool batch, then commits lease completion and terminal
attempt evidence together. Duplicate or cross-wired evidence advances neither
aggregate; ambiguous external-effect evidence enters the exact tool-recovery
wait in the same transaction. Generic projection cannot originate completion.
**Committed unimplemented functionality.** No daemon transport currently routes
an inbound `lease_claim` or `result` through these transactions or emits
`lease_claimed` or `result_recorded`; future bindings may acknowledge only the
complete correlation returned by the committed transaction. When loss wins
first, the fenced connection epoch plus the durable absence of a claim proves
that no execution capability was issued, and the same transaction records
`LostUnclaimed` with its exact proof. When claim wins first, the durable lease
is `Claimed` even if acknowledgement delivery is uncertain and loss follows
execution-possible law. Mere absence of a frame in process memory is never
proof. The Postgres representation commits the proof atomically with the
lost-unclaimed event and requires it before a successor generation can consume
that retry path. Every retryable loss admission — lost-unclaimed, whose
proof-backed retry reissues the never-executed attempt for every effect class,
and claimed pure or idempotent loss — reads its source attempt under a row lock
and requires it to still be in flight, so a concurrent terminal attempt update
serializes with the loss instead of racing past that live-source check. Claimed
retry instead requires a durable record binding the complete lost lease
correlation to the exact fresh physical-attempt dispatch. That record is an
idempotent reservation: if a process exits before the replacement attempt is
stored, recovery loads the exact reserved dispatch and may replay only that
replacement. One transaction retires the in-flight source attempt to its
effect-correct terminal history and commits the fresh replacement attempt with
its successor lease generation, and the Postgres representation rejects a
reserved replacement attempt committed without that successor generation, so the
durable claimed-retry states are exactly the loss over its still-in-flight
source — with or without the replayable reservation — and the complete consumed
retry whose successor lease is already offered; a crash can strand neither a
consumed one-shot preparation fence without its successor lease nor a retired
source without its replacement, and a different replacement cannot overwrite the
reservation. Without the proof, losing even an `Offered` lease conservatively
follows the execution-possible law: pure or idempotent work requires a fresh
physical attempt, while side-effecting work requires crash classification.

With that proof, loss before claim permits every effect class to be re-leased at
the checked successor lease-lineage generation. Loss after claim follows the
required retry law:

- `Pure` and `Idempotent` produce typed re-lease authority at the checked
  successor generation. After claim, that authority consumes the owning checked
  `ToolBatch`, retires the prior in-flight attempt to its effect-correct
  terminal history, installs and authorizes a fresh physical `ToolAttemptId`,
  retains every retired attempt identity in the updated batch and its complete
  reconstitution facts, and returns both attempt records. Before claim, the
  authority instead consumes the owning checked batch, verifies its complete
  original dispatch, and reissues the never-executed attempt through a fresh
  single-use durable runner-issuance guard. Only the private evidence produced
  by the applicable batch transition can authorize the re-lease. Preparing
  either retry consumes the loss authority's one-shot preparation fence, and
  complete reconstitution restores its durable consumed state. For an unclaimed
  retry, the predecessor guard moves one-way from issued to retired while the
  returned batch installs a distinct successor guard; retained batch copies
  cannot reopen the predecessor, and retry-marked authority is rejected by the
  ordinary offer transition; and
- `SideEffecting` produces typed crash-classification authority whose physical
  attempt is derived from the opaque lost lease and never produces re-lease
  authority.

Generation exhaustion, reuse of any current or retired attempt identity for
either claimed replacement or ordinary preparation, and a standalone
same-request authorization for claimed retry all fail closed. A claimed
replacement must retire the complete original session, turn, issuing turn
attempt, request, physical attempt, dispatch generation, and effect fence. Its
private evidence retains the complete source lease correlation and complete
replacement dispatch; the re-lease transition compares both without truncating
them to shared request or attempt identities. `RunnerLeaseLoss` has sealed
construction, so only `RunnerLease::lose` or `RunnerLease::reconstitute_loss`
over an already-lost checked projection can produce retry or
crash-classification authority. Re-leasing continues one logical tool request
and lease lineage. Its successor `RunnerGeneration` is distinct from the fresh
physical attempt's `ToolDispatchGeneration`, which starts at `first()` under the
tool-loop law. Every repeated physical execution therefore has its own attempt
identity and record as required by INV-004. Side-effecting loss composes with
the existing physical-attempt ambiguity machinery; this domain slice does not
duplicate or overwrite that attempt's outcome (INV-004, INV-025, INV-026,
INV-043).

The lease aggregate contains no channel handle or process-local connection
state. A reconnecting registration cannot recreate, complete, or discard a lease
from an advertisement. Why: the held streaming channel is transport, not lease
or claim authority. The application repository commits claim, terminal result,
and lease state together; the wire adapter projects only from those durable
facts during reconnect.

## Session composition

Workspace, repository, credentials, and sandbox are independent axes of one
session. A choice on any axis constrains no other axis, and no axis is ever
inferred from another:

- **workspace** — none, a plain directory on the runner, or a repository
  worktree;
- **repository** — none, an empty repository, or a populated repository, which
  is the state of whatever repository the session reaches rather than a separate
  request member. An empty repository is an ordinary starting state, and no
  repository at all is an ordinary session shape rather than a degenerate one;
- **credentials** — none, or one exact credential-profile name the selected
  runner advertises. A profile is grantable with no repository and no workspace,
  because a session may need a credential for work that has nothing to clone;
  and
- **sandbox** — one explicitly selected profile: `ambient`, which supervises
  without confining, or `workspace-restricted`, whose writable root is the
  session's own workspace and need not be a repository clone. Either profile is
  selectable with any choice on the other three axes.

Every combination is a stated choice at creation: nothing is inferred, not the
credential profile, not the repository, not the working directory. A creation
request that selects nothing on an axis states that absence explicitly and
receives absence, never a daemon-selected or runner-selected substitute. The
axes are independent in what they admit, and the request shape says exactly
which member carries each. Workspace and repository share one member, because a
repository reaches a session as its worktree and in no other way:
`WorkspaceRequirement::None` is both the no-workspace and the plain-directory
choice, `WorkspaceRequirement::RepositoryWorktree` carries the exact repository
key, and whether that repository is empty or populated is a fact about the
repository rather than a third value the request selects. A session with no
worktree therefore has no session repository, and it reaches a repository only
by cloning one into its writable root under a runner-configured repository key
and the optional credential profile that key's entry names. Credentials and
sandbox constrain neither of those: every credential choice composes with every
workspace choice, and either sandbox profile composes with every combination of
the other two. Runner capability varies over the same axes: a runner may
advertise no workspace capability and no repository entry at all, and such a
runner is a fully usable placement target for every session composition that
needs neither.

The four repository/credential compositions have exact outcomes. No repository
and no profile performs no repository operation and creates no grant; no
repository with a named profile creates the grant for other admitted dispatches;
a repository with no profile provisions anonymously only when its entry also
names no profile, otherwise it fails `credential_unavailable`; and a repository
with a named profile provisions with that grant only when the entry names the
same profile, otherwise it fails `credential_unavailable`. No case selects,
removes, or substitutes a credential implicitly
([runner credential lifecycle](configuration-and-credentials.md#runner-credential-lifecycle)
owns the configuration meaning of an absent repository profile).

The advertised executable tool set is therefore a function of the session's
actual capabilities rather than of the compiled registry. A declaration whose
arguments, paths, or working directory are defined relative to a session
repository is advertised only to a session that has one; a workspace-free
session advertises exactly the tools that can run in it
([model-call execution](model-call-execution.md#frontier-rendering) owns the
snapshot, and [tool loop](tool-loop.md#registry-placement-and-effect-metadata)
owns the declarations). No combination is rejected for being workspace-free, and
no contract on this page may assume that a repository exists.

Why: an earlier shape of this specification read as though a session were a
repository clone on the one runner, and four separate defects — a credential
inferred for a repository placement, a rejected empty repository, placement
fields missing from template creation, and a workspace-free restricted placement
advertising repository tools — were that single assumption surfacing in four
places. Stating the axes once is what keeps a fifth from appearing.

## Session placement and affinity

`SessionRunnerPlacement` starts with one request that is immutable between
explicit replacement transitions:

- a `RunnerSelector`, targeting a capability class or exact runner identity;
- `WorkingDirectorySelection`, either runner default or one exact bounded
  working-directory value;
- an optional `CredentialProfileName`;
- `WorkspaceRequirement`, either none or a repository worktree;
- one exact `RunnerSandboxProfile`, defaulted to `WorkspaceRestricted` only at
  the user/client construction boundary and always explicit in the domain; and
- one bounded map of exact tool names to
  `RunnerToolPermissionOverride::{Auto, Confirm}`.

The override map has at most 64 entries, rejects duplicate or undeclared tool
names as one invalid request, and is copied into the durable placement snapshot.
It is session policy rather than runner advertisement: a runner cannot add,
remove, or reinterpret it.

Every element of the request is an independent axis under
[session composition](#session-composition) and is supplied explicitly. An
absent `CredentialProfileName` is the stated choice of no credential rather than
a request that the daemon or runner select one, and it neither requires nor
implies a workspace. `WorkspaceRequirement::None` is admissible with either
sandbox profile and with any credential choice. A plain-directory workspace is
that same `WorkspaceRequirement::None` paired with an exact working-directory
selection: the runner provisions nothing and executes in the named directory,
and it never creates, renames, or deletes that directory, so retiring such a
placement releases nothing
([workspace provisioning and recovery](#workspace-provisioning-and-recovery)). A
runner that advertises no workspace capability therefore remains a valid target
for every placement that requires no worktree.

The working-directory value is exact nonempty UTF-8, excludes U+0000, and is at
most 4,096 bytes. The domain does not apply host-platform path parsing. A
repository-worktree requirement carries one exact repository key with the same
nonempty, U+0000-free, at-most-4,096-byte contract.

Before execution, placement is `Unpinned`. Mere attachment does not pin it. A
placement without a repository requirement can pin in the first atomic dispatch
transaction. A repository placement first requires one checked, single-use
`WorkspaceProvisioningAuthorization`. The daemon derives it only after
validating the candidate runner and registration; it binds the session,
placement revision, runner, registration revision, repository key, sandbox
profile, and optional credential profile. It authorizes only acquisition of that
repository and no model-selected tool. The runner rejects an unknown credential
profile before accepting the authorization and returns one
`ProvisionedWorkspace` receipt whose manifest facts match every correlation.

Dispatch through an existing pin consumes the exact tool-attempt authorization,
revalidates the frozen enrollment and current registration revision, and stores
the `InFlight` attempt and offered lease atomically. A crash therefore leaves
both facts or neither. **Committed unimplemented functionality.** No present
transaction performs the first dispatch boundary. Its future implementation must
atomically consume the workspace authorization and receipt when present,
validate the placement request against the same current registration, install
`Pinned` state, create any initial credential grant, mark the exact attempt in
flight, and store the offered lease. A crash may then leave either retryable
provisioning evidence or the complete pin/grant/lease boundary, never an
in-flight tool attempt without its lease. The pinned state contains the runner,
selected working directory, credential-profile selection, tool inventory,
runner-required tool inventory, provisioned workspace, sandbox profile, and
exact permission overrides. Ordinary attachment and lease creation accept only
that exact runner and current grant. Re-registration or reconnect changes none
of these facts, and there is no automatic migration or class-based rescheduling
(INV-044, INV-045).

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

Runner loss is explicit state, not implicit reassignment. Marking a pinned
runner lost retains the prior placement and disables future lease creation. A
registration-triggered loss also retains the exact registration revision that
failed the pin; connection loss carries no registration revision. Marking an
exact-identity request lost before pin retains that request and records
`RunnerLostBeforePin { runner }`, disabling eligibility and initial pin. A
user-directed pinned replacement supplies and installs a new complete placement
request, validated registration, working directory, credential-profile
selection, tool inventory, and provisioned workspace. It advances a positive
placement revision and returns one `RunnerPlacementChange` value carrying the
complete before-and-after placement requests and pinned facts needed for the
frontier-extending injected message. When credential grant authority changes,
the same result also carries complete before-and-after grant reconstitution
facts, including the prior narrowed tool inventory and successor inventory. A
pre-pin replacement instead returns a checked `RunnerPrePinReplacement` carrying
the lost exact identity and before-and-after requests; the successor is ordinary
`Unpinned`, and no pinned facts or semantic placement change exist.

Every replacement runner must be currently registered on a live connection, and
must differ from the lost runner except in the checked same-runner recovery that
[identity, enrollment, and registration](#identity-enrollment-and-registration)
admits for a registration-triggered loss. Pinned replacement provisions a fresh
workspace at the successor revision when the successor request requires one and
provisions nothing when it does not; pre-pin replacement provisions nothing
until eventual initial dispatch. Reconnect of the lost identity cannot consume
either replacement transition or clear a lost state. When a daemon-local model
call is authorized and in flight as the replacement runs, the replacement is
staged rather than refused: the transition appends its semantic boundary only
after that call reaches its observation boundary, so the call's own output
appends first and the prefix-only frontier law holds
([turn lifecycle and scheduling](turn-lifecycle-and-scheduling.md#runner-loss-session-recovery)).
Safe retry authority exists only for a pinned lost runner and can be consumed
only as part of its user replacement; it never causes automatic dispatch.

Reconstitution accepts a complete public raw-facts input and rejects ordinary
`Unpinned` above revision one unless append-only history proves an exact
lost-before-pin user replacement into that revision. It rejects
`RunnerLostBeforePin` unless the request selector is the retained exact runner.
Two further conditions each cause rejection on their own, and no exemption
attaches to either: a pinned or pinned-loss state that does not match its
current request and validated capabilities is rejected, and a stored
credential-grant lineage whose revision is newer than the placement revision is
rejected. A profileless placement with retained lineage additionally requires
the exact terminally revoked grant tombstone for that session, runner, and
revision; an omitted, active, or cross-wired tombstone fails closed. Durable
replacement-history verification is enforced by the persistence projection
described below. Pinned or pinned-loss reconstitution validates against the
exact registration snapshot that produced the pin and rejects any stored tool or
runner-required-tool inventory that differs from that checked result. A current
narrowed re-registration is reconciled separately and is not substituted for
that historical snapshot. This domain aggregate accepts every positive placement
revision because each is reachable through checked successor transitions.

The store retains append-only created, pinned, runner-lost-before-pin,
pre-pin-replaced, runner-lost, runner-replaced, abandoned, and profile-replaced
records behind one current pointer. A profile-replaced record carries the pinned
registration snapshot forward even though the replacement was validated against
the enrollment-owned current revision, so an availability-equivalent
re-registration cannot make profile replacement undurable. Relational transition
checks require contiguous event history, exact revision succession, unchanged
affinity facts at runner loss, profile-only changes for profile replacement, and
each stored tool's runner-required flag to match its declaration's runner-only
or combined locus. The generic snapshot writer does not store either runner
replacement event; their dedicated command-authorized transactions must
revalidate, under the runner lock order, that the supplied registration's
enrollment remains active, its connection remains live, and its revision remains
enrollment-owned current. Profile replacement remains a placement-local
operation and revalidates the enrollment and current registration under lock.
Every appended record advances the current-placement head in the same
transaction. Reconstitution reads the current record with its exact validated
registration and tool inventory. The loaded persistence wrapper retains that
historical registration and its durable revision so a caller can reconcile
against newer availability without reconstructing or guessing the pinned
evidence. The connection-loss propagation transaction consumes that evidence
when it persists `RunnerLost`; the generic snapshot writer does not install
loss.

Runner loss is an application-visible typed session state. A pinned placement
becomes `RunnerLost`; an unpinned placement whose exact-identity selector names
the lost runner becomes `RunnerLostBeforePin { runner }`. An unpinned
capability-class request has selected no runner and is unaffected. Once durable
loss commits, only two user commands can leave either lost state. For pinned
loss, `replace` names a different live runner, atomically activates the one
pending replacement enrollment, or — for a registration-triggered loss only —
re-enrolls the same runner against its current connection, then commits the
checked successor placement, grant lineage, semantic `RunnerPlacementChanged`
transcript entry, and next context frontier atomically. The entry is
reference-only and contains no credential value or unbounded runner output.
Replacing `RunnerLostBeforePin` updates the exact selector and returns to
`Unpinned` at the successor revision without fabricating a semantic boundary,
workspace, grant, or lease. `abandon` requires the exact current lost placement
and an empty active-turn slot, then installs terminal `RunnerAbandoned`
placement state. An active turn must first finish the existing stop,
approval-decision, or reconciliation flow; abandonment has no cancellation proof
and cannot end a turn. An idle or queued-only session fabricates no turn or
frontier and later exposes only daemon-executable tools. It creates no successor
turn and never rewrites an issued side effect as known. Neither command can
target an ordinary unpinned, live, stale, or already-replaced placement
(INV-026, INV-029, INV-037, INV-044).

## Sandbox profiles and approval

The sandbox profile is an immutable placement fact and appears in every session,
lease, dispatch, result, evidence, transcript, and user-inspection projection. A
client that omits it receives `WorkspaceRestricted` at the client construction
boundary; the domain and wire always carry the selected value. Changing profiles
requires the same explicit replacement frontier as changing runners.

For `WorkspaceRestricted`, the runner launches every executable tool as a fresh
bubblewrap process. It unshares user, mount, PID, IPC, UTS, cgroup, and network
namespaces; drops capabilities; clears inherited environment; mounts fresh
`/proc`, `/dev`, `/tmp`, and runtime directories; binds only the session's exact
writable root read-write; and binds configured toolchain and cache allowlist
paths read-only.

Confinement is defined over that writable root, which need not be a repository.
The root is the provisioned repository when the placement requires a worktree,
the exact selected working directory when the placement requires no worktree and
names one, and otherwise one private per-placement directory the runner creates
below its own state root, empty when it first creates it. Exactly one writable
root exists per placement in every case, so the profile means the same thing —
one writable subtree, no host interface, no inherited environment — for a
workspace-free session as for a repository-backed one. Each of the three roots
is identified by durable facts rather than by process memory: the provisioned
repository by its manifest, the selected working directory by the placement
value that names it, and the private root by its deterministic per-placement
path and its own manifest
([workspace provisioning and recovery](#workspace-provisioning-and-recovery)). A
restarted runner therefore re-adopts the root it was already using, with
whatever the session wrote into it, and never substitutes a fresh empty
directory for one holding session files. Why: defining restricted confinement in
terms of the session repository made a repository a precondition for being
sandboxed at all, which would have pushed every workspace-free session into
`ambient` for no security reason.

Repository provisioning uses the same profile before publication: it binds only
the authorized empty staging repository at the fixed guest workspace path, uses
a per-provisioning broker socket, injects only the selected helper credential,
verifies the resulting clone, and atomically publishes it. The runner refuses
restricted registration when the installed bubblewrap cannot prove the required
namespace and bind behavior. File tools use descriptor-relative traversal
beneath that writable root and refuse symlinks, magic links, device nodes,
sockets, and path escape. Writes replace a sibling temporary file atomically.
Shell and build/test tools receive no host path that was not bound into their
namespace.

The restricted network namespace has no host interface. A namespace-local shim
connects through one per-dispatch Unix socket to a runner-owned HTTPS broker.
The broker accepts only `CONNECT` to port 443, checks the requested hostname
before resolution, pins the resolved destination for that connection, parses the
TLS ClientHello, and requires its SNI to equal the admitted hostname. Version
one admits the `github.com` and `crates.io` hostname suffixes and the exact
`api.anthropic.com` hostname; runner configuration may remove entries but cannot
add another. CONNECT authorities are canonical ASCII DNS names: lowercase, no
trailing dot, no empty label, and no IP literal. A suffix match is
label-boundary exact: the host equals the base or ends in `.` plus the base, so
`notgithub.com` never matches `github.com`. Resolution rejects unspecified,
loopback, private, link-local, multicast, and otherwise nonpublic destinations
before pinning. Direct IP destinations, plaintext forwarding, other ports, DNS
rebinding, and missing or mismatched SNI fail closed. The broker proves a TLS
tunnel to the checked host; it does not claim visibility into the encrypted
application protocol.

For `Ambient`, the runner still uses one labeled bubblewrap supervisor but binds
the invoking user filesystem and shares host networking. It therefore provides
process supervision, not confinement, and every surface says `ambient`; no
surface calls it sandboxed. Full user powers include read access to every
same-user-readable path, including ungranted runner credential files and daemon
model-provider credential files if their paths are discoverable. The rule that
model-provider keys never reach the runner means the daemon has no runner-wire
or environment-injection path for them; it is not a filesystem-confidentiality
claim under `ambient`. Explicit profile selection accepts that exposure. A
runner has one global execution permit, so even different sessions cannot
execute tools concurrently in version one.

Runner-tool approval resolves in this order after locus selection and exact
argument validation:

1. an exact per-tool session override supplies `Auto` or `Confirm`;
2. otherwise `WorkspaceRestricted` supplies `Auto` for every version-one tool;
3. otherwise `Ambient` supplies `Auto` for `Pure` and `Confirm` for `Idempotent`
   or `SideEffecting`.

The frozen dangerous blanket remains daemon-local authority and cannot authorize
runner dispatch. A required confirmation is bound to the exact physical tool
attempt through the existing decision flow. Profile or override policy cannot
make a tool available, change its effect class, alter its arguments, or move its
locus. The resulting decision provenance and selected profile are retained with
the authorization, lease, and terminal evidence.

## Credential profiles and approval

A credential profile has two deliberately separate representations:

- the runner holds the profile credential value, provisioned out of band; and
- the daemon holds only its checked name, selection, policy, grant, and audit
  facts.

No credential-value control field exists in the runner-protocol domain.
Advertisements, registrations, placement, grants, leases, replacement changes,
and reconstitution inputs can carry only `CredentialProfileName`. Model-provider
credentials never enter runner configuration, wire state, or the injected
execution environment (INV-035). Explicit `ambient` may still read same-user
files through its full filesystem powers and carries no contrary confidentiality
guarantee.

For version one, the daemon compiles each exact tool/profile posture in a grant
from the selected sandbox profile and session override: `Auto` becomes
`Automatic`, while `Confirm` becomes `SessionPolicy`. The grant snapshot is the
durable effective posture; the runner advertisement never supplies it. An absent
pair, stale grant, or posture whose decision provenance does not match fails
closed. Profile policy cannot make an undeclared tool available and cannot alter
its effect class or admissible loci.

Before claiming a lease, the runner resolves the granted profile name against
its checked configuration. An unknown name rejects admission. For each dispatch
it opens the configured credential path without following symlinks, requires a
regular effective-user-owned file with exact `0600` mode, reads a bounded value,
and removes trailing `\n` and `\r`. The runner injects the value only under the
configured environment name inside the execution namespace; it is absent from
argv, wire state, manifests, and logs. Exact-value output redaction limits
accidental echo but is not a claim that arbitrary model-controlled execution
cannot misuse a credential within its scope.

Session creation records the requested profile as a placement axis, and that
choice is always explicit and never inferred. A session may be created with no
credentials at all: the request states either one named profile or an explicit
none, and neither the daemon nor the runner may substitute a profile for an
absent selection. A profileless placement creates no grant, resolves no
configured path, and injects no value. When it requires a repository worktree,
an entry with no profile is cloned anonymously, while an entry whose configured
clone requires a profile fails provisioning with the `credential_unavailable`
category rather than proceeding under a profile the placement did not select.
Conversely a named profile is grantable to a placement with no repository and no
workspace, because the credential belongs to the session's work rather than to a
clone ([session composition](#session-composition));
[runner credential lifecycle](configuration-and-credentials.md#runner-credential-lifecycle)
owns the configuration meaning of the optional repository profile. Why: a
silently inferred credential is an authorization the user never granted, and
refusing to guess is the only behavior that keeps the grant record a truthful
statement of intent.

The pinning transition snapshots the selected profile and validated advertised
tool set into one `CredentialProfileGrant`, because only then are the exact
runner and availability known. The grant binds the session, runner, profile, and
positive grant revision. A runner that did not advertise the profile cannot
receive the grant. Lease creation requires the current active grant, the same
pinned runner and profile, a tool present in the snapshot, and consumption of
the exact `RunnerToolAttemptAuthorization` produced after approval resolution
and bound to that tool. The grant records the exact tool/profile posture without
issuing a reusable standalone dispatch token.

Grant replacement is forward-only. It requires the supplied validated
registration to remain the enrollment-owned current revision, checks the current
grant revision, and installs one complete later snapshot. A retained stale
registration cannot mint a successor grant after later policy has been attached.
The result carries a `CredentialProfileChange` with the before-and-after profile
and tool inventories for later frontier injection. Runner replacement applies
the same current-registration gate, consumes the exact last-grant runner and
revision carried by the pinned placement, and creates a checked successor
revision. A profileless replacement carries both that placement evidence and the
lineage forward as a new terminal tombstone; omitting the tombstone is therefore
structurally rejected, and restoring a previously selected profile cannot
recreate revision one. Every prior revoked revision remains terminal. Every
successor binds its predecessor through the immediately prior placement's exact
runner and grant revision. Each independent lineage also carries the immutable
event ordinal of its revision-one placement through grant, audit, placement, and
lease references, so repeated equal runner and revision numbers within one
session cannot cross-wire provenance. Revocation is also forward-only and gates
later lease creation. A lease already offered is already dispatched and
completes or crash-classifies normally; revocation neither rewrites nor cancels
it. A revoked grant revision cannot become active again. Complete reconstitution
accepts a complete public raw-facts input, checks an independently authoritative
expected session, and rejects foreign runner facts, a profile absent from the
validated registration, or a tool set wider than the advertisement. Grants
created by initial pin or runner replacement contain the complete validated
registration tool inventory; explicit profile replacement may select a checked
subset. The store retains normalized grant snapshots and append-only issued,
replaced, and revoked audit events. Grant relations contain only profile names,
tool names, pair approval posture, and typed audit correlations: there is no
credential-value or generic payload column. A stored grant preserves the exact
approval derived from placement policy: an exact per-tool override wins, a
workspace-restricted sandbox is automatic without an override, a pure tool is
automatic without either, and every other pair uses session policy. Truncation
of immutable grant audit evidence is rejected. Lease insertion joins the current
unrevoked grant and exact tool/profile pair atomically with dispatch
authorization. Durable admission requires provenance matching that stored
placement-policy result: `Automatic` requires policy-derived automatic approval,
and `SessionPolicy` requires an exact user confirmation. The daemon-local
dangerous blanket is never accepted for runner insertion, including a direct
lease-row insert (INV-035, INV-045).

## Workspace provisioning and recovery

**Committed unimplemented functionality.** No present runner provisions,
recovers, releases, or reconciles a workspace, and no present durable daemon
producer constructs a workspace operation. The outbound broker can transport a
caller-constructed closed frame but does not authorize or journal it. Every
behavior in this section constrains that future implementation. The executable
runner leaves a typed `RecoveryUnavailable` seam whose exact `RecoveryGap` is
`UnbornHeadNotRepresentable`; it constructs no wire recovery fact because the
current recovery union cannot represent an empty clone's unborn `HEAD`.

The domain constructs a `WorkspaceProvisioningAuthorization` only for a
repository-backed successor request that the selected current registration can
satisfy. It binds a single-use identity, session, next placement revision,
enrollment, runner, registration revision, repository, sandbox, and optional
credential profile. A distinct successor uses the ordinary pinned-replacement
check; the same runner additionally requires the exact registration-loss
recovery evidence. A connection loss cannot authorize a same-runner successor,
and a stale registration, unsupported request, or request with no repository
produces no authorization. Durable command claim and authorization insertion,
dispatch, receipt consumption, and restart recovery remain the unimplemented
producer named above; the append-only relational representation and checked
readback exist independently. The application service supplies the complete
replacement command and one fresh UUIDv7 authorization candidate to a single
atomic transaction call. Its closed outcome distinguishes a stored retryable
authorization, a recorded terminal refusal, conflicting command reuse, and a
placement for which repository provisioning is not applicable. No present
production transaction implements that port, so the service alone neither claims
the command nor authorizes runner I/O.

`WorkspaceRequirement::RepositoryWorktree` is satisfiable only when the selected
validated registration advertises `WorkspaceCapability::WorktreePerSession` and
the repository key resolves in checked runner configuration to a credential-free
HTTPS clone URL. The runner accepts the provisioning authorization only when the
entry's optional profile equals the authorization's optional profile. Two absent
values authorize an anonymous clone; two equal present values authorize the
matching grant; one absent value or two unequal present values produce a
`credential_unavailable` refusal rather than a clone attempted under an
unselected or mismatched grant. That same optional equality governs every Git
operation, however the entry was reached — through the workspace manifest,
through this authorization, or through a checked `git_clone` argument
([configuration and credentials](configuration-and-credentials.md#runner-credential-lifecycle)
owns the helper that enforces it). The provisioned workspace binds the session,
placement revision, runner, repository key, exact clone URL identity, sandbox
profile, and working directory. Replacement always uses the successor placement
revision and cannot carry the prior workspace forward.

Runners are not cleanup authorities. Only the runner that provisioned a
workspace can delete it, and a runner that is replaced, revoked, or dead simply
leaves its workspace on disk: no cleanup authority resumes for a retired
identity, and no mechanism transfers ownership of an existing clone to a
successor. The system records the abandoned clone through the existing startup
workspace-leak report, which the user can read, and stops there. Reclaiming that
disk is an operator and tooling concern — periodic cleanup jobs, added per
backend over time — and is deliberately outside this contract. Why: making
cleanup a runner obligation turned every loss-based replacement into a state the
design could reach and not leave, while the alternative costs only disk — the
leak is recorded, it is bounded by the workspaces that one runner held, and no
correctness property depends on reclaiming it. The consequence is stated plainly
rather than solved: a replaced or dead runner's workspace is leaked, and the
leak record is the whole of the version-one response.

The runner opens one effective-user-owned real `0700` root without following its
final component, pins and retains its directory identity, and holds a
process-wide exclusive lock through that dirfd. Every state, staging, session,
and trash traversal is descriptor-relative beneath it with no symlink or magic
link traversal. A complete workspace lives at
`sessions/<canonical-session-uuid>/<placement-revision>/repo`, including its own
`.git`; no shared Git directory, linked worktree administration, user home path,
or credential-bearing remote URL is used. Provisioning creates a sibling staging
directory, performs the clone through the restricted profile and, when the
placement selected one, the granted credential profile, writes a versioned
`0600` non-secret manifest in the non-mounted placement parent, fsyncs the
manifest and containing directories, and atomically renames the prepared
placement directory before returning `ProvisionedWorkspace`. Exact replay
returns the matching ready receipt; conflicting facts fail closed. A clone of an
empty repository is an ordinary success, not a failure and not a reason to
remove the destination: the receipt and manifest record an unborn HEAD naming
the branch the repository's first commit will be born on, and publication
proceeds exactly as for a populated repository.

A placement that requires no worktree and names no working directory still needs
one writable root, and that private root is a managed workspace rather than
scratch space the runner forgets. It lives at the sibling path
`sessions/<canonical-session-uuid>/<placement-revision>/work`, and the runner
creates it on first use with the same fsynced non-secret manifest in the
non-mounted placement parent, recording no repository key, clone-URL digest, or
credential-profile name. Because the path is a function of durable placement
facts alone, a restarted runner recomputes it, authenticates it against that
manifest, and re-adopts the same directory rather than creating a second one,
and startup reconciliation treats it exactly as it treats a provisioned
worktree: a private root whose placement is retired is reported as a typed
retired-but-present leak, and no cleanup authority resumes for it. Why: a
restricted session's writable root is where its file and shell tools put durable
work, so a root the runner could not re-identify after a restart would discard
that work silently while the session kept running.

The workspace-manifest id is one daemon-correlated canonical UUID stable across
all lifecycle changes. It is distinct from the `workspace-manifest` content
digest: the digest authenticates the exact lifecycle-specific manifest bytes,
while ready, recorded, and release frames correlate the stable id. The manifest
lifecycle is the closed vocabulary `staging`, `ready`, `active`, or `releasing`.
Creation writes `staging`; the atomic publication rename writes `ready`; durable
`workspace_recorded` admission writes `active`; and an accepted release writes
`releasing` before the trash rename. Transitions only advance in that order,
equal replay retains the same value, and deletion is represented by absence
rather than a fifth lifecycle token. The manifest records that lifecycle, its
stable id, session, placement revision, runner, optional repository key, the
optional lowercase SHA-256 digest of the configuration-validated canonical clone
URL, optional credential-profile name, sandbox profile, relative workspace path,
and the bounded commit or branch facts needed for recovery; the repository-bound
members are absent together for a private root. The canonical URL is
credential-free, but its digest is sufficient identity and avoids repeating the
operator configuration value. Recovery resolves the repository key again and
requires the current canonical URL digest to equal the protected manifest value;
a changed mapping is `manifest_conflict` and can never reinterpret an existing
clone. The writable repository `.git/config` is not authority. The manifest
records no credential path or value. The same runner state root durably spools
one unacknowledged terminal result, one unacknowledged workspace release, and
one unacknowledged operation failure per the serial wire protocol.

Every Git invocation, in provisioning and in every Git tool alike, runs with its
effective configuration forced by the runner rather than validated after the
fact. The runner neutralizes ambient configuration by pointing
`GIT_CONFIG_SYSTEM` and `GIT_CONFIG_GLOBAL` at `/dev/null`, passes the transport
allowlist as command-line configuration — `protocol.allow=never`,
`protocol.https.allow=always`, and `protocol.ext.allow=never` — and disables
repository-local hooks. The same command line also empties the accumulated
credential-helper list before installing the helper that invocation is supposed
to use, so the effective helper set is exactly what the runner installed and
never what the repository configuration asked for: the fixed runner-owned helper
described by
[configuration and credentials](configuration-and-credentials.md#runner-credential-lifecycle)
for a Git tool reaching a remote under a granted profile, the per-provisioning
broker helper during provisioning, and no helper at all for an invocation that
reaches no remote. Command-line configuration outranks the model-writable
repository configuration, so no repository setting can move the effective
transport off HTTPS, and a re-enabled external helper or a hook cannot
substitute an executable for a fetch. Why: a repository-local
`credential.helper` whose value begins with `!` is a shell snippet Git runs, so
leaving the helper list unemptied would let an auto-approved, retry-classified
`git_fetch` execute model-authored code while every transport setting still read
as valid.

Every invocation that installs a credential helper also forces
`credential.useHttpPath=true` on that same guarded command line. An anonymous
remote invocation installs no helper. Why: with Git's false default, the helper
receives protocol and host but not the owner/repository path, so the exact-path
authorization check must return no credential and every authenticated remote
operation fails;
[runner credential lifecycle](configuration-and-credentials.md#runner-credential-lifecycle)
owns the helper and forced-configuration contract.

That forced configuration gates the transport and nothing else, and one
repository-local key defeats every check built on it. `url.<base>.insteadOf`
rewrites any URL beginning with the configured value to begin with `<base>`
instead, and it rewrites a URL given on the command line exactly as it rewrites
a stored remote. A model-written `url.<substitute>.insteadOf` whose value is the
canonical URL therefore redirects the operation to `<substitute>` over ordinary
HTTPS: the rewritten URL is HTTPS, so `protocol.allow=never` with
`protocol.https.allow=always` admits it; the stored `remote.<name>.url` is
untouched, so a check that reads it sees the canonical value; the substitute's
hostname can stay inside the admitted transport set, so the restricted broker's
hostname and SNI checks pass; and a public substitute needs no credential helper
at all. Passing an explicit URL rather than a remote name closes none of it, and
the rewrite table cannot be emptied the way the protocol and helper keys are:
`insteadOf` is an unbounded keyspace whose bases the writer chooses, so
command-line configuration can add entries to it but can never subtract them,
and no highest-priority value clears it.

The canonical repository binding therefore gets its own boundary, placed where
the transport boundary cannot reach. Every invocation that reaches a remote
first selects exactly one repository entry: provisioning uses the entry in its
placement authorization, an existing-worktree tool uses the exact key recorded
by the workspace manifest, and `git_clone` uses its checked `repository`
argument. The invocation then resolves the complete effective-URL sequence Git
will use and requires every member, byte for byte, to equal the canonical URL of
that selected entry. The runner does not compute that resolution itself; it asks
Git, under exactly the forced configuration, working directory, and repository
selection its guarded invocation will use, so each answer is post-rewrite rather
than an approximation of Git's rules.

Multiplicity follows the guarded operation rather than one generic query. A
literal URL resolves through `ls-remote --get-url` and must produce exactly one
canonical URL. A named fetch enumerates `remote get-url --all`; the result must
contain exactly one URL, and it must be canonical, because Git fetch consumes
only the first configured fetch URL. A named push enumerates
`remote get-url --push --all`; the returned nonempty sequence is exactly the
destination sequence Git push will use after `remote.<name>.pushurl` fallback,
and it must contain exactly one canonical URL. An empty result, a count other
than one, or an unequal member fails before network use. Extra fetch URLs and
additional push URLs are rejected, including a repeated canonical push URL.

A singular check is insufficient for push: `remote.<name>.pushurl` is
multi-valued, Git pushes to every configured value, and `remote get-url --push`
returns only the first. A canonical first value could therefore pass while a
later attacker-controlled destination received the pack, or while its later
failure obscured an already completed canonical push. Repeating the canonical
URL is also rejected: Git invokes the same destination twice, so a second
failure could report known process failure after the first invocation already
changed external state. Fetch does not share that fan-out — it consumes only its
first URL — but enumerating and rejecting fetch multiplicity makes the checked
count equal the operation's count instead of preserving another implicit
singular assumption. Validating the stored `remote.<name>.url` remains defense
in depth above these checks rather than the boundary itself. Why: every
pre-rewrite reading of the configuration reads exactly the value the model left
in place to be read, so only the complete effective sequence is evidence — and
the canonical binding is the whole of what stands between an auto-approved
remote operation and an attacker-chosen repository.

What that check covers is worth stating exactly, because the root cause survives
it. It resolves and then uses, and it holds because the two invocations are
adjacent under the runner's one global execution permit with repository hooks
disabled, so no model-authored code runs between them; admitting concurrent
execution or repository hooks would break it, and preserving this adjacency is a
condition on both. It binds the URL and not the bytes: fetching from the right
repository is no claim about what that repository serves. It binds the URL and
not everything else repository configuration reaches — configuration that
changes what Git runs rather than where it connects, such as content filters
applied on checkout or external diff and file-system-monitor programs, is
neutralized only where one of the command-line settings above names it, which is
a posture and not a closed set. Repository configuration is model-writable at
all because `.git` sits inside the writable root; moving it outside the model's
reach is the structural answer that would retire the class instead of
enumerating it, and it is recorded as a design question under
[tool safety](../open-questions.md#tool-safety) rather than settled here.

A daemon release is accepted only for an exact retired placement revision —
either superseded by replacement or terminal `RunnerAbandoned` — after no live
lease or unacknowledged result remains. The session itself need not be terminal
and may continue on its successor placement or with daemon-only tools.

A release exists only for a workspace the runner itself created: a provisioned
repository worktree, or the private root the runner made below its own state
root. Each of those is named by a workspace manifest, which is the identity the
`workspace_release` frame correlates against. A retired placement whose writable
root is the third kind — the exact working directory its own request named, the
plain-directory workspace of [session composition](#session-composition) — has
no manifest and therefore has no release: the daemon enqueues nothing for it,
the runner never renames or deletes it, and because that directory is not
runner-owned disk it is not reported as an unreclaimed-workspace leak either.
Retirement of such a placement is complete the moment the placement state is
durable. Why: the directory was named by the creation request rather than
provisioned by the runner, so treating retirement as a reason to delete it would
destroy a directory the system was only ever lent — and the release correlation
has no identity it could name for it in any case, which makes an unconditional
release unrepresentable on the wire as well as wrong.

Reachability is the second precondition, and it is independent of the first: a
release is enqueued only while the runner that holds the workspace is still
connected. Retirement whose predecessor connection is already durably lost —
heartbeat-loss replacement onto a different runner or onto a pending enrollment,
and every abandonment — enqueues no release, and durable loss of a connection
that still owed one retires that release as unowned. Either way the workspace
takes the recorded-leak response above rather than an exchange no identity can
complete. In version one the exchange therefore exists only for the checked
same-runner re-enrollment, where registration reconciliation retired the
placement while the connection and enrollment stayed healthy, so the runner
holding the workspace is the same runner still on the wire
([identity, enrollment, and registration](#identity-enrollment-and-registration)).
Why: both of the frames that can retire a release require the holding runner to
acknowledge deletion or report cleanup failure, and no cleanup authority resumes
for a retired identity, so a release addressed to an unreachable runner is a
durable record that is redelivered after every restart and that nothing can ever
clear — the leak this design already accepts, converted into a queue entry that
outlives it.

For a workspace it did create, the runner journals the release before it does
anything irreversible, following the same acknowledge-and-journal pattern that
provisioning and results already use. It first fsyncs a `release_accepted`
journal entry carrying the complete release correlation below its state root;
only then does it mark release in the manifest, atomically rename the placement
below `trash/`, fsync, and delete it by descriptor-relative traversal that
unlinks symlinks instead of following them. It advances the same entry to
`release_completed` after the deletion and resends `workspace_released` until
the daemon durably admits it and replies `workspace_release_recorded`; that
acknowledgement lets the runner discard the journaled release and free its
single workspace-operation slot. A crash anywhere in the interval therefore
resumes from the journal rather than from a manifest the runner may already have
deleted: `release_accepted` resumes the deletion and then reports,
`release_completed` resends the correlation, and a daemon restart cannot leave a
resent release without a boundary — exactly as for a retained result. A release
whose rename or deletion keeps failing does not hold that journal indefinitely:
the runner reports the `workspace_cleanup_failed` operation failure above, whose
`operation_failure_recorded` acknowledgement retires that journaled release
together with the failure and frees the slot, and the surviving placement is
reported as a `cleanup_failed` leak. Startup resumes deletion for trash proven
by a manifest or by a retained release entry, and may remove staging whose
manifest proves it was never published. It reconciles every ready or active
manifest with the daemon before execution and reports every unknown,
retired-but-present, conflicting, or otherwise unreconciled workspace as a typed
leak. It never silently deletes a reported leak. This startup report is visible
even when no session can be resumed.

## Open edges

- Remote runner transport, authentication, compatibility negotiation, and
  multi-host identity remain in
  [Protocols and persistence](../open-questions.md#protocols-and-persistence)
  and
  [Identity, credentials, and resource governance](../open-questions.md#identity-credentials-and-resource-governance).
- Automatic scheduling, load balancing, and MCP placement remain in
  [Scheduling and runners](../open-questions.md#scheduling-and-runners), as does
  the workspace-portability question that moving a session with a workspace to
  another runner depends on. More than one simultaneously enrolled runner is not
  an open question: it is committed functionality that version one defers
  ([the singleton-runner rule is temporary](#the-singleton-runner-rule-is-temporary)).
- Dynamic sandbox policy, catalog file parsing and reload, catalog revision
  rebinding, and concurrent execution remain in
  [Tool safety](../open-questions.md#tool-safety).
- General result-egress policy beyond exact injected-value redaction, including
  detection of transformed credential disclosure, remains in
  [Identity, credentials, and resource governance](../open-questions.md#identity-credentials-and-resource-governance).

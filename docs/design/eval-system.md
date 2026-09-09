# Evaluation system design

This committed unbuilt design extends [eval-system](../spec/eval-system.md)
using the daemon host, immutable input, journal and terminal result of
[workflows](workflows.md).

## Manifest and identity

An evaluation is one workflow run pinned to its program registration and exact
input manifest. The manifest pins the corpus artifact's SHA-256 digest, input
format, selected case positions in order, repeats, scorecard kind and the
selected judge binding. The daemon resolves the provider target/model and
non-secret credential reference, then constructs the immutable manifest with
that binding and operation contract before starting the workflow run. These
manifest bytes are the exact retained run input; credentials remain host-side.
Format-specific decoding preserves case authority context and category or notes
when present. Manifest admission requires exactly one repeat for the offline
corpus scorecard.

The workflow run identity and trial ordinal identify one trial. The manifest
fixes the mapping from each ordinal to a case position and repeat before any
trial executes; enumeration follows case order, then repeat order. A measured
repeat is a new trial; crash replay consumes the same trial's journal evidence.
Equal start retries require the same run, registration and exact input bytes.

The corpus is a digest-addressed immutable artifact. Its storage, access,
redaction, retention and deletion remain undecided under
[Corpus governance](../open-questions.md#graded-approval-judging); the blob
catalog is a storage candidate. Reference artifacts use immutable blob catalog
entries pinned and read by digest under [blob storage](../spec/blob-storage.md),
without paths or aliases. There is no mutable corpus registry.

An evaluation session creation effect verifies the calling run and trial
membership against that run's retained immutable manifest, then records an
explicit `Eval(run, trial)` creation cause. It does not require a sealed trial
row. Delegated sessions retain their ordinary delegated cause; ancestry rooted
in an Eval cause classifies every descendant as evaluation traffic. Retain the
run/manifest reference and delegation links for those reads, including when a
trial never seals; no path rewrites or deletes the required lineage.

## Effects

The native `ApprovalJudgeEval` program enumerates trials and scores their
outcomes with pure library code. Evaluation-specific effects admit only native
callers and use checked Rust-only records under the
[workflow codec contract](workflows.md#input-and-result), with the host's
grants, request ordinals and replay driver. Program code receives no provider,
database or filesystem handle. The daemon-independent library decodes the
digest-pinned corpus; sealing verifies its selected case positions against the
manifest.

| Capability / operation                   | Contract                                                                                                                                                                                                                                                                                             |
| ---------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `judge.evaluate`                         | Evaluate one manifest trial through the daemon judge adapter; journal its binding, request/contract identity, verdict and rationale or classified failure, provider-reported model and available usage. Host-generated call identities are retained evidence; each measured repeat has its own call. |
| `blob.read`                              | Read exact immutable reference bytes by digest through the catalog; use them as opaque input or through the case's declared decoder.                                                                                                                                                                 |
| `eval-record.seal`                       | Record the calling run's complete snapshot from its manifest and retained trial evidence, returning a receipt for equal-retry adoption.                                                                                                                                                              |
| Session creation for an evaluation trial | Verify the manifest reference and record the Eval cause through the host's checked session creation path.                                                                                                                                                                                            |

Requests commit before effects and deliveries before program resumption.
Provider simulation uses the scripted adapter only in tests; replay tests
disable provider access and require identical evidence and scorecards.

## Recording and sealing

`evaluation_run` and `evaluation_trial` form one sealed snapshot that evaluation
code cannot modify. The run row is keyed by the workflow run identity and
retains measurement metadata, scorecard kind and full scorecard; registration
and manifest resolve through the workflow run. Trial rows are keyed by that run
and trial ordinal and retain the case/repeat binding, expected label/provenance,
outcome and journal evidence reference, including available rationale, provider
identity and token usage with its cache accounting semantics. Failed and
ambiguous outcomes are explicit trial evidence.

Seal verifies the calling run, exact manifest membership and one resolved
outcome for every planned trial. It re-decodes the digest-pinned corpus to
derive expectations and label provenance at the selected case positions, then
derives the scorecard from that accepted evidence using the pinned scoring
implementation. Any supplied summary must agree. One transaction inserts the
run, all trials and the scorecard or inserts nothing; evaluation code cannot
update, delete or truncate sealed rows or insert later trials. Corpus-derived
fields in sealed evidence remain subject to the retention and deletion rules
decided under [Corpus governance](../open-questions.md#graded-approval-judging);
sealing does not prohibit governance actions. Equal snapshot retries adopt the
committed receipt, including commit-before-delivery recovery; changed identity
bindings, evidence or summary under the same run conflict.

The program seals before returning the same scorecard through the workflow's
Terminal/Answer result. A committed seal is a complete measurement snapshot, not
workflow terminal authority: a crash or cancellation after sealing leaves the
snapshot unchanged, while the journal determines execution status. The journal
owns progress and recovery; no mutable evaluation lifecycle tables or second
success record are added.

## Failure semantics

Missing corpus artifacts or reference blobs, wrong case payloads and
inadmissible manifests fail without a complete evaluation snapshot. A provider
failure never becomes a fabricated deny or disappears from trial evidence.
Recovery adopts durable effect proof; an unanswered non-idempotent provider call
without such proof becomes ambiguous and is not automatically repeated as the
same measured trial.

The live scorecard counts failed and ambiguous trials as unsuccessful requested
repeats, retaining their separate classifications in trial evidence. A run that
accounts for every planned trial can seal even when some measurements failed.
The offline corpus scorecard requires a verdict for every case; a failed or
ambiguous trial stops that run without a scorecard or seal. Cancelled, faulted
or interrupted execution before sealing retains its manifest and journal and
does not report a complete evaluation. Workflow cancellation, restart and
unavailable-native-revision behavior follow the
[workflows contract](workflows.md).

## Launch boundary

Both `signalbox-approval-judge-eval` and `approval-judge-eval` become operator
clients of the daemon's user-authorized evaluation launch command and workflow
read command. The evaluation launch command builds the native `Input` manifest
with the resolved provider target/model, non-secret credential reference, corpus
digest and selected case positions, then encodes it before invoking generic
workflow start with those exact bytes. Clients submit corpus input and print
their respective retained scorecards; provider selection, credentials, execution
and recording belong to the daemon. The daemon is required, and offline provider
simulation remains in tests. There is no embedded production runner or direct
database recording client.

After their final callers are removed, a forward DDL migration drops
`approval_judge_eval_run`, `approval_judge_eval_call` and their exclusive
triggers/functions, without stored-data conversion, backfill or dual recording.
Nothing builds a lasting dependency on the judge-specific recording surface.
Evaluation results remain report-only, without a quality gate or threshold.

## Approval-judge mapping

Approval-judge evaluation is the first corpus program. Its daemon-independent
library owns corpus decoding and both pure scorers; the daemon retains judge
composition. Reuse the production prompt, renderer, provider adapter and decoder
through `judge_eval_case`, outside durable approval decisions
(`apps/signalboxd/src/approval_judge_eval.rs:132`,
`apps/signalboxd/src/approval_judge_eval.rs:163`). Preserve the paid-call
ceiling and preflight of all selected cases before provider work
(`apps/signalboxd/src/bin/approval-judge-eval.rs`).

| Current scorecard and evidence                                                                                                                                                                                                                                                                                                                                                                                                                        | Designed recording                                                                                                                                                                                                                                                                                                                                                                                                                    |
| ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Offline corpus: accuracy, three-disposition precision/recall with true/false counts and nullable ratios, ordered verdicts with expected/actual, correctness, rationale and label provenance (`crates/approval-judge-eval/src/lib.rs`). The command consumes one recorded response per case (`apps/signalboxd/src/bin/signalbox-approval-judge-eval.rs`); scoring stops on a judge error (`apps/signalboxd/src/bin/signalbox-approval-judge-eval.rs`). | One trial per case; expected/provenance is derived from the digest-pinned corpus at the selected positions, actual/rationale from journaled judge evidence. The run stores the unchanged complete corpus scorecard. Recorded-response execution is a test fixture.                                                                                                                                                                    |
| Live JSONL: per-category and overall majority, instability, stability-unmeasured, partial/unmeasured and failed-call counts, escalation calibration, score metadata and per-case evidence (`crates/approval-judge-eval/src/lib.rs`). Optional recording stores binding, digests, cache-accounting semantics and successful-call usage (`crates/persistence/src/approval_judge_eval.rs:63`, `crates/persistence/src/approval_judge_eval.rs:99`).       | One trial per selected case/repeat, including failures and ambiguity. Preserve strict majority against requested repeats, ties, nullable stability, per-case rationales/reported models, notes/posture, speculative tools, scoring version and scorecard digests. The run retains the full live scorecard and binding; trials retain usage. Corpus artifact SHA-256 identity is separate from the scorecard's existing digest fields. |

Sealing carries the atomic recording and evidence/summary agreement enforced by
`crates/persistence/src/approval_judge_eval.rs:626` into the general tables. The
two metric sets remain distinct. ExecStage, generalized session scoring,
exporters, dashboards, external trackers and automated optimization are not part
of this design.

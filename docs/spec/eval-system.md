# Evaluation system

The evaluation system measures the approval judge against a labeled corpus and
reports a scorecard.

`scripts/review_judge_eval.py` runs externally supplied review findings through
the daemon's configured judgment session template. Its JSONL cases carry `id`,
`head_sha`, `base_sha`, `pr_title`, `pr_scope`, `findings` (with `finding_id`),
and `context`. It reuses one detached scratch worktree per head SHA across runs
under the configured workspace, keeps case inputs and outputs separate, supplies
the base diff, and retains the structured tool-written judgment with session,
turn, frontier, usage, and elapsed-time evidence. The caller can select a
catalog alias with `--alias` or a direct catalog entry with `--selection-id`,
and override reasoning with `--effort`. It performs no publication. A failed
trial with no observed terminal frontier stops new session submissions until the
caller resolves it. Labels and scoring stay with the caller.

## Overview

The evaluation system defines, on top of the [workflows](workflows.md) layer,
what an evaluation is and what its corpus and expectations are. Its recording
schema lives in the migrations. Both daemon-package operator binaries upload
corpus input, launch the eval workflow through the daemon and print their
distinct sealed scorecards.
`signalbox-approval-judge-eval --socket PATH CORPUS.json RESPONSES.json` uses
recorded responses. `approval-judge-eval --socket PATH --cases CASES.jsonl` uses
the daemon's configured approval judge, with repeat, filter and limit selection;
`--responses FILE` supplies recorded answers instead. Provider selection,
credentials, execution and recording belong to the daemon. The
daemon-independent library decodes both corpus shapes and computes their
existing scorecards. Synthetic labels retain their free-text provenance.

## Workflow trials

The compiled `approval-judge-eval` revision `1` program pins a corpus blob
SHA-256 digest, input format, ordered case positions, repeats and non-secret
judge binding in its immutable run input. At least one selected case is
required. Trials follow case order, then repeat order, with a maximum of 1,000
calls. Offline scoring requires one repeat. `corpus.load` reads and preflights
every selected case before provider work, rejecting duplicate selected live case
names. Only selected live JSONL rows are decoded. `judge.evaluate` addresses a
trial ordinal in that retained manifest. The attempt reuses its decoded,
preflighted corpus across trials; recovery reloads it once when needed.

The host adapters reuse the catalog's verified blob reads and `judge_eval_case`.
Judge answers retain call identity, rendered-request digest, binding and
contract, verdict/rationale or failure, reported model and available token
usage. Failed trials retain observed model identity, including a substituted
lineage. Configured usage limits apply. The common host journals requests and
answers; an admitted unanswered judge call without durable proof becomes
ambiguous without provider retry. Invalid requests and unavailable pinned
bindings retain their rejection on recovery. Adoption applies corpus preflight;
infrastructure failures leave the request unanswered. The native program
computes the existing offline or live scorecard through the pure library and
returns it as the workflow result. Live scoring counts failed and ambiguous
repeats as unsuccessful; an offline trial without a verdict faults without a
scorecard. Missing blobs and inadmissible cases fail before provider work. The
program calls `eval-record.seal` before returning the scorecard. On first seal,
the host re-decodes the pinned corpus, verifies complete manifest trial
membership and derives the scorecard from retained journal answers; a supplied
scorecard must agree. `evaluation_run` and `evaluation_trial` commit as one
immutable snapshot keyed by the workflow run and trial ordinal, preserving
corpus labels and provenance, verdicts, classified failures, ambiguity and full
journal evidence. Equal identity and content retries adopt the same receipt,
including recovery after commit but before delivery, without rereading the
corpus; changed identity, evidence or summary conflicts. A partial snapshot
cannot commit, and sealed rows reject update, delete, truncate and late trial
insertion. Sealing does not determine workflow terminal status.

The daemon composes Corpus, Judge, Blob and EvalRecord adapters when blob
storage is available. Evaluation launch resolves the judge binding and encodes
the exact immutable manifest before generic workflow start. Each attempt
composes its adapters from the installed configuration snapshot. Recorded
responses are pinned in that manifest, one per trial, and execute through the
same judge adapter without provider access. The process read command returns
byte ranges of the sealed scorecard. The typed TypeScript fixture uses the same
effect records; token counts and pull-request identities use decimal strings.

## Design decisions

Replay uses the daemon's current approval-judge prompt, renderer,
structured-output contract, and decoder without entering the daemon's durable
decision path. Why: the harness measures the deployed judge, not a fork of it.

Evaluation verdicts gate nothing; every evaluation surface is report-only.

The checked-in seed corpus and response file contain synthetic strings only, so
no real request data enters the repository.

## Planned

- Evaluation-created sessions whose provenance stays walkable through delegation
  lineage for as long as evaluation rows are read:
  [design](../design/eval-system.md).

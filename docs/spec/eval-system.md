# Evaluation system

The evaluation system measures the approval judge against a labeled corpus and
reports a scorecard.

## Overview

The evaluation system defines, on top of the [workflows](workflows.md) layer,
what an evaluation is and what its corpus and expectations are. Its recording
schema lives in the migrations. Only the approval-judge harness is built.

The harness is the `signalbox-approval-judge-eval` workspace crate, a temporary
standalone evaluation surface for the three-disposition approval judge that the
[tool loop](tool-loop.md) owns. Its data is a JSON corpus of synthetic cases.
Each case pairs a tool request and its frozen authority context with the
expected disposition and free-text provenance for that label. The corpus is
consumed directly as opaque evaluation input after JSON shape decoding.

The daemon-independent library decodes both corpus shapes and computes their
distinct scorecards. Both entry points live in the daemon package. The offline
entry point replays recorded provider responses in corpus order through a
scripted model adapter, requires one response per corpus case, and prints the
scorecard as JSON.

The live-provider runner in the daemon is not part of the harness. It reads its
own JSONL case file in its own case shape, sends each case to a configured
provider, prints its own scorecard, and can record the run in judge-specific
tables.

## Workflow trials

The compiled `approval-judge-eval` revision `1` program pins a corpus blob
SHA-256 digest, input format, ordered case positions, repeats and non-secret
judge binding in its immutable run input. Trials follow case order, then repeat
order, with a maximum of 1,000 calls. Offline scoring requires one repeat.
`corpus.load` reads and preflights every selected case before provider work,
rejecting duplicate selected live case names; `judge.evaluate` addresses a trial
ordinal in that retained manifest. The attempt reuses its decoded, preflighted
corpus across trials; recovery reloads it once when needed.

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
scorecard. Missing blobs and inadmissible cases fail before provider work. No
evaluation snapshot is sealed.

`WorkflowRuntime::with_eval` supplies the runner's host services and enables
native eval registration; the default daemon rejects that registration. Operator
launch composition remains with the planned launch boundary. The typed
TypeScript fixture calls the same Corpus, Judge and Blob methods; token counts
and pull-request identities use decimal strings across the JavaScript boundary.

## Design decisions

Replay uses the daemon's current approval-judge prompt, renderer,
structured-output contract, and decoder without entering the daemon's durable
decision path. Why: the harness measures the deployed judge, not a fork of it.

Evaluation verdicts gate nothing; every evaluation surface is report-only.

The checked-in seed corpus and response file contain synthetic strings only, so
no real request data enters the repository.

The live-provider runner is an operator-driven surface outside the offline
harness, because it spends provider quota.

## Planned

- Evaluation-created sessions whose provenance stays walkable through delegation
  lineage for as long as evaluation rows are read:
  [design](../design/eval-system.md).
- Sealed workflow evaluation recordings and operator launch commands, after
  which the judge-specific tables are dropped without data conversion:
  [design](../design/eval-system.md).
- The judge-specific recording surface is temporary; nothing may build on it in
  a way that outlives it: [design](../design/eval-system.md).

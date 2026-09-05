# Evaluation system

The evaluation system measures the approval judge against a labeled corpus and
reports a scorecard.

## Overview

The evaluation system defines, on top of the
[program substrate](program-substrate.md), what an evaluation is and what its
corpus and expectations are. Its recording schema lives in the migrations. Only
the approval-judge harness is built.

The harness is the `signalbox-approval-judge-eval` workspace crate, a temporary
standalone evaluation surface for the three-disposition approval judge that the
[tool loop](tool-loop.md) owns. Its data is a JSON corpus of synthetic cases.
Each case pairs a tool request and its frozen authority context with the
expected disposition and free-text provenance for that label. The corpus is
consumed directly as opaque evaluation input after JSON shape decoding.

The library loads a corpus, replays each case through the judge, and scores the
verdicts into a scorecard. The offline entry point replays recorded provider
responses in corpus order through a scripted model adapter, requires one
response per corpus case, and prints the scorecard as JSON.

The live-provider runner in the daemon is not part of the harness. It reads its
own JSONL case file in its own case shape, sends each case to a configured
provider, prints its own scorecard, and can record the run in judge-specific
tables.

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
- Reference artifacts pinned by digest as immutable blobs under the contract
  [blob storage](blob-storage.md) owns: [design](../design/eval-system.md).
- Judge evaluations on the program substrate, after which the judge-specific
  tables and their data are dropped without migration:
  [design](../design/eval-system.md).
- The judge-specific recording surface is temporary; nothing may build on it in
  a way that outlives it: [design](../design/eval-system.md).

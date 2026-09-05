# Evaluation system

The evaluation system measures the approval judge against a labeled corpus and
reports a scorecard.

## Map

The evaluation system defines, on top of the
[program substrate](program-substrate.md), what an evaluation is and how its
corpus, expectations, and results are recorded. Only the approval-judge harness
is built.

The harness is the `signalbox-approval-judge-eval` workspace crate, a temporary
standalone evaluation surface for the three-disposition approval judge that the
[tool loop](tool-loop.md) owns. Its data is a JSON corpus of synthetic cases.
Each case pairs a tool request and its frozen authority context with the
expected disposition and the provenance of that label. A portable manifest names
the corpus, its version, and the source of its cases.

The harness has three parts. The library loads a corpus through a pluggable
store contract, replays each case through the judge, and scores the verdicts
into a scorecard. Both the disk store and the database store verify the corpus
digest on load. The offline entry point replays recorded provider responses
through a scripted model adapter and prints the scorecard as JSON. The
live-provider runner in the daemon sends the same cases to a configured provider
and can record the run in judge-specific tables.

## Decisions

Replay uses the daemon's current approval-judge prompt, renderer,
structured-output contract, and decoder without entering the daemon's durable
decision path. Why: the harness measures the deployed judge, not a fork of it.

Repository paths in a manifest are never fetched: the operator supplies a
checkout that holds the manifest and its source files, and the recorded
repository identity is provenance only. Why: corpus metadata never drives a
network request.

Evaluation verdicts gate nothing; every evaluation surface is report-only.

The checked-in seed manifest, corpus, and response file contain synthetic
strings only, so no real request data enters the repository.

The live-provider runner is an operator-driven surface outside the offline
harness, because it spends provider quota.

No case field admits a number. Why: RFC 8785 number serialization is not
implemented, so a numeric field would leave the digest undefined.

## Contracts

The corpus digest is independent of storage form. It is SHA-256 in lowercase
hexadecimal over the corpus's logical cases, each serialized to canonical JSON
and ordered by case identifier bytewise. Canonical JSON sorts object keys
bytewise, has no insignificant whitespace, and escapes strings by RFC 8785. For
corpus format version one, the preimage is the bytes `signalbox-eval-corpus-v1`,
one zero byte, the case count as an unsigned 64-bit big-endian integer, and then
for each case its canonical-JSON byte length as an unsigned 64-bit big-endian
integer followed by those bytes. No test pins the preimage or the escaping rule;
`corpus_digest` in the harness crate is the one implementation.

## Not built

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

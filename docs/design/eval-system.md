# Evaluation system design

This committed unbuilt design extends [eval-system](../spec/eval-system.md)
using the daemon host, immutable input, journal and terminal result of
[workflows](workflows.md).

Corpus access, redaction, retention and deletion remain undecided under
[Corpus governance](../open-questions.md#graded-approval-judging).

## Evaluation session provenance

An evaluation session creation effect verifies the calling run and trial
membership against that run's retained immutable manifest, then records an
explicit `Eval(run, trial)` creation cause. It does not require a sealed trial
row. Delegated sessions retain their ordinary delegated cause; ancestry rooted
in an Eval cause classifies every descendant as evaluation traffic. Retain the
run/manifest reference and delegation links for those reads, including when a
trial never seals; no path rewrites or deletes the required lineage.

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

ExecStage, generalized session scoring, exporters, dashboards, external trackers
and automated optimization are not part of this design.

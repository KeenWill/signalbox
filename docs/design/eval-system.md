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

ExecStage, generalized session scoring, exporters, dashboards, external trackers
and automated optimization are not part of this design.

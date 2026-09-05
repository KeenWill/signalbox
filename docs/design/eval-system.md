# Evaluation system design

This design is not built; it extends [eval-system](../spec/eval-system.md) with
the committed evaluation work that binds present code.

## Goal

Run approval-judge evaluations on the
[program substrate](../spec/program-substrate.md) with durable evaluation
provenance and digest-pinned reference artifacts, then retire the judge-specific
recording tables.

## Shape

A session created by an evaluation carries the eval creation cause, naming the
evaluation run and the trial that created it;
[sessions-and-transcript](../spec/sessions-and-transcript.md) owns the cause
vocabulary. A session the model delegates from inside an evaluation-created
session carries its ordinary delegated cause. Evaluation traffic is therefore
every session whose creation-cause ancestry, followed through the recorded
delegation lineage, roots in an eval cause; the predicate is transitive, not a
single column.

A reference artifact is an immutable blob that a case pins by digest under the
contract [blob storage](../spec/blob-storage.md) owns. A run reads it through
the blob catalog, never through a path or alias.

Judge evaluations move onto the substrate. Until then the live-provider runner
records into judge-specific tables. Once judge evaluations run on the substrate,
those tables and their recorded data are dropped without migration, because a
recorded run is a reproducible measurement, not history.

## Constraints on present code

No path deletes or rewrites a stored delegation link while evaluation rows that
depend on that lineage are still read.

Nothing builds on the judge-specific recording surface in a way that outlives
it; a reader of those tables is a reader of the harness, not of the daemon.

## Acceptance

A session created by an evaluation carries the eval cause with run and trial
identity, and a query that walks delegation lineage classifies every descendant
as evaluation traffic.

A case can name a reference artifact by blob digest, and the run reads it
through the blob catalog.

Judge evaluations run on the substrate, and the judge-specific tables are gone
together with everything that read them.

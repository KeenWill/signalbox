# Evaluation system

**Foundation contract.** This page owns the evaluation semantics built on the
[program substrate](program-substrate.md): what an evaluation is, how its
corpus, expectations, trials, and stages are recorded, and how evaluation
traffic stays unmistakably separate from production traffic. The entire surface
below other than the explicitly implemented standalone approval-judge harness is
committed ahead of code as Stage 0. Stage 0 was verified through PR #580
(`agent/program-substrate-spec`). Execution, registration, journaling, and
replay are owned by the substrate page and not restated here; model scoring is
ordinary session traffic owned by
[model-call execution](model-call-execution.md); the sandboxed process boundary
for stage executors is owned by [tool loop](tool-loop.md)'s execution surface.

## Evaluations are programs

**Committed unimplemented functionality.** No present surface defines
evaluations. An evaluation definition is a registered program whose grant list
includes `corpus`, `eval-record`, and as needed `session`, `judge`, and
`exec-stage` — exactly the substrate's closed capability vocabulary, so an
evaluation registration can receive every capability this page describes.
Registering an evaluation is inserting rows — never a change to this repository.
A stock built-in orchestrator ships with the substrate so the simplest
evaluation — run one executor per case, score with the expectation grammar — is
a registration row naming a corpus and an executor, with no program authored.
Why: the evaluation system exists to make creating evaluations cheap in the
user's own projects, so the zero-authoring path is part of the contract, not a
convenience added later.

**Committed unimplemented functionality.** No present surface separates
evaluation provenance. Sessions created by evaluation programs carry the `eval`
creation cause with run and trial identity. A session the model delegates from
inside an evaluation-created session carries its ordinary delegated cause —
delegation stays the model's autonomy — so the separating predicate is
transitive, not single-column: evaluation traffic is every session whose
creation-cause ancestry, followed through the recorded delegation lineage, roots
in an `eval` cause, and the stored delegation linkage must keep that ancestry
walkable for as long as evaluation rows are read. Evaluation model calls are
metered with the same configured billing rates as all traffic. Evaluation
verdicts gate nothing: every evaluation surface is report-only until repeats,
baselines, and comparison views exist, and any future gating is a separate
decision this page does not commit.

## Corpus and expectations

### Standalone approval-judge harness

The implemented `signalbox-approval-judge-eval` workspace crate is the temporary
standalone evaluation surface for the current three-disposition approval judge,
verified against this PR (`agent/eval-corpus-stores`). Its version-one JSON
corpus carries each synthetic tool request and frozen authority context; valid
JSON argument text is normalized by the daemon renderer before judging. It also
carries an expected `approve`, `deny`, or `escalate_to_human` disposition and
nonempty free-text label provenance. Its replay uses the daemon's current
approval-judge prompt, request renderer, structured output contract, and
decision decoder without entering the daemon's durable decision path. The
library reports every case verdict, exact-match accuracy, and one-vs-rest
precision and recall for every disposition; a rate whose denominator is zero has
no decimal value and retains its zero denominator.

The operator entry point is offline: it consumes a portable corpus manifest and
ordered recorded responses, loads the corpus through the pluggable store
contract, feeds those responses through the repository's deterministic scripted
model adapter, and emits the scorecard as JSON. Each recorded response names
both its case id and a fingerprint of the rendered request identity it was
recorded against — SHA-256 in lowercase hexadecimal over one JSON object with
bytewise-sorted keys and no insignificant whitespace, covering the case id,
every request field, and the exact judge system prompt, absent optional fields
serialized as null — and replay rejects a response whose fingerprint does not
match the corpus case at its position, including after a case rename or a prompt
revision. The checked-in seed manifest, corpus, and response file contain
synthetic strings only. The existing `signalboxd` live-provider runner remains a
separate explicitly operator-driven surface and is not part of this offline
contract.

The standalone harness implements a pluggable corpus-store contract with
enumeration and digest-verified load operations. Its disk store resolves a
repository case path relative to a portable manifest and retains it as a
checkout-root-relative provenance path. Its database store keeps
evaluation-corpus registration rows and ordered case rows in one instance's
PostgreSQL database; an import library call verifies a repository or embedded
database-native manifest and inserts both atomically. Repeating an identical
import is idempotent, while reusing a suite name and version for different
metadata or cases fails closed. Enumeration returns suite name, author-chosen
version, corpus format version, digest, case count, and one source descriptor:
repository identity plus path, database-native, or a blob digest reference with
byte length and optional instance-local store binding. Database registrations
also bind the case-identifier replay sequence with an order-sensitive digest, so
reordering durable case rows fails closed independently of the logical digest.

A version-one portable manifest names its own format version, suite name, corpus
version, corpus format version, and tagged case source. Repository sources carry
an author-supplied repository identity and a portable path relative to the
manifest; database-native sources embed cases for import; blob sources carry a
SHA-256 digest and byte length plus an optional store binding. Integrity binds
the logical corpus digest, each case's canonical-JSON digest, and for repository
sources the exact source-file byte digest. Repository paths are not fetched: the
operator supplies a checkout containing the manifest and source, so the recorded
repository identity is provenance rather than ambient network authority.

The corpus digest is storage-form-independent: SHA-256 in lowercase hexadecimal
over the corpus's logical cases, each serialized to canonical JSON (object keys
sorted bytewise, no insignificant whitespace, and numbers serialized by RFC 8785
JSON Canonicalization Scheme section 3.2.2.3) and ordered by case identifier
bytewise. Corpus numbers are exactly finite IEEE 754 binary64 values;
registration rejects NaN, infinities, and values outside that domain. RFC 8785's
ECMAScript serialization governs decimal versus exponent notation and renders
negative zero as `0`; its string escaping rules govern all JSON strings. Shared
corpus admission rejects U+0000 in every case string because PostgreSQL JSONB
cannot preserve it, keeping repository, disk, and database storage forms
aligned. The versioned preimage below pins that algorithm for corpus format
version one. The
exact digest preimage is the UTF-8 bytes `signalbox-eval-corpus-v1` followed by
one zero byte, the case count as an unsigned 64-bit big-endian integer, and
then, for each case in that order, its canonical-JSON byte length as an unsigned
64-bit big-endian integer followed by those bytes. Lengths count bytes, not
characters. This aggregate framing is owned by this page rather than inferred
from the program substrate's single-file preimages, so the same logical corpus
computes the same identity whether loaded from repository files, an artifact, or
rows, and a run verifies its corpus after the content moves between admitted
storage forms. The checked-in corpus is only one manifest-backed fixture;
neither the contract nor the database assumes that repository, one database, or
one Signalbox instance. Per the [pre-alpha rule](../../AGENTS.md), no compatibility
machinery attends any of this: corpus formats and storage may change freely
until first durable deployment.

**Committed unimplemented functionality.** No present corpus store loads case
content through a blob binding. The portable reference and database registration
shape reserve that backend without selecting a blob client or duplicating the
blob-storage contract. No present daemon command attaches or detaches a corpus
live; a future command must select the target instance, construct the chosen
store from that instance's repository/database/blob bindings, verify or import
the manifest, update the instance's registration set, and make subsequent
evaluation dispatch resolve through that refreshed set.

**Committed unimplemented functionality.** No present general substrate surface
checks the expectation grammar described below. That grammar spans three check
kinds — closed-vocabulary labels, typed numeric constraints
(exact-within-tolerance, range, count, boolean), and reference-artifact
comparisons by named continuous metric with thresholds — declared per case, each
check optional. A case with a missing reference degrades that check to
`unmeasured` and never loses its row. A reference artifact is an immutable blob
a case pins by digest under the contract [blob storage](blob-storage.md) owns;
no named-artifact aggregate is required — mutable aliases, producer provenance,
and ownership above a blob remain the open aggregate question recorded in
[open-questions](../open-questions.md#general-purpose-artifacts), and nothing in
this grammar depends on it.

## Trials, stages, and recording

**Committed unimplemented functionality.** No present surface records evaluation
results. An evaluation run records: run identity with the pinned program digest,
corpus digest, and configuration; one trial row per case and repeat ordinal; and
one stage row per trial stage with a status from the closed set `scored`,
`skipped`, `infrastructure`, `unmeasured`, plus metrics, error, and duration;
one relational case-projection row per trial containing the resolved slice
labels and other declared grouping dimensions as canonical typed key/value rows;
and one check-outcome row per declared check per scoring stage, recording the
check kind, its target, the expected value or threshold as resolved from the
corpus, the nullable measured value, and a check verdict from the distinct
closed set `passed`, `failed`, `unmeasured`, `not_run`. Numeric, label, boolean,
and reference-artifact checks use `passed` or `failed` when measured; a missing
reference uses `unmeasured`; and a check not attempted because its stage was
`skipped` or `infrastructure` uses `not_run`. The check-outcome rows are what
make the promised views derivable without reopening external corpus content; the
trial's case projection makes slice membership derivable under the same
constraint. The trial's case projection is resolved from the digest-verified
corpus and committed with the trial before scoring begins, so a later corpus
move cannot change grouping. A scoring stage commits each check-outcome row only
after scoring has produced that row's measured value and verdict, or established
`unmeasured` or `not_run`; in the same transaction it records the check kind,
target, and resolved expectation from the already digest-verified case. Thus no
pre-scoring placeholder check-outcome row exists. A stage mixing measured and
missing-reference checks is represented check by check, and its single stage
status summarizes without substituting. Durable cost is recorded per model call
with the model and rate version that priced that call — an evaluation may score
with one model and judge with another, and the configured rate catalog versions
each model's rates independently, so no single run-level rate version exists;
run-level cost is an aggregation grouped by model and rate version, never a
stored scalar that forgets its pricing. Stages order cheap to expensive, and a
stage failure is recorded on its row without discarding the trial's other
stages. Aggregation — accuracy by slice, stability across repeats, thresholded
pass rates, run-versus-run comparison — is derived by SQL views over these rows,
never stored as opaque summaries alone. Repeats are fresh-journal repeats, so
verdict stability is measured against genuinely re-sampled nondeterminism, while
an interrupted run resumes by replay without re-spending a model call.

**Committed unimplemented functionality.** No present surface executes
evaluation stage code. Heavyweight subject-under-test execution — building a
project, running generated code, comparing artifacts — happens in stage
executors: sandboxed processes provisioned from a source pinned at registration
(repository and commit, or artifact digest) under the same supervised execution
boundary as session exec tools, exchanging one typed JSONL request and response
per stage. The registration additionally records an executor environment
identity — the command line and a declared environment digest naming the
toolchain or image the executor expects — and every stage row carries the
environment identity that actually ran it, so a comparison view can distinguish
sampled trial instability from an environment change between runs rather than
attributing recompiled-toolchain drift to the subject. The recorded identity is
measurement provenance, not hermeticity: this page commits no
environment-reproduction machinery. Executor output is validated against the
stage's declared schema host-side; executor failure is a recorded stage status,
never a run fault. The isolate never runs subject code, and executors never
touch the database or hold credentials.

**Committed unimplemented functionality.** No present surface records
judge-evaluation runs in the database; the standalone judge-evaluation harness
emits scorecard output only, and an in-flight change proposes judge-specific
recording tables ahead of this system. Whatever judge-specific recording surface
exists when the substrate's rows land is superseded by them: once judge
evaluations run on the substrate, any such tables and their recorded data are
dropped without migration — those recorded runs are reproducible measurements,
not history that binds. Per the pre-alpha rule in `AGENTS.md`, this destruction
is deliberate and carries no compatibility ceremony, and nothing may build on a
judge-specific recording surface in a way that outlives it.

## Open edges

- Evaluation exporters toward external trackers:
  [open-questions](../open-questions.md#program-substrate-and-evaluations).

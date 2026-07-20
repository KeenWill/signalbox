# Decision log

An append-only, dated record of decisions below foundation weight, newest first.
Each entry states context, the decision, rejected alternatives, and what it
affects, in roughly ten to twenty lines. Foundation-weight changes — altering
accepted ADR semantics, moving a boundary between domain, storage, wire, or
framework representations, weakening an invariant, or introducing a technology
that constrains several components — require a full record under
[decisions/](decisions/README.md) instead. Unresolved questions live in
[open-questions.md](open-questions.md).

## 2026-07-20 — ADR-0044 post-merge review corrections

**Context.** Post-merge review of the pull request that introduced
[ADR-0044](decisions/0044-hub-runtime-foundations.md) found four defects: the
configuration clause classified the `DATABASE_URL` password under ADR-0017,
whose scope is provider and integration credentials, silently closing the
database-credential delivery decision ADR-0032 reserves; the identifier
redaction rules contradicted the mandatory corruption diagnostic keys; the
mandatory session key made registry-level corruption events unreportable; and
the closed taxonomy left ADR-0035's concurrent-staleness category unplaced.

**Decision.** Amend ADR-0044 in place with four noted corrections: the
database-credential delivery channel remains open per ADR-0032 and the hubd
slice's environment read is explicitly provisional — reversing an unauthorized
silent closure back to ADR-0032's reservation; opaque UUID-backed aggregate and
command identifiers are classified loggable under ADR-0033, with redaction
targeting user content, credential material, and free-form payload values; the
session corruption key is conditional on session-scoped operations; and
concurrent staleness is consumed inside adapters, outside the operator taxonomy.

**Rejected alternatives.** Leaving the identifier contradiction for implementers
to reconcile: each slice would pick its own resolution, and the mandatory keys
or the allowlist would be violated silently. Superseding ADR-0044 with a new
record: the corrections restore accepted meaning rather than change a decision.

**Affects.** [ADR-0044](decisions/0044-hub-runtime-foundations.md) only; no
code, schema, or other accepted semantics.

## 2026-07-20 — Orientation-doc refresh through the ADR-0041 boundary

**Context.** A documentation-truth audit found the orientation documents stopped
absorbing accepted decisions at the ADR-0038 boundary: the glossary lacked
entries for the concepts owned by ADR-0036 through ADR-0041, the scenarios
preamble described a `Covered by:` coverage mechanism the repository does not
use, the architecture overview omitted ADR-0038/0040/0041 from its decided
chains, and vision.md retained pre-implementation phrasings about a future
domain.

**Decision.** Refresh citations and entries only, changing no semantics:
glossary entries linking each owning record ([glossary.md](glossary.md)),
citation and preamble corrections in [scenarios.md](scenarios.md), decided-chain
and outbox pointers in [architecture.md](architecture.md), and two
meaning-preserving tense fixes in [vision.md](vision.md). Every addition links
to its owner rather than restating it.

**Rejected alternatives.** A full vision.md rewrite: deferred to the owner, who
has not commissioned one. Restating decided semantics in the overview documents:
the one-place rule keeps each normative statement with its owning record.

**Affects.** [glossary.md](glossary.md), [scenarios.md](scenarios.md),
[architecture.md](architecture.md), and [vision.md](vision.md); no code, schema,
or accepted semantics.

## 2026-07-19 — Destination features recorded as owner-directed direction

**Context.** Milestone selection needed the owner's post-model-call product
direction written down: compaction as a first-class surface, inter-session
messaging, orchestrator sessions and linking, a linking/visibility authority
model, platform goal mode, and the tool system as the layer carrying them.
Nothing recorded where those features sit relative to reserved, accepted, or
required future decision records.

**Decision.** Add a directional [Destination features](target-model.md) section
to the target model mapping each feature to its owning reserved, accepted, or
required future decision record, plus Target rows in the concept status map for
inter-session messaging, session linking and visibility authority, and
persistent goal identity and lifecycle, plus the standing update-subscription
lifecycle the planned callback surface requires. This is owner-directed
direction only, at ordinary weight: no semantics are decided, no open question
closes, and every feature still needs its owning decisions before code.

**Rejected alternatives.** Recording the direction as ADR proposals: none of the
features has settled semantics to propose. Leaving it to the priority order
alone: the order says when, not what the destination is or which seats own it.

**Affects.** [target-model.md](target-model.md) and milestone selection under
[goal-mode.md](goal-mode.md); no code, schema, or accepted semantics.

## 2026-07-19 — CodeRabbit pre-merge checks mirror repository rules

**Context.** Several repository rules — invariant-catalog honesty, decision
weights, single statement of record, description budget and claim accuracy,
testing style, migration immutability, goal-mode surface freezes, sealed-spine
prose, and append-only decision records — bind every pull request but are
judgment calls no CI step can run, so compliance depended on reviewers
remembering to check. CodeRabbit's pre-merge checks can evaluate such criteria
per pull request from a version-controlled `.coderabbit.yaml`.

**Decision.** Adopt nine custom pre-merge checks in `.coderabbit.yaml` as
verdict-logic mirrors of rules owned by [AGENTS.md](../AGENTS.md),
[testing-style.md](testing-style.md), [goal-mode.md](goal-mode.md), and
[decisions/README.md](decisions/README.md). Ownership stays with those
documents; the YAML restates only operational pass/fail logic, and a comment
above each check names its owning document. The three mechanical checks —
migration immutability, frozen-surface citation, append-only decision records —
run in `error` mode now; the six judgment checks run in `warning` mode pending
calibration against real reviews, with the catalog-honesty and
description-accuracy checks first in line for promotion to `error`.
`request_changes_workflow` and `override_requested_reviewers_only` are enabled
so failing checks gate approval and only requested reviewers can override them.

**Rejected alternatives.** Configuring the checks only in CodeRabbit's web UI:
unreviewable, unversioned, and subject to a documented 1,000-character limit on
in-app instructions that these checks exceed. Encoding rules CI already enforces
(fmt, clippy, spine sync, mdformat) as checks: redundant with faster,
deterministic enforcement. Checks for the consumer rule, wave hygiene, and
CI-green status: unverifiable from the sandbox this configuration was authored
in, so they would ship untested.

**Affects.** `.coderabbit.yaml` (new); CodeRabbit review behavior on every
future pull request; no code, schema, or accepted semantics. As a change to
process and tooling rules, the pull request introducing this file fires the
ordinary-decision trigger of its own check 2 — this entry is that check's
required record, satisfying the checker with the checker's own paperwork.

## 2026-07-19 — Meaningful review re-requests and surfaced stack replacement

**Context.** The finished-pull-request protocol re-requested external reviews on
the final commit after any rebase, so quiet-state pull requests re-summoned full
bot passes after merge-main and formatting commits — about 15–20% of CI runs
reviewed nothing new. Separately, an autonomous run replaced an open
pull-request stack with a rewrite without the owner seeing the choice. Codex
review also requires an explicit trigger, so relying on implicit branch events
can leave the requested review absent.

**Decision.** External review re-requests follow the meaningful-diff bar now
stated in the finished-pull-request bullets of [AGENTS.md](../AGENTS.md), Codex
is invoked there through an explicit `@codex review` comment, and replacing or
abandoning an open stack is surfaced to the owner under that file's
working-autonomously guidance. `AGENTS.md` carries the normative wording.

**Rejected alternatives.** Re-requesting on every push: keeps spending full
review passes on commits that cannot change an approval. Relying on implicit
Codex automation: does not reliably start a review. A hard approval gate on
stack replacement: heavier than the problem — surfacing before the replacement
lands preserves owner visibility without blocking work.

**Affects.** Review-request behavior on every pull request, stack management in
autonomous runs, and the linked rule home. It changes no code, CI configuration,
or review-wave semantics.

## 2026-07-19 — Post-milestone-2 audit corrections and tracked obligations

**Context.** A post-milestone-2 audit of the turn-activation stack reviewed CI
coverage, documentation truthfulness, and the fail-closed posture of the new
scheduling and activation seams. It found that
`cargo test --workspace --all-targets --all-features` excludes doctests, so the
domain's 53 `compile_fail` sealing proofs never ran in CI; that several overview
documents had gone stale behind merged code; and that one coverage gap and
several deliberately accepted asymmetries were recorded nowhere.

**Decision.** CI's `validate` job and the validation sequence gain
`cargo test --workspace --all-features --doc`. The scheduling reconstitution
fail-closed matrix is incomplete — `AcceptedInputSchedulingReconstitutionInput`
has 39 reconstitution-failure variants and tests exercise 11 — so a follow-up is
commissioned to complete that matrix; the INV-009/INV-016 rows in
[invariants.md](invariants.md) are corrected now to claim only test-exercised
coverage, and the follow-up restores the stronger claims with their tests. When
that matrix lands, the three `expect()` calls in
`prepare_earliest_queued_activation`
(`crates/domain/src/turn_eligibility.rs:1844/1850/1856`) must become typed
errors under ADR-0035's no-panic reconstitution posture. Accepted on record:
orphan empty context-frontier headers remain committable, and read-side
mitigation — lifecycle-referenced loads plus the domain's `UnreferencedSnapshot`
rejection — is the design; the attempt-Prepared durable boundary at activation
is enforced by domain reconstitution plus the monotonic triggers, not a
schema-forced same-transaction ban, an accepted defense-in-depth asymmetry.
Tracked for future slices: preparation failures that can only be caller bugs
currently conflate into `Corruption::Inconsistent` (the
`crates/persistence/src/submit_input.rs` preparation mapping and the
`crates/persistence/src/start_eligible_turn.rs` internal guards) pending a typed
caller-error family; activation's zero-row guarded UPDATE under the held
scheduler lock can only mean divergence, never a stale wakeup, and must become a
distinct `Inconsistent` outcome; and the future reclassification slice must
widen the pending-steering replay decode (the `queued_effect_count == 0`
coupling and the migration-0005 active-source trigger) or original-command
replay fails closed as corruption. Two process facts: the milestone-2
wave-history report [goal-mode.md](goal-mode.md) requires was not posted — the
rule postdated most of that stack's waves — and future milestones comply; and
`ActiveTurnPhase`'s wait variants remain publicly constructible pending the
`StopRequested` slice sealing them under ADR-0041's discipline.

**Rejected alternatives.** Fixing the tracked items inside this corrective
package: each needs its owning slice's tests, and this package is docs,
comments, and one CI step with zero behavior changes. Leaving the obligations
unrecorded: they would be silently lost between milestones. Weakening
INV-009/INV-016 permanently instead of commissioning the matrix: the checks
exist; only their tests are missing. Adding schema bans for the orphan-header
and attempt-boundary asymmetries now: migration weight for boundaries the read
side and triggers already hold.

**Affects.** `.github/workflows/rust.yml` and the validation sequences in
[AGENTS.md](../AGENTS.md) and `README.md`; annotation and status corrections in
[domain-spine.md](domain-spine.md), [target-model.md](target-model.md),
[goal-mode.md](goal-mode.md), and `README.md`; the INV-009/INV-016 rows in
[invariants.md](invariants.md); and pointer comments in
`crates/domain/src/submit_input.rs`, `crates/persistence/src/submit_input.rs`,
and `crates/persistence/src/start_eligible_turn.rs`. No behavior, schema, API,
or accepted semantics change; the tracked obligations bind their future slices.

## 2026-07-19 — Machine-enforced 80-column Markdown formatting with mdformat

**Context.** Documentation prose had no wrapping rule: line lengths drifted per
author, and rewraps produced noisy diffs with no checker to keep them from
recurring. Rust code already has `cargo fmt --check` in CI; Markdown had no
equivalent. Two candidates were run against the full repository docs: `mdformat`
0.7.22 with `mdformat-gfm` 0.4.1, and `prettier` 3.6.2
(`--prose-wrap always --print-width 80`).

**Decision.** `mdformat` with the GFM plugin, pinned in the CI install and the
[AGENTS.md](../AGENTS.md) install note (`mdformat==0.7.22`,
`mdformat-gfm==0.4.1`), wraps all Markdown at the repository root and under
`docs/` to 80 columns (`wrap = 80`, `number = true` in `.mdformat.toml`). GFM
tables are exempt from wrapping for now: both tools preserve table cell content
exactly and only normalize column padding, so the wide invariant-catalog and
spine-inventory rows stay single-line. Fenced code blocks are untouched; long
unbreakable tokens (URLs, inline code) may exceed 80 alone on a line. The run is
idempotent, and `mdformat --check *.md docs/` joins the validation sequence in
[AGENTS.md](../AGENTS.md) and the `validate` CI job. The spine checker's
aggregate-total regex now tolerates padded table cells.

**Rejected alternatives.** `prettier`: on this repository it additionally
renormalized 18 emphasis spans across six files from the repo's `*emphasis*`
style to `_emphasis_` (pure churn), and it needs a Node toolchain and an npm
download per CI run where mdformat installs as two small wheels on the Python
already required by `scripts/check_domain_spine.py`. Core mdformat without the
GFM plugin: breaks GFM tables. Wrapping table rows: destroys the
one-row-per-invariant review surface. A bespoke wrap script: code to own for a
solved problem.

**Affects.** Every Markdown file at the root and under `docs/` (one-time
mechanical rewrap), `.mdformat.toml`, the `validate` CI job, the validation
sequence in [AGENTS.md](../AGENTS.md), the documentation checklist in
[CONTRIBUTING.md](../CONTRIBUTING.md), and `scripts/check_domain_spine.py`.
Semantics of no document change.

## 2026-07-19 — Atomic Postgres eligible-turn activation

**Context.** The application eligibility port supplies the three owner-generated
identities required by sealed domain preparation, while PostgreSQL stores the
complete evidence-free scheduling projection and deferred lifecycle constraints.
The missing boundary is one authoritative transaction that turns a
nonauthoritative scheduling hint into either a committed earliest-turn
activation or a no-op.

**Decision.** Implement the application transaction port as a purpose-specific
PostgreSQL repository. Lock the session scheduler row first, reconstruct the
current session and complete accepted-input scheduling projection through their
checked domain seams, and let the domain select and prepare the earliest queued
turn. A missing session or stale guarded update returns `NoEligibleTurn`; the
domain's closed result maps an occupied slot or empty queue to the same no-op
and maps candidate identity conflicts exactly, while malformed durable records
fail closed. For a prepared activation, insert the origin semantic entry,
complete ordered starting frontier, and prepared initial attempt before one
guarded lifecycle update binds the exact lineage, frontier, and attempt and
acquires the active slot. Commit all four effects together only when that update
affects exactly one row. Map owner-global entry/frontier and attempt-key
conflicts to the supplied identity that collided, rolling back every partial
effect. Reuse SubmitInput's fixed-count complete scheduling loader rather than
add a second SQL-shaped projection path.

**Rejected alternatives.** Selecting a target in SQL would duplicate
domain-owned eligibility and could skip earlier work. Treating a missing session
or stale wake-up as corruption would give hints authority they do not have.
Committing entry, frontier, or attempt records before guarded activation
succeeds would expose unowned partial history. Retrying identity conflicts would
hide caller-supplied identity failure. Adding a scheduler loop, wake-up source,
startup recovery hook, dispatch, static eligible failure, `StopRequested`
production, or interrupt behavior would cross boundaries not authorized by this
adapter slice.

**Affects.** `crates/persistence/src/start_eligible_turn.rs`, shared complete
scheduling loading in `crates/persistence/src/submit_input.rs`, and
real-PostgreSQL enforcement for INV-001, INV-002, INV-009, and INV-015. It
changes no schema, dependency, domain or application API, domain spine, wake-up
policy, or recovery orchestration.

## 2026-07-19 — Owner-ratified matching-interrupt milestone deferral

**Context.** Choosing the milestone outcome for a matching `Interrupt` required
owner judgment because the current slices cannot construct ADR-0027's
cancellation, immediate-successor, and applied-proof authority. That choice was
an owner gate and should have blocked and been reported on the affected
matching-interrupt track under [goal-mode.md](goal-mode.md), while other
unblocked work continued, rather than being made within that track.

**Decision.** The owner ratifies the current nonclaiming preparation failure for
this milestone. The existing “Authoritative occupied-slot SubmitInput
preparation” entry below remains the single statement of record for that
behavior and for the required scope of the first `StopRequested` storage slice.
Future autonomous runs report an equivalent owner gate instead of deciding it.

**Rejected alternatives.** Treating the milestone outcome as permanent would
conflict with ADR-0027. Repeating its detailed behavior or the `StopRequested`
obligation here would create a second normative statement. Omitting the gate
correction would leave the delivery record inaccurate.

**Affects.** The provenance of the existing deferral and future application of
the blocker rule. It changes no code, schema, ADR, transition, or current
pull-request behavior.

## 2026-07-19 — Canonical replay origins include reclassified steering

**Context.** SubmitInput replay used another turn's immutable applied result as
its complete origin evidence. ADR-0027 also permits pending steering to become
visible turn-origin work without rewriting that original `PendingSteering`
command result, so a later command targeting the reclassified turn could not
replay through the receipt-only seam.

**Decision.** Supply a purpose-specific turn-origin input containing the
immutable receipt, current accepted-input lifecycle, and accepted-input-keyed
immutable queue association. Domain reconstitution admits either a directly
created `TurnOrigin` correlated with `OriginOf`, or a `PendingSteering` receipt
correlated with `ReclassifiedAsTurnOrigin` plus a purpose-specific terminal
source input containing its canonical origin, explicit terminal-record owner,
and `TurnDisposition`. Reclassified origins form one flat oldest-to-newest
chain: every source must be the bound turn, own its terminal disposition,
precede the steering input, and satisfy any turn-bearing cancellation or
reconciliation proof, while accepted-input and command identities remain unique
across the complete chain. This admits every ADR-0027 terminal outcome and
arbitrarily long reclassified-origin chains without recursive validation or
coupling replay to the scheduling projection's currently narrower terminal
subset. Applied predecessor/source replay and occupied-state rejection replay
consume this checked shape. Replaying the pending command itself still ignores
later lifecycle progress and returns its immutable original result.

**Rejected alternatives.** Treating every origin as a `TurnOrigin` receipt
excludes valid reclassification. Reusing the accepted-input scheduling
projection excludes terminal outcomes and source turns that projection does not
yet construct. Trusting a reclassified disposition without its keyed queue
association or checked terminal source accepts cross-record and lifecycle claims
that this purpose cannot prove. Rewriting the original pending receipt would
violate durable replay.

**Affects.** `crates/domain/src/submit_input.rs`, its spine, and INV-009/INV-012
replay enforcement. It adds no reclassification producer, storage spelling,
transition, or persistence behavior.

## 2026-07-19 — Atomic Postgres occupied-slot input handling

**Context.** The domain now admits checked occupied-slot preparation and replay,
while PostgreSQL stores only vacant-slot receipts. First handling must use the
same complete scheduling projection as restart and must not persist results
whose owner evidence is not representable.

**Decision.** Under the existing command claim, lock the session, scheduler, and
defaults pointer in that order; load the complete evidence-free scheduling
projection and active-origin-anchored session acceptance tail; then select
vacant- or occupied-slot preparation. Load complete origin graphs and only
lifecycle-referenced frontier memberships with fixed-count set queries before
checked reconstruction. Extend normalized receipts with actual-active-turn
evidence. Store after-current origins and configuration-free pending steering
with deferred exact command, source-origin, and lifecycle correlations; the
source must remain active, terminalization is rejected until steering closes,
and the acceptance-side check locks the source lifecycle so both operations
serialize. Semantic origin entries cannot reference pending steering.
Reconstruct checked canonical source and predecessor origins from their
receipts, current lifecycle, and immutable queue facts. Store only
occupied-state rejections whose canonical origin replay is constructible.
Matching interrupt remains an explicit nonclaiming repository outcome, and
safe-point-stopping storage remains closed until complete `StopRequested` owner
evidence exists.

**Rejected alternatives.** Inferring active state from one lifecycle row
bypasses the domain aggregate. Per-turn receipt and frontier queries scale with
stored history. A second steering-source column duplicates the delivery source.
Nullable queue or configuration fields on steering admit impossible effects.
Letting pending steering outlive its active source makes it unconsumable.
Storing a stopping discriminator without its proof-bearing owner projection
makes the stored conclusion its own authority.

**Affects.**
`crates/persistence/migrations/202607180005_occupied_slot_submit_input.sql`,
`crates/persistence/src/submit_input.rs`, and PostgreSQL enforcement for
INV-002, INV-005, INV-007, INV-008, INV-009, INV-012, INV-015, INV-016, and
INV-028. It adds no dependency, interrupt transition, `StopRequested` producer,
steering consumption, reclassification storage, or protocol behavior.

## 2026-07-19 — Dependency caching for the Rust CI jobs

**Context.** Both jobs in `.github/workflows/rust.yml` start from a cold runner
and recompile every dependency on every run. Across three recent successful
runs, `validate` took 247–261 s: ~10 s rustup download of the pinned 1.97.0
toolchain, ~3 s crates.io index and crate downloads, ~110 s dependency
compilation under `cargo check`, and ~120 s test-profile compilation, with
clippy (~6 s), docs (~2 s), and test execution (\<1 s) marginal.
`postgres-integration` took 211–224 s: ~9 s toolchain, ~133 s compilation, and
~76 s executing 32 serial container tests, of which only ~8 s is the one-time
`postgres:18.4-alpine3.23` pull and first container start.

**Decision.** Add `Swatinem/rust-cache` v2.9.1, pinned by full commit SHA per
the existing action-pinning convention, to both jobs with
`cache-on-failure: true` so red runs still seed the cache. Each job keeps its
own default cache key (rust-cache keys on the job id): the build graphs differ
(`--workspace --all-features` versus
`-p signalbox-persistence --features postgres-integration`), and rust-cache
skips saving on an exact-key hit, so a shared key would leave one job
permanently restoring the other's mismatched artifacts. The default rustup path
is kept for the toolchain: the ~10 s pinned-toolchain download recurs per run
either way, because rust-cache does not cache the toolchain. Caches saved from a
pull-request run are scoped to that pull request; the cross-PR win arrives once
the first post-merge `main` run seeds the base-branch cache that every pull
request can read. Expected steady state: dependency compilation drops out,
leaving workspace-crate compilation plus the fixed ~76 s integration-test
execution — roughly 60–90 s for `validate` and 100–110 s for
`postgres-integration`.

**Rejected alternatives.** An explicit toolchain step (`dtolnay/rust-toolchain`)
or caching `~/.rustup`: an explicit install performs the same uncached ~10 s
download, and restoring a multi-hundred-megabyte toolchain cache does not beat
that floor. Docker image caching via `actions/cache` with
`docker save`/`docker load`: the measured pull-plus-first-start cost is ~8 s,
below a typical save/restore round-trip for the image, and the tarball would
pressure the 10 GB cache quota the Rust caches use. sccache with GHA-backed
storage: it caches per-crate compilation and mostly overlaps rust-cache's win at
the cost of a second moving part; worth revisiting only if lockfile churn makes
full-cache misses common. Namespace.so runners, which the owner has used and
likes: faster machines with native build caching would beat GH-native caching
outright, but plain GitHub Actions keeps the repository self-contained with no
external account or billing dependency, and per-job caching captures most of the
win; Namespace remains the escalation path if GH-native cache hit rates or
restore times disappoint in practice.

**Affects.** `.github/workflows/rust.yml` only — each job gains one cache step.
No code, ADR, invariant, or local validation-sequence change.

## 2026-07-19 — Adaptive review-fix waves and reply-at-push triage

**Context.** The finished-pull-request rules capped review-fix waves at a fixed
two, and the cap was repeatedly overridden in practice. A wave's value tracks
the prior wave's hit rate and the content under review — hand-written parser
code stayed substantive for five waves, while style-guide reviews went
self-referential by wave three — and deferring reviewer replies to a later batch
decoupled fix commits from their rationale.

**Decision.** The finished-pull-request rules in [AGENTS.md](../AGENTS.md) now
govern review-fix waves by adaptive hit-rate continuation with a five-wave
escalation backstop and push-time reply triage, and the goal-mode owner
alignment-review request in [goal-mode.md](goal-mode.md) reports each pull
request's wave history. Those two documents are the rules' single normative
homes; this entry records the ownership and rationale without restating the
operative rules.

**Rejected alternatives.** Raising the fixed cap: the same arbitrariness, wrong
for both extremes. Unbounded continuation: no churn bound. Agent-judged "review
quality" thresholds: self-serving without the accepted-finding anchor.

**Affects.** The finished-pull-request rules in [AGENTS.md](../AGENTS.md), owner
alignment-review reporting in [goal-mode.md](goal-mode.md), and every future
review loop. It changes no code, ADR, or validation rule.

## 2026-07-19 — Workspace expect-table crate for Debug-derived snapshot tables

**Context.** [Testing-style](testing-style.md) rules 9–12 send value-shaped
claims to expect-test snapshots and require curated, byte-stable tables, but the
only renderer was the ad-hoc `table(headers, rows)` helper in the domain crate's
`test_support`, which took pre-stringified cells, forced each test module to
hand-build `Vec<Vec<String>>` plumbing, and could not be imported by other
crates' tests. Rule 12 already anticipated lifting it into shared test support.

**Decision.** Add the dev-only workspace crate `signalbox-expect-table`
(`crates/expect-table`). Input is any `T: Debug` row set: each row is formatted
with `{:?}` and a hand-written recursive-descent parser reads the derived-Debug
grammar (structs, unit/tuple/struct enum variants, tuples, lists, `Option`,
string and char literals with escapes, numbers) into a value tree. The parser
never fails: an unparseable region — a custom `Debug` impl, say — degrades
locally to one verbatim atomic cell. Columns are struct fields unioned across
rows in first-appearance order; nested structs (and struct variants,
indistinguishable in the grammar) flatten to dotted headers to depth 3,
adjustable via `Table::new(rows).max_depth(n)`; `Some` unwraps to its payload
while a unit `None` renders as the literal text `None` — the grammar cannot
distinguish `Option::None` from a domain unit variant such as
`TranscriptAncestry::None`, and erasing a domain value is the worse failure —
with one deliberate asymmetry: when dotted descendant columns carry a prefix's
data, a bare prefix column holding nothing but structurally absent cells —
missing fields or unit `None` leaves, judged by provenance carried from the
parse tree, never by rendered text (an observed empty string renders quotes-kept
as `""` and keeps its column, as does the literal string `"None"`) — is
suppressed as redundant, so a `None` row under a flattened prefix reads as an
empty run of descendant cells; output is a Unicode box-drawing table with
numeric columns right-aligned, every line right-trimmed, a trailing newline, and
no ordering ever taken from a `HashMap` — braced map and set `Debug` output
parses entry by entry and renders entries sorted by rendered key text rather
than iteration order. `table`, `cases`, and `transposed` mirror Jane Street's
[expectable](https://github.com/janestreet/expectable) (`print`, `print_cases`,
`print_record_transposed`, and its `~nested_columns`/`~align` defaults) with
OCaml Ascii_table-style borders, hosted on
[expect-test](https://github.com/rust-analyzer/expect-test) snapshots. The crate
has zero runtime dependencies; the domain crate consumes it as a dev-dependency,
replacing and deleting the `test_support::table` helper.

**Rejected alternatives.** Serde-based row extraction: the domain crate
deliberately carries no serde, and every fixture type would need derive
annotations. A trait- or derive-based column protocol: the orphan rule blocks
foreign types, and the annotation burden recurs per fixture where `Debug` is
already ubiquitous. Adopting `tabled` or similar: snapshot byte-stability would
track a third-party formatter's churn, and its derive repeats the annotation
burden.

**Affects.** `crates/expect-table` (new workspace member), the domain crate's
`[dev-dependencies]` and the snapshots in `queue_order.rs` and
`replace_session_defaults.rs`, and rule 12's helper naming in
[testing-style.md](testing-style.md). Production dependency graphs, the domain
spine (test-only dev-dependency; not spine-covered), and all runtime crates are
unaffected.

## 2026-07-19 — Exact accepted delivery in scheduling origin records

**Context.** The ADR-0041 scheduling seam correlated an origin tail entry with
its turn, acceptance position, delivery kind, historical target, and queue
priority, but the record did not repeat the accepted delivery itself.
Independently supplied tail facts could therefore change the versioned
configuration choice while retaining the same delivery kind and target, while
records outside an active tail could bypass delivery/order validation.

**Decision.** Carry the exact immutable accepted `DeliveryRequest` in every turn
scheduling record and validate every origin's delivery/order and
historical-target relationship, whether or not an active tail exists. Correlate
every configured delivery's expected defaults version with its frozen provenance
and every explicit `ReplaceWith` request with the exact frozen requested model;
a historical `UseSessionDefault` request cannot be rederived without its
immutable defaults row. An active tail origin must additionally equal that
complete delivery value, and its claimed observation must reach every origin
position known by the same scheduling read. This preserves the structural
distinction between using a session default and explicitly replacing it.

**Rejected alternatives.** Comparing only the expected defaults version would
still miss a changed explicit model-selection override. Rederiving
`UseSessionDefault` would require historical defaults outside this
purpose-specific input. Trusting the adapter to repeat the accepted delivery
would bypass domain-owned correlation.

**Affects.** `crates/domain/src/turn_eligibility.rs`, its domain-spine
constructor, accessor, and failure inventory, one application fixture, and
INV-008/INV-009/INV-016 scheduling enforcement. It adds no storage
representation, transition, or accepted delivery mode.

## 2026-07-19 — Checked rejected SubmitInput receipt replay

**Context.** ADR-0034 requires equal replay to return the originally recorded
terminal result, while ADR-0035 requires domain-owned validation of complete
purpose-specific facts. Rejections produced against an occupied slot name the
authoritative active turn directly or depend on the command's expected active
turn; accepting only bare result fields would let replay pair them with another
session or another turn.

**Decision.** Reconstruct `ActiveTurnPresent` and `ActiveTurnMismatch` only with
the actual turn's canonical same-session origin receipt, a distinct
durable-command identity, and exact expected/actual turn correlation.
Configuration and position rejections for `AfterCurrentTurn`, plus position
exhaustion for `NextSafePoint`, require the expected active turn's canonical
origin; vacant-start variants require no origin. Session absence and
no-active-turn retain their smaller result-specific projections. Matching
interrupt configuration and position rejections remain unavailable because
preparation is nonclaiming for that state in this milestone.
`SafePointUnavailableWhileStopping` replay remains closed until the first
`StopRequested` storage slice can supply the exact owner-correlated stop
evidence required by ADR-0035 and ADR-0041.

**Rejected alternatives.** Bare active-turn identifiers cannot establish
canonical same-session ownership. Optional evidence without delivery-specific
requiredness would admit both missing occupied-state facts and invented
vacant-state facts. Exposing stopping replay from a copied phase discriminator
would make the stored conclusion its own authority. Reconstructing
matching-interrupt rejections that live preparation cannot record would create
replay-only terminal behavior.

**Affects.** `crates/domain/src/submit_input.rs`, its public reconstitution
signatures and spine, INV-002/INV-009/INV-012/INV-028 enforcement wording, and a
mechanical vacant-start decoder update in
`crates/persistence/src/submit_input.rs`. It adds no schema, occupied-slot
storage, stopping proof loader, interrupt application, lifecycle transition, or
persistence effect.

## 2026-07-19 — Checked applied SubmitInput receipt replay

**Context.** ADR-0034 requires equal command replay to return the immutable
originally recorded result after mutable aggregate state advances, while
ADR-0035 requires the domain seam to validate complete purpose-specific facts
rather than trust storage constraints. Vacant-start replay existed, but
after-current and pending-steering receipts need their exact predecessor or
source origin to prove ownership, identity separation, and acceptance
chronology.

**Decision.** Split applied reconstitution into purpose-named turn-origin and
pending-steering inputs. Turn-origin replay accepts `StartWhenNoActiveTurn` with
no predecessor or `AfterCurrentTurn` with the exact canonical predecessor-origin
receipt; after-current validation rejects reused turn, accepted-input, or
command identities and requires its position to follow that predecessor.
Pending-steering replay requires the exact canonical same-session source-origin
receipt, rejects reused accepted-input or command identities, and requires its
position to follow the source. It derives the original `PendingSteering` binding
from immutable command and acceptance facts and deliberately accepts no
current-disposition field, so later consumption or reclassification cannot
rewrite command replay.

**Rejected alternatives.** Comparing pending replay to the accepted input's
current disposition would report normal lifecycle progress as corruption. Bare
source or predecessor identifiers cannot prove same-session canonical origins or
chronology. Adapter-owned correlation would make database constraints the
semantic authority. A nullable applied record would allow turn-origin and
steering-only fields to mix.

**Affects.** `crates/domain/src/submit_input.rs`, the SubmitInput spine,
applied-replay enforcement links for INV-002, INV-008, INV-009, INV-012,
INV-016, and INV-028, plus a mechanical rename in the existing vacant-start
persistence decoder. Rejected-result replay, occupied-slot storage, lifecycle
validation, and interrupt application remain later slices.

## 2026-07-19 — Validated scheduling input construction and historical tail correlation

**Context.** The first ADR-0041 scheduling slice stored a canonical active phase
inside its public reconstitution input and compared every later origin delivery
with the currently active turn. That exposed a phase before aggregate validation
and rejected valid histories accepted during a scheduler gap or against a
previously active turn. Identity-only tail checks also left an origin position
available to a different pending-steering entry.

**Decision.** Keep prepared and running reconstitution inputs as inert owner,
attempt, and state facts; construct the canonical attempt and phase only inside
successful aggregate reconstitution. Correlate each tail origin's immutable
delivery with its own queue priority and an earlier nonqueued historical target
when the delivery names one. Correlate pending steering against both the
complete origin identity and position inventories. If the complete tail contains
an accepted interrupt against the current active owner, reject the evidence-free
phase instead of ignoring the proof-bearing conclusion it requires.

**Rejected alternatives.** Requiring every post-anchor origin to target the
current active turn erases valid acceptance history. Checking only delivery
discriminants misses priority and predecessor corruption. Exposing a canonical
phase from the unvalidated input bypasses its owning seam. Treating an accepted
interrupt as compatible with an evidence-free phase lets the stored lifecycle
conclusion omit its contradictory evidence.

**Affects.** `crates/domain/src/turn_eligibility.rs`, its domain-spine
declarations, and the ADR-0041 enforcement summary in `docs/invariants.md`. It
adds no proof constructor, storage representation, wait phase, stop phase, or
lifecycle transition.

## 2026-07-19 — Authoritative occupied-slot SubmitInput preparation

**Context.** ADR-0027 defines the terminal outcomes for input submitted while a
turn owns the session slot, and ADR-0041 now provides one checked scheduling
aggregate containing the active turn, its canonical origin, exact phase, current
session, and validated acceptance tail. The existing domain boundary prepared
only the no-active-turn state and could not safely correlate occupied-slot
candidates.

**Decision.** Add purpose-named preparation consuming that complete scheduling
projection. Represent applied acceptance as the closed
`TurnOrigin | PendingSteering` algebra: after-current input creates ordinary
origin work with frozen configuration, while next-safe-point input creates a
configuration-free steering binding to the exact active turn. Derive the next
position only from the aggregate's validated tail and reject any new
accepted-input or turn candidate that reuses the active origin's identity.
Record active-slot presence and stale active targets as typed terminal results.
Matching interrupt remains a nonclaiming preparation failure until interrupt
application can construct its complete correlated authority. Defer stopping
safe-point handling until `StopRequested` has a complete owner projection; that
slice must add its ADR-0027 safe-point rejection and the matching-interrupt
recorded rejections for cancellation-only stop and already-applied
fatal-mismatch interrupt.

**Rejected alternatives.** Independent session, active-turn, phase, and
last-position arguments would allow cross-wired preparation. Nullable
turn-origin fields would permit impossible applied shapes. Treating stale
targets or active-slot presence as adapter errors would leave a committed
command without its authoritative terminal meaning. Claiming matching interrupt
before its transition boundary exists would violate the ratified milestone
deferral.

**Affects.** `crates/domain/src/{submit_input,turn_eligibility}.rs`, domain
exports and spine, and live-preparation enforcement links for INV-007, INV-008,
INV-012, INV-016, and INV-028. Existing vacant-slot persistence receives only
mechanical exhaustive-match updates; occupied-slot replay, storage, interrupt
application, steering consumption, lifecycle transition, and acknowledgement
remain later slices.

## 2026-07-19 — Evidence-free active scheduling reconstitution

**Context.** The accepted scheduling projection predates ADR-0041 and admitted
only one prepared current attempt, while the refinement requires a validated
active-origin-anchored acceptance tail and closes proof-bearing phases until
their complete owner facts exist. Implementing that refinement must preserve the
decision log's original record rather than retroactively rewriting it.

**Decision.** Extend the scheduling projection with a required session-scoped
acceptance tail whenever an active turn exists. Validate its exact origin
anchor, gap-free positions through the observed last position, unique
identities, and disposition/delivery correlations against the complete
turn-origin inventory. Admit only evidence-free prepared and running
current-attempt inputs; keep `StopRequested`, approval-wait, and recovery-wait
construction closed until purpose-specific complete owner projections exist.
ADR-0041 remains the normative statement for the validation pattern.

**Rejected alternatives.** Editing the prior scheduling decision in place would
erase history from an append-only log. Accepting arbitrary active phases would
let a phase discriminator manufacture evidence-bearing authority. A filtered
pending-steering list or uncorrelated origin receipt could omit accepted work or
pair it with the wrong slot owner.

**Affects.** `crates/domain/src/turn_eligibility.rs`, its domain-spine
declarations, one application fixture, and ADR-0041 enforcement links in
`docs/invariants.md`. It adds no persistence loader, proof constructor, wait
storage, stop storage, lifecycle transition, or new accepted semantics.

## 2026-07-18 — Separate queued-origin facts from guarded turn lifecycle storage

**Context.** The durable-input slice already stores immutable origin order and
frozen configuration in append-only `queued_input_origin`, while ADR-0004,
ADR-0010, ADR-0022, ADR-0030, and ADR-0036 require later eligibility to
serialize per session and atomically bind lifecycle, semantic, frontier, and
attempt facts. The storage foundation needs database enforcement and
migration-safe backfill before the authoritative aggregate transition exists,
without making raw SQL or a repository method an eligibility producer.

**Decision.** Keep `queued_input_origin` as the immutable order/configuration
fact and add one correlated mutable `turn_lifecycle` row, backfilling every
existing origin as queued and requiring every future accepted-origin insertion
to be queued before a guarded update may advance it. Give every session one
identity-only `session_scheduler` row for typed row locking. Materialize
owner-global initial semantic-entry and context-frontier identities separately
from complete one-based frontier membership; the header's immutable declared
count makes both gaps at commit and later membership appends invalid. A failed
turn's terminal frontier must equal its starting frontier followed by the exact
`TurnFailed` entry, while an `After` start must equal the predecessor terminal
frontier followed by its own origin. Store turn attempts separately, requiring
every attempt to be inserted prepared before guarded updates may advance it,
with partial unique indexes for the active session slot and live attempt plus
deferred final-state validators over the complete start/frontier/attempt shape.
A failed terminal may have no attempt for an eligible static failure; when it
has an ended attempt, that attempt's disposition must be `known_failure` or
startup `lost`. Keep the continuation reference constrained null until the
migration that durably represents the owning wait or closure deliberately admits
successors. Migration 003 admits only ordinary priority, so this schema's
immediate-predecessor lookup is position-based; the migration that admits
another priority must replace it with the domain-derived total scheduling order
and correlation. The baseline session schema likewise admits only no ancestry,
so a migration admitting forks must extend first-turn validation with the exact
source transcript prefix. The first schema admits only the lifecycle payloads
whose correlations are representable now; later proof-bearing or wait variants
require their owning aggregate migration. No production path activates a turn.

**Rejected alternatives.** Expanding the append-only queued-origin row into
mutable lifecycle state would mix write-once order/configuration with guarded
transitions and complicate backfill. A nullable generic lifecycle document would
weaken closed payload checks and cross-table correlations. Locking immutable
session provenance would couple scheduling to unrelated session reads.
Database-generated identifiers, an activation repository, or a persistence-owned
start constructor would cross accepted identity and aggregate-authority
boundaries.

**Affects.**
`crates/persistence/migrations/202607180004_turn_lifecycle_storage.sql`,
scheduler creation in CreateSession, queued-lifecycle insertion in SubmitInput,
and real-PostgreSQL enforcement for INV-001, INV-004, INV-005, INV-006, INV-007,
INV-009, and INV-015. It changes no domain or application API, accepted
transition, scheduler wake-up policy, or dependency.

## 2026-07-18 — Application-owned eligible-turn activation orchestration

**Context.** The domain now owns a complete scheduling projection and one sealed
earliest-queued activation candidate, while ADR-0033 assigns the origin-entry,
starting-snapshot, and initial-attempt identities to application orchestration.
Eligibility remains derived from authoritative durable state and ADR-0010 makes
wake-ups nonauthoritative hints, so the application cannot preload a projection
or accept a caller-selected turn.

**Decision.** Expose one application generator port for exactly those three
UUIDv7 candidate identities and one atomic transaction port taking only
`SessionId` plus `AcceptedInputTurnActivationIdentities`. The closed transaction
result is either `NoEligibleTurn` for a false or stale hint or the committed
`ActivatedAcceptedInputTurn` view. The service mints each identity once, calls
the transaction once, and returns its result or failure unchanged. Projection
loading, earliest-turn selection, domain preparation, durable identity collision
handling, and atomic commit remain inside the transaction implementation;
sweeps, wake-up delivery, startup recovery, runtime dispatch, and automatic
retry remain outside this use case.

**Rejected alternatives.** Taking a target `TurnId` would let application or
work-source policy skip earlier queued work. Loading or preparing before the
port would separate eligibility from its authoritative serialization boundary.
Passing three raw identities instead of the domain grouping would repeat a
correlation already owned by the domain. Returning the sealed prepared candidate
would expose an uncommitted value as the application result. Retrying
transaction failures or driving a scheduler loop inside the service would hide
commit ambiguity and combine distinct ADR-0010 ports.

**Affects.** `crates/application/src/start_eligible_turn.rs`, application
exports and spine, and application enforcement links for INV-002 and INV-009. It
adds no persistence adapter, schema, domain semantics, work-source port, sweep,
startup hook, recovery behavior, runtime dispatch, dependency, or
eligible-failure transition.

## 2026-07-18 — expect-test dev-dependency for snapshot assertions

**Context.** The [testing style guide](testing-style.md) fixes forward-looking
snapshot norms — expect tests for shape-is-the-assertion values, supplementing
invariant-linked asserts, curated tables — but no snapshot machinery existed.
Matrix-outcome and derived-order tests spelled their shapes only through
per-case `assert_eq!` chains that hide the whole at a glance.

**Decision.** Add [`expect-test`](https://github.com/rust-analyzer/expect-test)
1.5 as a `signalbox-domain` dev-dependency — a small, focused inline-snapshot
crate with one transitive dependency (`dissimilar`), no build-time or runtime
cost outside tests, and in-place `UPDATE_EXPECT=1` re-blessing — with owner
approval per the dependency rules. A crate-private `table` helper in the domain
crate's `test_support` module renders pipe-separated, left-aligned,
right-trimmed tables so snapshot stability is owned in-repo, unit-tested per the
guide. Exemplar conversions in `queue_order.rs` and
`replace_session_defaults.rs` demonstrate the guide's full style — one-knob
fixtures, assert-against-fixture, snapshots supplementing the invariant-cited
asserts — without renaming any test.

**Rejected alternatives.** `insta`: heavier and serde-oriented, and its review
TUI is unneeded now; revisit if future corpus or LLM-integration tests outgrow
inline snapshots. A third-party table crate: snapshot stability would then
depend on that crate's formatting churn, and the needed renderer is ~40 lines.

**Affects.** `crates/domain/Cargo.toml`, the `test_support` module in
`crates/domain/src/lib.rs`, and exemplar tests in
`crates/domain/src/queue_order.rs` and
`crates/domain/src/replace_session_defaults.rs`; enforcement links in
`docs/invariants.md` are unchanged because every cited test keeps its decisive
asserts.

## 2026-07-18 — Repository-owned testing style guide

**Context.** The
[testing section of CONTRIBUTING.md](../CONTRIBUTING.md#testing) owns what to
test — layers, determinism, merge gates — but nothing owned how tests are
written: fixture shape, what an assertion may reference, or snapshot discipline.
Each pull request re-derived those choices, reviews re-litigated them per test,
and multi-positional-integer fixture helpers and re-encoded magic seeds were
accumulating in domain test modules.

**Decision.** Test style — fixture and assertion rules plus forward-looking
expect-test snapshot norms — is owned by
[docs/testing-style.md](testing-style.md) as numbered rules cited by number in
review. This entry authorizes that document as the rules' single home and does
not restate them. CONTRIBUTING.md keeps owning what to test; the two documents
cross-link and restate nothing.

**Rejected alternatives.** Inlining the style rules into CONTRIBUTING.md's
testing section: it merges two ownerships into one section, and style rules
would be diluted among layer requirements that change on a different cadence.
Leaving style to per-agent prompting: rules stated only in prompts are
unreviewable, drift between runs, and cannot be cited by number in review.

**Affects.** `docs/testing-style.md` (created); pointer lines in `AGENTS.md`,
`CONTRIBUTING.md`, and `README.md`. The expect-test dev-dependency, the `table`
helper, and exemplar conversions land in a stacked follow-up.

## 2026-07-18 — Closed accepted-input scheduling projection and eligibility candidate

**Context.** ADR-0035 requires restart to reconstruct a purpose-specific
complete scheduling projection rather than minting starts, attempts, entries, or
snapshots from isolated records. The currently decided semantic-entry set can
represent an ancestry-free first turn and continuation after a failed
predecessor, but not ancestry prefixes or terminal outcomes whose required
semantic markers remain open. No accepted predicate yet selects ADR-0027's
static eligible-failure alternative.

**Decision.** Reconstitute one session's complete accepted-input scheduling
facts as a failed-terminal prefix, at most one active `Running` turn with an
exact owner-correlated `Prepared` current attempt, and a queued suffix in
derived durable order. Repeated stored session, turn, accepted-input
`OriginOf(turn)`, queue, entry, snapshot, attempt, lineage, and frontier facts
are validated collection-wide; starts and resolved snapshots remain sealed until
those checks pass. Within the closed slice, each started turn has exactly one
origin entry, each failed turn appends exactly one failure marker, and a failed
terminal frontier is the start membership followed by that marker. Pure
eligibility consumes the complete projection, rejects any active slot or
available identity collision, selects the earliest queued origin itself, and
returns one sealed candidate containing the origin entry, prefix-preserving
snapshot, opaque start, and `Active(Running { Prepared initial attempt })`. It
implements no static eligible-failure transition.

**Rejected alternatives.** A caller-selected target could skip earlier queued
work. A bare active-slot flag would omit the exact attempt record ADR-0035
requires. Accepting arbitrary terminal predecessors would imply not-yet-decided
semantic markers. Supporting `SingleSource` with an empty or opaque prefix would
lose ancestry content. Public start, entry, snapshot, attempt, or activated-turn
construction would bypass the complete correlations. Treating identity
uniqueness as process memory would overstate the candidate; persistence must
still enforce fresh durable identities and atomic commit.

**Affects.** `crates/domain/src/{turn_eligibility,turn_lifecycle}.rs`, domain
exports and spine, and S01/S03/S09 enforcement links for INV-009 and INV-015.
Persistence, application scheduling, database slot/uniqueness enforcement,
ancestry resolution, non-failed terminal variants, attempt advancement, static
rejection, and commit authority remain later boundaries.

## 2026-07-18 — Closed initial semantic-entry values and inert reconstitution inputs

**Context.** ADR-0036 fixes exactly two initial semantic transcript-entry
payloads, while ADR-0030 and ADR-0035 require opaque entry and resolved-snapshot
construction to remain with a complete validating aggregate seam. The first
representation boundary must expose typed storage-independent inputs without
letting a plausible identifier, payload, or ordered list mint semantic-history
or frontier authority.

**Decision.** Represent the initial payload as the closed
`OriginAcceptedInput { accepted_input } | TurnFailed { turn }` enum and the
immutable semantic entry as a private-field value exposing only identity,
source, payload, and source-qualified reference. Add private-field
reconstitution-input values for one semantic entry and one complete
resolved-snapshot record. These inputs are inert: neither has a public
`reconstitute` operation, and the semantic entry has no public producer. The
following scheduling boundary must consume the complete collections and validate
subject, lifecycle, order, ownership, membership, and frontier correlations
before constructing either opaque value.

**Rejected alternatives.** A generic message or “other” variant would reopen the
closed ADR-0036 set. Public entry construction from identifiers and payload
would skip exact origin/failure correlation. Standalone snapshot construction
from an ordered list would let persistence mint a start dependency without the
aggregate facts ADR-0035 requires. SQL-shaped nullable discriminators or record
types would move storage representation into the domain.

**Affects.** `crates/domain/src/{semantic_entry,context_frontier}.rs`, their
exports, the domain spine, and the INV-005 enforcement index. This boundary
constructs no semantic entry or resolved snapshot, performs no lifecycle
transition, chooses no rendering or storage encoding, and adds no dependency.

## 2026-07-18 — Domain-owned stored-actor validation and submit lock mode

**Context.** The SubmitInput persistence adapter compared the stored actor
against the baseline owner itself, so that semantic payload check lived outside
the domain reconstitution seam and the natural adapter path would launder a
corrupted stored actor into `Owner`. Separately, submit's session-row
`FOR UPDATE` formed a lock-order cycle with defaults replacement: submit orders
session row before pointer row, while a replacement holds the pointer row when
its version-row insert requests `FOR KEY SHARE` on the session row through the
non-deferrable session foreign key.

**Decision.** Every `SubmitInputReconstitutionInput` constructor takes the
stored actor, and domain reconstitution compares it against the canonical
command's actor as `StoredActorMismatch`; persistence keeps only decode-level
rejection of unknown or malformed actor spellings. Submit takes its session-row
lock as `FOR NO KEY UPDATE`, which stays self-exclusive for per-session position
serialization but does not conflict with referential-integrity `KEY SHARE`, with
the constraint recorded beside the query and the interleaving forced
deterministically in the Postgres suite. Domain unit tests now exercise every
reconstitution-failure variant and both preparation failures.

**Rejected alternatives.** Keeping the owner comparison in the adapter repeats
the semantic check per adapter and leaves any path that skips it laundering a
corrupted actor. Accepting an actor in `SubmitInput::new` would open the
non-owner command boundary ADR-0039 closes. Reordering submit to lock the
pointer row first would leave the session-row ordering read unserialized against
concurrent submits. Retrying serialization failures in the adapter would mask
the cycle instead of removing it.

**Affects.** `crates/domain/src/submit_input.rs`,
`crates/persistence/src/submit_input.rs`, the SubmitInput section of
`docs/domain-spine.md`, the INV-012 enforcement wording in `docs/invariants.md`,
and the PostgreSQL integration suite. It changes no schema, migration, ADR
semantics, application API, or dependency.

## 2026-07-18 — Postgres implements the application durable-input port

**Context.** The application crate now owns the one-call
`SubmitInputTransaction` port and its closed recorded-or-conflict outcome, while
`SubmitInputRepository` owns the corresponding atomic PostgreSQL handling and
complete replay behavior. The adapter must join those existing seams without
adding a second handling path or moving identity generation into persistence.

**Decision.** Implement `SubmitInputTransaction` directly for
`SubmitInputRepository`, delegate to its inherent atomic handler, and
exhaustively translate recorded domain results and conflicting reuse into the
application outcome while retaining `SubmitInputRepositoryError`. Exercise
`SubmitInputService` with deterministic fresh accepted-input and turn candidates
before and after a pool/repository restart; replay returns the original recorded
identities and leaves one typed command, accepted input, and queued origin.

**Rejected alternatives.** A wrapper repository would add another public type
without policy. Repeating transaction logic in the trait method would create a
competing lookup and commit path. Generating candidates in the adapter would
cross the application-owned identity boundary. Testing only the inherent
repository would not enforce the composed service contract.

**Affects.** `crates/persistence/src/submit_input.rs`, the restarted S01
PostgreSQL integration test, and direct application-plus-persistence enforcement
wording for INV-002, INV-007, INV-008, INV-010, INV-012, and INV-028. It adds no
schema, domain or application semantics, protocol, retry policy, alias source,
lifecycle state, or dependency.

## 2026-07-18 — Atomic Postgres durable input acceptance

**Context.** ADR-0022, ADR-0027, ADR-0034, and ADR-0035 require normalized typed
command and effect records, owner-global lookup before mutable validation,
immutable accepted-input ordering and configuration provenance, atomic terminal
handling, and checked historical replay. The domain now supplies complete
no-active-turn preparation and reconstitution, while turn lifecycle and
alias-definition storage remain later boundaries.

**Decision.** Admit `submit_input` as the registry's third closed kind and store
the exact canonical owner actor, content, delivery, configuration choice, and
typed terminal result in an append-only command record. Applied starts
atomically add one immutable accepted-input record and one ordinary
queued-origin record with the exact session position, selected defaults version,
requested model, frozen model, and explicit baseline policy spellings. Deferred
keys and a closed correlation trigger reject missing, duplicated, or cross-wired
effects at commit; purpose-specific loads still pass every complete fact through
domain reconstitution and fail closed, including rejecting a non-owner actor at
this baseline boundary. First handling locks the existing session and its
current-defaults pointer, serializes position selection on the session row, and
passes no alias definition, so alias selection records `UnknownModelAlias`.
Registry replay reconstructs and compares before any current-state read. A
failure proven before commit rolls back without leaving a command claim or
consuming a position; a commit error can remain ambiguous and is recovered by
replaying the same command identity and payload.

**Rejected alternatives.** Embedding effects or configuration in registry JSON
would weaken typed correlation and replay. Using a process lock or
`max(position) + 1` without the session lock would race concurrent acceptance.
Rereading the mutable current-defaults pointer for historical replay would
invalidate receipts after replacement. Synthesizing an alias definition would
invent an unavailable authority. Adding an application port, turn record,
active-work behavior, or generic repository would cross the authorized slice.

**Affects.** `crates/persistence/migrations/202607180003_submit_input.sql`, the
closed command registry and cross-kind handlers, typed UUID/ordinal mappings,
`crates/persistence/src/submit_input.rs`, and real-Postgres enforcement for
INV-002, INV-007, INV-008, INV-010, INV-012, and INV-028. It adds no dependency,
application API, protocol adapter, alias-definition source, or turn-lifecycle
state.

## 2026-07-18 — Application-owned durable input orchestration

**Context.** The domain `SubmitInput` slice defines the canonical actor-bearing
command and closed recorded result, while ADR-0033 assigns accepted-input and
future-turn identity generation to application orchestration. ADR-0027 and
ADR-0034 require owner-global lookup before mutable session validation, so the
application cannot prepare against a preloaded session.

**Decision.** Represent the admitted application input as a private-field
`SubmitInputRequest` carrying command identity, session, checked content, and
explicit delivery treatment. Reuse `InvalidDurableCommandId` before canonical
construction; the domain constructor fixes `Actor::Owner` because ADR-0039
admits no other baseline command actor. Compose one generator port that always
supplies a fresh UUIDv7 accepted-input candidate and supplies a future-turn
candidate only for `StartWhenNoActiveTurn`, `Interrupt`, and `AfterCurrentTurn`;
`NextSafePoint` passes no turn because it initially creates none. One async
transaction port accepts the unprepared command and delivery-correlated
candidates. The service constructs once, generates each applicable identity
once, calls the transaction once, and returns its recorded domain result, typed
conflicting reuse, or adapter failure unchanged. Authoritative lookup, session
loading, position allocation, preparation, and commit remain inside the
transaction implementation. Adapter failure may be commit-ambiguous, so callers
retain the command identity and exact payload until a terminal result and
recover by resubmitting that same command.

**Rejected alternatives.** Accepting an actor would expose command agency not
admitted by ADR-0039. Application preparation would move mutable validation
ahead of owner-global lookup. Generating identities in domain, persistence, or
database code would cross ADR-0033's boundary. Minting a turn for
`NextSafePoint` would create or discard an identity before that fact exists.
Flattening recorded rejections into application errors would erase terminal
replay meaning. Automatic retry inside the service would hide adapter policy;
lost-acknowledgement recovery instead resubmits the retained command and
payload.

**Affects.** `crates/application/src/submit_input.rs`, its exports, the
application domain spine, and the
INV-001/INV-002/INV-007/INV-008/INV-012/INV-028 enforcement links. It adds no
persistence adapter, schema, alias source, turn lifecycle, scheduler, protocol,
outbox event, retry policy, or dependency.

## 2026-07-18 — Canonical SubmitInput domain boundary

**Context.** ADR-0027 fixes the caller payload, accepted disposition, queue
facts, and configuration provenance for durable input; ADR-0037 and ADR-0039 now
fix its content and actor fields. The first priority milestone stops before the
turn aggregate, so its domain API must prepare the authoritative no-active-turn
state without implying lifecycle, eligibility, frontier, slot, attempt,
steering, or interrupt authority.

**Decision.** Spell actor provenance as the closed `Actor` enum and baseline
content as `UserContent::Text` carrying checked `NonEmptyUnicodeText`, whose
failed construction retains the exact rejected string. Represent `SubmitInput`
with command identity, session, the ADR-0039 baseline `Owner` actor fixed by its
only public constructor, content, and delivery, and implement comparison and
hashing over every field except command identity. A purpose-named no-active-turn
preparation accepts an application-minted input identity, a delivery-correlated
optional future-turn candidate, and the locked prior position: start requests
freeze exact origin configuration and ordinary order, while active-work requests
return a typed `NoActiveTurn` result; `NextSafePoint` requires no turn candidate
because it initially creates no turn. Missing session, stale defaults, unknown
alias, and exhausted input-position ordinal are distinct typed recorded results.
Purpose-specific complete reconstitution validates command, accepted-input,
disposition, queue, defaults, requested-selection, and frozen-selection
correlations without constructing a turn aggregate or authorizing persistence.

**Rejected alternatives.** A generic command/result envelope would weaken
ADR-0034's command-specific algebra. Treating active-work requests, unknown
aliases, or ordinal exhaustion as infrastructure errors would leave canonical
intent free to acquire a different meaning on retry. Creating a turn state or
accepting an interrupt would cross into the next priority milestone and
overstate unavailable aggregate authority. Public constructors for applied
receipts would let callers manufacture correlations that preparation or
reconstitution must establish.

**Affects.** `crates/domain/src/{actor,user_content,submit_input}.rs`, domain
exports, the domain spine, and domain enforcement links for INV-001, INV-002,
INV-005, INV-007, INV-008, INV-012, INV-020, and INV-028. Persistence,
application orchestration, protocol mapping, acknowledgement, and turn
activation remain separate slices.

## 2026-07-18 — Domain spine as the owner's API-review surface

**Context.** Source files surround every public item with rustdoc, tests, and
`compile_fail` proofs, so reviewing domain shape means reading past enforcement
scaffolding. The owner reviews type shapes rather than implementations and needs
one diffable surface showing the public API of the domain and application
crates, kept current by something stronger than instruction-following.

**Decision.** Add `docs/domain-spine.md`: a hand-maintained mirror of the public
type and function surface of both crates — full enum variants,
sealed-constructor markers, transition signatures, collapsed accessors,
load-bearing derive notes. Source stays authoritative; the mirror is updated
from it, never the reverse. Every pull request changing a public item updates
the spine in the same change (`AGENTS.md` rule), and the `validate` CI job runs
`scripts/check_domain_spine.py`, which fails when an exported name is absent
from the spine or a per-module inventory count disagrees with the lib.rs export
surface.

**Rejected alternatives.** Reviewing rustdoc output yields no diffable
pull-request artifact. `cargo public-api` needs a nightly toolchain and a new
tool dependency for what a small repository-local script checks.
Instruction-only maintenance drifts silently the first time a run does not load
the spine. Splitting tests into sibling files shrinks sources but still leaves
no single review surface.

**Affects.** `docs/domain-spine.md`, `scripts/check_domain_spine.py`, the
`validate` job in `.github/workflows/rust.yml`, the spine paragraph in
`AGENTS.md`, and the README design-documents index.

## 2026-07-18 — Goal-mode rules split and unbounded stacks

**Context.** Autonomous milestone runs need durable operating rules, but the
root `AGENTS.md` is injected into every agent for every task, and the published
prompting guidance for the models running these agents ranks contradictory or
duplicated instructions as the leading failure mode. An earlier draft capped
open pull requests at three and told runs to "finish and merge" at the cap,
contradicting owner-only merging; the owner had already hit that cap while
steering real runs.

**Decision.** Keep in `AGENTS.md` only the rules that bind every agent and pull
request: an explicit autonomy grant, the finished-pull-request checklist
delivered awaiting owner merge, and stack hygiene without any depth cap — stacks
may grow as deep as the work requires and the owner merges in batches. Move
milestone selection, frozen surfaces, blocker handling, orchestration, progress
checkpoints, the milestone check-in gate, and goal-writing guidance to
`docs/goal-mode.md`, loaded only by autonomous milestone runs. The
milestone-selection algorithm lives there alone; `docs/target-model.md` links to
it and owns only the priority order and status map.

**Rejected alternatives.** Keeping everything in `AGENTS.md` burdens single-task
agents with rules that do not bind them. Any fixed stack cap re-creates the
merge contradiction and stalls autonomous runs on the owner's availability.
Restating the selection algorithm in two documents invites the divergence the
one-place rule exists to prevent.

**Affects.** `AGENTS.md`, `docs/goal-mode.md` (new), the milestone-selection
paragraph of `docs/target-model.md`, and future goal prompts, which can now stay
lean.

## 2026-07-18 — Postgres implements the application defaults-replacement port

**Context.** The application crate owns the one-call
`ReplaceSessionDefaultsTransaction` port and its closed recorded-or-conflict
outcome, while the persistence crate already owns the atomic PostgreSQL handler
required by ADR-0027 and ADR-0034. The final adapter must connect those seams
without duplicating replay, current-pointer, or reconstruction policy.

**Decision.** Implement the application port directly for
`ReplaceSessionDefaultsRepository`. Delegate to its existing atomic handler and
exhaustively translate repository applied and rejected variants into the
application's recorded domain result, while translating owner-global conflicting
reuse to the application conflict and retaining
`ReplaceSessionDefaultsRepositoryError` unchanged. Exercise the application
service through the real adapter for first apply, equal replay, conflict,
recorded stale rejection and later replay, concurrent same-expected replacement,
current-session observation, immutable creation receipt, and infrastructure
failure.

**Rejected alternatives.** Making application depend on persistence would
reverse the adapter direction. A wrapper type would add a second public
repository without policy. Repeating SQL, preparation, or domain reconstruction
in the trait method would create a competing transaction path. Erasing
repository errors would discard the infrastructure-versus-integrity distinction.

**Affects.** `crates/persistence/src/replace_session_defaults.rs`,
S01/INV-002/INV-008/INV-012 PostgreSQL integration enforcement, and the
corresponding invariant-catalog wording. It adds no schema, domain or
application semantics, protocol, authentication, client, retry policy, or
dependency.

## 2026-07-18 — Application-owned session-defaults replacement orchestration

**Context.** The domain replacement slice defines the exact canonical command
and typed applied-or-rejected results, while ADR-0027 and ADR-0034 require
owner-global command lookup before mutable current-state validation and make one
transaction authoritative for replay, rejection, and compare-and-set
installation. Application orchestration must validate boundary identity and
invoke that transaction without preloading a `Session` or preparing against a
potentially stale snapshot.

**Decision.** Represent the application input as a private-field
`ReplaceSessionDefaultsRequest` carrying exactly command identity, session
identity, expected current version, and complete replacement defaults. Reuse the
existing public `InvalidDurableCommandId` to reject nil and max UUID sentinels
before canonical construction. Define an async application-owned
`ReplaceSessionDefaultsTransaction` port that accepts the canonical unprepared
domain command, and a closed application outcome containing either its recorded
applied-or-rejected domain result unchanged or typed conflicting owner-global
reuse. A generic service constructs once, calls the port exactly once, and
returns terminal outcomes or transaction failure unchanged; it does not load
current session state, prepare, retry, or translate results.

**Rejected alternatives.** Preparing from an application-loaded `Session` would
race the authoritative pointer and move replay lookup after mutable validation.
Depending on persistence or SQLx would reverse the adapter direction. A generic
durable-command service or shared request trait would abstract one admitted
command without a demonstrated second common policy. Flattening domain rejection
variants into application errors would erase recorded terminal meaning. Retrying
here would obscure whether a transaction committed; resubmission with the same
command identity is the accepted recovery.

**Affects.** `crates/application/src/replace_session_defaults.rs`, application
exports, the generalized documentation of `InvalidDurableCommandId`, and
INV-001/INV-002/INV-008/INV-012 enforcement links. It adds no persistence
adapter, schema, SQL, session read, protocol, authentication, client, hub
wiring, `SubmitInput`, or retry policy.

## 2026-07-18 — Atomic Postgres session-defaults replacement

**Context.** ADR-0022, ADR-0027, and ADR-0034 require one owner-global command
claim, normalized purpose-specific records, immutable defaults versions, and a
compare-and-set current pointer. The existing schema admits only
`CreateSession`, whose reverse foreign key cannot express a second typed command
family, while the domain now supplies the closed replacement payload and
applied-or-rejected reconstitution seam.

**Decision.** Extend the registry's closed kind set and replace its one-kind
reverse foreign key with a deferred closed-case constraint trigger requiring
exactly the matching typed record at commit. Store the replacement payload and
closed terminal result in one append-only normalized record with constrained
direct/alias selection fields, positive full-`u64` versions, variant-specific
result columns, and an applied-result foreign key to the exact immutable
installed version; the target session has no unconditional foreign key so a
missing-session rejection remains recordable. Under `READ COMMITTED`, inspect
the registry before mutable state, claim unseen IDs with the owner-global
primary key, reconstruct equal replay from immutable receipt facts, and use a
guarded pointer update as the concurrency boundary. A lost pointer
compare-and-set reloads current state and records the resulting stale rejection;
every effect and the typed receipt commit together. Known other command kinds
produce conflict, while purpose-specific loads report them distinctly from both
absence and corruption.

**Rejected alternatives.** A registry JSON payload or generic command repository
would weaken the accepted typed boundary. One typed table per result variant
would require another cross-table exclusivity trigger without adding domain
distinctions. Keeping the CreateSession reverse foreign key would make the
registry falsely single-kind. Locking in application memory or selecting
serializable isolation would add coordination policy when the pointer
compare-and-set already orders replacements. Requiring an applied receipt to
join the mutable current pointer would invalidate historical replay after a
later replacement. An unconditional target-session foreign key would make the
accepted missing-session rejection impossible to store.

**Affects.**
`crates/persistence/migrations/202607180002_replace_session_defaults.sql`, the
internal registry inspection,
`crates/persistence/src/replace_session_defaults.rs`, cross-kind handling in
`crates/persistence/src/create_session.rs`, transaction-scoped current-session
loading, and S01/INV-002/INV-008/INV-012 PostgreSQL integration enforcement. It
adds no application port, protocol, authentication, input submission, retry
policy, generic repository, or new dependency.

## 2026-07-18 — Canonical session-defaults replacement domain boundary

**Context.** ADR-0027 already admits one idempotent session-level command
carrying command identity, session identity, expected current defaults version,
and a complete replacement, and fixes compare-and-set installation of the next
immutable version. ADR-0034 requires structural replay equality plus closed
typed applied-or-rejected results, while ADR-0035 requires complete checked
reconstitution. The remaining Rust spelling and the unavoidable results of
session absence, stale current version, and exhausted ordinal are implementation
choices rather than new lifecycle semantics.

**Decision.** Represent `ReplaceSessionDefaults` with exactly those four caller
fields and exclude only `command_id` from equality and hashing. Preparation
against a matching complete `Session` produces either a private-field applied
result carrying the target and complete installed successor, a typed
current-version mismatch, or typed version exhaustion; a separately named
absence preparation produces the typed missing-session rejection after the
transaction establishes absence. A cross-wired supplied `Session` is a
nonterminal preparation error, not a recorded rejection. Reconstitution takes
purpose-specific typed result and immutable installed-version facts, validates
target ownership, expected/result/installed versions, checked succession, and
complete defaults equality, and returns one correlated applied-or-rejected
receipt without authorizing an effect. It deliberately does not load or validate
the mutable current-defaults pointer: a later command may advance that pointer
without invalidating equal replay of this historical result.

**Rejected alternatives.** Treating stale state, absence, or exhaustion as
infrastructure failure would let a retry reinterpret already handled intent or
make the checked ordinal panic boundary ambiguous. Including command identity in
equality would contradict owner-global lookup followed by payload comparison.
Requiring the current pointer to remain at the installed version would make a
valid later replacement corrupt an earlier command receipt. Returning only a new
version would omit the complete replacement and target correlation. Public
constructors from result fields or accepting a preassembled versioned value
would let boundary code bypass command/effect checks.

**Affects.** `crates/domain/src/replace_session_defaults.rs`, its public
exports, and INV-002/INV-008/INV-012 enforcement links. It adds no persistence
record, pointer update, transaction, application port, protocol,
acknowledgement, `SubmitInput`, or change to already accepted work.

## 2026-07-18 — Postgres adapters implement application-owned session ports

**Context.** The application crate owns the atomic `CreateSessionTransaction`
and current-snapshot `SessionReader` ports, while the existing persistence
repositories already implement their PostgreSQL semantics as inherent
operations. The adapter layer must connect those seams without making
application depend on SQLx or duplicating command and reconstitution behavior.

**Decision.** Make the persistence crate depend inward on the application crate
and implement both application ports directly for their purpose-specific
PostgreSQL repositories. Map the closed creation outcome variants exhaustively,
retain repository infrastructure and corruption errors as the adapter error
types, and delegate the session query to the existing database-consistent
complete-projection load. Exercise both services through the real adapters
against the pinned PostgreSQL image.

**Rejected alternatives.** Depending on persistence from application would
reverse the accepted dependency direction. Wrapper repositories would duplicate
no policy and add another public type per port. Reimplementing SQL or domain
reconstruction in the trait methods would create competing persistence paths.
Erasing repository errors behind strings would discard the
infrastructure-versus-integrity boundary.

**Affects.** The persistence crate's focused application dependency,
`crates/persistence/src/create_session.rs`, `crates/persistence/src/session.rs`,
and S01/INV-002/INV-008/INV-012 integration coverage. It adds no protocol,
authentication, client, cache, hub wiring, input submission, defaults
replacement, or retry policy.

## 2026-07-18 — Application-owned current-session query port

**Context.** ADR-0038 defines `load_session(SessionId)` as a separate
current-snapshot query that returns the complete domain `Session`, distinguishes
true session absence from malformed durable state, and leaves exact repository
trait spelling and async types as implementation choices. The application crate
needs that query without depending on persistence or reconstructing storage
facts.

**Decision.** Define an async `SessionReader` application port whose only query
input is `SessionId` and whose successful value is `Option<Session>`. Compose it
in a generic `LoadSessionService` that delegates exactly once and returns the
adapter's complete session, true absence, or error unchanged. Use an immutable
receiver because this is a read capability; adapters retain ownership of
database consistency and fail-closed integrity classification.

**Rejected alternatives.** Returning persistence records would reverse the
domain boundary. Loading by durable-command identity or through the creation
receipt would conflate replay history with current conversational state.
Returning a partial or application-reconstructed session would weaken complete
checked reconstitution. Retrying, caching, or translating adapter errors here
would invent policy and could hide whether a later load observes a different
current-defaults pointer.

**Affects.** `crates/application/src/load_session.rs`, its public exports, and
INV-002/INV-008/INV-012 enforcement links. It adds no persistence adapter, SQLx
dependency, command handling, defaults replacement, protocol, authentication,
client, cache, or hub wiring.

## 2026-07-18 — Application-owned CreateSession orchestration ports

**Context.** ADR-0033 places hub-minted session UUIDv7 generation in application
orchestration, while ADR-0034 makes the atomic persistence boundary
authoritative for first handling, equal replay, and conflicting reuse. The
admitted domain candidate fixes owner initiation with no ancestry, and ADR-0038
forbids replacing the recorded command receipt with a loaded current session.

**Decision.** Represent the application input as a private-field
`CreateSessionRequest` whose fallible `try_new` constructor rejects ADR-0033's
nil and max command UUID sentinels before canonical command construction.
Compose a `SessionIdGenerator` port, with a UUIDv7 production implementation,
and an async `CreateSessionTransaction` port in a generic
`CreateSessionService`. Each execution mints one fresh candidate, fixes
`OwnerInitiated` plus `None`, prepares through the domain seam, and invokes the
atomic port exactly once. Return the port's recorded
`CreateSessionAppliedResult` unchanged for first handling or equal replay, a
typed conflicting-reuse outcome for a different payload, and a nonterminal error
for preparation or transaction failure. Do not retry, pre-load command state, or
load/return the current `Session`.

**Rejected alternatives.** Depending on the persistence crate would reverse the
intended adapter direction. Generating identities in Postgres or domain code
would violate ADR-0033. Returning the invocation's fresh candidate on replay
would replace the recorded receipt. Returning `Session` would conflate the
receipt with ADR-0038's separate current snapshot. Retrying inside the use case
would obscure whether the transaction committed; the caller may resubmit the
same command ID and let the atomic port resolve replay.

**Affects.** `crates/application/src/create_session.rs`, the application crate's
focused UUIDv7 dependency and public exports, and
INV-001/INV-002/INV-003/INV-008/INV-012 enforcement links. It adds no
persistence adapter, current-session load, protocol, authentication, client or
hub wiring, fork, input-submission, or defaults-replacement behavior.

## 2026-07-18 — Atomic CreateSession handling and complete replay load

**Context.** The accepted CreateSession domain candidate and relational record
family provide the sealed input and complete durable shape required by ADR-0034
and ADR-0035, but neither supplies the database operation that claims an
owner-global command identity, commits its effects, handles concurrent
duplicates, and reconstructs the recorded result after restart.

**Decision.** Accept only `PreparedCreateSession` at the persistence boundary.
In one transaction, first load any complete existing command by owner-global
identity; otherwise claim the registry identity with
`INSERT ... ON CONFLICT DO NOTHING`, load the committed winner after a lost
race, or insert the session, immutable defaults version one, current-defaults
pointer, and typed command record before commit. Compare reconstituted domain
payloads rather than raw rows: equal replay returns the original applied result
even when the caller supplied a different candidate session identity, while
unequal reuse returns a typed conflict. Treat an incomplete, unsupported,
inconsistent, or domain-invalid claimed record as corruption rather than unseen
work. Use PostgreSQL's default `READ COMMITTED` isolation plus the registry
uniqueness boundary; add no locking or persistence dependency.

**Rejected alternatives.** A precheck outside the transaction would race with
another handler. Returning the replay caller's candidate session would change
the recorded outcome. Comparing SQL fields ad hoc would duplicate domain
equality and reconstitution rules. Treating incomplete claims as absent would
permit reused identities to escape fail-closed handling. Serializable
transactions, advisory locks, or an application mutex add retry or coordination
machinery that the unique registry claim does not require.

**Affects.** `crates/persistence/src/create_session.rs`, its public module
export, S01/INV-012 PostgreSQL integration tests, and INV-012's enforcement
link. It does not add application, hub, wire, authentication, input-submission,
fork, or recorded-rejection behavior and does not claim INV-007 enforcement.

## 2026-07-18 — Initial CreateSession relational record family

**Context.** ADR-0022 and ADR-0034 select normalized purpose-specific records
but deliberately leave concrete names and fields to each admitted command slice.
The accepted CreateSession domain slice currently admits only owner-initiated
creation with no transcript ancestry and only an applied result. Its first
migration must retain the complete typed payload, created session, immutable
provenance, initial defaults, and current-defaults pointer without choosing the
still-open transcript-frontier storage representation or pretending that the
repository transaction already exists.

**Decision.** Use an append-only owner-global `durable_command` registry and an
append-only one-to-one `create_session_command` typed record, correlated by
deferred foreign keys so neither can commit alone. Store the created session and
its independent `creation_cause` and `ancestry_kind` in `session`; store
immutable model-selection versions in `session_defaults_version` and the
replaceable pointer in `session_current_defaults`, with deferred reverse foreign
keys requiring every session to have both a current-defaults pointer and its
backing `create_session_command` record at commit. Make the current-defaults
pointer's foreign key deferrable so a version and its pointer can be written
together in either statement order. Use native UUID columns, positive full-`u64`
`numeric(20, 0)` versions, closed text discriminators, XOR direct/alias UUID
payloads, and composite foreign keys that correlate the command payload with its
created session, provenance, and version-one defaults. All application-supplied
identity columns have no database default.

**Rejected alternatives.** A JSON or byte payload would contradict ADR-0034's
typed-record boundary. A shared untyped model-selection UUID would make the
storage record less explicit about distinct domain kinds. Nullable
ancestry-source columns would choose a `TranscriptFrontier` encoding before its
trusted representation exists; the admitted `none` discriminator retains the
independent ancestry fact without inventing fork storage. Leaving the registry
or current pointer completeness to adapter convention would permit torn
committed shapes that deferred foreign keys can reject. Database-generated UUID
defaults would move minting authority away from application orchestration.

**Affects.** `crates/persistence/migrations/202607180001_create_session.sql`,
its PostgreSQL integration tests, and the first CreateSession record mapping and
transaction that will consume this schema. It adds no repository operation,
acknowledgement boundary, fork behavior, recorded rejection, authentication, or
wire contract, and therefore does not claim transaction-level INV-007
enforcement.

## 2026-07-17 — PostgreSQL 18 production and integration-test baseline

**Context.** [ADR-0032](decisions/0032-postgres-implementation-dependencies.md)
requires an explicitly tagged supported PostgreSQL image and requires production
and integration tests to use the same major, while leaving the exact tag as an
implementation decision. Signalbox is greenfield: it has no deployed database,
compatibility obligation, or accepted schema feature requiring an older major.
PostgreSQL 18 has a longer remaining upstream support window than PostgreSQL 17.

**Decision.** Establish PostgreSQL 18 as the production and integration-test
major baseline and pin the initial Testcontainers image to
`postgres:18.4-alpine3.23`. Production deployment must select PostgreSQL 18
while this baseline is current. Compatible patch-image updates remain ordinary
dependency maintenance under ADR-0032 and stay explicit rather than following a
floating tag.

**Rejected alternatives.** PostgreSQL 17.10 is supported and has more elapsed
production history, but Signalbox has no existing deployment or schema
compatibility need that benefits from starting one major behind, and doing so
would shorten the baseline's remaining support window. A floating `18`,
`18-alpine`, or `latest` tag would make test inputs change without repository
review and is already rejected by ADR-0032.

**Affects.** `crates/persistence/tests/postgres_integration.rs`, the
Docker-backed CI test baseline, and future production database-version
selection. It changes no domain, transaction, migration, or schema semantics.

## 2026-07-17 — Materialize complete membership for first context-frontier storage

**Context.** [ADR-0022](decisions/0022-persistence-representation.md) and
[ADR-0030](decisions/0030-context-frontier-snapshots.md) deliberately permit
complete membership, parent-plus-append, or shared immutable prefixes when each
representation resolves to the same complete ordered-distinct source-qualified
sequence. The first S01/S03 persistence slice benefits more from direct
constraints and inspectability than from prefix compression, and choosing among
those already-permitted physical forms does not change domain semantics.

**Decision.** Store one immutable snapshot header for the composite
`(owning session, context-frontier identity)` and materialize one membership row
per one-based ordered position carrying the source session and semantic-entry
identity. Enforce unique position and unique source-qualified entry membership
within each snapshot, require every member to reference its immutable semantic
entry, and insert the complete contiguous membership in the transaction that
first binds the snapshot. Load and reconstitution read the complete sequence
directly; no parent traversal, cache, digest, or content canonicalization is
part of the first representation.

**Rejected alternatives.** Parent-plus-append reduces repeated rows but makes
complete validation, missing-parent behavior, and query depth part of the first
adapter. Shared-prefix nodes can save more space but add reference accounting
and migration complexity before measurements exist. A serialized identity array
weakens relational foreign keys and duplicate enforcement. Content-addressed
deduplication would change identity and authority semantics rather than merely
optimize storage.

**Affects.** The first context-frontier migration, persistence records and
mappings, complete-snapshot integration tests, and ADR-0035 reconstitution
queries. Equal-content snapshots still retain distinct identities. A later
measured migration may introduce parent or shared-prefix storage only if it
preserves every existing composite identity and exact resolved sequence required
by ADR-0030.

## 2026-07-17 — Prepared model calls borrow resolved frontier projections

**Context.** [ADR-0005](decisions/0005-model-call-retry-semantics.md) and
[ADR-0030](decisions/0030-context-frontier-snapshots.md) require the exact call
frontier to exist on the prepared record before send authorization. The
preceding value slice makes a resolved projection available through a sealed
construction seam, while the existing `CurrentModelCall::prepared` accepts only
the call identity and turn-wide pinned target.

**Decision.** Add a nonoptional private `ContextFrontier` field to both
`CurrentModelCall` and `EndedModelCall`. Make the crate-private prepared entry
borrow a `ResolvedContextFrontierSnapshot` and copy its exact identified
frontier into the call, so the record cannot be created through that seam from a
bare frontier reference. Preserve the field through every successful nonterminal
and terminal transition and every rejected transition's unchanged current value,
and expose only a read accessor. Keep the frontier off `PinnedProviderTarget`:
the target is one turn-wide fact, while each call records its own frontier. The
later aggregate still owns selection of the lifecycle-correct resolved
projection and atomic persistence.

**Rejected alternatives.** Taking a bare `ContextFrontier` at prepared entry
would discard the resolved-projection boundary before aggregate validation
exists. Storing an optional frontier or attaching one after creation would admit
the frontierless prepared state the records exclude. Moving the frontier onto
`PinnedProviderTarget` would incorrectly make one frontier apply to the whole
turn. Owning the complete resolved projection inside every call would duplicate
semantic contents instead of retaining the accepted identified reference.

**Affects.** `crates/domain/src/model_call.rs`, provider-evidence test fixtures
that construct canonical calls, and INV-014/INV-015 enforcement links in
`docs/invariants.md`. Requested-selection recording,
call-to-turn/attempt/session correlation, semantic-entry eligibility, safe-point
consumption, authoritative snapshot-identity freshness, and atomic persistence
remain later aggregate work.

## 2026-07-17 — UUID-backed context-frontier values and sealed prefix derivation

**Context.** [ADR-0030](decisions/0030-context-frontier-snapshots.md) fixes
context-frontier identity, resolution, equality, immutability, and construction
authority while deliberately leaving its semantic pseudocode, initial Rust
identity backing, and trusted transition spelling open. The first domain slice
needs to compare and derive resolved frontier candidates without making a raw
identifier, structurally plausible entry list, or generally callable service
into lifecycle authority.

**Decision.** Follow the existing `define_identity!` private-field UUID-newtype
convention for the distinct `ContextFrontierId` and `SemanticTranscriptEntryId`
Rust values; this selects only their in-process backing, not generation, minting
authority, database or wire encoding, serialization, or formatting. Spell
ADR-0030's accepted value algebra as private-field structs and store the
resolved projection's ordered references in an immutable boxed slice. Name the
explicit content operation `same_semantic_content`; keep complete-candidate
validation and `derive_appending_candidate` crate-private; and return every
rejected candidate input unchanged through owned typed errors. Place the opaque
`AcceptedInputTurnStart` beside starting lineage in `turn_lifecycle`; expose
observation only and no production constructor until the eligibility aggregate
can derive both fields authoritatively.

**Rejected alternatives.** Bespoke identity implementations would duplicate the
crate's existing UUID-newtype contract. Retaining a mutable-capacity `Vec`
inside the resolved value or adding an ordered-set dependency provides no
benefit over an immutable boxed slice at this boundary. Public candidate seams
or a general turn-start constructor would expose construction beyond the future
trusted producers. Binding frontiers to model calls in this value slice would
combine a separate transition change before its call-correlation tests and
documentation are reviewable.

**Affects.** New `crates/domain/src/context_frontier.rs`; the lifecycle-owned
start value in `crates/domain/src/turn_lifecycle.rs`; identity test support and
public re-exports in `crates/domain/src/lib.rs`; INV-001, INV-015, and INV-030
enforcement links in `docs/invariants.md`; and representation wording in
ADR-0022, ADR-0030, `docs/open-questions.md`, and `docs/glossary.md`.
Eligibility, model-call binding, steering consumption, aggregate authority,
persistence, and ancestry-boundary resolution remain later slices.

## 2026-07-17 — Atomic-only prepared fatal-mismatch candidate

**Context.** [ADR-0004](decisions/0004-turn-and-attempt-lifecycle.md) permits
live completed-call invalidation during `Prepared` to end directly as fatal
known failure, while that attempt state has no `StopRequested` or
fatal-reconciliation edge. The preceding lifecycle binding deliberately requires
a fatal-stop fallback and therefore covers only `Running` and `StopRequested`.

**Decision.** Represent the prepared path with a separate crate-private
consuming binding and `PreparedFatalMismatchAtomicCandidate`. It couples exact
sealed facts to `AfterFatalMismatch(KnownFailure)` and `Failed`, carries a
canonical set of logical dependencies that the later aggregate must close in the
same transaction, and has no stop fallback. Any unclassified operation or
blocking ambiguity rejects with the original facts and source phase.

**Rejected alternatives.** Reusing the stoppable-attempt binding would require
an invalid optional fallback. Synthesizing `StopRequested` or
`AfterFatalMismatch(Ambiguous)` invents transitions. Dropping open logical
dependencies loses required same-transaction work. Treating the candidate as
commit proof would bypass canonical aggregate, steering, and slot guards.

**Affects.** New internal `crates/domain/src/fatal_mismatch/prepared.rs`, its
parent-module registration, and enforcement links for INV-006 and INV-014.
Canonical logical closure, remaining terminal guards, steering reclassification,
slot release, cancellation intent, startup handling, and atomic persistence
remain later work.

## 2026-07-17 — Sealed live fatal-mismatch lifecycle candidate binding

**Context.** [ADR-0031](decisions/0031-direct-fatal-terminalization.md) owns the
stop-versus-direct-closure rule, while the preceding slice derives its sealed
post-evidence inputs but commits no lifecycle transition. The Rust
implementation needs a candidate representation that couples those inputs to
existing attempt and turn values without implying aggregate or commit authority.

**Decision.** For `Running` and `StopRequested` attempt projections, represent
local binding as a crate-private consuming `FatalMismatchLifecycleBinding` that
retains the source facts, exact supplied `ActiveTurnPhase`, and a closed outcome
enum. Represent the closed branch with one private-field value coupling
`EndedTurnAttempt`, `TurnDisposition`, and the exact fatal-stopped fallback. A
separate private marker candidate couples nonempty ambiguity and fatal-cause
values for the narrow `turn_lifecycle` construction seam. Local rejection
retains the unchanged facts and phase.

**Rejected alternatives.** Raw tuples or independent optional fields permit
invalid combinations. A public marker constructor lets sibling code pair
unrelated values. Omitting the fallback loses a value the later aggregate needs
on rejected terminalization. Exposing the binding publicly or naming it as a
committed transition would overstate its authority.

**Affects.** New internal `crates/domain/src/fatal_mismatch/lifecycle.rs`, one
sealed marker-construction seam in `crates/domain/src/turn_lifecycle.rs`, and
enforcement links for INV-006, INV-025, INV-026, and INV-029. Prepared atomic
invalidation binding, aggregate guards, canonical mutation, steering, slot
ownership, cancellation intent, startup handling, and atomic persistence remain
later work.

## 2026-07-17 — Sealed post-evidence fatal-closure derivation

**Context.** The sealed provider-target mismatch fact prevents cross-wired
evidence, while [ADR-0031](decisions/0031-direct-fatal-terminalization.md) also
forbids callers from supplying completion, guard, cause-set, ambiguity-set, or
disposition authority. The domain needs one representation from which those
facts are derived together.

**Decision.** A crate-private `CompleteFatalMismatchProjection` owns the current
attempt plus `BTreeMap`-canonicalized entries for every owned logical dependency
and issued operation. Consuming one sealed mismatch fact derives complete fatal
causes, applies the effect only to its exact compatible call state, and retains
exact unfinished blockers in a `BTreeSet` independently from optional canonical
nonempty `U`. A `Prepared` projection accepts only completed-call invalidation
against classified call history; the other two timing effects reject unchanged.
Construction remains test-only until the authoritative aggregate is its sole
production source. The result proves derivation from that projection, not a
committed lifecycle transition.

**Rejected alternatives.** Public fields or constructors would let sibling code
forge completeness or derived results. Raw tuples permit cross-wiring. Sequences
permit duplicate keys or order-sensitive equality. Application-, storage-,
wire-, or framework-owned projection types would move the accepted domain
boundary.

**Affects.** New internal `crates/domain/src/fatal_mismatch.rs`, one
canonical-union helper in `turn_attempt.rs`, and enforcement links for INV-006,
INV-014, INV-025, INV-026, and INV-029. Lifecycle binding, remaining aggregate
guards, steering, slot ownership, cancellation intent, startup recovery, and
atomic persistence remain later work.

## 2026-07-17 — Sealed provider-target mismatch correlation facts

**Context.** [ADR-0031](decisions/0031-direct-fatal-terminalization.md) requires
the aggregate to apply one trusted fatal-mismatch fact through one of three
timing-specific call effects. The existing ADR-0005 evidence slice validated
each correlation but returned only a failure reference, so later closure code
could pair a valid failure for call A with call B or a different timing effect.

**Decision.** Replace those crate-private producer results with a private-field
`AppliedProviderTargetMismatch` coupling the exact
`ProviderTargetMismatchFailureRef`, affected `ModelCallId`, and closed
`ProviderTargetMismatchEffectView`. Only the existing evidence-correlation and
invalidation boundaries construct it. The value proves correlation and timing
branch only; it does not prove that call, attempt, or turn state changed or that
a transaction committed.

**Rejected alternatives.** Returning a bare failure and supplying call or effect
separately permits cross-wiring. A public constructor or raw tuple lets callers
mint timing authority. Applying aggregate state changes inside provider evidence
crosses the evidence/aggregate boundary.

**Affects.** `crates/domain/src/provider_evidence.rs` and INV-014 enforcement.
Complete fatal-cause and closure derivation, lifecycle binding, aggregate
guards, persistence, and startup recovery remain later work.

## 2026-07-17 — Merging an ADR pull request is its acceptance

**Context.** ADRs carried a `Status:` line with a Proposed-to-Accepted
lifecycle, so records could sit on `main` while still undecided and acceptance
required a second status-flip pull request (PR #33) or a draft claiming
`Accepted` before the owner had decided (PR #31). Both paths caused reviewer
churn and slowed the decision pipeline without adding safety.

**Decision.** An ADR file under `docs/decisions/` on `main` is accepted and
authoritative. The pull request that introduces it is the proposal: while it is
open the record is under review, the repository owner's merge is the act of
acceptance, and only the owner merges ADR pull requests. Records carry no
`Status:` line, and a draft may not claim acceptance. A rejected proposal is
closed unmerged and recorded as a dated entry in this log naming the pull
request and reason. Supersession is unchanged: the old record is preserved and
both directions are linked. The header format shrinks accordingly (Date, Depends
on, Supersedes, Superseded by, Decision questions).

**Rejected alternatives.** Keeping statuses with a same-pull-request flip: still
requires an agent round-trip after the owner decides and still allows undecided
records on `main`. Mechanical enforcement through CODEOWNERS and required
review: declined as unnecessary ceremony for a single-owner repository where
only the owner merges. Deleting rejected proposals without a log entry: loses
the record that prevents relitigating the same option.

**Affects.** `docs/decisions/README.md` (process and format sections), removal
of every `Status:` line from the ten in-tree records as a meaning-preserving
mechanical correction (all were `Accepted` after PR #33 merged), and
`CONTRIBUTING.md` wording. Open ADR pull requests should drop their `Status:`
lines before merge.

## 2026-07-17 — Keyed provider-target evidence and validated mismatch producers

**Context.** ADR-0005 fixes the typed observation payload, the evidence record
keyed by `ProviderTargetEvidenceId` whose identifier lookup precedes
current-state validation, the completed-call invalidation that is unique by
invalidated call, and the three mismatch-failure reference kinds whose opaque
value `crates/domain/src/turn_attempt.rs` reserved for a provider-evidence
slice. Trust classification of raw provider data remains ADR-0007 scope, and
outcome authority, aggregate precedence, and persistence do not exist yet.

**Decision.** Store evidence records in a `BTreeMap`-keyed
`ProviderTargetEvidenceLog` whose crate-private recording returns a typed
first-versus-replayed outcome, rejects identifier reuse with the unchanged
existing record and exact rejected input, and durably records a fresh identifier
only when the claimed variant is consistent with the exact target derived from
the canonical call record. Bind that recording to the canonical record by taking
the call identity and its target paired in a crate-private `CanonicalCallTarget`
that outside the module is constructed only from a real `CurrentModelCall` or
`EndedModelCall`, so a caller cannot record one call's observation against
another call's target. Keep `ProviderTargetEvidence` and
`ProviderTargetMismatchInvalidation` private-field values with no public
constructor; neither copies the exact target, which is always read from the
canonical call record during validation. Own the completed-call invalidation's
per-call uniqueness in a `BTreeMap`-keyed
`ProviderTargetMismatchInvalidationLog` whose crate-private admission looks up
the at most one existing value for the call itself, so the first valid
correlated mismatch fixes it, structurally equal evidence replay is idempotent,
and any later observation is a typed rejection without depending on a caller to
pass the lookup; the raw per-value admission stays module-private behind that
log. Produce `ProviderTargetMismatchFailureRef` only from validating
correlations — nonterminal-call mismatch on a call past send authorization,
terminal-ambiguity resolution, and the invalidation — that check call identity,
mismatch payload, and reported-versus-target inequality, and reject an unsent
`Prepared` call that has no provider interaction to report a target; the raw
constructors in `turn_attempt` become crate-private instead of test-only, and
every producer seam stays crate-private for the later aggregate.

**Rejected alternatives.** Public evidence or invalidation construction from raw
identifiers overstates trust and lets callers mint fatal authority. Accepting
the call identity and target as independent recording inputs lets a caller
cross-wire one call's observation with another's target, durably accepting a
mislabeled match. Exposing per-value invalidation admission and trusting each
caller to perform the per-call lookup lets a later mismatch be admitted as
another first whenever the caller forgets it. Copying the exact target into
evidence or invalidation contradicts ADR-0005's derive-from-the-canonical-record
rule and duplicates the pinned fact. A boolean mismatch flag instead of the
typed observation erases the reported identity. Rejecting structurally equal
replays breaks the ADR's idempotence. Validating only inside a future aggregate
leaves the failure-reference constructors unguarded in the meantime.

**Affects.** `crates/domain/src/provider_evidence.rs`, the raw-constructor
visibility in `crates/domain/src/turn_attempt.rs`, re-exports in
`crates/domain/src/lib.rs`, and enforcement links in `docs/invariants.md`; trust
classification, outcome-authority currency, turn-membership checks, aggregate
classification precedence, startup derivation, and persistence remain later
work.

## 2026-07-17 — Private-field current and ended model-call transitions

**Context.** ADR-0005 owns the complete model-call transition table and assigns
serialized classification and correlation to the turn aggregate. The preceding
value slice makes pinned targets and prepared call records constructible, but it
does not choose how a call advances or terminalizes without letting other
callers forge in-flight or terminal records.

**Decision.** Add crate-private consuming transitions on the private-field
`CurrentModelCall`: send authorization from `Prepared`, a payload-free
best-effort cancellation request accepted only from `InFlight` (the table's
single cancellation-request edge), classified terminal dispositions (`Prepared`
admits only known failure), and proof-correlated unsent cancellation that
validates the applied interrupt proof's predecessor against the call's turn.
Successful terminal history is a separate private-field `EndedModelCall`
preserving identity, pinned fact, and disposition with no transition back.
Rejections return the unchanged current call plus the exact rejected input in
one boxed payload. Cancellation-cause retention stays with the attempt's stop
causes rather than being duplicated at the call level.

**Rejected alternatives.** Public local transitions bypass the aggregate's
serialization and guards. One state enum containing terminal variants readmits
terminal-to-nonterminal edges. Idempotently replaying a cancellation request on
an already-requested call adds a self-loop the accepted table does not contain;
the serialized aggregate already knows the durable request exists. A
cause-carrying call-level cancellation state duplicates the attempt stop-cause
algebra. Permitting `Prepared` to classify completion, refusal, ambiguity, or
proof-free cancellation admits outcomes an unsent request cannot have.

**Affects.** `crates/domain/src/model_call.rs`, the `EndedModelCall` re-export
in `crates/domain/src/lib.rs`, and enforcement links in `docs/invariants.md`;
trusted evidence correlation, outcome-authority transfer, aggregate guards,
persistence, and startup scanning remain later work.

## 2026-07-17 — Pinned provider-target fact and model-call record values

**Context.** ADR-0005 pins one exact hub-resolved provider/model target as a
durable turn fact before the first `ModelCallId` exists, requires every model
call to carry that exact target at creation, and fixes the
`Prepared`/`InFlight`/`CancellationRequested`/`Terminal(disposition)` call
algebra. The deployment facts needed to resolve a frozen selection, and the
aggregate that serializes call creation, do not exist yet, so this slice needs
representations that cannot overstate resolution or aggregate authority.
Provider-identity normalization and detailed provenance remain the open ADR-0007
questions.

**Decision.** Represent the normalized provider/model identity space as a
private UUID-backed `ProviderModelIdentity` under the existing identity
convention, wrap the hub-resolved role as `ResolvedProviderTarget`, and pin turn
and target in a private-field `PinnedProviderTarget` whose producer stays
crate-private for the later resolution-owning slice. Factor the call record as a
private-field `CurrentModelCall` that holds its pinned fact and is created only
by a crate-private prepared entry, so a call cannot exist without an exact
target. Keep terminal dispositions as ADR-0005's exact five-variant enum and
nonterminal states as a closed three-variant enum. Represent no
target-resolution-failure entity: ADR-0005 records that failure as the attempt
and turn failure, which is already representable.

**Rejected alternatives.** A raw provider string or provider/model string pair
decides normalization and encoding that ADR-0007 owns. An optional or defaulted
target field admits the targetless call ADR-0005 prohibits. Public pin or call
constructors let callers mint resolution results and bypass the aggregate. A
separate resolution-failure record duplicates the attempt/turn failure that
already records it. One state enum containing terminal variants lets an ended
call re-enter nonterminal states.

**Affects.** `crates/domain/src/model_call.rs`, its re-exports and the
`test_support` constructor in `crates/domain/src/lib.rs`, and enforcement links
in `docs/invariants.md`; target resolution, call-state transitions,
provider-target evidence, aggregate correlation, and persistence remain later
work.

## 2026-07-17 — Checked session-defaults version succession

**Context.** `SessionConfigurationDefaultsVersion` is a private ordinal counter
and `VersionedSessionConfigurationDefaults::replace` installs the next version
on each explicit replacement. The successor was computed by an `expect` on
`u64::checked_add`, so an exhausted counter aborted the process by panic instead
of being reported to the caller. The sibling ordinal `SessionInputPosition`
already exposes a checked successor returning `Option`, and the
ordinal-input-positions decision explicitly rejected panicking as a way to
signal an unrepresentable ordinal.

**Decision.** Replace `SessionConfigurationDefaultsVersion::next` with
`checked_next(self) -> Option<Self>`, mirroring
`SessionInputPosition::checked_next`, and have
`VersionedSessionConfigurationDefaults::replace` return `Option<Self>` (`None`
when the counter is exhausted) so the exhaustion is propagated rather than
swallowed by a panic. No other version semantics change.

**Rejected alternatives.** Keeping the panicking `next`: it terminates the
process on a representable domain condition and contradicts the
checked-successor convention already established for session input positions.
Introducing a dedicated typed error struct for exhaustion: `Option` matches the
existing sibling successor and carries the only possible reason without adding a
one-variant error type.

**Affects.** `crates/domain/src/configuration.rs` and its tests, and a
`crates/domain/src/delivery_request.rs` test that constructs a later version.
Refines the 2026-07-15 "Ordinal session-defaults versions" decision's "successor
operation" to a checked successor; storage and wire encodings remain open.

## 2026-07-17 — CreateSession payload carries unversioned defaults and derives establishment

**Context.** ADR-0003 defines the `CreateSession` payload as command identity,
creation provenance, and initial configuration defaults, and states that session
creation also establishes the first immutable version of model-selection-only
defaults, which are operationally associated with the session but not a third
creation-provenance fact. The slice needed a representation of that
creation-time coupling without claiming committed command handling.

**Decision.** Represent `CreateSession` as a private-field payload value whose
defaults field is the unversioned `SessionConfigurationDefaults`, and derive the
established value with an `establish_initial_defaults` method that always
applies `VersionedSessionConfigurationDefaults::establish` to the carried
payload. The caller therefore cannot supply a defaults version at creation,
version one is the only establishable version, and provenance remains a separate
two-fact value the defaults cannot join. The payload claims no validation,
deduplication, session identity minting, or persistence. Per ADR-0001 the
durable-command comparison payload is every caller-supplied semantic field
except the identifier itself, so `CreateSession` carries `command_id` for the
ADR-0003 terminology but excludes it from structural equality and hashing: two
payloads differing only in `command_id` compare equal (equal replay), while a
provenance or defaults difference is a distinct payload (conflicting reuse of
one identifier is then detectable), matching the sibling `DeliveryRequest`
payload, which omits command identity entirely.

**Rejected alternatives.** Carrying a `VersionedSessionConfigurationDefaults`
field lets a caller claim an arbitrary creation-time version. A free-standing
established-session value pairing provenance with versioned defaults claims an
applied creation without the aggregate's atomic validation. Adding defaults to
`SessionCreationProvenance` makes them a third provenance fact, which the ADR
excludes. Omitting the coupling method leaves establishment as a convention
instead of a typed derivation. Including `command_id` in structural equality
would make the INV-012 replay comparison treat two otherwise-identical payloads
as distinct solely because their identifiers differ, contradicting ADR-0001's
canonical comparison payload and the projection the deduplication boundary uses.

**Affects.** `crates/domain/src/session.rs`, its re-exports from
`crates/domain/src/lib.rs`, and the INV-012 enforcement link in
`docs/invariants.md`; authoritative creation handling, owner authority,
deduplication and replay, session identity minting, and persistence remain later
slices.

## 2026-07-17 — Opaque transcript frontier and session-provenance value spelling

**Context.** ADR-0003 requires every session to record an immutable creation
cause and an independent transcript ancestry of none or exactly one exact source
frontier, and states its pseudocode is not final Rust spelling. The
representation of a boundary in semantic history is undecided
(semantic-transcript-entry identity remains an open question), and the
turn-lifecycle slice deliberately declined to invent a frontier token.

**Decision.** Represent the cause as the closed one-variant enum
`SessionCreationCause::OwnerInitiated` (spelled with the `Session` prefix for
the flat crate namespace), ancestry as a two-variant enum whose single-source
variant carries the source session and an opaque `TranscriptFrontier`, and
provenance as a private-field pair requiring both facts. Back the frontier with
a private UUID token that has no public constructor, accessor, or raw-part
conversion, so equality compares exact boundaries while the trusted producer
arrives with the slice that fixes semantic-history boundaries.

**Rejected alternatives.** A `#[non_exhaustive]` cause enum: reserved extension
examples are added as typed variants by the ADR that defines their initiating
identity, and a wildcard arm today would silently absorb causes that cannot
exist yet. A public UUID-backed frontier identity via `define_identity!`: it
exports a durable identity kind the ADR-0001 identity set does not list and lets
callers mint unvalidated fork points. An `Option`-wrapped source struct instead
of an ancestry enum: the ADR gives explicit `None` its own meaning, which the
named variant documents and extension preserves.

**Affects.** New `crates/domain/src/session.rs`, its re-exports from
`crates/domain/src/lib.rs`, and enforcement links for INV-003 and INV-030 in
`docs/invariants.md`; atomic creation-time validation, frontier selection from
real source history, persistence, and the `CreateSession` payload coupling
remain later slices.

## 2026-07-17 — Shared test constructors for domain identities

**Context.** Every unit-test module built domain identities with the same
`Type::from_uuid(Uuid::from_u128(value))` pattern behind small named helpers, so
`turn_id` was defined identically in three modules, `direct` in two, and
`session_id`, `model_call_id`, and `accepted_input_id` each carried their own
copy. The repetition added no test meaning and drifted independently as modules
were added.

**Decision.** Add a `#[cfg(test)] pub(crate) mod test_support` in
`crates/domain/src/lib.rs` that generates the identity constructors (`turn_id`,
`session_id`, `accepted_input_id`, `model_call_id`, `direct`, `alias`) from one
macro, and import them where each test module previously defined its own. This
is a mechanical test-only refactor: no production types, public API, or asserted
behavior change, and the full validation sequence still passes.

**Rejected alternatives.** Emitting a `from_u128` constructor from
`define_identity!` onto every identity type: it would touch call sites
throughout and add a constructor to production types solely for tests. A generic
`id::<T>(u128)` helper behind a new trait: it adds a trait and turbofish call
sites for no readability gain over the terse named constructors the tests
already used. Leaving the duplication: it keeps five helpers drifting across
modules.

**Affects.** The `#[cfg(test)]` test modules of
`crates/domain/src/{accepted_input,configuration,delivery_request,queue_order}.rs`
and the new `test_support` module in `crates/domain/src/lib.rs`. No non-test
code, re-exports, or invariants change.

## 2026-07-16 — Canonical reconciliation and active-phase values

**Context.** ADR-0004 fixes the tagged nonempty ambiguity set, proof-bearing
reconciliation and terminal values, and exact active-phase variants. ADR-0027
fixes the starting-lineage algebra. The exact context-frontier and aggregate
evidence boundaries needed to construct a complete start or claim a lifecycle
transition do not yet exist, so this slice needs representations that cannot
overstate that authority.

**Decision.** Represent starting lineage and issued-operation kinds as closed
enums. Store ambiguity references in a private `BTreeSet`, rejecting empty and
duplicate caller collections so valid reorderings compare equal. Keep the
applied stop proof opaque and the reconciliation marker's fields private; expose
only observation of their exact payloads. Represent each active phase and
terminal disposition as the ADR's exact structural variant, without optional
attempt or wait-subject fields. These standalone values claim neither aggregate
guard satisfaction nor a valid state transition.

**Rejected alternatives.** A vector permits duplicates and event-order-dependent
equality. Stringly operation identifiers collapse distinct physical-operation
kinds. Public proof or marker construction from raw identifiers lets callers
mint lifecycle authority. Optional fields or a catch-all wait admit invalid
phase shapes. Inventing a frontier token or incomplete aggregate would choose an
undecided semantic boundary.

**Affects.** `crates/domain/src/turn_lifecycle.rs`, its re-exports from
`crates/domain/src/lib.rs`, and enforcement links in `docs/invariants.md`; exact
frontier construction, `AcceptedInputTurnStart`, the authoritative turn
aggregate, eligibility, production proof construction, terminal guards,
persistence, and startup recovery remain later work.

## 2026-07-16 — Private-field current and ended attempt transitions

**Context.** ADR-0004 owns the complete attempt-state transition table and
assigns its transitions to the turn aggregate. The preceding turn-attempt value
slice makes stop and terminal values constructible, but it does not choose how
the aggregate enters or leaves a current Rust attempt without letting other
callers forge `Running`, `StopRequested`, or terminal history.

**Decision.** Represent the live component as a private-field
`CurrentTurnAttempt` that factors one `TurnAttemptId` from its nonterminal
state. Keep its prepared entry and all consuming transitions crate-private so
the later aggregate remains the only public lifecycle authority. Preserve
identity on success and return the unchanged current value plus the exact
rejected input on failure. Represent successful terminal history as a separate
private-field `EndedTurnAttempt` with no transition back to current state; keep
aggregate-owned correlation, operation classification, wait changes, full
terminal guards, and atomic persistence outside this component.

**Rejected alternatives.** Public local transitions remain an aggregate-guard
bypass even when fields are private. A publicly constructible state value with
identity in each variant also allows callers to forge later states and repeats
identity handling. Mutating transitions can leave rejected inputs or partial
state changes implicit. Returning a bare error discards the authoritative
current value and the input that failed. Letting callers pair `TurnAttemptId`
with `AttemptEnd` bypasses predecessor validation.

**Affects.** `crates/domain/src/turn_attempt.rs`, re-exports from
`crates/domain/src/lib.rs`, and enforcement links in `docs/invariants.md`; the
authoritative turn aggregate, applied-proof and mismatch correlation, effect
classification, waits, persistence, and startup scan remain later work.

## 2026-07-16 — Canonical turn-attempt stop and terminal values

**Context.** ADR-0004 requires cancellation-only stop to retain one
applied-interrupt proof, fatal stop to retain a nonempty set of ADR-0005
mismatch references plus any applied interrupt, and terminal history to exclude
several dishonest stop/disposition combinations. The representation of the
nonempty set and `ProviderTargetEvidenceId` backing remain below foundation
weight.

**Decision.** Store fatal failures in a private `BTreeSet` initialized from one
opaque trusted reference, making equality canonical and empty construction
unavailable without adding a dependency. Model the three ADR-0005 reference
kinds behind an opaque value so raw evidence or call identities cannot mint
fatal authority; trusted construction remains with a later provider-evidence
transition. Represent `ProviderTargetEvidenceId` as a private UUID-backed
identity under the existing identity convention. Use distinct unstopped,
cancellation-stop, and fatal-stop disposition enums, and return a typed error
with unchanged causes when a distinct second interrupt proof would otherwise be
lost.

**Rejected alternatives.** A vector permits duplicates and event-order-dependent
equality. A caller-supplied set needs an empty-case boundary and exposes the
collection representation. Public mismatch-reference constructors over raw IDs
overstate evidence authority. An optional cancellation flag or one catch-all
terminal-disposition enum admits invalid combinations that ADR-0004 excludes.

**Affects.** `crates/domain/src/turn_attempt.rs`, the `ProviderTargetEvidenceId`
export in `crates/domain/src/lib.rs`, and enforcement links in
`docs/invariants.md`; current-attempt transitions, trusted mismatch correlation,
turn aggregate guards, waits, persistence, and startup scanning remain later
work.

## 2026-07-16 — Opaque applied-interrupt result as proof boundary

**Context.** ADR-0001, ADR-0004, and ADR-0027 require cancellation authority to
come only from the matching applied interrupt result, correlated with its exact
predecessor, accepted input, and immediate successor. The current pure-domain
foundation has no complete `SubmitInput`, authoritative turn aggregate, or
persistence commit boundary, so a public raw-fact constructor would overstate
its authority.

**Decision.** Keep `AppliedInterruptProof` at the accepted private two-field
shape and expose it only from an opaque `AppliedInterruptCommandResult`. A
module-private handled-result projection and correlation function reject
recorded rejection, non-interrupt or cross-wired delivery,
target/session/origin/position mismatches, and invalid immediate-successor queue
facts. No sibling module can supply those synthetic facts. The later
transaction-owning adapter will be a child of `applied_interrupt`, which can use
the private seam while exposing only a guarded aggregate operation to sibling
modules. That adapter is the first production producer and remains responsible
for authoritative state, fact-set completeness, and commit atomicity; this
staged seam validates pure correlations only.

**Rejected alternatives.** Public construction from IDs or an untrusted applied
flag: either lets callers mint cancellation authority. Adding session or
successor to the proof: that changes the accepted algebra instead of retaining
correlation in the applied result. Defining an incomplete public `SubmitInput`,
a synthetic transaction token, or a persistence-shaped record: each crosses a
deferred boundary and claims semantics this slice cannot enforce.

**Affects.** `crates/domain/src/applied_interrupt.rs` and its re-exports from
`crates/domain/src/lib.rs`; canonical command handling, persistence,
cancellation transitions, effect evidence, ambiguity, and terminal guards remain
later work.

## 2026-07-16 — Ordinal input positions and collection-wide queue derivation

**Context.** ADR-0027 requires immutable per-session input positions plus
ordinary or immediate-after-interrupt priority facts to form one total order
over currently known work. It leaves the position representation and pure
derivation API open. A single record cannot implement the relational interrupt
rule or carry a starting predecessor before eligibility.

**Decision.** Represent `SessionInputPosition` as a private ordinal beginning at
one with a checked successor. Supply each derivation item as an explicit
session/turn/order projection and reject mixed-session collections without
adding session identity to the normative order value. Sort ordinary roots by
position, emit each root's unique recursive interrupt-successor chain, and
require later-accepted interrupt targets to advance through that derived order.
Return typed errors for malformed facts and leave storage and wire encodings
open. Two validity checks are interpretations rather than quoted ADR rules and
are documented as such on their error variants: interrupt acceptance positions
must follow their predecessor's (from ADR-0027's requirement that active-work
modes target the current active turn) and interrupt targets must advance
monotonically (formalizing "a later request must target the new authoritative
active state").

**Rejected alternatives.** UUID or timestamp positions: neither expresses
deterministic session acceptance order. Implementing `Ord` on one
`AcceptedInputQueueOrder`: interrupt priority is relational and needs the
complete set. Storing an optional direct predecessor: priority insertion would
make it premature and rewritable. Treating same-session scope as an unchecked
public precondition, silently tie-breaking malformed facts by `TurnId`, or
panicking: each would weaken the domain boundary or invent queue semantics not
accepted by the ADR.

**Affects.** `crates/domain/src/queue_order.rs` and its re-exports from
`crates/domain/src/lib.rs`; eligibility, starting lineage/frontier, persistence,
session locking, and scheduling remain later slices.

## 2026-07-16 — Delivery-request caller payload representation

**Context.** ADR-0027 defines four discriminated delivery requests. Three create
origin work and carry a model-selection override bound to the caller's expected
session-defaults version; safe-point steering must carry no independent
configuration. The first caller-payload slice needs a Rust representation
without implementing command handling or authoritative-state validation.

**Decision.** Represent `DeliveryRequest` as a domain enum with named fields for
its exact caller-supplied payload. Group the expected defaults version and
`ModelSelectionOverride` in a `PerInputConfigurationChoices` value with private
fields and read-only accessors. Give `NextSafePoint` only its expected
active-turn field, making an independent configuration choice unconstructible.

**Rejected alternatives.** Optional configuration on every variant: it would
admit both missing origin configuration and forbidden steering configuration.
Separate version and override fields on each origin-producing variant: it would
repeat one semantic unit and make partial refactors easier to cross-wire. A
wire-oriented request struct with nullable fields: domain construction would no
longer establish the discriminated payload.

**Affects.** `crates/domain/src/delivery_request.rs` and its re-exports from
`crates/domain/src/lib.rs`; acceptance validation, command identity, content,
storage, and wire mappings remain later slices.

## 2026-07-15 — Ordinal session-defaults versions

**Context.** ADR-0027 versions session model-selection defaults — creation
establishes version one and each explicit update installs a complete later
immutable version — without fixing a version representation. The caller's
expected version participates in equality comparison at acceptance.

**Decision.** `SessionConfigurationDefaultsVersion` is a private ordinal counter
starting at one with a successor operation; equality is the acceptance-time
comparison. Storage and wire encodings remain open.

**Rejected alternatives.** UUID version identities: they lose the accepted
"version one" and succession semantics. Timestamps: wall-clock coupling and
collision risk without adding meaning.

**Affects.** `crates/domain/src/configuration.rs`.

## 2026-07-15 — UUID-backed model-selection keys

**Context.** ADR-0027 defines `DirectModelSelection` as a canonical domain-owned
key with immutable semantic meaning and `ModelAlias` as an owner-configured
alias name, and represents `FrozenAliasDefinition` as "an immutable definition
version or value selecting exactly one `DirectModelSelection`", leaving concrete
encodings open. The first configuration slice needs backing values.

**Decision.** `DirectModelSelection` and `ModelAlias` are private UUID-backed
newtypes with deliberately named UUID conversions, following the representation
convention the amended ADR-0001 accepted for identity newtypes.
`FrozenAliasDefinition` takes the value form: it stores exactly the selected
`DirectModelSelection`. Deployment-key mapping, storage, wire, display, and
serialization encodings remain open, as does adding a definition-version
identity if a later slice needs one.

**Rejected alternatives.** String-backed keys: they invite provider-native
unnormalized identifiers into domain equality, which ADR-0027 forbids. A
definition-version identity inside `FrozenAliasDefinition` now: nothing
constructible needs it yet.

**Affects.** `crates/domain/src/configuration.rs`; the `define_identity` macro
becomes crate-visible for domain keys that follow the identity representation
convention.

## 2026-07-15 — Adopt a lightweight decision process

**Context.** The repository carried roughly fifty thousand words of design
documentation against a few hundred lines of code. Normative content was
duplicated across the ADRs, the decision ledger, the invariant catalog, the
scenarios, the architecture narratives, and the testing strategy, and every
change was required to reconcile all of them. The duplication and per-row status
bookkeeping, not the existence of decision records, were the main cost to review
and to agent-driven implementation.

**Decision.** Normative content lives in exactly one place; other documents link
to it. The decision ledger is replaced by this log and
[open-questions.md](open-questions.md). The five accepted ADRs (0001, 0003,
0004, 0005, 0027) remain the normative specification for decided semantics until
superseded; executable tests progressively become the enforcement of record as
slices are implemented. Ordinary decisions are made in pull requests and
recorded here; full ADRs are reserved for foundation-weight changes. Derived
documents (invariant catalog, architecture, testing strategy, process documents)
shrink to overviews, catalogs, and links in follow-up changes, and the scenarios
are frozen as design fixtures that convert to integration tests over time.

**Rejected alternatives.** Deleting `docs/decisions/` and making code comments
and tests the primary specification immediately: most decided semantics have no
implementing code yet, and recorded rejected alternatives are what prevent
re-litigating settled questions. Keeping the full ledger process: its
reconciliation cost outweighed its inventory value.

**Affects.** `docs/decision-ledger.md` (deleted), `docs/decisions.md` and
`docs/open-questions.md` (created), `docs/decisions/README.md` (simplified), and
ledger links in `README.md`, `CONTRIBUTING.md`, `AGENTS.md`, and
`docs/architecture.md`. The foundation ADRs' `Decision-ledger questions` header
lines become `Decision questions` and ADR-0003's "future ledger scope" becomes
"future decision scope" as meaning-preserving reference corrections. The
invariant catalog, architecture, testing strategy, and process documents follow
in separate pull requests.

# Contributing to Signalbox

Signalbox is currently establishing its domain language, invariants, and
architectural boundaries. Contributions should improve that foundation without
implying that undecided implementation choices are settled.

## Before proposing a change

Read the [vision](docs/vision.md), [architecture](docs/architecture.md),
[invariants](docs/invariants.md), [decision log](docs/decisions.md), and
[open questions](docs/open-questions.md). Check
[accepted ADRs](docs/decisions/README.md) before revisiting a decision. Ordinary
decisions, including closing an ordinary open question, are made in the pull
request and recorded in the decision log. A foundation-weight change — altering
accepted ADR semantics or direction, moving a boundary between domain, storage,
wire, or framework representations, weakening an invariant, introducing a
technology that constrains several components, or closing a foundation-weight
open question — must begin as an ADR pull request; the record is under review
while the pull request is open, and the owner's merge is its acceptance.

## Contribution rules

- Keep each pull request narrowly scoped and independently reviewable.
- Distinguish accepted direction, working terminology, and open questions.
- Add or update concrete scenarios alongside the decision that changes lifecycle
  behavior or introduces a new lifecycle edge.
- Give domain concepts distinct identities; do not reuse wire, storage, or
  framework types as domain types.
- Do not introduce speculative product code, deployment configuration, or
  package trees during the foundation phase.
- Add a dependency when it provides clearer types or interfaces, replaces code
  Signalbox would otherwise need to own, or supplies another focused capability
  with a concrete benefit. Prefer small, narrowly scoped dependencies and
  explain the benefit and tradeoffs in the pull request.
- Get the project owner's explicit agreement before adding a large dependency
  with substantial transitive, build-time, runtime, or architectural cost.
- Update directly affected documentation in an implementation pull request when
  needed to keep it accurate. Avoid unrelated rewording, cleanup, restructuring,
  or formatting.
- Use original explanations and examples that make sense without private
  context.
- Avoid drive-by rewording of unrelated design decisions.

## Testing

This section owns the testing strategy — the test categories and their coverage
obligations; how tests are written and how a test's value is judged is owned by
the [testing style guide](docs/testing-style.md).

Tests prove domain transitions and recovery semantics before optimizing
transport or deployment; they use the same distinct identities as the production
design and assert durable state, not only a returned status or rendered screen.
Prefer deterministic inputs: fixed clocks, seeded identifiers, scripted provider
and runner fakes, bounded schedulers, and explicitly advanced streams. Tests
that depend on real provider availability or timing are never the merge gate.
Postgres is the test database for persistence behavior, using ephemeral
containers; SQLite is not a substitute for transaction, constraint, locking, or
recovery semantics.

Expected layers, each added with the first implementation of the behavior it
covers:

- **Pure domain transitions:** table-driven state-machine cases covering every
  allowed transition and rejecting every invalid predecessor/state combination,
  plus property tests for identity preservation and terminal-state monotonicity.
  Model effects as requested decisions rather than performing I/O so ordering
  (for example "persist before provider send") is assertable.
- **Postgres integration:** real migrations, constraints, transactions,
  idempotency keys, and compare-and-set fencing against ephemeral Postgres;
  duplicate and stale-generation races prove state advances at most once and
  current state is never overwritten.
- **Fake external boundaries:** scripted provider adapters that stream
  deterministically, fail at chosen points, report identity, mismatch, or
  refusal, block until cancellation, and expose received context so call
  frontiers can be asserted; fake runners exercise approval binding, disconnect
  ambiguity, and fencing. Each real provider adapter additionally runs the same
  contract cases plus provider-specific parsing and provenance cases;
  credentialed live-provider smoke tests may exist but are never the merge gate.
  A production isolation or containment claim for a runner profile requires real
  containment testing of that profile; fake-runner tests never substantiate an
  isolation label.
- **Restart and recovery:** stop the hub at named durability boundaries (before
  acceptance; after acceptance but before scheduling; after attempt creation but
  before send; after send but before outcome persistence; during waits; after
  outcome persistence but before acknowledgement) and assert both the final
  state and the absence of forbidden effects; re-running the startup scan
  changes nothing.
- **Protocol fixtures and end-to-end slices:** language-neutral compatibility
  fixtures before a second independently versioned process or language client
  merges, and one narrow deterministic end-to-end slice per major capability
  covering its defining failure/restart path.

Test names or metadata should reference scenario and invariant identifiers when
the connection is meaningful, for example `S12_INV011_rejects_stale_generation`.
The concrete required cases for each slice live with the ADR or decision that
authorizes it and in the tests themselves.

## Validation

Run the repository-wide format, lint, test, and documentation commands listed in
the root [AGENTS.md](AGENTS.md). For documentation changes, also:

1. Check Markdown links and headings. Markdown prose is machine-wrapped at 80
   columns; `mdformat --check *.md docs/` (see [AGENTS.md](AGENTS.md)) must
   pass.
2. Search for contradictions with the invariant catalog and decision records.
3. Confirm that examples do not present provisional terminology or behavior as
   stable API.
4. Review `git diff --check` and the complete diff.

A pull request should also run any path-specific commands documented by the
nearest descendant `AGENTS.md`.

## Security

Report security concerns through [SECURITY.md](SECURITY.md), not a public issue.

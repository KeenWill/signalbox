# Agent guidance

Signalbox is built in small slices that implement contracts in `docs/spec/` or
owner-approved plans. Do not add speculative product behavior.

## Where things live

- Cross-crate and wire contracts: `docs/spec/`. `docs/spec/README.md` states the
  page conventions and the verification reference each page carries.
- Public API of the domain and application crates: `docs/domain-spine.md`,
  checked by CI against source.
- Invariants: INV-tagged tests. `docs/invariants.md` is their generated index.
- Deferred or undecided design: `docs/open-questions.md`.
- Test style: `docs/agents/testing-style.md`. Literal-provenance and label rules
  for production and test code: `docs/style.md`.
- Rules for autonomous milestone runs: `docs/agents/goal-mode.md`.
- CI runner routing: `.github/workflows/README.md`. Change a job's `runs-on` and
  that file in the same pull request.

Decisions are recorded in pull-request descriptions and git history. Document
only what the code cannot say.

## Minimum mechanism

Build the smallest thing that satisfies the behavior the task names: no config
key, table, threshold, gate, or policy the task does not name. Measurement in
the daemon is fine; a decision on top of a measurement is the owner's and lives
outside the daemon. Removing a mechanism the behavior does not need is a better
change than adding one.

In review, a finding that a mechanism can be removed is valid. A finding that
asks for a guard, handler, or check the task does not name is declined ("not
enforced; deferred") unless the gap corrupts stored data or breaks a
`docs/spec/` contract.

## Pre-alpha compatibility

No production instance exists; every daemon and client is rebuilt from this
tree. Wire formats, schema types, and stored values change to their correct
shape. Machinery protecting an old deployed version (compatibility shims,
dual-read or dual-write paths, version-tolerant decoding, legacy-value aliases,
data-upgrade scaffolding) protects deployments that do not exist and is a
defect: do not add it, and do not request it in review (owner ruling
2026-08-11).

The one compatibility task owed is a one-time migration carrying a live database
across a change that invalidates its stored data. Compatibility a `docs/spec/`
page already retains stays until that page retires it. This rule ends at the
first durable deployment, defined by the freeze condition in
[process-protocol](docs/spec/process-protocol.md).

## Dependencies

Add a dependency when it gives clearer types or interfaces, replaces code
Signalbox would otherwise own, or supplies a focused capability. Prefer small
dependencies and state the tradeoff in the pull request. Ask the owner before
adding a large one.

Do not add version-coupled pins or tests that fail on every dependency bump by
construction. A bump that changes nothing merges as-is; real breakage gets a
real fix.

## Public-source hygiene

Code, documentation, commit messages, pull-request text, and branch names cite
only public sources. Do not include or allude to non-public material of any
organization. Public open-source work may be cited regardless of publisher. The
owner's private repositories may be named as provenance, not cited as rules.

## Working on a change

- Within an assigned task, proceed without asking: branch, implement, run the
  validation sequence, open and revise the pull request. Stop at owner gates:
  merges, foundation-weight decisions, large dependencies. When two rules
  conflict in practice, report the conflict instead of resolving it silently.
- Fix every defect you introduce. A pre-existing defect outside the assigned
  change is recorded, not fixed.
- A change to a public item in the domain or application crates updates
  `docs/domain-spine.md` in the same pull request.
- A change to behavior a `docs/spec/` page describes updates that page and its
  verification reference in the same pull request. In a stack, the bottom spec
  diff covers the behavior its children implement; a child adds a spec edit only
  for behavior the bottom diff does not describe.
- Foundation-weight changes (cross-crate or wire semantics, a boundary between
  domain, storage, wire, or framework representations, weakening an invariant, a
  technology that constrains several components, closing a recorded open
  question) are proposed as a spec diff at the bottom of the implementing stack
  and merge only with that stack. Owner merge is acceptance.
- Raising a hard safety ceiling requires a reviewed code change with a test and
  rationale.
- Keep domain types distinct from storage records, protocol messages, and
  framework types.
- Name tests for the scenario and invariant they enforce when the connection is
  meaningful (`S12_INV011_rejects_stale_generation`). A test that enforces an
  accepted invariant carries the INV tag in its name or doc comment; regenerate
  the index with `python3 scripts/generate_invariants.py --write` in the same
  change.
- Update directly affected documentation in the implementing pull request. Do
  not reword, restructure, or reformat unrelated text.
- Do not add `Co-Authored-By`, session, or URL trailers to commits or
  pull-request text.
- Do not adopt the review-slog toolkit as a merge gate until its
  [blocking condition](docs/open-questions.md#review-slog-toolkit-adoption) is
  cleared.

## Pull requests

- The owner merges every pull request. Deliver each one with CI green on its
  final commit and open ready for review, not as a draft (owner ruling
  2026-08-25). Keep it narrow.
- The description claims only what the code enforces; a contract binding future
  implementers is described as a contract. Keep it under 350 words.
- Reply to every review comment in its thread: name the fixing commit, or state
  why the finding is declined. Fix P1 findings; defer P3 findings; use judgment
  on P2s. Reviewers reward building less.
- Stacks: each pull request targets the branch below it and is reviewed against
  that base. Check that a base branch still exists before stacking on it; when a
  base merges, retarget or rebase the rest. Open pull requests early. Do not
  force-push or rewrite a shared branch unless it is necessary and safe, and
  preserve owner-authored and externally added commits. Tell the owner before
  replacing an open stack with a rewrite.

## Validation

A documentation-only change runs the documentation bar; a change to code, tests,
dependency manifests, or generated contracts runs the complete bar. CI runs
both.

Documentation bar:

```bash
python3 scripts/generate_invariants.py --check
python3 scripts/check_domain_spine.py
python3 scripts/test_check_domain_spine.py
python3 scripts/check_docs_consistency.py
python3 scripts/test_check_docs_consistency.py
python3 scripts/check_migration_versions.py
python3 scripts/test_check_migration_versions.py
python3 scripts/check_numeric_bounds.py
python3 scripts/test_check_numeric_bounds.py
python3 scripts/check_panic_gate.py
python3 scripts/test_check_panic_gate.py
python3 scripts/check_style_rules.py
python3 scripts/test_check_style_rules.py
python3 scripts/test_postgres_integration_suites.py
mdformat --check *.md docs/
git diff --check
```

Complete bar (the documentation bar plus):

```bash
cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --no-fail-fast --workspace --all-targets --all-features
cargo test --no-fail-fast --workspace --all-features --doc
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps
cargo metadata --no-deps --format-version 1
```

Pass `--no-fail-fast` to every Cargo test run so one early failure does not hide
another. The ignored persistence suite needs its feature:
`cargo test --no-fail-fast -p signalbox-persistence --features postgres-integration --tests -- --ignored`;
without `--features postgres-integration` it runs zero tests and exits
successfully.

CI runs the ignored suites from
[`.github/postgres-integration-suites.toml`](.github/postgres-integration-suites.toml),
which names each suite's package, features, shard count, and exclusions. Change
a suite there, not in the workflow; `scripts/check_docs_consistency.py` fails
when the manifest, the workflow, and the command above disagree.

For documentation changes, also check repository-relative links and the rendered
Markdown. Put path-specific instructions in the nearest descendant `AGENTS.md`.

## Tooling

Run `mdformat` and other non-cargo tools inside the devenv environment
(`devenv shell`, or `devenv shell -- mdformat --check *.md docs/`), not from
system or Homebrew binaries: a plugin-less mdformat silently corrupts GFM
tables. Drop `--check` to rewrap in place; wrapping rules are in
`.mdformat.toml`.

Cargo runs inside devenv use sccache (`devenv.nix` sets `RUSTC_WRAPPER`); the
cache is per user and shared across worktrees. Bypass it for one command with
`RUSTC_WRAPPER= cargo <args>`. CI does not use it.

Run `git worktree list` before using another checkout of this clone. Do not
check out a branch by name in a second worktree: worktrees share branch refs, so
a commit can move another worktree's HEAD. Use
`git checkout --detach origin/<branch>` and `git push origin HEAD:<branch>`.

The Postgres integration suites start one container per test through
testcontainers, and only `ContainerAsync`'s `Drop` removes it, so a test process
that dies without unwinding leaves containers that pin host memory. Reclaim them
with [`tooling/sweep-test-containers.sh`](tooling/sweep-test-containers.sh): it
reports without `--apply`, and removes only containers older than two hours (by
default) that carry the label
`signalbox_persistence::disposable_test_container_labels` attaches. Start test
containers through that helper so the sweep can find them. On a shared machine,
run the sweep on a timer.

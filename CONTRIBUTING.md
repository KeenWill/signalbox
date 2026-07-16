# Contributing to Signalbox

Signalbox is currently establishing its domain language, invariants, and architectural boundaries. Contributions should improve that foundation without implying that undecided implementation choices are settled.

## Before proposing a change

Read the [vision](docs/vision.md), [architecture](docs/architecture.md), [invariants](docs/invariants.md), [decision log](docs/decisions.md), and [open questions](docs/open-questions.md). Check [accepted ADRs](docs/decisions/README.md) before revisiting a decision. A change that alters accepted direction or closes an open foundational question should normally begin as a proposed ADR.

## Contribution rules

- Keep each pull request narrowly scoped and independently reviewable.
- Distinguish accepted direction, working terminology, and open questions.
- Add or update concrete scenarios when changing lifecycle behavior.
- Give domain concepts distinct identities; do not reuse wire, storage, or framework types as domain types.
- Do not introduce speculative product code, deployment configuration, or package trees during the foundation phase.
- Add a dependency when it provides clearer types or interfaces, replaces code Signalbox would otherwise need to own, or supplies another focused capability with a concrete benefit. Prefer small, narrowly scoped dependencies and explain the benefit and tradeoffs in the pull request.
- Get the project owner's explicit agreement before adding a large dependency with substantial transitive, build-time, runtime, or architectural cost.
- Update directly affected documentation in an implementation pull request when needed to keep it accurate. Avoid unrelated rewording, cleanup, restructuring, or formatting.
- Use original explanations and examples that make sense without private context.
- Avoid drive-by rewording of unrelated design decisions.

## Validation

Run the repository-wide format, lint, test, and documentation commands listed in the root [AGENTS.md](AGENTS.md). For documentation changes, also:

1. Check Markdown links and headings.
2. Search for contradictions with the invariant catalog and decision records.
3. Confirm that examples do not present provisional terminology or behavior as stable API.
4. Review `git diff --check` and the complete diff.

A pull request should also run any path-specific commands documented by the nearest descendant `AGENTS.md`.

## Security

Report security concerns through [SECURITY.md](SECURITY.md), not a public issue.

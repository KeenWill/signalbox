# Contributing to Signalbox

Signalbox is currently establishing its domain language, invariants, and architectural boundaries. Contributions should improve that foundation without implying that undecided implementation choices are settled.

## Before proposing a change

Read the [vision](docs/vision.md), [architecture](docs/architecture.md), [invariants](docs/invariants.md), and [decision ledger](docs/decision-ledger.md). Check [accepted ADRs](docs/decisions/README.md) before revisiting a decision. A change that alters accepted direction or closes an open foundational question should normally begin as a proposed ADR.

## Contribution rules

- Keep each pull request narrowly scoped and independently reviewable.
- Distinguish accepted direction, working terminology, and open questions.
- Add or update concrete scenarios when changing lifecycle behavior.
- Give domain concepts distinct identities; do not reuse wire, storage, or framework types as domain types.
- Do not introduce product code, dependencies, deployment configuration, or speculative package trees during the foundation phase.
- Use original explanations and examples that make sense without private context.
- Avoid drive-by rewording of unrelated design decisions.

## Validation

For the current documentation-only repository:

1. Check Markdown links and headings.
2. Search for contradictions with the invariant catalog and decision ledger.
3. Confirm that examples do not present provisional terminology or behavior as stable API.
4. Review `git diff --check` and the complete diff.

Once executable tooling exists, the root [AGENTS.md](AGENTS.md) will list the required repository-wide format, lint, test, and documentation commands. A pull request should also run the path-specific commands documented by the nearest future `AGENTS.md`.

## Security

Report security concerns through [SECURITY.md](SECURITY.md), not a public issue.

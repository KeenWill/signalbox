# Agent guidance

Signalbox is in its design and foundation phase. The present scope is documentation, repository policy, non-executable diagrams, and typed pseudocode; speculative implementation is not permitted.

Authoritative starting points are `docs/architecture.md`, `docs/invariants.md`, `docs/decision-ledger.md`, and accepted records under `docs/decisions/`. Accepted ADRs override earlier narrative documents; reconcile stale documents in the same change.

Do not silently change a foundational decision or close a recorded open question. Propose an ADR and exercise the change against scenarios. Keep domain types distinct from storage records, protocol messages, and framework types. Keep pull requests narrow and reviewable.

Until tooling exists, validate with `git diff --check`, repository-relative link checks, and a review of the rendered Markdown. When tooling is added, list the exact repository-wide format, lint, test, and documentation commands here. Put future path-specific instructions in the nearest descendant `AGENTS.md`, scoped only to that subtree.

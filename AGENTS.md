# Agent guidance

Signalbox is in its design and foundation phase. Implementation is limited to mechanical workspace and tooling scaffolding plus narrowly scoped domain slices authorized by accepted decisions or explicit owner-approved plans; speculative product behavior is not permitted.

Authoritative starting points are `docs/architecture.md`, `docs/invariants.md`, `docs/decision-ledger.md`, and accepted records under `docs/decisions/`. Accepted ADRs override earlier narrative documents; reconcile stale documents in the same change.

Do not silently change a foundational decision or close a recorded open question. Propose an ADR and exercise the change against scenarios. Keep domain types distinct from storage records, protocol messages, and framework types. Keep pull requests narrow and reviewable.

Dependencies are allowed when they provide clearer types or interfaces, replace code Signalbox would otherwise need to own, or supply another focused capability with a concrete benefit. Prefer small, narrowly scoped dependencies and explain their tradeoffs in the pull request. Before adding a large dependency with substantial transitive, build-time, runtime, or architectural cost, ask the user and wait for explicit approval.

Directly affected documentation may be updated in an implementation pull request to keep it accurate. Avoid unrelated rewording, cleanup, restructuring, or formatting.

Run the repository-wide validation sequence:

```bash
cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps
cargo metadata --no-deps --format-version 1
git diff --check
```

For documentation changes, also check repository-relative links and review the rendered Markdown. Put future path-specific instructions in the nearest descendant `AGENTS.md`, scoped only to that subtree.

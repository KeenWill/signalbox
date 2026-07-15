# Agent guidance

Signalbox is in its design and foundation phase. Implementation is limited to mechanical workspace and tooling scaffolding until accepted plans authorize product slices; speculative product behavior is not permitted.

Authoritative starting points are `docs/architecture.md`, `docs/invariants.md`, `docs/decision-ledger.md`, and accepted records under `docs/decisions/`. Accepted ADRs override earlier narrative documents; reconcile stale documents in the same change.

Do not silently change a foundational decision or close a recorded open question. Propose an ADR and exercise the change against scenarios. Keep domain types distinct from storage records, protocol messages, and framework types. Keep pull requests narrow and reviewable.

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

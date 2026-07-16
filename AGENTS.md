# Agent guidance

Signalbox is in its design and foundation phase. Implementation is limited to mechanical workspace and tooling scaffolding until accepted plans authorize product slices; speculative product behavior is not permitted.

Authoritative starting points are `docs/architecture.md`, `docs/invariants.md`, `docs/decision-ledger.md`, and accepted records under `docs/decisions/`. Accepted ADRs override earlier narrative documents; reconcile stale documents in the same change.

Do not silently change a foundational decision or close a recorded open question. Propose an ADR and exercise the change against scenarios. Keep domain types distinct from storage records, protocol messages, and framework types. Keep pull requests narrow and reviewable.

Dependencies are allowed when they provide clearer types or interfaces, replace code Signalbox would otherwise need to own, or supply another focused capability with a concrete benefit. Prefer small, narrowly scoped dependencies and explain their tradeoffs in the pull request. Before adding a large dependency with substantial transitive, build-time, runtime, or architectural cost, ask the user and wait for explicit approval.

For Rust modules, prefer the modern file layout: use `input.rs` for `mod input;` and `input/disposition.rs` for its children rather than `input/mod.rs`. Introduce `mod.rs` only when consistency with an existing subtree makes it the less disruptive choice.

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

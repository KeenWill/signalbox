# Agent guidance

Signalbox is in its design and foundation phase. Implementation is limited to mechanical workspace and tooling scaffolding plus narrowly scoped domain slices authorized by accepted decisions or explicit owner-approved plans; speculative product behavior is not permitted.

Authoritative starting points are `docs/architecture.md`, `docs/invariants.md`, `docs/decisions.md`, `docs/open-questions.md`, and accepted records under `docs/decisions/`. Accepted ADRs are the normative specification for decided semantics; executable tests become the enforcement of record as slices implement them. When selecting milestones, also consult `docs/target-model.md`, the owner's directional product target — it guides destination and ordering but is not authoritative and never overrides the sources above.

**Working autonomously.** Within an assigned task, proceed without asking: branch, implement, run the validation sequence (it defines done for any code change), and open and revise pull requests. Stop only at owner gates — merges, foundation-weight decisions, large dependencies — and when two rules conflict in practice, stop and report the conflict rather than reconciling it silently. Autonomous milestone-delivering runs additionally follow `docs/goal-mode.md`.

**Domain spine.** `docs/domain-spine.md` mirrors the public API of the domain and application crates as bare declarations and is the owner's primary review surface. Any change to a public item in those crates updates the spine in the same pull request; CI checks its exported names and inventory counts against source.

**Finished pull requests.** The owner merges every pull request; deliver each one finished and awaiting owner merge:

- CI is green on the final commit.
- Every reviewer comment receives an in-thread reply, never deferred to a later wave: an actionable finding is fixed with the fixing commit named or declined with a stated reason; a question or informational comment is answered in-thread, no disposition recorded. Replies post when the wave's fix commit is pushed, or when the wave concludes without one; outdated threads are resolved at the same time, a replied thread a later fix commit or rebase outdates is resolved then, and a final sweep before declaring the pull request finished confirms no unresolved outdated thread remains.
- External reviews are re-requested on the final commit after any rebase.
- The description is at most 350 words, states the count of meaningfully changed lines (excluding lockfiles), and claims only what the code enforces — a contract binding future implementors is described as a contract, not an enforcement.
- Review-fix waves continue while the latest wave produced at least one accepted finding. One wave is one review pass and its disposition round — every actionable finding accepted and fixed or declined with its reason stated — whether or not any fix commit results; a quiet wave, whose review pass returns no actionable findings or whose findings are all declined, produces no accepted finding and concludes the loop: the pull request is finished and awaits owner merge. After five waves on one pull request, stop and escalate to the owner regardless of hit rate, reporting the wave history in one line.

**Stacked pull requests.** Stacks may grow as deep as the work requires; the owner merges in batches, so never wait on a merge to continue. Keep every stack linear and healthy:

- Each pull request targets the immediately preceding branch, and its diff is reviewed against that immediate base, not `main`.
- Verify a base branch still exists before stacking on it; when a base merges, fetch and retarget or rebase the remainder without discarding work.
- Open draft pull requests early so the stack is visible; mark each ready only after its own validation passes.
- Never force-push or rewrite a shared branch without first proving it necessary and safe; preserve owner-authored and externally added changes.

Every normative statement lives in exactly one place — an accepted ADR, a decision-log entry, a catalog row or scenario that is its own statement of record, or an implemented test — and other documents link to it rather than restating it. Do not restate content owned elsewhere in the overview documents (architecture, the invariant catalog's ADR-backed summaries, scenarios); a row or fixture that is itself the statement of record changes only together with the decision that authorizes the change.

Decisions have two weights. Ordinary decisions are made in the pull request and recorded as a dated entry in `docs/decisions.md` stating context, decision, rejected alternatives, and what it affects. Foundation-weight changes — altering accepted ADR semantics, moving a boundary between domain, storage, wire, or framework representations, weakening an invariant, introducing a technology that constrains several components, or closing an open question whose resolution has any of these effects — require an ADR under `docs/decisions/`, exercised against scenarios. Do not silently change a foundational decision or close a recorded open question. Keep domain types distinct from storage records, protocol messages, and framework types. Keep pull requests narrow and reviewable.

Tests reference the scenario and invariant identifiers they enforce when the connection is meaningful (for example `S12_INV011_rejects_stale_generation`, or a doc comment naming the invariant). When a test becomes the enforcement of an accepted invariant, link it from the invariant catalog's enforcement column in the same change. Test style rules live in `docs/testing-style.md`.

Dependencies are allowed when they provide clearer types or interfaces, replace code Signalbox would otherwise need to own, or supply another focused capability with a concrete benefit. Prefer small, narrowly scoped dependencies and explain their tradeoffs in the pull request. Before adding a large dependency with substantial transitive, build-time, runtime, or architectural cost, ask the user and wait for explicit approval.

Directly affected documentation may be updated in an implementation pull request to keep it accurate. Avoid unrelated rewording, cleanup, restructuring, or formatting.

Run the repository-wide validation sequence:

```bash
python3 scripts/check_domain_spine.py
cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps
cargo metadata --no-deps --format-version 1
git diff --check
```

For documentation changes, also check repository-relative links and review the rendered Markdown. Put future path-specific instructions in the nearest descendant `AGENTS.md`, scoped only to that subtree.

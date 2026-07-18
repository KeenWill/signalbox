# Agent guidance

Signalbox is in its design and foundation phase. Implementation is limited to mechanical workspace and tooling scaffolding plus narrowly scoped domain slices authorized by accepted decisions or explicit owner-approved plans; speculative product behavior is not permitted.

Authoritative starting points are `docs/architecture.md`, `docs/invariants.md`, `docs/decisions.md`, `docs/open-questions.md`, and accepted records under `docs/decisions/`. Accepted ADRs are the normative specification for decided semantics; executable tests become the enforcement of record as slices implement them. When selecting milestones, also consult `docs/target-model.md`, the owner's directional product target — it guides destination and ordering but is not authoritative and never overrides the sources above.

**Goal-mode operating rules.** Autonomous feature-building runs additionally follow the rules below.

**Milestone selection.** Milestones come from the priority order in `docs/target-model.md`: the earliest unfinished step whose blocking decisions are accepted, or that step's blocking decision proposed as the milestone. A milestone adds one coherent capability toward its step, and a new public domain type ships with a consumer in the same pull request or stack. Domain machinery for steps that cannot yet execute is frozen — in particular the fatal-mismatch and provider-evidence surface until model-call preparation exists.

**Spine maintenance.** `docs/domain-spine.md` mirrors the public API of the domain and application crates as bare declarations and is the owner's primary review surface. Any change to a public item in those crates updates the spine in the same pull request; the spine diff is the artifact the owner reviews.

**Finished pull requests.** The owner merges every pull request. A pull request is finished only when CI is green on its final commit; every reviewer-bot comment is triaged in-thread — fixed with the fixing commit named, or declined with a reason; external reviews are re-requested on the final commit after any rebase; outdated threads are resolved; and the description is at most 350 words, states the meaningful changed lines (excluding lockfiles), and claims only what the code enforces — a contract binding future implementors is described as a contract, not an enforcement. At most two review-fix waves; after that, stop and escalate to the owner.

**Stacking and hygiene.** Keep at most three pull requests unmerged from `main`, counting ones left open by earlier runs; at three, the run's first duty is finishing and merging them, not writing new code. Verify a pull request's base still exists before stacking on it, and retarget any pull request whose base branch has merged.

**Blockers over wrappers.** When the meaningful next step is owner-gated — a needed ADR, a dependency approval, an unclear priority — stop and report the gate. Delegating services, re-export batches, and polish of unconsumed machinery are not substitutes.

**Milestone check-ins.** A milestone is complete when all of its pull requests are merged; stop there and request an owner alignment review before starting the next. Do not roll from one milestone into the next autonomously.

Every normative statement lives in exactly one place — an accepted ADR, a decision-log entry, a catalog row or scenario that is its own statement of record, or an implemented test — and other documents link to it rather than restating it. Do not restate content owned elsewhere in the overview documents (architecture, the invariant catalog's ADR-backed summaries, scenarios); a row or fixture that is itself the statement of record changes only together with the decision that authorizes the change.

Decisions have two weights. Ordinary decisions are made in the pull request and recorded as a dated entry in `docs/decisions.md` stating context, decision, rejected alternatives, and what it affects. Foundation-weight changes — altering accepted ADR semantics, moving a boundary between domain, storage, wire, or framework representations, weakening an invariant, introducing a technology that constrains several components, or closing an open question whose resolution has any of these effects — require an ADR under `docs/decisions/`, exercised against scenarios. Do not silently change a foundational decision or close a recorded open question. Keep domain types distinct from storage records, protocol messages, and framework types. Keep pull requests narrow and reviewable.

Tests reference the scenario and invariant identifiers they enforce when the connection is meaningful (for example `S12_INV011_rejects_stale_generation`, or a doc comment naming the invariant). When a test becomes the enforcement of an accepted invariant, link it from the invariant catalog's enforcement column in the same change.

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

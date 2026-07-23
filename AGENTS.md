# Agent guidance

Signalbox is built in narrowly scoped slices authorized by recorded decisions or
explicit owner-approved plans; speculative product behavior is not permitted.

The normative surface is the living specification: the subsystem pages under
`docs/spec/` (implemented behavior), the invariant catalog `docs/invariants.md`
(laws, with INV-tagged tests as the enforcement of record), and
`docs/domain-spine.md` (public API shapes). `docs/decisions.md` is the
append-only record of recorded choices; `docs/open-questions.md` is the one home
of deferred design; `docs/scenarios.md` and `docs/testing-style.md` govern
scenarios and tests; `docs/architecture.md` orients but owns nothing. The
historical ADR corpus formerly under `docs/decisions/` is retired: its content
was distilled into `docs/spec/` (mapping in `docs/spec/README.md`), git history
is its archive, and it is not citable as current authority. When selecting
milestones, consult the priority order in `docs/target-model.md` — directional,
never overriding the sources above.

**Working autonomously.** Within an assigned task, proceed without asking:
branch, implement, run the validation sequence (it defines done for any code
change), and open and revise pull requests. Stop only at owner gates — merges,
foundation-weight decisions, large dependencies — and when two rules conflict in
practice, stop and report the conflict rather than reconciling it silently.
Autonomous milestone-delivering runs additionally follow `docs/goal-mode.md`.
Replacing or abandoning an open pull-request stack — closing its pull requests
in favor of a rewrite — is surfaced to the owner before the replacement lands,
never decided silently.

**Domain spine.** `docs/domain-spine.md` mirrors the public API of the domain
and application crates as bare declarations and is the owner's primary review
surface. Any change to a public item in those crates updates the spine in the
same pull request; CI checks its exported names and inventory counts against
source.

**Living specification.** A pull request that changes behavior described by a
`docs/spec/` page updates the owning section in the same pull request, exactly
as the spine rule above. Pages state implemented behavior only; deferred or
undecided work appears in a page's Open edges list and in
`docs/open-questions.md`, never as speculative prose.

**Finished pull requests.** The owner merges every pull request; deliver each
one finished and awaiting owner merge:

- CI is green on the final commit.
- Every reviewer comment receives an in-thread reply, never deferred to a later
  wave: an actionable finding is fixed with the fixing commit named or declined
  with a stated reason; a question or informational comment is answered
  in-thread, no disposition recorded. Replies post when the wave's fix commit is
  pushed, or when the wave concludes without one; outdated threads are resolved
  at the same time, a replied thread a later fix commit or rebase outdates is
  resolved then, and a final sweep before declaring the pull request finished
  confirms no unresolved outdated thread remains.
- External reviews are re-requested after a change that could alter what a
  reviewer already approved — code, tests, normative documentation or
  specifications, contract-bearing comments, or claims in the description — not
  after one that leaves a previously green state effectively unchanged (a
  rename, non-semantic comment-only edits, or a merge of `main` with no
  meaningful interaction). Codex runs only on an explicit `@codex review`
  comment.
- The description is at most 350 words, states the count of meaningfully changed
  lines (excluding lockfiles), and claims only what the code enforces — a
  contract binding future implementors is described as a contract, not an
  enforcement.
- Review-fix waves continue while the latest wave produced at least one accepted
  finding. One wave is one review pass and its disposition round — every
  actionable finding accepted and fixed or declined with its reason stated —
  whether or not any fix commit results; a quiet wave, whose review pass returns
  no actionable findings or whose findings are all declined, produces no
  accepted finding and concludes the loop: the pull request is finished and
  awaits owner merge. After five waves on one pull request, stop and escalate to
  the owner regardless of hit rate, reporting the wave history in one line. A
  re-report of an already-fixed finding made against a stale head is declined by
  standing policy, naming the fixing commit. When a wave's accepted findings are
  predominantly defects in code the previous wave's fixes introduced, stop and
  escalate to the owner instead of continuing the loop.

**Stacked pull requests.** Stacks may grow as deep as the work requires; the
owner merges in batches, so never wait on a merge to continue. Keep every stack
linear and healthy:

- Each pull request targets the immediately preceding branch, and its diff is
  reviewed against that immediate base, not `main`.
- Verify a base branch still exists before stacking on it; when a base merges,
  fetch and retarget or rebase the remainder without discarding work.
- Open draft pull requests early so the stack is visible; mark each ready only
  after its own validation passes.
- Never force-push or rewrite a shared branch without first proving it necessary
  and safe; preserve owner-authored and externally added changes.

Every normative statement lives in exactly one place — the owning `docs/spec/`
page, a decision-log entry, an invariant row, a scenario that is its own
statement of record, or an implemented test — and other documents link to it
rather than restating it. Do not restate content owned elsewhere in overview
documents; a row or fixture that is itself the statement of record changes only
together with the decision that authorizes the change.

Decisions have two weights. Ordinary decisions — a new dependency, a provisional
parameter or limit, a policy or process change, a migration choice the
specification does not already fix — are made in the pull request and recorded
as a dated entry in `docs/decisions.md` stating context, decision, rejected
alternatives, and what it affects. Implementing behavior the specification and
recorded decisions already fix is not itself a decision; the pull-request
description and the spec update are its record. Foundation-weight changes —
changing normative semantics in a `docs/spec/` page beyond recording behavior
the same stack implements, moving a boundary between domain, storage, wire, or
framework representations, weakening an invariant, introducing a technology that
constrains several components, or closing an open question whose resolution has
any of these effects — are proposed as a specification diff reviewed at the
bottom of the implementing stack, with a `docs/decisions.md` entry recording the
choice; owner merge is acceptance. Do not silently change a foundational
decision or close a recorded open question. Keep domain types distinct from
storage records, protocol messages, and framework types. Keep pull requests
narrow and reviewable.

Tests reference the scenario and invariant identifiers they enforce when the
connection is meaningful (for example `S12_INV011_rejects_stale_generation`, or
a doc comment naming the invariant). When a test becomes the enforcement of an
accepted invariant, link it from the invariant catalog's enforcement column in
the same change. Test style rules live in `docs/testing-style.md`.

Dependencies are allowed when they provide clearer types or interfaces, replace
code Signalbox would otherwise need to own, or supply another focused capability
with a concrete benefit. Prefer small, narrowly scoped dependencies and explain
their tradeoffs in the pull request. Before adding a large dependency with
substantial transitive, build-time, runtime, or architectural cost, ask the user
and wait for explicit approval.

Directly affected documentation may be updated in an implementation pull request
to keep it accurate. Avoid unrelated rewording, cleanup, restructuring, or
formatting.

Run the repository-wide validation sequence:

```bash
python3 scripts/check_domain_spine.py
cargo fmt --all -- --check
mdformat --check *.md docs/
cargo check --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets --all-features
cargo test --workspace --all-features --doc
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps
cargo metadata --no-deps --format-version 1
git diff --check
```

Repository tool commands such as mdformat run inside the devenv environment
(`devenv shell` to enter it, or one-off as
`devenv shell -- mdformat --check *.md docs/`), never via system or Homebrew
binaries: a plugin-less mdformat silently corrupts GFM tables. The environment
installs the pinned toolchain CI uses from `tooling/requirements-mdformat.txt`;
drop `--check` to rewrap in place. Wrapping rules live in `.mdformat.toml`.

For documentation changes, also check repository-relative links and review the
rendered Markdown. Put future path-specific instructions in the nearest
descendant `AGENTS.md`, scoped only to that subtree.

# Agent guidance

Signalbox is built in narrowly scoped slices authorized by implemented contracts
or explicit owner-approved plans; speculative product behavior is not permitted.

**Documentation discipline.** A document earns its place only when it tells a
reader who has the code something the code cannot say. Public API type shapes
belong in `docs/domain-spine.md`, which is mechanically checked. Invariants
belong in INV-tagged tests that fail; `docs/invariants.md` is only their
generated index. Decisions belong in pull-request descriptions and git history.
Prose under `docs/spec/` is reserved for cross-crate and wire contracts:
agreements between components or across versions that no single crate can state.
Deferred design has one home in `docs/open-questions.md`; `docs/scenarios.md`
and `docs/agents/testing-style.md` govern scenarios and tests; `docs/style.md`
governs literal provenance and label discipline in production and test code
alike; `docs/architecture.md` orients but owns nothing. The retired ADR corpus
and decision ledger remain available in git history and are not citable as
current authority. When selecting milestones, consult the priority order in
`docs/target-model.md` — directional, never overriding the sources above.

**Public-source hygiene.** Repository content — code, documentation, commit
messages, pull-request text, branch names — cites only public sources. Never
include or allude to non-public material — internal systems, documents, or
vocabulary of any organization — regardless of origin. Citing public open-source
work is fine regardless of publisher. References to the owner's own private
repositories are permitted as provenance notes, never as normative sources.

**Working autonomously.** Within an assigned task, proceed without asking:
branch, implement, run the validation sequence (it defines done for any code
change), and open and revise pull requests. Stop only at owner gates — merges,
foundation-weight decisions, large dependencies — and when two rules conflict in
practice, stop and report the conflict rather than reconciling it silently.
Autonomous milestone-delivering runs additionally follow
`docs/agents/goal-mode.md`. Replacing or abandoning an open pull-request stack —
closing its pull requests in favor of a rewrite — is surfaced to the owner
before the replacement lands, never decided silently. Defer-and-record applies
only to a pre-existing defect encountered outside the assigned change; an agent
must fix every defect it introduces.

**Domain spine.** `docs/domain-spine.md` mirrors the public API of the domain
and application crates as bare declarations and is the owner's primary review
surface. Any change to a public item in those crates updates the spine in the
same pull request; CI checks its exported names and inventory counts against
source.

**Living specification.** A pull request that changes behavior described by a
`docs/spec/` page updates the owning section in the same pull request, exactly
as the spine rule above. Every paragraph on a page belongs to exactly one of
three categories, and a page that cannot say which a paragraph is has a defect:

- **Implemented behavior**, which is what a page states by default.
- **Committed unimplemented functionality** — a capability the owner has decided
  will exist, recorded because it constrains what present change may do. Such a
  paragraph names itself unimplemented, states that no present surface provides
  it, and carries only that compatibility constraint. It is neither a
  description of the system nor an open question, and it is admitted only where
  a present contract must stay compatible with it.
- **Deferred or undecided work**, recorded in `docs/open-questions.md` — its one
  home — with a page's Open edges list carrying pointers to it, never
  substantive speculative prose.

The distinction that matters is force, not tense: committed functionality binds
a future change, an open question binds nobody, and conflating them either
invites an implementer to build against an undecided design or lets a decision
be quietly relitigated.

Within a stack, the bottom specification diff satisfies this rule for the
behavior its child pull requests implement; a child adds its own spec edit only
for behavior the bottom diff does not describe.

**Finished pull requests.** The owner merges every pull request; deliver each
one finished and awaiting owner merge:

- CI is green on the final commit. An owner merge is terminal: a merged pull
  request is converged, and no agent waits for another verdict on landed work.
- The pull request is open ready for review, which is how every pull request is
  opened. Draft is only for a branch being force-iterated right now, and the
  agent that marked it draft marks it ready again before that session ends; a
  draft left behind is a defect, not a holding state, and pending validation is
  never a reason to leave one (owner ruling 2026-08-25, after finding opened
  drafts nobody ever flipped). Validation gates the review request, not the
  draft flag.
- Every reviewer comment receives an in-thread reply, never deferred to a later
  wave. For an accepted finding, push the commit or commits that resolve it,
  then reply naming the fixing commit or commits. For a declined finding, reply
  with the reason either immediately or during that pull request's disposition
  round. Answer a question or informational comment in-thread without recording
  a disposition. After a wave's fixes are pushed, complete every reply and
  resolve eligible threads before moving to another pull request, propagating a
  stack, or requesting another review wave. Resolve outdated threads at the same
  time; resolve a replied thread when a later fix commit or rebase outdates it.
  A final sweep before declaring the pull request finished confirms no
  unresolved outdated thread remains.
- A review request is posted only once CI is green on the exact head it names,
  and that head SHA is named in the request. A wave's fixes are never pushed
  while a requested review on a prior head is still in flight; let the in-flight
  review land, push the fixes for its accepted findings, then complete its
  disposition (replies naming the pushed fixing commits) before requesting the
  next wave against the new head. A pull request whose diff touches only files
  that already opened with the non-authoritative-planning banner on its base
  branch (for example [`docs/agents/backlog.md`](docs/agents/backlog.md)) and
  that still open with it at the pull request's head, or that it adds as new
  files opening with that banner, gets no review request at all: those files
  decide nothing and are reviewed for nothing. Stamping the banner onto a file
  that already existed without one — or removing it from a file that carried it
  — is a reclassification, not an exemption: either pull request is requested
  and reviewed like any other.
- Before the first review request, run the pre-push self-review checklist over
  the pull request's own diff — the first-wave findings agents can catch
  themselves:
  - grep the whole body of every test the diff adds or modifies — under
    `#[test]`, `#[tokio::test]`, or any other test attribute, including the
    lines the diff itself leaves untouched — for `for`/`while` loops and
    `if`/`match` conditionals, and unroll or split them per
    `docs/agents/testing-style.md` rule 2 before pushing;
  - confirm each added assertion compares against a fixture accessor, not a
    literal re-encoding a value the fixture already states (rule 6);
  - confirm every new INV-tagged test is discoverable from its test name or
    attached doc comment, then regenerate
    [`docs/invariants.md`](docs/invariants.md) with
    `python3 scripts/generate_invariants.py --write`;
  - re-verify any count or inventory the diff states against the current head;
  - when the diff changes what a `docs/spec/` page states as implemented
    behavior, re-verify that behavior against the head and advance the page's
    verified-against-ref line; a wording, link, or formatting edit that changes
    no stated behavior leaves the reference alone, since the reference records
    verification against code and not the fact of an edit.
- External reviews are re-requested only after a semantically meaningful diff
  that could alter what a reviewer approved: code, tests, normative documents,
  or claims in the description. A source-comment-only diff does not trigger a
  re-request even when the comment describes a contract. Never re-request after
  a clean merge-forward, a rename, or any comment-only edit. Codex runs only on
  an explicit `@codex review` comment.
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
  awaits owner merge. Wave five is the ordinary cap. An agent may take exactly
  one extension of up to three further waves without asking only when the
  preceding wave produced an accepted finding and none of that wave's findings
  was a defect introduced by an earlier wave's fix. Otherwise it stops at the
  cap and dispositions any later automatic finding as "Escalated without
  disposition". The extension is a hard stop after wave eight; no further review
  is requested. Report the per-wave history in the one-line form goal mode
  requires. The wave count is relative to the base that was reviewed. A forward
  merge that materially changes the code under review and therefore the pull
  request's diff resets the counter because earlier hit rates describe code that
  is no longer under review; merely advancing the base does not. The reset never
  weakens the regression rule: a finding caused by an earlier wave's own fix
  still counts as a regression regardless of any reset. When a pull request's
  open-thread count exceeds roughly fifty, a wave's replies may batch, but a
  reply-and-resolve sweep across every open thread is mandatory before the pull
  request is declared finished. A re-report of an already-fixed finding made
  against a stale head is declined by standing policy, naming the fixing commit;
  a finding materially identical to one dispositioned in a prior wave is a
  re-raise, declined by the same standing policy with a link to the prior
  thread. Neither standing decline applies to a finding that reproduces on the
  current head: a defect reintroduced by a later wave's edits is live and is
  dispositioned on its merits. If a wave finds a defect introduced by one of
  this effort's own earlier fix waves, fix it in that wave's own disposition
  round — an agent's own regression is always must-fix and never grounds to stop
  (owner ruling 2026-07-31, after three same-night escalations each ended in the
  identical authorization). The cap and the extension gate govern only whether a
  further review is requested, never whether that fix is made, so a
  self-regression found at the cap or at the wave-eight hard stop is still fixed
  and the pull request then finishes without another review. Stop and escalate
  to the owner only when the defect was introduced by a different effort's fix,
  because repairing it means editing work someone else owns.

**Stacked pull requests.** Stacks may grow as deep as the work requires; the
owner merges in batches, so never wait on a merge to continue. Keep every stack
linear and healthy:

- Each pull request targets the immediately preceding branch, and its diff is
  reviewed against that immediate base, not `main`.
- Verify a base branch still exists before stacking on it; when a base merges,
  fetch and retarget or rebase the remainder without discarding work.
- Open pull requests early so the stack is visible, ready for review rather than
  as drafts, and request each one's review only after its own validation passes.
- Never force-push or rewrite a shared branch without first proving it necessary
  and safe; preserve owner-authored and externally added changes.

Every normative statement has exactly one owner. Normative system prose lives in
its cross-component or versioned-wire contract, while invariants live in their
INV-tagged tests. Repository process rules live in `AGENTS.md` or the process
document it names as their owner; scenarios and fixtures may themselves be the
statement of record. Other documents link to an owner rather than restating it,
and an owning scenario or fixture changes only with the owner-approved change
that authorizes it. Raising a hard safety ceiling requires a reviewed code
change with a test and rationale.

Ordinary implementation choices are made in the pull request and remain durable
in its description and git history. Foundation-weight changes — changing
normative cross-crate or wire semantics, moving a boundary between domain,
storage, wire, or framework representations, weakening an invariant, introducing
a technology that constrains several components, or closing an open question
with any of those effects — are proposed as a specification diff at the bottom
of the implementing stack; owner merge is acceptance. A foundation spec diff
describes behavior its stack implements and merges only with that stack, so
`main` never carries prose describing unimplemented behavior as implemented.
Committed unimplemented functionality, per the three categories above, is the
one admitted exception, and it is not a weakening of this rule: it describes no
behavior and states only the compatibility constraint it places on future
change. Do not silently change a foundational contract or close a recorded open
question. Keep domain types distinct from storage records, protocol messages,
and framework types. Keep pull requests narrow and reviewable.

**Pre-alpha compatibility.** Signalbox is pre-alpha: no production instance
exists, and every daemon and client is rebuilt at will from this tree. Wire
formats, schema types, and stored values change freely to their correct current
shape. Machinery protecting an old deployed version — compatibility shims,
dual-read or dual-write paths, version-tolerant decoding, legacy-value aliases,
data-upgrade scaffolding — protects deployments that do not exist and is a
defect, not prudence: neither add it nor request it in review (owner ruling
2026-08-11). The only compatibility work owed is a one-time migration carrying a
live database across a change that invalidates its stored data. Compatibility an
owning implemented contract already retains — the durable-command
`storage_version` decoding in
[persistence-protocol](docs/spec/persistence-protocol.md), the credential
`migration_backfill` fallback in
[configuration-and-credentials](docs/spec/configuration-and-credentials.md) —
stands until a migration or the contract's own sunset retires it. That surface
only narrows: a change retires tolerated shapes rather than adding them,
migrating stored data where its owning contract permits — append-only command
records, which [identity-and-commands](docs/spec/identity-and-commands.md)
forbids rewriting, keep their readers until an authorized change to that
contract retires them. This rule is rescinded at the first durable deployment
identified by the freeze condition in
[process-protocol](docs/spec/process-protocol.md); compatibility policy is then
decided explicitly.

Tests reference the scenario and invariant identifiers they enforce when the
connection is meaningful (for example `S12_INV011_rejects_stale_generation`, or
a doc comment naming the invariant). When a test becomes the enforcement of an
accepted invariant, give it the INV tag in its name or attached doc comment and
regenerate the invariant index in the same change. Test style rules live in
`docs/agents/testing-style.md`; the literal-provenance and label discipline
binding production and test code alike lives in `docs/style.md`; the process
documents that govern how agents work on the repository are collected under
`docs/agents/`.

Dependencies are allowed when they provide clearer types or interfaces, replace
code Signalbox would otherwise need to own, or supply another focused capability
with a concrete benefit. Prefer small, narrowly scoped dependencies and explain
their tradeoffs in the pull request. Before adding a large dependency with
substantial transitive, build-time, runtime, or architectural cost, ask the user
and wait for explicit approval.

Directly affected documentation may be updated in an implementation pull request
to keep it accurate. Avoid unrelated rewording, cleanup, restructuring, or
formatting.

**Operational traps.** Do not adopt the review-slog toolkit as a merge gate
until its
[blocking condition](docs/open-questions.md#review-slog-toolkit-adoption) is
cleared.

Run `git worktree list` before working with another checkout of this clone.
Never check out a branch by name in a second worktree: worktrees share branch
refs, so a commit can move another worktree's HEAD and leave its index showing
spurious staged deletions. Use `git checkout --detach origin/<branch>` and
`git push origin HEAD:<branch>`.

Always pass `--no-fail-fast` to Cargo test runs. The default can let one early
flaky failure truncate the remaining tests and hide another defect. The ignored
persistence integration suite must include its feature:
`cargo test --no-fail-fast -p signalbox-persistence --features postgres-integration --tests -- --ignored`;
without `--features postgres-integration` it runs zero tests and exits
successfully.

These suites start one PostgreSQL container per test through testcontainers,
whose Rust client ships no Ryuk reaper: a container is removed by
`ContainerAsync`'s `Drop` and by nothing else, and it is created with
`AutoRemove: false`, so a test process that dies without unwinding strands every
container it started. A stranded container's database state is the bounded
RAM-backed tmpfs `signalbox_persistence::disposable_postgres_state_tmpfs`
mounts, so until removal it pins host memory rather than disk. Nothing
in-process reclaims those — the client's optional `watchdog` feature is left off
deliberately, because it `expect`s every stop and removal and so panics its
background thread on the first error, which both abandons the containers it had
not reached and skips re-raising the signal, leaving a process that no longer
dies on SIGTERM. Reclaim what an interrupted run leaves behind with
[`tooling/sweep-test-containers.sh`](tooling/sweep-test-containers.sh), which
removes containers past an age bound — two hours by default, far above any
suite's runtime — together with their volumes. It reports what it would remove
and changes nothing until passed `--apply`. On a shared machine, run it on a
timer: an interrupted run is the case nothing in-process converts, so a periodic
sweep is what bounds the leak there rather than any code change.

The sweep selects positively, on the label
`signalbox_persistence::disposable_test_container_labels` attaches to every
container this repository's suites start, and on nothing else — not the image,
not the global testcontainers label, and not a list of names to spare. A
container that carries no such label is another party's to remove, so a sweep on
a shared daemon leaves it alone whatever it is running. Start a test container
through that helper, or the sweep will not reclaim it.

The mark is safe only because a marked container is short-lived, so anything
that can be configured to hold one for longer — the load benchmark's
`--duration-seconds`, say — refuses a setting that would outlive
`signalbox_persistence::DISPOSABLE_TEST_CONTAINER_LIFETIME_HOURS`, which is the
same bound the sweep defaults to.

CI runs these ignored suites from
[`.github/postgres-integration-suites.toml`](.github/postgres-integration-suites.toml),
which names each suite's package, features, shard count, and exclusions. Both
`.github/workflows/rust.yml` and `scripts/check_docs_consistency.py` read that
manifest instead of restating it, and the docs-consistency check fails when the
manifest, the workflow, and the command documented above disagree. Change a
suite there, not in the workflow.

[`.github/workflows/README.md`](.github/workflows/README.md) owns the CI
runner-routing contract: which runner every workflow job uses, and why. Change a
job's `runs-on` and that file in the same pull request.

Size validation to the change. A documentation-only change runs the
documentation bar below; a change to code, tests, dependency manifests,
generated contracts, or any other semantic surface runs the complete bar. CI is
authoritative and is the backstop.

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

Repository tool commands such as mdformat run inside the devenv environment
(`devenv shell` to enter it, or one-off as
`devenv shell -- mdformat --check *.md docs/`), never via system or Homebrew
binaries: a plugin-less mdformat silently corrupts GFM tables. The environment
installs the pinned toolchain CI uses from `tooling/requirements-mdformat.txt`;
drop `--check` to rewrap in place. Wrapping rules live in `.mdformat.toml`.

Cargo invocations inside the devenv environment use sccache as a shared compiler
cache (`devenv.nix` sets `RUSTC_WRAPPER`): dependency compilation is cached once
per machine, in sccache's per-user default cache directory (override with
`SCCACHE_DIR`), and reused across checkouts and worktrees, while workspace
crates keep incremental compilation. Continuous integration does not enter this
environment and does not use sccache; its caching is configured in
`.github/workflows/rust.yml`. To bypass the cache for a single command, clear
the wrapper: `RUSTC_WRAPPER= cargo <args>`.

For documentation changes, also check repository-relative links and review the
rendered Markdown. Put future path-specific instructions in the nearest
descendant `AGENTS.md`, scoped only to that subtree.

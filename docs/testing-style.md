# Testing style

This document owns test style: how a test body reads, how fixtures are shaped, what an assertion may reference, and how snapshot (expect) tests are used. The [testing section of CONTRIBUTING.md](../CONTRIBUTING.md#testing) owns what to test — the expected layers, determinism requirements, and merge gates — and is not restated here. Naming stays as [AGENTS.md](../AGENTS.md) states it: tests reference the scenario and invariant identifiers they enforce when the connection is meaningful.

The numbered rules are normative for new and modified tests; cite them by number in review. Apply them to existing tests only when already changing those tests for another reason. Every bad→good rewrite below is condensed from a real diff in this repository's domain or application test sweep — identifiers are shortened for the page, not invented.

## Fixtures and assertions

1. **A test's body contains its whole story.** A reader determines what is verified, with which inputs, and why the expected result is correct from the test body alone: complete (everything needed to understand the result is present) and concise (nothing else is). ([Software Engineering at Google, ch. 12](https://abseil.io/resources/swe-book/html/ch12.html))

2. **Tests are verified by inspection, not by tests of tests.** Test bodies are straight-line code: no loops, no conditionals, and no expectation recalculated by logic mirroring the code under test. An expected value is a hardcoded literal or a value the fixture already states (rule 6). Logic that must exist moves into a helper, and a nontrivial helper gets its own tests. ([Don't put logic in tests](https://testing.googleblog.com/2014/07/testing-on-toilet-dont-put-logic-in.html))

3. **DAMP over DRY.** Duplication between tests is fine when it helps each test read on its own; extract only plumbing that is irrelevant to the behavior under test. ([Tests too DRY? Make them DAMP](https://testing.googleblog.com/2019/12/testing-on-toilet-tests-too-dry-make.html))

4. **One meaningful knob per fixture.** Fixture constructors and builders carry canonical defaults; each test states only the values the behavior depends on, at the call site — never a helper taking five positional integers whose meanings live at the definition. Where a fixture also needs an identity seed, derive it from the one knob instead of adding a second free integer — decorrelated (for example, descending as the knob ascends), so an implementation reading identity where it should read the knob's value cannot accidentally pass. When the independence of two values is itself the behavior under test, the fixture takes both as named knobs.

5. **A test that cares about a value states it.** State a value the test depends on explicitly even when the canonical default happens to match; a test's meaning must not depend on defaults defined elsewhere.

6. **Assert against the fixture, not re-derived constants.** Write `assert_eq!(chosen.origin(), earlier.accepted_input_id())`, never `accepted_input_id(20)` re-encoding a magic seed from the setup. Fixture-based assertions follow the setup when it changes; re-encoded constants silently diverge from it.

7. **One behavior per test, named for the behavior.** The repository's `sNN_invNNN_...` naming convention already does this; keep it. A test that needs "and" in its description is two tests.

8. **Judge every test as a classifier.** For each test, name the real bug it would catch and the false alarm it could raise. The ideal test fails only when a requirement changes; a test that fails on behavior-preserving refactors is a cost, not a safety net. ([Test suites as classifiers](https://blog.nelhage.com/post/test-suites-as-classifiers/))

### Rewrites from the test sweeps

Rule 2 — a loop over same-behavior cases unrolls into named straight-line calls (domain sweep, `turn_attempt.rs`):

```rust
// Bad: three cases share one anonymous failure site inside a loop.
for current in [running(), cancellation_stopped(), fatal_stopped()] {
    let error = current.clone().begin_running().unwrap_err();
    assert_eq!(error.into_parts(), (current, AttemptedTransition::BeginRunning));
}

// Good: unrolled onto a #[track_caller] check helper (rule 16); a
// failure names the state that caused it.
assert_begin_running_rejects_unchanged(running());
assert_begin_running_rejects_unchanged(cancellation_stopped());
assert_begin_running_rejects_unchanged(fatal_stopped());
```

Rules 4 and 5 — a facts struct with a canonical `matching` baseline turns eight positional arguments into one named perturbation (domain sweep, `session.rs`):

```rust
// Bad: which of the eight arguments is the perturbed one?
let error = SessionReconstitutionInput::new(
    session_id(2), session_id(1), provenance, session_id(1),
    first, session_id(1), first, defaults(3),
).reconstitute().unwrap_err();
assert_eq!(error.failure(), Failure::RequestedSessionMismatch);

// Good: the test perturbs exactly the named fact it cares about.
let requested_other_session = reconstitution_failure(CurrentSessionFacts {
    requested_session: session_id(2),
    ..CurrentSessionFacts::matching(session_id(1))
});
assert_eq!(requested_other_session, Failure::RequestedSessionMismatch);
```

Rule 4 — an argument every call repeats identically is a canonical default, not a parameter (application sweep, `submit_input.rs`):

```rust
// Bad: a second free argument the behavior never depends on.
let request = request(1, "hello");

// Good: one knob — the command-identity seed; the fixture's doc comment
// states the canonical session, content, and delivery it carries.
fn request(command: u128) -> SubmitInputRequest { /* canonical defaults */ }
let request = request(1);
```

Rule 5 — meaningful emptiness is named, not spelled `[]` (application sweep, `submit_input.rs`):

```rust
// Bad: [] could mean "responds with nothing"; the reader cannot tell.
let service =
    SubmitInputService::new(FakeIds::new([], []), FakeTransaction::returning([]));

// Good: the emptiness is the point — any mint or handling call panics.
let service = SubmitInputService::new(
    FakeIds::expecting_no_calls(),
    FakeTransaction::expecting_no_calls(),
);
```

Rule 6 — the expectation follows the fixture it came from (application sweep, `load_session.rs`):

```rust
// Bad: 4 re-encodes the fixture's version seed; renumber the setup and
// this assert silently pins the stale value.
assert_eq!(loaded.current_configuration_defaults().version().as_u64(), 4);

// Good: fixture-based, so it moves with the setup.
assert_eq!(
    loaded.current_configuration_defaults().version(),
    current.current_configuration_defaults().version()
);
```

## Expect tests

Snapshot assertions use [`expect-test`](https://github.com/rust-analyzer/expect-test), a domain-crate dev-dependency arriving with its first adopting tests. `UPDATE_EXPECT=1 cargo test` re-blesses snapshots in place.

9. **Use expect tests where the value's shape is the assertion:** matrix outcomes, derived orders and projections, and error `Display` output. Prior art: Jane Street's [expect-test workflow](https://blog.janestreet.com/the-joy-of-expect-tests/) and [testing with expectations](https://blog.janestreet.com/testing-with-expectations/); [How to Test](https://matklad.github.io/2021/05/31/how-to-test.html) describes the single check-function, data-driven form these settle into.

10. **Snapshots supplement invariant enforcement; they never replace it.** A test linked from an enforcement column in [the invariant catalog](invariants.md) keeps its precise targeted asserts; a snapshot proves output-didn't-change, not invariant-holds.

11. **Never bless a diff you haven't read.** Review a snapshot update with the same care as a code change; the snapshot diff is the review surface.

12. **Curate snapshots for the reader.** Deterministic ordering, relevant fields only, rendered from the observed value under test. A snapshot of everything asserts nothing and degenerates into a [change-detector test](https://testing.googleblog.com/2015/01/testing-on-toilet-change-detector-tests.html). Table-shaped output in domain unit tests uses the `table` helper in that crate's `test_support` module (arriving with the expect-test adoption): pipe-separated, left-aligned, right-trimmed lines that stay byte-stable under re-blessing. Another test crate's first table-shaped snapshot lifts that helper into test support it can import instead of hand-rolling a variant. Prior art for tables in expect tests: [expectable](https://github.com/janestreet/expectable).

Rules 2, 9, and 10 — a matrix whose expectation mirrors the code becomes a decisive assert plus an expect table (domain sweep, `turn_attempt.rs`):

```rust
// Bad: the expectation recalculates the transition rule under test.
for d in all_cancellation_dispositions() {
    assert_eq!(prepared().end_after_cancellation(proof(1), d).is_ok(), d == Cancelled);
}

// Good: the decisive accepting edge stays targeted; the grid is displayed.
assert!(prepared().end_after_cancellation(proof(1), Cancelled).is_ok());
expect![[r#"
    attempted end                               | outcome
    ------------------------------------------- | --------
    after cancellation (exact proof): Cancelled | ends
    after cancellation (exact proof): Ambiguous | rejected
"#]]
.assert_eq(&table(&["attempted end", "outcome"], &rows));
```

## Laws versus values

13. **Targeted asserts state laws; snapshots state values.** A targeted assert expresses a relation between observed values — equality, replay stability, identity preservation, terminal irreversibility, order-independence. A snapshot cannot state a relation; it can only display both sides and leave the comparison to the reader. Use each instrument for what it can state: a law gets an assert that fails when the relation breaks, a value gets a snapshot that shows what it is. This distinction is the rationale behind rules 9–12 — a snapshot proves output-didn't-change precisely because output is all it states, which is why rule 10 keeps the law asserts and rule 9 sends value-shaped claims to expect tests.

## Snapshot supplements and blessing

14. **Prefer supplementary snapshots going forward.** On an error or rejection path, printing the complete error payload as a supplement to the decisive assert catches unintended changes no assert mentions; a transition-result snapshot alongside the law asserts documents the full result for the reader. Prefer this shape in new tests; existing tests are not obligated to retrofit it.

15. **A pretty-printer owes the reader; a blessing owes the reviewer.** Rendering helpers owe deterministic ordering, relevant fields only, and right-trimmed lines — a printer that cannot promise byte-stability under re-blessing is not finished. A bulk re-bless is reviewed row-by-row, never skimmed: an unread blessing is exactly the [change-detector](https://testing.googleblog.com/2015/01/testing-on-toilet-change-detector-tests.html) failure mode that rule 11 exists to prevent — the suite keeps passing while its meaning drifts.

## Check helpers

16. **A check helper hides plumbing, never meaning.** Mark it `#[track_caller]` so a failure names the call site, not the helper's interior. It may absorb service wiring, cloning, and error unwrapping; it never absorbs a behavior-relevant value — those stay at the call site. Its name states what it checks (`assert_begin_running_rejects_unchanged`, not `check_case`), and a helper containing logic — branching, rendering, computation — gets its own tests (rule 2).

From the application sweep, `submit_input.rs`:

```rust
// Bad: the shapes under test and the plumbing share one loop body.
for result in results { /* build service, execute, assert pass-through */ }

// Good: the helper hides service plumbing only; each behavior-relevant
// shape stays at its own call site.
assert_recorded_result_passes_through(SubmitInputResult::Rejected(
    SubmitInputRejectedResult::SessionNotFound { session: session_id(2) },
));
assert_recorded_result_passes_through(SubmitInputResult::Rejected(
    SubmitInputRejectedResult::NoActiveTurn {
        session: session_id(2),
        expected_active_turn: turn_id(6),
    },
));
```

## Split versus unroll

17. **A loop leaves a test body one of two ways.** Few cases exercising one behavior unroll in place into straight-line calls (rule 2); cases exercising distinct behaviors split into separately named tests — one behavior per test (rule 7). Before renaming or splitting any test, grep [the invariant catalog](invariants.md) for citations: cited names are stable identifiers, preserved as-is or updated in the catalog in the same change.

From the application sweep, `replace_session_defaults.rs` — two behaviors, so a split, not an unroll:

```rust
// Bad: two behaviors iterate behind one name.
fn s01_inv008_inv012_recorded_applied_and_rejected_results_pass_through() {
    for (command, recorded) in [(applied_cmd, applied), (rejected_cmd, rejected)] { /* … */ }
}

// Good: one behavior per test, each named for its behavior.
fn s01_inv008_inv012_recorded_applied_result_passes_through() { /* … */ }
fn s01_inv008_inv012_recorded_rejected_result_passes_through() { /* … */ }
```

## Fixture and helper placement

18. **Helpers live where their meaning lives.** Module-local helpers sit next to the tests that read them; identity constructors used across a crate's test modules stay in that crate's `test_support`. A one-knob constructor is named for the domain concept it produces — `accepted_ordinary(acceptance)`, `pending_steering(source_turn)` — never for its mechanics.

## What not to test

19. **Delete tests that cannot fail meaningfully.** A test that restates field storage (construct, then read every getter back), a tautological `matches!` over the code's own values, or a change-detector snapshot of incidental structure adds false-alarm cost without catching bugs. Judge every candidate as rule 8 judges it: if no real bug makes it fail and a behavior-preserving refactor might, it is a cost, not a safety net.

From the domain sweep, `turn_attempt.rs`:

```rust
// Bad: constructs a value, then matches it against itself.
let end = AttemptEnd::WithoutStop { disposition };
assert!(matches!(end, AttemptEnd::WithoutStop { disposition: actual }
    if actual == disposition));

// Good: the retained cause is read back from the observed value and
// displayed where a reviewer can see it (rule 12).
expect![[r#"
    family             | disposition | retained cause
    ------------------ | ----------- | -------------------
    without stop       | Lost        | -
    after cancellation | Cancelled   | interrupt command 1
"#]]
.assert_eq(&attempt_end_family_table(&ends));
```

## Failure messages

20. **A subtle law fails with its name attached.** When a targeted assert guards a law a maintainer could plausibly "fix" away — replay stability, identity preservation, terminal irreversibility — prefer an assert form or message that names the violated expectation: matklad's "artisanally crafted error message" ([How to Test](https://matklad.github.io/2021/05/31/how-to-test.html)). `expect_err("cross-wired current-session facts must fail closed")` teaches at the failure site; a bare `unwrap()` teaches nothing.

## Example

A queue-order derivation test written against multi-positional-integer helpers, violating rules 4 and 6:

```rust
let position = positions(3);

assert_eq!(
    derive_accepted_input_total_order([
        ordinary(1, position[0]),
        ordinary(2, position[1]),
        interrupt(3, position[2], 1),
    ]),
    Ok(vec![turn_id(1), turn_id(3), turn_id(2)])
);
```

The reader must know that `interrupt`'s trailing `1` re-encodes the first `ordinary` call's turn seed, and that `turn_id(3)` re-derives the interrupt's identity from another magic integer. The one-knob rewrite keeps a single meaningful parameter — the acceptance ordinal — and derives the identity seed from it:

```rust
/// Ordinary work accepted at the given ordinal; its turn seed derives from
/// it, decorrelated per rule 4.
fn accepted_ordinary(acceptance: u64) -> AcceptedInputQueueWork { /* … */ }

/// Interrupt work accepted at the given ordinal, immediately after the
/// exact predecessor fixture.
fn accepted_interrupt(
    acceptance: u64,
    predecessor: AcceptedInputQueueWork,
) -> AcceptedInputQueueWork { /* … */ }
```

The same behavior, stated at the call site and asserted against the fixtures:

```rust
let first = accepted_ordinary(1);
let second = accepted_ordinary(2);
let interrupt = accepted_interrupt(3, first);

assert_eq!(
    derive_accepted_input_total_order([first, second, interrupt]),
    Ok(vec![first.turn(), interrupt.turn(), second.turn()])
);
```

Each fixture call states exactly one acceptance ordinal — its identity seed derives from it, decorrelated so a derivation ordering by identity instead of acceptance cannot pass — the interrupt relation names the predecessor fixture itself, and the expected order is spelled in fixture values, so the assertion cannot silently diverge from the setup.

Because this test is linked from an invariant enforcement column, the exact assert above is decisive and stays (rule 10). A snapshot may supplement it to make the derived shape reviewable at a glance (rules 9 and 12):

```rust
expect![[r#"
    derived | accepted | priority
    ------- | -------- | --------
    1       | 1        | ordinary
    2       | 3        | interrupt immediately after input 1
    3       | 2        | ordinary
"#]]
.assert_eq(&derived_order_table(&[first, second, interrupt]));
```

The rendering helper draws only the fields the derivation depends on, in derived order, through the shared `table` helper; the snapshot is read, not regenerated blind, when it changes.

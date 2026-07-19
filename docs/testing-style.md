# Testing style

This document owns test style: how a test body reads, how fixtures are shaped, what an assertion may reference, and how snapshot (expect) tests are used. The [testing section of CONTRIBUTING.md](../CONTRIBUTING.md#testing) owns what to test — the expected layers, determinism requirements, and merge gates — and is not restated here. Naming stays as [AGENTS.md](../AGENTS.md) states it: tests reference the scenario and invariant identifiers they enforce when the connection is meaningful.

The numbered rules are normative for new and modified tests; cite them by number in review. Apply them to existing tests only when already changing those tests for another reason.

## Fixtures and assertions

1. **A test's body contains its whole story.** A reader determines what is verified, with which inputs, and why the expected result is correct from the test body alone: complete (everything needed to understand the result is present) and concise (nothing else is). ([Software Engineering at Google, ch. 12](https://abseil.io/resources/swe-book/html/ch12.html))

2. **Tests are verified by inspection, not by tests of tests.** Test bodies are straight-line code: no loops, no conditionals, and no expectation recalculated by logic mirroring the code under test. An expected value is a hardcoded literal or a value the fixture already states (rule 6). Logic that must exist moves into a helper, and a nontrivial helper gets its own tests. ([Don't put logic in tests](https://testing.googleblog.com/2014/07/testing-on-toilet-dont-put-logic-in.html))

3. **DAMP over DRY.** Duplication between tests is fine when it helps each test read on its own; extract only plumbing that is irrelevant to the behavior under test. ([Tests too DRY? Make them DAMP](https://testing.googleblog.com/2019/12/testing-on-toilet-tests-too-dry-make.html))

4. **One meaningful knob per fixture.** Fixture constructors and builders carry canonical defaults; each test states only the values the behavior depends on, at the call site — never a helper taking five positional integers whose meanings live at the definition. Where a fixture also needs an identity seed, derive it from the one knob instead of adding a second free integer — decorrelated (for example, descending as the knob ascends), so an implementation reading identity where it should read the knob's value cannot accidentally pass. When the independence of two values is itself the behavior under test, the fixture takes both as named knobs.

5. **A test that cares about a value states it.** State a value the test depends on explicitly even when the canonical default happens to match; a test's meaning must not depend on defaults defined elsewhere.

6. **Assert against the fixture, not re-derived constants.** Write `assert_eq!(chosen.origin(), earlier.accepted_input_id())`, never `accepted_input_id(20)` re-encoding a magic seed from the setup. Fixture-based assertions follow the setup when it changes; re-encoded constants silently diverge from it.

7. **One behavior per test, named for the behavior.** The repository's `sNN_invNNN_...` naming convention already does this; keep it. A test that needs "and" in its description is two tests.

8. **Judge every test as a classifier.** For each test, name the real bug it would catch and the false alarm it could raise. The ideal test fails only when a requirement changes; a test that fails on behavior-preserving refactors is a cost, not a safety net. ([Test suites as classifiers](https://blog.nelhage.com/post/test-suites-as-classifiers/))

## Expect tests

Snapshot assertions use [`expect-test`](https://github.com/rust-analyzer/expect-test), a domain-crate dev-dependency arriving with its first adopting tests. `UPDATE_EXPECT=1 cargo test` re-blesses snapshots in place.

9. **Use expect tests where the value's shape is the assertion:** matrix outcomes, derived orders and projections, and error `Display` output. Prior art: Jane Street's [expect-test workflow](https://blog.janestreet.com/the-joy-of-expect-tests/) and [testing with expectations](https://blog.janestreet.com/testing-with-expectations/); [How to Test](https://matklad.github.io/2021/05/31/how-to-test.html) describes the single check-function, data-driven form these settle into.

10. **Snapshots supplement invariant enforcement; they never replace it.** A test linked from an enforcement column in [the invariant catalog](invariants.md) keeps its precise targeted asserts; a snapshot proves output-didn't-change, not invariant-holds.

11. **Never bless a diff you haven't read.** Review a snapshot update with the same care as a code change; the snapshot diff is the review surface.

12. **Curate snapshots for the reader.** Deterministic ordering, relevant fields only, rendered from the observed value under test. A snapshot of everything asserts nothing and degenerates into a [change-detector test](https://testing.googleblog.com/2015/01/testing-on-toilet-change-detector-tests.html). Table-shaped output in domain unit tests uses the `table` helper in that crate's `test_support` module (arriving with the expect-test adoption): pipe-separated, left-aligned, right-trimmed lines that stay byte-stable under re-blessing. Another test crate's first table-shaped snapshot lifts that helper into test support it can import instead of hand-rolling a variant. Prior art for tables in expect tests: [expectable](https://github.com/janestreet/expectable).

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

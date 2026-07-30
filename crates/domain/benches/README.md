# Domain microbenchmarks

These suites measure the same six deterministic, pure-CPU domain paths. They use
fixed synthetic inputs and perform no filesystem, database, network, or
subprocess work inside a measured function.

## Run the suites

Instruction counts and cache behavior use
[Gungraun](https://gungraun.github.io/gungraun/), the maintained continuation of
`iai-callgrind`. The harness runs Callgrind with cache simulation enabled so
fixture setup is excluded from both the instruction and simulated-cache metrics.
On Linux, install Valgrind and the matching runner, then run:

```console
cargo install --locked --version '=0.19.4' gungraun-runner
SIGNALBOX_DOMAIN_INSTRUCTION_BENCHMARK=1 \
  cargo bench -p signalbox-domain --bench instruction_counts
```

The run marker distinguishes an explicit instruction benchmark from Cargo
executing the custom bench binary during `cargo test --all-targets`. An explicit
run with debug assertions exits nonzero instead of publishing an empty report.

The detailed profiles are written below `target/gungraun`. The Rust workflow
runs this command and copies its complete terminal report into the GitHub job
summary. That job is explicitly report-only and allowed to fail. It has no
limit, comparison threshold, or required status. A structurally invalid fixed
fixture exits nonzero instead of publishing a meaningless count; because the job
is tolerated, that correctness check cannot block a merge.

Wall-clock measurements use [Divan](https://github.com/nvzqz/divan) and are
local only:

```console
cargo bench -p signalbox-domain --bench wall_clock
```

Do not add the wall-clock suite to CI. Shared-runner load changes elapsed time
enough to obscure the small regressions these benchmarks are intended to
investigate.

## Targets and interpretation

The exact shapes are stated beside their constructors in `targets/mod.rs`.
Changing a fixture changes the baseline and must be called out separately from a
code-performance change.

- `reconstitute_scheduling` enters `reconstitute_inner` with 64 turns, 97
  semantic entries, 97 shared snapshots, and a 16-entry active acceptance tail.
  This target preserves a pre-refactor comparison point for that function. A
  sustained instruction increase means the complete scheduling read path does
  more CPU work. A 4% increase is an investigation lead, not an acceptance
  threshold: first confirm the compiler, dependencies, harness, and fixture are
  equivalent, then use the profile and code diff to decide whether the added
  work is justified.
- `build_deep_frontier` builds 64 structurally shared layers containing 512
  entries. A change tracks the cost of persistent frontier construction and
  append validation.
- `prove_shared_prefix` proves that a 384-entry frontier is a prefix of a
  512-entry descendant through the structural-sharing fast path. The lineage
  index lookup is logarithmic in the number of snapshots but avoids scanning
  semantic entries. Its absolute count is intentionally small, so inspect the
  disassembly or profile before treating a percentage alone as important.
- `derive_interrupt_total_order` orders 256 turns arranged as 64 ordinary roots
  with three interrupt successors each. A change tracks sorting, map/set work,
  chain traversal, and chronology validation.
- `project_compaction_frontier` projects a 97-entry complete pre-compaction
  transcript. A change tracks the common no-summary scan and reference-position
  index used by compaction projection.
- `canonicalize_tool_arguments` parses and canonicalizes a fixed 20,169-byte
  JSON object with 128 reverse-ordered members. A change tracks the complete
  provider-text parsing, lexical key ordering, and compact serialization path.

Instruction counts are the CI signal because a fixed binary and input execute
the same simulated instructions independent of runner load. Compare runs only
when the compiler, dependencies, harness, and fixture are equivalent.
Simulated-cache statistics can still vary with changes to generated code or
cache configuration, so preserve the full report when investigating a delta.

Neither suite is a performance gate. Several weeks of observations are needed
before normal variance and meaningful effect sizes are understood, and this
repository deliberately has no automatic threshold or failure condition.

## Add a target

Add one fixed-input constructor and measured function to `targets/mod.rs`, then
register that same pair in both `instruction_counts.rs` and `wall_clock.rs`.
Keep setup outside the measured region, retain borrowed inputs in the returned
value when necessary so teardown is not counted, and use `black_box` only at the
harness boundary. Document the exact item counts and byte sizes beside the
fixture, explain what a delta means here, and run both suites before opening a
pull request.

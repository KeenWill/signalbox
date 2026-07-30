mod targets;

use divan::{Bencher, black_box};

fn main() {
    if std::env::var_os("SIGNALBOX_DOMAIN_WALL_CLOCK_BENCHMARK").is_none() {
        return;
    }
    if cfg!(debug_assertions) {
        eprintln!("domain wall-clock benchmarks require a release bench profile");
        std::process::exit(2);
    }
    divan::main();
}

#[divan::bench]
fn reconstitute_scheduling(bencher: Bencher) {
    bencher
        .with_inputs(targets::scheduling_fixture)
        .bench_values(|input| black_box(targets::reconstitute_scheduling(input)));
}

#[divan::bench]
fn build_deep_frontier(bencher: Bencher) {
    bencher
        .with_inputs(targets::frontier_build_input)
        .bench_values(|input| black_box(targets::build_deep_frontier(input)));
}

#[divan::bench]
fn prove_shared_prefix(bencher: Bencher) {
    let input = targets::prefix_proof_fixture();
    bencher.bench_local(|| black_box(targets::prove_shared_prefix(&input)));
}

#[divan::bench]
fn derive_interrupt_total_order(bencher: Bencher) {
    bencher
        .with_inputs(targets::total_order_fixture)
        .bench_values(|input| black_box(targets::derive_interrupt_total_order(input)));
}

#[divan::bench]
fn project_compaction_frontier(bencher: Bencher) {
    let input = targets::compaction_projection_fixture();
    bencher.bench_local(|| black_box(targets::project_compaction_frontier(&input)));
}

#[divan::bench]
fn canonicalize_tool_arguments(bencher: Bencher) {
    bencher
        .with_inputs(targets::tool_arguments_fixture)
        .bench_values(|input| black_box(targets::canonicalize_tool_arguments(input)));
}

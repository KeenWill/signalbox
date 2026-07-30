mod targets;

use gungraun::{
    Callgrind, CallgrindMetrics, LibraryBenchmarkConfig, library_benchmark, library_benchmark_group,
};
use std::hint::black_box;

#[library_benchmark]
#[bench::fixed(setup = targets::scheduling_fixture)]
fn reconstitute_scheduling(input: targets::SchedulingFixture) -> targets::SchedulingResult {
    black_box(targets::reconstitute_scheduling(input))
}

#[library_benchmark]
#[bench::fixed(setup = targets::frontier_build_input)]
fn build_deep_frontier(
    input: targets::FrontierBuildInput,
) -> signalbox_domain::ResolvedContextFrontierReconstitutionInput {
    black_box(targets::build_deep_frontier(input))
}

#[library_benchmark]
#[bench::fixed(setup = targets::prefix_proof_fixture)]
fn prove_shared_prefix(input: targets::PrefixProofFixture) -> (bool, targets::PrefixProofFixture) {
    let result = black_box(targets::prove_shared_prefix(&input));
    (result, input)
}

#[library_benchmark]
#[bench::fixed(setup = targets::total_order_fixture)]
fn derive_interrupt_total_order(
    input: Vec<signalbox_domain::AcceptedInputQueueWork>,
) -> Vec<signalbox_domain::TurnId> {
    black_box(targets::derive_interrupt_total_order(input))
}

#[library_benchmark]
#[bench::fixed(setup = targets::compaction_projection_fixture)]
fn project_compaction_frontier(
    input: targets::ProjectionFixture,
) -> (targets::ProjectionResult, targets::ProjectionFixture) {
    let result = black_box(targets::project_compaction_frontier(&input));
    (result, input)
}

#[library_benchmark]
#[bench::fixed(setup = targets::tool_arguments_fixture)]
fn canonicalize_tool_arguments(input: String) -> signalbox_domain::NormalizedToolArguments {
    black_box(targets::canonicalize_tool_arguments(input))
}

library_benchmark_group!(
    name = domain_microbenchmarks;
    benchmarks =
        reconstitute_scheduling,
        build_deep_frontier,
        prove_shared_prefix,
        derive_interrupt_total_order,
        project_compaction_frontier,
        canonicalize_tool_arguments
);

mod benchmark_runner {
    use super::{Callgrind, CallgrindMetrics, LibraryBenchmarkConfig, domain_microbenchmarks};

    gungraun::main!(
        config = LibraryBenchmarkConfig::default()
            .pass_through_env("SIGNALBOX_DOMAIN_INSTRUCTION_BENCHMARK")
            .tool(Callgrind::default().format([CallgrindMetrics::All]));
        library_benchmark_groups = domain_microbenchmarks
    );

    pub(super) fn run() {
        main();
    }
}

fn main() {
    if std::env::var_os("SIGNALBOX_DOMAIN_INSTRUCTION_BENCHMARK").is_none() {
        return;
    }
    if cfg!(debug_assertions) {
        eprintln!("domain instruction benchmarks require a release bench profile");
        std::process::exit(2);
    }
    benchmark_runner::run();
}

//! Allocator for the daemon's concurrent Rust and supported native allocations.

#[global_allocator]
static ALLOCATOR: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

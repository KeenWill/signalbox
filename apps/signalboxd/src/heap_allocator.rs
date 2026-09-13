//! Allocator for the daemon's concurrent, mixed-lifetime Rust allocations.

#[global_allocator]
static ALLOCATOR: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

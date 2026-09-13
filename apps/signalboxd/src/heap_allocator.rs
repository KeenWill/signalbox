//! Allocator for the daemon's concurrent, mixed-lifetime Rust allocations.

#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

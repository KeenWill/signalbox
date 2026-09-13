# Daemon memory allocation design

This design is unbuilt. It extends the daemon composition described in
[turn lifecycle and scheduling](../spec/turn-lifecycle-and-scheduling.md).

## Goal

Reclaim temporary worker buffers while small, long-lived allocations remain,
without accumulating freed buffers in the daemon's resident memory.

## Design

Select [mimalloc](https://github.com/microsoft/mimalloc) as the Rust global
allocator at the `signalboxd` executable's composition root, using its
[MiMalloc wrapper](https://github.com/purpleprotocol/mimalloc_rust). Its
size-segregated pages and normal reclamation handle mixed allocation lifetimes.
The wrapper adds its bundled native allocator, built with the existing C
toolchain. Other executables keep their allocator selection.

Do not add memory limits, worker limits, allocator settings or periodic trimming
tasks.

## Compatibility constraints

The allocator choice changes no domain, wire or stored representation. It
applies to Rust allocations throughout the linked daemon, including native
libraries whose allocation callbacks use Rust's allocator. Native libraries that
allocate independently retain their own allocation boundary.

## Acceptance criteria

An isolated child of the daemon test binary allocates a 128 MiB buffer burst
across 32 workers, interleaved with small surviving allocations. After freeing
the buffers and continuing ordinary allocation activity, at least 96 MiB leaves
the resident set. The regression uses the daemon's composition-root allocator
selection and fails with that selection removed.

The daemon test suite passes, and the live observation reports resident memory
alongside allocator counters, dispatches, sessions and retained evidence bytes.

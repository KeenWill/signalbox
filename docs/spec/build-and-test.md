# Build and test

Signalbox's build tools compile its crates and verify their contracts.

## Overview

Cargo runs the full-workspace CI checks. Bazel builds the Rust workspace
libraries and binaries with all features enabled, including fixture binaries,
and runs the newtype, domain, and blob-store unit tests through native Rust
actions. A separate Bazel PostgreSQL suite runs the persistence integration
binaries.

## Design decisions

Native Bazel actions track Rust sources, dependencies, and test executables for
content-based reuse within a compatible native environment.

## Boundary contracts

Cargo manifests and the Cargo lockfile remain the Rust dependency source. The
Bazel module pins its Rust compiler to the workspace toolchain version. The
Linux Bazel graph pins GCC and its sysroot. Unit tests run with a pinned loader
and runtime libraries included in their test inputs. Shared results require
compatible x86-64 Linux kernels; host tools and external services are not
covered by this unit-test contract.

## Planned

None.

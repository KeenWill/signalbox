# Build and test

Signalbox's build tools compile its crates and verify their contracts.

## Overview

Cargo runs the full-workspace CI checks. Bazel builds the Rust workspace
libraries and binaries with all features enabled, including fixture binaries,
and runs their unit tests through native Rust actions. Separate Bazel PostgreSQL
suites run the persistence and program-host integration binaries with a shared
image digest declared as a compilation input.

## Design decisions

Native Bazel actions track Rust sources, dependencies, and test executables for
content-based reuse within a compatible native environment.

## Boundary contracts

Cargo manifests and the Cargo lockfile remain the Rust dependency source. The
Bazel module pins its Rust compiler to the workspace toolchain version. The
Linux Bazel graph pins GCC and its sysroot. Rust tests run with a pinned loader,
runtime libraries, and executable patcher included in their test inputs. Shared
results require compatible x86-64 Linux kernels. Unit targets that use host
utilities always execute; the daemon's socket tests retain the host ownership
mapping outside the Bazel sandbox.

## Planned

None.

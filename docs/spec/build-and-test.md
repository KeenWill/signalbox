# Build and test

Signalbox's build tools compile its crates and verify their contracts.

## Overview

Bazel checks workspace Rust formatting, Clippy, and documentation with warnings
denied. Bazel builds the Rust workspace libraries and binaries with all features
enabled, including fixture binaries, and runs their unit tests, standalone
integration targets, and library doctests through native Rust actions. Generated
JavaScript contract tests use a declared Node.js toolchain. Compile-fail
diagnostic fixtures use host Cargo and always execute. Bazel PostgreSQL suites
run the ignored tests selected by the suite manifest, with its features, skips,
binary partitions, and shard counts. Each manifest shard runs on its own CI
worker. Their compilation is cacheable; database tests always execute. Daemon
PostgreSQL targets use sorted JSON maps. The Rust workflow binds the Bazel
results to `validate` under its change-scope gate. File-media format and
registry conformance tests use native Bazel targets; process-isolation tests
retain their provisioned Cargo job.

## Design decisions

The checker job runs its eight Python suites through Bazel with declared Python
and Rustfmt toolchains. Suites using host Git or shell utilities always execute.
Markdown formatting uses a Bazel test with declared files, formatter packages,
and configuration. Web-contract and model-projection generation run as native
Bazel actions with declared output trees and snapshot-comparison tests. The
contract check builds and renders API declarations through Bazel with the pinned
nightly Rustdoc toolchain. The optional historical API digest retains Cargo in a
separate job.

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

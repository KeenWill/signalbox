# Build and test

Signalbox's build tools compile its crates and verify their contracts.

## Overview

Cargo builds the Rust workspace and runs the required CI checks. Bazel compiles
and tests the newtype crate through native Rust actions.

## Design decisions

Native Bazel actions declare compilation inputs and test executables so their
results can be reused by content.

## Boundary contracts

Cargo manifests and the Cargo lockfile remain the Rust dependency source. The
Bazel module pins its Rust compiler to the workspace toolchain version.

## Planned

None.

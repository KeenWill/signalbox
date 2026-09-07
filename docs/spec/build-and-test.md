# Build and test

Signalbox's build tools compile its crates and verify their contracts.

## Overview

Cargo builds the Rust workspace and runs the required CI checks. Bazel compiles
and tests the newtype, domain, and blob-store crates through native Rust
actions.

## Design decisions

Native Bazel actions track Rust sources, dependencies, and test executables for
content-based reuse within a compatible native environment.

## Boundary contracts

Cargo manifests and the Cargo lockfile remain the Rust dependency source. The
Bazel module pins its Rust compiler to the workspace toolchain version.
Remote-cache clients require identical native toolchains and compatible runtime
environments; the Bazel graph does not yet declare host linker and system
libraries.

## Planned

None.

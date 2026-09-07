# Bazel

Install [Bazelisk](https://github.com/bazelbuild/bazelisk) and a C/C++ compiler
with the platform development libraries. Bazelisk selects `.bazelversion`;
`rules_rust` downloads the Rust compiler pinned in `MODULE.bazel`. Rust and
rustfmt share that module's version constant. Renovate groups updates to it with
`rust-toolchain.toml`; manual toolchain changes update both files.

```bash
bazel build //crates/newtype:signalbox_newtype
bazel test //:bazel_tests
```

The current Bazel suite runs the newtype, domain, and blob-store unit tests on
x86-64 Linux. The blob-store target enables `test-support`. Cargo commands and
CI cover the full workspace. See [Build and test](spec/build-and-test.md).

`crate_universe` reads the workspace Cargo manifests and `Cargo.lock` to
generate third-party dependency targets. Change dependencies with Cargo as
usual; the next Bazel invocation refreshes their generated definitions. The
Cargo host tools use the same Rust version constant as compilation and rustfmt.

The syscall crate is a Cargo workspace member with its own unsafe-code lint
policy. This lets the importer treat all in-repository crates as first-party
packages rather than generating machine-specific external path dependencies.

Use a remote cache only with an identical native compiler, sysroot, and runtime
environment across its clients. To select an accessible endpoint:

```bash
bazel test --remote_cache=grpc://CACHE_HOST:9092 //:bazel_tests
```

Local endpoint settings can also live in the ignored `.bazelrc.local`. Each
worktree uses its own Bazel output directory; the remote service shares action
results and artifacts. No shared writable output directory or local disk cache
is configured. Host linker and system libraries are not declared inputs, so
Bazel cannot reliably invalidate their cached results when those tools change.
Cross-host sharing requires a pinned native toolchain and compatible runtime;
the current validation covers fresh output directories on the same host.

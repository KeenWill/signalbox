# Bazel

Install [Bazelisk](https://github.com/bazelbuild/bazelisk). Bazelisk selects
`.bazelversion`; `rules_rust` downloads the Rust compiler pinned in
`MODULE.bazel`. Rust and rustfmt share that module's version constant. Renovate
groups updates to it with `rust-toolchain.toml`; manual toolchain changes update
both files.

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

The Linux build downloads a pinned GCC toolchain and sysroot. Unit tests run
through a pinned Ubuntu loader and runtime, included as Bazel test inputs.
Clients require compatible x86-64 Linux kernels. To select an accessible cache:

```bash
bazel test --remote_cache=grpc://CACHE_HOST:9092 //:bazel_tests
```

Local endpoint settings can also live in the ignored `.bazelrc.local`. Each
worktree uses its own Bazel output directory; the remote service shares action
results and artifacts. No shared writable output directory or local disk cache
is configured. Self-hosted CI uses the `BAZEL_REMOTE_CACHE` repository variable
when configured; hosted jobs run without the private cache. On a private
Tailscale client, use `--remote_cache=grpc://bazel-cache:9092`.

The runtime launcher applies to the current unit-test binaries. Tests that
invoke host programs or external services need those inputs declared before
their results can join the shared cache.

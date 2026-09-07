# Bazel

Install [Bazelisk](https://github.com/bazelbuild/bazelisk) and a C/C++ compiler
with the platform development libraries. Bazelisk selects `.bazelversion`;
`rules_rust` downloads the Rust compiler pinned in `MODULE.bazel`. Update that
compiler pin together with `rust-toolchain.toml`.

```bash
bazel build //crates/newtype:signalbox_newtype
bazel test //:bazel_tests
```

The current Bazel suite contains the newtype unit tests. Cargo commands and CI
cover the full workspace. See [Build and test](spec/build-and-test.md).

To use an accessible remote cache, pass its endpoint explicitly:

```bash
bazel test --remote_cache=grpc://CACHE_HOST:9092 //:bazel_tests
```

Local endpoint settings can also live in the ignored `.bazelrc.local`. Each
worktree uses its own Bazel output directory; the remote service shares action
results and artifacts. No shared writable output directory or local disk cache
is configured. Native linker and system-library differences can prevent reuse
between hosts; this setup does not yet pin that part of the toolchain.

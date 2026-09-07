# Bazel

Install [Bazelisk](https://github.com/bazelbuild/bazelisk). Bazelisk selects
`.bazelversion`; `rules_rust` downloads the Rust compiler pinned in
`MODULE.bazel`. Rust and rustfmt share that module's version constant. Renovate
groups updates to it with `rust-toolchain.toml`; manual toolchain changes update
both files.

```bash
bazel build //:rust_build
bazel test //:bazel_tests
```

Bazel builds the workspace libraries and binaries on x86-64 Linux with all
features enabled, including test-support surfaces and fixture binaries, matching
Cargo's all-features CI build. These targets do not validate default-feature
release artifacts. The unit suite covers the workspace libraries and binaries.
Cargo commands and CI cover the full workspace. See
[Build and test](spec/build-and-test.md).

`crate_universe` reads the workspace Cargo manifests and `Cargo.lock` to
generate third-party dependency targets. Change dependencies with Cargo as
usual; the next Bazel invocation refreshes their generated definitions. The
Cargo host tools use the same Rust version constant as compilation and rustfmt.

The syscall crate is a Cargo workspace member with its own unsafe-code lint
policy. This lets the importer treat all in-repository crates as first-party
packages rather than generating machine-specific external path dependencies.

The Linux build downloads a pinned GCC toolchain and sysroot. V8 uses a
checksum-pinned archive selected from `Cargo.lock`; Bazel resolves each new
release and records its digest in `MODULE.bazel.lock`. Commit that lockfile
after dependency updates. The V8 patch keeps native build outputs inside the
declared output directory. The Rust test launcher uses pinned patchelf to give
temporary executable copies the pinned Ubuntu loader and library paths. This
preserves self-reexecution and lets declared helper binaries run with a cleared
environment. Clients require compatible x86-64 Linux kernels. To select an
accessible cache:

```bash
bazel test --remote_cache=grpc://CACHE_HOST:9092 //:bazel_tests
```

Local endpoint settings can also live in the ignored `.bazelrc.local`. Each
worktree uses its own Bazel output directory; the remote service shares action
results and artifacts. No shared writable output directory or local disk cache
is configured. Self-hosted CI uses the `BAZEL_REMOTE_CACHE` repository variable
when configured; hosted jobs run without the private cache. On a private
Tailscale client, use `--remote_cache=grpc://bazel-cache:9092`.

The runtime launcher applies to the Rust unit-test binaries. The convergence
CLI, Claude/Codex adapter, and daemon unit targets use host utilities and carry
Bazel's `external` tag so their tests always execute while compilation remains
cacheable. The daemon socket tests also run outside the sandbox to inspect the
host's real ownership mapping. Other tests declare their fixture files as Bazel
inputs. Cargo continues to run the integration tests and doctests.

The PostgreSQL suites cover persistence and the JavaScript program host. They
share the image digest in `tooling/postgres_test_image.rs`, which Renovate
updates. The image pin, migrations, and example configuration are declared
compilation inputs. With Docker:

```bash
bazel test --local_test_jobs=1 --test_env=DOCKER_HOST=unix:///var/run/docker.sock //crates/persistence:postgres_tests
bazel test --local_test_jobs=1 --test_env=DOCKER_HOST=unix:///var/run/docker.sock //crates/program-runtime:postgres_tests
```

Each test binary has its own cached result. PostgreSQL targets are manual so
`//:bazel_tests` remains the non-PostgreSQL suite. The additive `bazel-postgres`
CI job runs one binary at a time, with 16 test threads per binary; Cargo retains
the full PostgreSQL gate.

The checker job runs its eight Python suites with
`bazel test //:python_checker_tests`. Bazel supplies Python 3.14, packages from
`tooling/requirements-mdformat.txt`, and the shared Rustfmt toolchain. The Git
fixture suite and two shell-script suites carry `external` because they execute
host utilities; their results always run. The other five results are cacheable.

`bazel test //:markdown_format` checks repository Markdown with the same pinned
mdformat and GFM plugin as devenv. Its inputs include the Markdown files and
`.mdformat.toml`; generated `docs/api` files remain excluded. The checker CI job
uses this target instead of installing a separate Python environment.

The sweep's container-label check scans the declared workspace Rust source
manifest inside test runfiles. Direct Python invocation continues to select
tracked sources with Git.

`bazel test //:rust_integration_tests` runs native macro, ownership-seam,
provider-loopback, and filesystem conformance tests. It is included in
`//:bazel_tests`. Filesystem conformance checks the host storage classification
outside the sandbox and always executes; the other results are cacheable.

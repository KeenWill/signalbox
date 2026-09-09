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
release artifacts. The ordinary suite covers workspace unit tests, integration
binaries, and doctests. A GitHub-hosted Bazel job runs the provisioned
host-isolation gate. The required Rust `validate` job retains Cargo checks for
the catalog workspace dependency boundary and Codex schema fixtures, including
Rust lockfile-only updates. See [Build and test](spec/build-and-test.md).

`crate_universe` reads the workspace Cargo manifests and `Cargo.lock` to
generate third-party dependency targets. Change dependencies with Cargo as
usual; the next Bazel invocation refreshes their generated definitions. The
Cargo host tools use the same Rust version constant as compilation and rustfmt.

Workspace packages call `rust_package()` from `tooling/bazel/package.bzl` for
Cargo metadata, lint settings, and Rust source inputs. Extra exported fixtures
are listed in its `exports` argument. The build macros in
`tooling/bazel/rust.bzl` add Cargo-managed third-party dependencies and compiler
defaults; their `deps` and `proc_macro_deps` arguments list first-party targets.
Rule attributes can override shared defaults. `tooling/bazel/testing.bzl`
provides unit, standalone integration, PostgreSQL, and Rustdoc JSON helpers.
Tests retain explicit source roots, features, fixtures, and execution
exceptions. Repeated dependency and feature lists are shared within each
package.

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
inputs. `//:rust_doc_tests` runs all workspace library doctests, including
compile-fail sealing proofs. The standalone integration targets include the
compile-fail diagnostic fixtures, which use the host Cargo environment and
always execute.

The PostgreSQL suites read packages, features, skipped test names, binary
partitions, and shard counts from `.github/postgres-integration-suites.toml`.
They include ignored library tests and standalone integration binaries. With
Docker, run a suite by its manifest name (hyphens become underscores):

```bash
bazel test --local_test_jobs=1 --test_env=DOCKER_HOST=unix:///var/run/docker.sock //:postgres_persistence
bazel test --local_test_jobs=1 --test_env=DOCKER_HOST=unix:///var/run/docker.sock //:postgres_workflow_runtime
```

Daemon PostgreSQL targets use Cargo's sorted JSON map feature selection; the
workspace dependency graph also serves the program host's insertion-ordered JSON
requirements.

The shared image digest, migrations, and example configuration are declared
compilation inputs. PostgreSQL targets are manual and external: compilation is
cacheable, tests always execute. The runtime partitions libtest's ignored-test
inventory across each target's manifest-derived shards. CI gives each manifest
shard its own matrix worker; each worker runs one partition per binary at a time
with 16 test threads. The Rust workflow calls `bazel.yml` and binds its ordinary
and PostgreSQL results to `validate` under the Rust change-scope gate.

The checker job runs its Python suites with
`bazel test //:python_checker_tests`. Bazel supplies Python 3.14, packages from
`tooling/requirements-mdformat.txt`, and the shared Rustfmt toolchain. The Git
fixture suite and two shell-script suites carry `external` because they execute
host utilities; their results always run. The remaining results are cacheable.

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

`bazel test //:media_integration_tests` checks text, image, archive, office,
PDF, SVG, video, and registry behavior. These fixture-based targets are included
in `//:bazel_tests` and cache their results. The `//:host_integration_tests`
targets cover the audio worker, process isolation, and real Bubblewrap checks
with CI confinement requirements enabled. They need a delegated cgroup and the
real sandbox. The GitHub-hosted `bazel-host-integration` job provisions both and
runs this group; the Rust workflow binds its result to `validate`.

The importer conformance corpus runs in `//:rust_integration_tests`. Its JSONL
inputs, golden files, and Cargo configuration are declared separately. Golden
paths remain absolute under Cargo and are anchored in the test runfiles under
Bazel.

`bazel test //:generated_artifact_tests` builds the web-contract and model
projection generators, runs them into declared output trees, and compares the
results with checked-in snapshots in the checker CI job. Both generation actions
and comparisons use the shared cache. The output trees are available from
`bazel build //:web_contract_output //:model_projection_output`.

The generator CLIs also accept an optional output directory after their existing
arguments. Omitting it retains their Cargo regeneration behavior.

`bazel test --config=api //:api_documentation_current` builds Rustdoc JSON for
all 36 configured API crates, renders Markdown into `bazel-bin/api_pages`, and
compares it with the committed API snapshots. Rustdoc, rendering, and the
comparison are separate cacheable actions. The API configuration selects the
pinned nightly toolchain; Renovate groups its Cargo renderer and Bazel pins. The
required contract check uses this target. The report-only API digest reads
current JSON from `//:api_json` and uses Cargo to document its historical event
base.

`//crates/web-contract:generated_roundtrip` runs the generated JavaScript
contract tests with a pinned Node.js toolchain and declared JSON fixtures.
Ordinary PostgreSQL selections reuse the ignored suites’ compiled binaries; the
non-PostgreSQL suite does not execute their ignored tests.

`//:rust_clippy_tests` runs Clippy with `clippy.toml` and warnings denied.
`//:rust_format_tests` checks native workspace crate roots, Cargo build scripts,
and their modules, matching Cargo formatting. `//:rust_documentation` builds
Cargo's documentation entrypoints with warnings denied. These actions use the
declared Rust toolchain and remote cache.

`coverage.yml` runs `bazel coverage` for the workspace and the persistence,
daemon, and terminal-client PostgreSQL selections. It retains their named
exclusions and reports each suite's outcome without gating merges. LCOV merges
hits across binaries and declared subprocess fixtures; the report excludes
dedicated test and benchmark files and marks region coverage unavailable.
Instrumented binaries keep a relative path to the declared runtime libraries.

`bazel test //clients/web/...` runs Biome, TypeScript, Vitest, the Vite build,
and the existing Chromium, Firefox, and WebKit Playwright assertions. The npm
lockfile supplies package versions; its pnpm translation is generated input. The
browser runtime follows the locked Playwright package, with image checksums
retained in the Bazel lockfile and declared DejaVu fonts. Browser evidence is
retained in the test's undeclared outputs. The web CI job uses the shared cache.

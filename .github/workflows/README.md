# CI runner contract

Linux jobs use ephemeral, repository-scoped Kata runners. Change a job's
`runs-on` and this file in the same pull request.

| Label | Jobs |
| --- | --- |
| `cluster-signalbox` | Linux builds, tests, orchestration, reports and Docker suites |
| `cluster-signalbox-host` | Host integration and the exec tool-eval family |
| `macos-latest` | Swift validation and real-daemon integration |

Both cluster pools provide Nix, Rustup, Node, Python, Docker and browser
libraries. The host pool additionally provides sudo inside its disposable Kata
VM for cgroup delegation and deterministic fixture installation. Neither pool
mounts host data or a Kubernetes service-account token. The pools exclude
nodes running databases and never preempt services.

Fork pull requests are skipped. Repository settings require approval for all
outside collaborators, and a read-only runner hook rejects fork events before
workflow steps execute, including steps with `if: always()`. Same-repository
pull requests, pushes and the workflows' trusted scheduled/manual events use
the cluster pools. Existing provider-smoke and tool-eval eligibility checks
continue to apply.

`scripts/postgres_integration_suites.py` checks the Docker-capable pool,
PostgreSQL matrix binding and final validation dependency. Native macOS jobs
still require macOS capacity; the Linux cluster cannot execute them.

## Comparison checkouts

Scope jobs fetch only the exact comparison revisions at depth one, without tags.
Rust and provider-smoke pull requests ask GitHub for the merge-base SHA and diff
its tree against the event head; push comparisons and Swift scope use the
event's two endpoints. Local Git produces the complete NUL-delimited path list,
including deletions, without the GitHub changed-files API's list limits. A
comparison API or fetch failure fails the job instead of returning an empty
scope. The API digest fetches its exact event baseline for the baseline build.
Contract checks fetch depth-one trees for `main` and the pull request base
branch because migration ordering checks compare against both baselines. They do
not need the intervening commit history.

Provider eligibility keeps its comparison code inline in the workflow: it reads
proposed files without executing scripts from the proposed checkout. Fetch
authentication exists only in the fetch process environment.

## PostgreSQL dependency selection

The Rust scope passes its complete changed-path list to `postgres-matrix`. For
existing Rust source edits, that job uses `bazel query` to select suites whose
transitive source inputs changed. Query includes every branch of configurable
dependencies and compiles nothing. It uses the builds pool because loading the
Bazel graph needs more memory than the orchestration pool provides. Selected
suites retain every manifest shard.

Shared inputs, build definitions, non-Rust files, missing/deleted sources,
unrecognized inputs, and unavailable path detail run every suite. A failed query
fails matrix generation. Documentation prose, browser source and native client
changes can bypass PostgreSQL through the coarse path gate; generated browser
contracts keep the bar. API generation and API digest reports reuse the existing
Rust scope, while documentation consistency checks still run.

## Shared PostgreSQL fixtures

Linux Bazel PostgreSQL selections enable `SIGNALBOX_TEST_SHARED_POSTGRES=1`.
Each libtest process lazily starts a shared PostgreSQL server and clones the
migration-set template into a separate database for each fixture. Every live
fixture reserves its own bounded tablespace. A full server starts another server
instead of blocking tests that need multiple fixtures. Slots become reusable
only after a successful database drop; failed setup or cleanup retains the
reservation until the process exits. `TESTCONTAINERS_COMMAND=keep` retains
databases and servers.

Bazel's runtime wrapper owns server cleanup and waits for it before returning to
Bazel, which reaps detached descendants. A failed or killed test binary still
triggers wrapper cleanup. Killing the entire wrapper or runner with `SIGKILL`
can leave labeled containers holding host memory until the
[existing sweeper](../../tooling/sweep-test-containers.sh) reclaims them. Cargo
can opt in with the same variable and uses a pidfd guardian tied to the test
process. Nextest retains its existing run-owned server path. Ordinary Bazel
selections retain their existing fixture behavior. No suite filters or test
concurrency settings change.

## PostgreSQL result reuse

Cacheable PostgreSQL test selections use the shared Bazel action cache. Explicit
`external` exceptions in `BUILD.bazel` retain fresh execution for host-dependent
probes. Each shard writes a Bazel JSON event stream and publishes observed local
and remote hits, actual executions, retries and execution seconds in its job
summary. Cached historical test durations do not count as execution in this run.
Missing results or an incomplete stream are labeled explicitly. Reporting is
informational and cannot replace the suite command's exit status.

Each PostgreSQL shard also samples its Docker daemon's running containers every
five seconds using one-shot Docker Engine API reads through the local Unix
socket. It reports aggregate observed CPU, charged memory and working-memory
peaks. CPU is derived from successive per-container cumulative counters; the
first sample establishes a baseline. This captures test containers even when
their cgroups are outside the runner pod. The report excludes the runner and
Docker daemon; do not add it to pod totals without checking cgroup placement.
Linux Docker working-memory statistics exclude inactive file cache; charged
memory includes it. Short-lived containers, their final CPU increments and peaks
between samples can be missed, and sampling failures are reported as gaps.
Sampling errors do not change the suite command's exit status. Interrupted
collection is unmeasured. Per-shard JSON samples and the test-result cache
report are retained together for seven days, so actual executions, cache hits
and retries can be analyzed separately.

## Bazel scratch

The shared setup action allocates a unique directory under `RUNNER_TEMP` for
Bazel outputs, the output user root (including the install base), repository
downloads, and Bazelisk downloads. On self-hosted runners this is the mounted
workspace scratch volume. GitHub-hosted jobs use their runner temporary storage.
The runner clears temporary storage between jobs; these directories are not
shared between jobs. Remote action and download caching still use the configured
endpoint. The change applies to every caller of the action, including coverage.

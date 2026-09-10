# CI runner contract

Which runner every workflow job uses, and why. Change a job's `runs-on` and this
file in the same pull request.

## Pools

| Label                         | Use it for                                                                                                                |
| ----------------------------- | ------------------------------------------------------------------------------------------------------------------------- |
| `signalbox-orchestration`     | Change scope, eligibility, matrix generation, and result-only aggregation; no compilation                                 |
| `signalbox-builds`            | Required Rust builds/tests, documentation, formatting, web checks, supply-chain checks, and provider compatibility smokes |
| `signalbox-integration-tests` | Required PostgreSQL suites needing Docker/testcontainers                                                                  |
| `signalbox-reports`           | Non-Docker API digest, instruction-count, and web-smoke reports                                                           |
| `signalbox-coverage`          | Instrumented Rust coverage                                                                                                |
| `signalbox-smokes`            | Docker-dependent smoke checks and report-only tool-eval families                                                          |
| `ubuntu-latest`               | GitHub-hosted fallbacks and host-dependent jobs below                                                                     |
| `macos-latest`                | macOS jobs                                                                                                                |

The self-hosted ARC scale sets are managed outside this repository. Only
`signalbox-integration-tests`, `signalbox-coverage`, and `signalbox-smokes`
provide Docker.

`rust.yml` calls `bazel.yml` for ordinary Rust tests on `signalbox-builds` and
manifest PostgreSQL suites on `signalbox-integration-tests`. Its
`validate-checks` aggregation uses orchestration; final `validate` stays on
builds because it also executes two Cargo contract checks. The web job uses
builds with declared browser runtimes and fonts. Report-tier jobs and Docker
live smokes limit Cargo compilation to two jobs; the API digest also limits
Bazel to two jobs. Docker sidecar CPU allowances are separate from runner
compilation budgets.

## The routing rule

Self-hosted-eligible Linux jobs — merge-gating or report-only — use one
canonical expression, unless pinned in the sections below:

```yaml
runs-on: ${{ github.event_name == 'pull_request'
  && (github.event.pull_request.head.repo.full_name != github.repository
      || contains(fromJSON('["dependabot[bot]","renovate[bot]"]'),
                  github.event.pull_request.user.login))
  && 'ubuntu-latest' || '<pool>' }}
```

- In jobs carrying this expression, same-repo pull requests from people take the
  self-hosted arm, as does every non-`pull_request` event the workflow accepts
  (pushes to `main`, schedules, dispatches).
- Fork pull requests, and pull requests authored by exactly the two matched bot
  accounts (`dependabot[bot]`, `renovate[bot]`), take the hosted arm. That is
  the whole enforced condition: no other bot identity is excluded, nothing
  inspects who pushed a commit, and the pinned jobs in the next section enforce
  weaker gates or none.
- The bot check keys on the pull request author
  (`github.event.pull_request.user.login`), not `github.actor`: a human
  reopening or re-running a bot pull request would otherwise mask the bot and
  route its code to self-hosted hardware.

`scripts/postgres_integration_suites.py` generates the PostgreSQL matrix and
executes each partition with manifest-derived Bazel arguments. Its workflow
checks read the matrix binding, Docker pool, and `validate` dependency as YAML
fields; they do not interpret shell commands.

## Jobs pinned to a self-hosted pool

The smoke and eval workflows hard-code their pool instead of carrying the
expression, each with its own gate. The provider smokes merge-gate the pull
requests they apply to — each `required` aggregate is a binding check — while
the tool-eval and tool-smoke jobs are report-only:

| Jobs                                                 | Pool                      | Gate on proposed code                                                                                      |
| ---------------------------------------------------- | ------------------------- | ---------------------------------------------------------------------------------------------------------- |
| Provider smoke workflows — `gate`, `required`        | `signalbox-orchestration` | `gate` checks out the head for path inspection without executing it; `required` reports the binding result |
| Provider smoke workflows — `smoke`                   | `signalbox-builds`        | Skips fork pull requests using the same-repository check only; same-repository bot pull requests still run |
| `tool-evals.yml` — `eligibility`                     | `signalbox-orchestration` | Excludes fork pull requests and Dependabot/Renovate authors                                                |
| `tool-evals.yml` — git/workspace/web `eval` families | `signalbox-smokes`        | Requires same-repository pull requests whose author is neither Dependabot nor Renovate                     |
| `tool-smokes.yml` — `live-smokes`                    | `signalbox-smokes`        | Requires same-repository pull requests whose author is neither Dependabot nor Renovate                     |
| `tool-smokes.yml` — `web-smoke`                      | `signalbox-reports`       | Disabled; retains the same-repository and Dependabot/Renovate author gate                                  |

## Jobs pinned to GitHub-hosted runners

The runner image has no `sudo` (pods run with `no-new-privileges`), no Nix, no
`gh` CLI, or host-provided Playwright system dependencies. Jobs needing host
facilities stay hosted for now, although this may change over time.

| Job                                               | Why                                                           |
| ------------------------------------------------- | ------------------------------------------------------------- |
| `bazel.yml` `bazel-host-integration`              | privileged cgroup delegation via `sudo`                       |
| `tool-evals.yml` exec family                      | `sudo` fixture installs into `/usr/local`                     |
| `devenv-smoke.yml` `linux`                        | Nix; committed-lock evaluation and disposable script fixtures |
| `devenv-lock.yml` `relock`                        | Nix                                                           |
| `devenv-lock.yml` `propose`                       | `gh` CLI and the write token (the job never runs Nix)         |
| `swift.yml` `swift-validate`, `swift-real-daemon` | macOS                                                         |

The `bazel-postgres` job uses the canonical routing expression with
`signalbox-integration-tests`, or `ubuntu-latest` for fork and named bot pull
requests.

## Comparison checkouts

Scope jobs fetch only the exact comparison revisions at depth one, without tags.
Rust and provider-smoke pull requests ask GitHub for the merge-base SHA and diff
its tree against the event head; push comparisons and Swift scope use the
event's two endpoints. Local Git produces the complete NUL-delimited path list,
including deletions, without the GitHub changed-files API's list limits. A
comparison API or fetch failure fails the job instead of returning an empty
scope. The API digest fetches its exact event baseline for the baseline build.
Contract checks inspect the current tree and need no history.

Provider eligibility keeps its comparison code inline in the workflow: it reads
proposed files without executing scripts from the proposed checkout. Fetch
authentication exists only in the fetch process environment.

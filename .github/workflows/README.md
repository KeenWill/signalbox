# CI runner contract

Which runner every workflow job uses, and why. Change a job's `runs-on` and this
file in the same pull request.

## Pools

| Label              | What it is                                          | Use it for                                                     |
| ------------------ | --------------------------------------------------- | -------------------------------------------------------------- |
| `signalbox`        | Self-hosted ARC scale set (Linux, no Docker daemon) | Ordinary Rust/lint/check jobs                                  |
| `signalbox-docker` | Self-hosted ARC scale set with a DinD sidecar       | Jobs needing Docker or testcontainers                          |
| `ubuntu-latest`    | GitHub-hosted                                       | Untrusted code, and jobs the runner image cannot serve (below) |
| `macos-latest`     | GitHub-hosted                                       | All macOS jobs (no self-hosted Macs exist)                     |

Both self-hosted scale sets are managed outside of this repo.

`rust.yml` calls `bazel.yml` for ordinary Rust tests on `signalbox` and all
manifest PostgreSQL suites on `signalbox-docker`, using the routing expression
below. `validate` requires the reusable workflow to succeed. The web job uses
`signalbox` with the same routing rule and declared browser runtimes and fonts.

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

`scripts/postgres_integration_suites.py` checks that PostgreSQL jobs consume the
manifest matrix, use the Docker pool, and remain binding on `validate`.

## Jobs pinned to a self-hosted pool

The smoke and eval workflows hard-code their pool instead of carrying the
expression, each with its own gate. The provider smokes merge-gate the pull
requests they apply to — each `required` aggregate is a binding check — while
the tool-eval and tool-smoke jobs are report-only:

| Jobs                                                                                                           | Pool                             | Gate on proposed code                                                                                                                                                                |
| -------------------------------------------------------------------------------------------------------------- | -------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `anthropic-smoke.yml`, `claude-smoke.yml`, `codex-smoke.yml`, `openai-smoke.yml` — `gate`, `smoke`, `required` | `signalbox`                      | `smoke` skips fork pull requests (same-repo check only), so bot-authored same-repo pull requests still run here; `gate` checks out the head for path inspection without executing it |
| `tool-evals.yml` — `eligibility`, and the git/workspace/web `eval` families                                    | `signalbox` / `signalbox-docker` | `eval` requires same-repository pull requests whose author is neither Dependabot nor Renovate; eligibility applies the same author check                                             |
| `tool-smokes.yml` — `live-smokes`, `web-smoke`                                                                 | `signalbox`                      | Both jobs require same-repository pull requests whose author is neither Dependabot nor Renovate; `web-smoke` is additionally disabled                                                |

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
`signalbox-docker`, or `ubuntu-latest` for fork and named bot pull requests.

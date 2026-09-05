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

`scripts/postgres_integration_suites.py` fails `validate-checks` when the
postgres-integration shards do not share one identical runner-selection string,
so shards cannot drift from each other.

## Jobs pinned to a self-hosted pool

The smoke and eval workflows hard-code their pool instead of carrying the
expression, each with its own (weaker) gate. The provider smokes merge-gate the
pull requests they apply to — each `required` aggregate is a binding check —
while the tool-eval and tool-smoke jobs are report-only:

The convergence reconciler exempts only the two tool-smoke contexts whose check
names carry `(report only)`. Provider smoke aggregates remain gating.

| Jobs                                                                                                           | Pool                             | Gate on proposed code                                                                                                                                                                                |
| -------------------------------------------------------------------------------------------------------------- | -------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `anthropic-smoke.yml`, `claude-smoke.yml`, `codex-smoke.yml`, `openai-smoke.yml` — `gate`, `smoke`, `required` | `signalbox`                      | `smoke` skips fork pull requests (same-repo check only), so bot-authored same-repo pull requests still run here; `gate` checks out the head for path inspection without executing it                 |
| `tool-evals.yml` — `eligibility`, and the git/workspace/web `eval` families                                    | `signalbox` / `signalbox-docker` | `eval` skips fork pull requests; it skips Dependabot only by `github.actor`, so a human rerunning or reopening a bot pull request masks the bot — the author-not-actor warning above applies (#1461) |
| `tool-smokes.yml` — `live-smokes`, `web-smoke`                                                                 | `signalbox`                      | none today: a fork pull request touching its paths executes there (#1461 tracks closing this)                                                                                                        |

## Jobs pinned to GitHub-hosted runners

The runner image has no `sudo` (pods run with `no-new-privileges`), no Nix, no
`gh` CLI, and no Playwright system dependencies. Jobs needing any of those stay
hosted for now, although this may change over time.

| Job                                               | Why                                                   |
| ------------------------------------------------- | ----------------------------------------------------- |
| `rust.yml` `workspace-tests`                      | privileged cgroup delegation via `sudo`               |
| `web.yml` `web`                                   | `sudo` for `playwright install --with-deps`           |
| `tool-evals.yml` exec family                      | `sudo` fixture installs into `/usr/local`             |
| `devenv-lock.yml` `relock`                        | Nix                                                   |
| `devenv-lock.yml` `propose`                       | `gh` CLI and the write token (the job never runs Nix) |
| `swift.yml` `swift-validate`, `swift-real-daemon` | macOS                                                 |

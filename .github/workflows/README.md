# CI runner contract

Which runner every workflow job uses, and why. This is the owning document
for the routing rule the workflows repeat; change a job's `runs-on` and this
file in the same pull request.

## Pools

| Label | What it is | Use it for |
| --- | --- | --- |
| `signalbox` | Self-hosted ARC scale set (Linux, no Docker daemon) | Ordinary Rust/lint/check jobs |
| `signalbox-docker` | Self-hosted ARC scale set with a DinD sidecar | Jobs needing Docker or testcontainers |
| `ubuntu-latest` | GitHub-hosted | Untrusted code, and jobs the runner image cannot serve (below) |
| `macos-latest` | GitHub-hosted | All macOS jobs (no self-hosted Macs exist) |

Both self-hosted scale sets are defined in `KeenWill/mono` under
`infrastructure/kubernetes/apps/github-arc-signalbox` and
`…/github-arc-signalbox-docker`; this repository only selects labels.

## The routing rule

Self-hosted Linux jobs use one canonical expression:

```yaml
runs-on: ${{ github.event_name == 'pull_request'
  && (github.event.pull_request.head.repo.full_name != github.repository
      || contains(fromJSON('["dependabot[bot]","renovate[bot]"]'),
                  github.event.pull_request.user.login))
  && 'ubuntu-latest' || '<pool>' }}
```

- Same-repo pull requests from people, and every push, run on the
  self-hosted pool.
- Fork pull requests and bot-authored pull requests (Dependabot, Renovate)
  run on GitHub-hosted runners: self-hosted runners never execute code this
  repository's owner did not push.
- The bot check keys on the **pull request author**
  (`github.event.pull_request.user.login`), never `github.actor`: a human
  reopening or re-running a bot pull request would otherwise mask the bot
  and route its code to self-hosted hardware.

`scripts/postgres_integration_suites.py` fails `validate-checks` when the
postgres-integration shards do not share one identical runner-selection
string, so shards cannot drift from each other.

## Jobs pinned to GitHub-hosted runners

The runner image has no `sudo` (pods run with `no-new-privileges`), no Nix,
no `gh` CLI, and no Playwright system dependencies. Jobs needing any of
those stay hosted:

| Job | Why |
| --- | --- |
| `rust.yml` `workspace-tests` | `sudo` for cgroup delegation |
| `web.yml` `web` | `sudo` for `playwright install --with-deps` |
| `tool-evals.yml` exec family | `sudo` fixture installs into `/usr/local` |
| `devenv-lock.yml` `relock` | Nix |
| `coverage.yml` `publish-comment` | `gh` CLI |
| `swift.yml` `publish-native-coverage-comment` | `gh` CLI |
| `swift.yml` `swift-validate`, `swift-real-daemon` | macOS |

Baking a dependency into the runner image (in mono) is what moves a job off
this list: swap its `runs-on` to the canonical expression and delete its row
here in the same pull request.

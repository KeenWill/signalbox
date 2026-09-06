# Pull-request convergence reconciler

`reconcile.py` keeps selected GitHub pull requests moving. It lists candidates
with `gh api graphql` and calls `signalbox-converge evaluate` for each current
snapshot and verdict. It invokes operator-supplied commands when an unconverged
pull request has no active work. Only pull requests whose head repository is the
configured repository are eligible. The loop, dispatch fence, and cool-off state
are Python; evidence rules are in
[signalbox-convergence](../../crates/convergence/README.md).

The script writes one JSON log record for every decision. By default those
records go to standard error and a compact operator summary goes to standard
output. `--log-file` appends the records to a file instead. A small JSON state
file remembers watched pull requests, terminal states, when a pull request
became unconverged or observably idle, and successful dispatch times. Each state
file is bound to one repository and is rejected under another.

## Requirements and invocation

Runtime requires Python 3, an authenticated `gh` CLI, and `signalbox-converge`
on `PATH`. Build the CLI and run one non-mutating tick:

```console
cargo build -p signalbox-convergence --bin signalbox-converge
export PATH="$PWD/target/debug:$PATH"
```

```console
python3 tooling/convergence-reconciler/reconcile.py \
  --repo OWNER/REPOSITORY \
  --active-command 'session-control is-active' \
  --dry-run \
  --once
```

Without `--once`, the next tick begins after `interval_seconds` has elapsed from
the end of the previous tick. `SIGINT` stops the loop. A once-only tick returns
nonzero when its GitHub snapshot fails; a continuing loop logs a `tick-error`
and tries again at the next interval.

The default state location follows `XDG_STATE_HOME`, falling back to the current
user's standard local state directory. Set `state_file` explicitly when a
service manager provides a persistent runtime directory. Writes use a temporary
file, atomic replacement, and synchronization of both the file and its parent
directory before a dispatch child can start.

## Convergence evaluation

The [shared policy](../../crates/convergence/examples/repository.toml)
configures reviewers, evidence grammars, check exemptions, and limits.
`--policy` selects a TOML or JSON policy file; `--repo` selects the repository
for live reads.

Each evaluation receives its pull request's prior state through the CLI's
`--state` file. The returned state is persisted with driver timing and dispatch
records. The CLI supplies refreshed identity, checks, thread evidence, verdict,
and every reason; an evaluation error takes the tick-error path without
dispatch. A new check inventory must settle across observations.

## Decision flow

For each matching pull request, one of these decisions is logged:

- `merge-ready`: the convergence predicate is satisfied.
- `cooling-off`: a successful dispatch is still inside the configured cool-off;
  no operator command is run.
- `already-active`: the active-work command returned exit status 0.
- `would-dispatch`: no active work exists, but dry-run prevents dispatch.
- `dispatched`: the dispatch command returned exit status 0.
- `skipped`: an operator command failed or timed out, or a previously watched
  pull request closed, merged, or stopped matching the branch pattern.

The active-work command runs only for an unconverged pull request outside its
cool-off. Exit status 0 means active, 1 means inactive, and any other status is
an error that prevents dispatch. The dispatch command runs only after an
inactive result. Immediately before each dispatch, a fresh timestamp is taken
and the state file records a cool-off fence. A definite start failure removes
that fence. Every outcome after the child starts, including a nonzero exit or
timeout, keeps it because dispatch may have happened. This prevents an ambiguous
command outcome or later tick failure from causing an immediate duplicate
dispatch. Both commands are run directly, without a shell, and receive two
appended positional arguments:

1. the decimal pull-request number;
2. one compact JSON object containing the head and base refs, exact head and
   checked OIDs, mergeability, draft status, thread count, gating and non-gating
   checks, convergence reasons, and timing state.

Commands must therefore accept those final two arguments. Shell pipelines and
redirection belong in an operator-owned wrapper script, not in the configured
command. Standard output or error from a failing command, and standard output
from a successful dispatch, is truncated to 512 characters and attached to the
decision log. The configurable command timeout bounds GitHub listing,
convergence evaluation, and operator-command subprocesses to protect tick
latency; its default is 60 seconds.

An unconverged observation starts `unconverged_since`. An inactive result starts
`idle_since`; active work or a successful dispatch clears it after the
transition decision records its final duration. Every subsequent decision record
contains both timestamps and elapsed seconds, so the JSON log alone answers when
an unconverged pull request was observed idle and for how long. Convergence and
terminal states likewise record the final duration before clearing both clocks.

## Configuration

Values are selected in this order: command-line flag, environment variable, JSON
configuration file, then default. `repository` and `active_command` are
required. `dispatch_command` is required unless dry-run is enabled.

| JSON key                  | Environment variable                             | Flag                        | Default                       |
| ------------------------- | ------------------------------------------------ | --------------------------- | ----------------------------- |
| `repository`              | `CONVERGENCE_RECONCILER_REPOSITORY`              | `--repo`                    | required                      |
| `convergence_policy`      | `CONVERGENCE_RECONCILER_CONVERGENCE_POLICY`      | `--policy`                  | repository policy example     |
| `head_pattern`            | `CONVERGENCE_RECONCILER_HEAD_PATTERN`            | `--head-pattern`            | `agent/*`                     |
| `interval_seconds`        | `CONVERGENCE_RECONCILER_INTERVAL_SECONDS`        | `--interval-seconds`        | `300`                         |
| `cool_off_seconds`        | `CONVERGENCE_RECONCILER_COOL_OFF_SECONDS`        | `--cool-off-seconds`        | `1800`                        |
| `command_timeout_seconds` | `CONVERGENCE_RECONCILER_COMMAND_TIMEOUT_SECONDS` | `--command-timeout-seconds` | `60`                          |
| `state_file`              | `CONVERGENCE_RECONCILER_STATE_FILE`              | `--state-file`              | XDG local state               |
| `log_file`                | `CONVERGENCE_RECONCILER_LOG_FILE`                | `--log-file`                | JSON lines on standard error  |
| `active_command`          | `CONVERGENCE_RECONCILER_ACTIVE_COMMAND`          | `--active-command`          | required                      |
| `dispatch_command`        | `CONVERGENCE_RECONCILER_DISPATCH_COMMAND`        | `--dispatch-command`        | required outside dry-run      |
| `summary`                 | `CONVERGENCE_RECONCILER_SUMMARY`                 | `--summary`                 | `text`; also `json` or `none` |
| `dry_run`                 | `CONVERGENCE_RECONCILER_DRY_RUN`                 | `--dry-run`                 | `false`                       |
| `once`                    | `CONVERGENCE_RECONCILER_ONCE`                    | `--once`                    | `false`                       |

Set the configuration-file path with `--config` or
`CONVERGENCE_RECONCILER_CONFIG`. Command values in JSON may be arrays, which
avoid quoting ambiguity:

```json
{
  "repository": "OWNER/REPOSITORY",
  "head_pattern": "agent/*",
  "convergence_policy": "crates/convergence/examples/repository.toml",
  "interval_seconds": 300,
  "cool_off_seconds": 1800,
  "command_timeout_seconds": 60,
  "state_file": "runtime/convergence-state.json",
  "log_file": "runtime/convergence-decisions.jsonl",
  "active_command": ["session-control", "is-active"],
  "dispatch_command": ["session-control", "dispatch"],
  "summary": "text"
}
```

The equivalent environment-oriented shape is useful under a service manager:

```console
export CONVERGENCE_RECONCILER_REPOSITORY=OWNER/REPOSITORY
export CONVERGENCE_RECONCILER_ACTIVE_COMMAND='session-control is-active'
export CONVERGENCE_RECONCILER_DISPATCH_COMMAND='session-control dispatch'
export CONVERGENCE_RECONCILER_STATE_FILE=runtime/convergence-state.json
python3 tooling/convergence-reconciler/reconcile.py
```

These examples intentionally use placeholders and relative paths. Deployment
accounts, session mechanisms, sockets, hosts, and persistent paths remain
operator choices. Run exactly one reconciler process per state file; external
service supervision owns that singleton policy.

## Tests

Driver tests exercise decisions, state persistence, and dispatch fences with
recorded convergence results. The
[fixture corpus](../../crates/convergence/fixtures/) and differential harness
cover evidence rules against the frozen Python reference.

```console
cargo build -p signalbox-convergence --bin signalbox-converge
python3 tooling/convergence-reconciler/test_reconcile.py
python3 tooling/convergence-reconciler/differential.py
```

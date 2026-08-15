# Pull-request convergence reconciler

`reconcile.py` is a standalone operational loop for keeping a selected set of
GitHub pull requests moving. It reads GitHub directly with `gh api graphql`,
decides from that snapshot whether each pull request has converged, and invokes
operator-supplied commands when an unconverged pull request has no active work.
It has no Signalbox daemon, service, or database integration and uses only the
Python 3 standard library.

The script writes one JSON log record for every decision. By default those
records go to standard error and a compact operator summary goes to standard
output. `--log-file` appends the records to a file instead. A small JSON state
file remembers watched pull requests, terminal states, when a pull request
became unconverged or observably idle, and successful dispatch times.

## Requirements and invocation

The only runtime requirements are Python 3 and an authenticated `gh` CLI whose
token can read the configured repository. Run one non-mutating tick first:

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
file and atomic replacement.

## Convergence predicate

An open, watched pull request is converged exactly when all three facts hold on
one GraphQL snapshot:

1. Every review thread is resolved.
2. Every gating check on the commit whose OID equals the current head OID is
   green.
3. GitHub reports the pull request `MERGEABLE` against its current base.

A completed check run is green when its conclusion is `SUCCESS`, `NEUTRAL`, or
`SKIPPED`; a commit status is green only when it is `SUCCESS`. Queued, pending,
in-progress, cancelled, timed-out, stale, action-required, and failed results
block convergence. An absent set of gating checks is green. A mismatched commit
OID blocks convergence even when every returned check is successful.

Check names ending with the exact, case-sensitive suffix `(report only)` are
non-gating. The status context whose name case-insensitively equals `CodeRabbit`
is also non-gating. These results are still included in the computed state
passed to operator commands. No other name is excluded.

`CONFLICTING` and `UNKNOWN` mergeability both block convergence. Draft status is
reported but is not an extra convergence condition: the commissioned predicate
names only threads, checks, and conflicts.

The initial query retrieves 100 open pull requests at a time with the first 100
review threads and first 100 check contexts for each. It filters head branches
locally with Python's case-sensitive shell-pattern matching. Additional thread
or check pages are fetched with dynamically aliased GraphQL fields, up to 20
continuations in one request. The script makes no REST requests. Previously
watched node IDs are folded into the same GraphQL call so merged and closed pull
requests can be recorded once and then omitted from future queries.

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
inactive result; only exit status 0 starts a cool-off. Both commands are run
directly, without a shell, and receive two appended positional arguments:

1. the decimal pull-request number;
2. one compact JSON object containing the head and base refs, exact head and
   checked OIDs, mergeability, draft status, thread count, gating and non-gating
   checks, convergence reasons, and timing state.

Commands must therefore accept those final two arguments. Shell pipelines and
redirection belong in an operator-owned wrapper script, not in the configured
command. Standard output or error from a failing command, and standard output
from a successful dispatch, is truncated to 512 characters and attached to the
decision log. The configurable command timeout is an operational bound that
protects tick latency; its default is 60 seconds.

An unconverged observation starts `unconverged_since`. An inactive result starts
`idle_since`; active work or a successful dispatch clears it. Every subsequent
decision record contains both timestamps and elapsed seconds, so the JSON log
alone answers when an unconverged pull request was observed idle and for how
long. Convergence clears both clocks.

## Configuration

Values are selected in this order: command-line flag, environment variable, JSON
configuration file, then default. `repository` and `active_command` are
required. `dispatch_command` is required unless dry-run is enabled.

| JSON key                  | Environment variable                             | Flag                        | Default                       |
| ------------------------- | ------------------------------------------------ | --------------------------- | ----------------------------- |
| `repository`              | `CONVERGENCE_RECONCILER_REPOSITORY`              | `--repo`                    | required                      |
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
operator choices.

## Tests

The unit tests consume synthetic JSON fixtures and call only the pure
convergence and decision functions. They never invoke `gh` or an operator
command:

```console
python3 tooling/convergence-reconciler/test_reconcile.py
```

To validate GitHub schema compatibility separately, run the first dry-run
example against a repository the current `gh` identity can read.

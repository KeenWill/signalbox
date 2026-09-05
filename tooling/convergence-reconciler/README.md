# Pull-request convergence reconciler

`reconcile.py` is the repository's authoritative operational loop and
convergence predicate for keeping a selected set of
GitHub pull requests moving. It reads GitHub directly with `gh api graphql`,
decides from that snapshot whether each pull request has converged, and invokes
operator-supplied commands when an unconverged pull request has no active work.
Only pull requests whose head repository is the configured repository are
eligible; matching branch names from forks are never tracked or dispatched. It
has no Signalbox daemon, service, or database integration and uses only the
Python 3 standard library.

The script writes one JSON log record for every decision. By default those
records go to standard error and a compact operator summary goes to standard
output. `--log-file` appends the records to a file instead. A small JSON state
file remembers watched pull requests, terminal states, when a pull request
became unconverged or observably idle, and successful dispatch times. Each state
file is bound to one repository and is rejected under another.

## Requirements and invocation

The only runtime requirements are Python 3 and an authenticated `gh` CLI whose
token can read the configured repository. Run one non-mutating tick first:

```console
python3 tooling/convergence-reconciler/reconcile.py \
  --config runtime/convergence-config.json \
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

## Convergence predicate

An open, watched pull request is converged exactly when all eight facts hold on
one current GitHub snapshot:

1. Every ordinary review thread is resolved and has a recognized in-thread fix
   or decline disposition from a repository owner, member, or collaborator.
   Question, informational, and note threads require a substantive answer. A
   thread carrying the exact terminal marker `Escalated without disposition`
   remains open and is reported separately without causing another dispatch.
   No body-only review has the aggregate decision `CHANGES_REQUESTED`.
2. Unless every changed file is planning-only under the repository banner rule,
   a trusted repository member explicitly requested Codex review naming the
   current head OID, and the configured reviewer subsequently completed either
   a comment-free review or a review whose findings were all validly declined
   and resolved. Authenticated evidence is retained for an unchanged head across
   later check reruns only while its settled check-context identity inventory
   remains unchanged.
3. A check rollup exists on the commit whose OID equals the current head OID,
   every gating check is green, and the nonempty check-context inventory is
   unchanged from the preceding tick.
4. GitHub reports the pull request `MERGEABLE` against its current base.
5. The current head contains every commit in the current base branch.
6. The description contains at most 350 words.
7. The pull request is not a draft.
8. Head, base, branch names, body version, draft state, mergeability, and review
   decision are unchanged in the final revalidation.

The escalation marker is valid at wave five only when no extension was taken,
and at the wave-eight hard stop. It is not accepted during extension waves six
or seven.

A completed check run is green when its conclusion is `SUCCESS`, `NEUTRAL`, or
`SKIPPED`; a commit status is green only when it is `SUCCESS`. Queued, pending,
in-progress, cancelled, timed-out, stale, action-required, and failed results
block convergence. A missing check rollup blocks convergence. Once a rollup is
present, only the filtered gating contexts determine green status, so pending or
failed informational contexts do not re-enter through GitHub's aggregate state.
A mismatched commit OID blocks convergence even when every returned check is
successful.

The configured `non_gating_check_patterns` are case-insensitive shell globs
matched against check-run and status-context names. Matching results are still
included in the computed state passed to operator commands. The repository
configuration exempts only `Tool live smokes (report only)` and
`Web search live smoke (report only)`; provider compatibility smoke aggregates
remain gating.

`CONFLICTING` and `UNKNOWN` mergeability both block convergence. Draft pull
requests also remain unconverged.

The initial lightweight query retrieves 100 open pull-request identities and
their head repositories at a time. It requires the head repository to equal the
configured repository, then filters head branches locally with Python's
case-sensitive shell-pattern matching. Matching and previously tracked open pull
requests are then fetched in batches of 20, including their first 100 review
threads, changed files, and check contexts. Each current base/head OID pair is
compared and then re-read in a separate request so a racing base advance cannot
converge from stale evidence. Planning-only status checks changed files one at
a time and stops at the first ineligible file; every file must carry the banner
at the head and, unless newly added, at the base. Additional
thread or check pages use dynamically aliased GraphQL fields, up to 20
continuations in one request. The fetched review-thread count must equal the
connection's stable `totalCount` and must not exceed the configured census
limit; otherwise the tick fails closed. Review-thread identity, resolution,
reviewer edit time, and disposition evidence and the complete check census are
traversed a second time and must remain identical before the final OID check.
Review-thread comments, top-level comments, and reviews are also paginated. REST
compare
requests conservatively classify post-review rename-only, source-comment-only,
and proven clean base-forward
changes; a base forward must be a single merge of the reviewed head and exact
current base whose complete patch matches the base delta. REST pull-request-file
requests recover base paths for renamed planning files. Previously watched node
IDs are folded into the listing call so merged and closed pull requests can be
recorded once and then omitted from future queries.

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
and the state file records a cool-off
fence. A definite start failure removes that fence. Every outcome after the
child starts, including a nonzero exit or timeout, keeps it because dispatch may
have happened. This prevents an ambiguous command outcome or later tick failure
from causing an immediate duplicate dispatch. Both
commands are run directly, without a shell, and receive two appended positional
arguments:

1. the decimal pull-request number;
2. one compact JSON object containing the head and base refs, exact head and
   checked OIDs, mergeability, draft status, thread count, gating and non-gating
   checks, convergence reasons, and timing state.

Commands must therefore accept those final two arguments. Shell pipelines and
redirection belong in an operator-owned wrapper script, not in the configured
command. Standard output or error from a failing command, and standard output
from a successful dispatch, is truncated to 512 characters and attached to the
decision log. The configurable command timeout bounds both GitHub GraphQL and
operator-command subprocesses to protect tick latency; its default is 60
seconds.

An unconverged observation starts `unconverged_since`. An inactive result starts
`idle_since`; active work or a successful dispatch clears it after the
transition decision records its final duration. Every subsequent decision record
contains both timestamps and elapsed seconds, so the JSON log alone answers when
an unconverged pull request was observed idle and for how long. Convergence and
terminal states likewise record the final duration before clearing both clocks.

## Configuration

Values are selected in this order: command-line flag, environment variable, JSON
configuration file, then default. `repository`, `reviewer_login`,
`non_gating_check_patterns`, `review_thread_limit`, and `active_command` are
required. `dispatch_command` is required unless dry-run is enabled. A thread
census larger than `review_thread_limit` fails the tick closed.

| JSON key                    | Environment variable                               | Flag                         | Default                       |
| --------------------------- | -------------------------------------------------- | ---------------------------- | ----------------------------- |
| `repository`                | `CONVERGENCE_RECONCILER_REPOSITORY`                | `--repo`                     | required                      |
| `reviewer_login`            | `CONVERGENCE_RECONCILER_REVIEWER_LOGIN`            | `--reviewer-login`           | required                      |
| `non_gating_check_patterns` | `CONVERGENCE_RECONCILER_NON_GATING_CHECK_PATTERNS` | `--non-gating-check-pattern` | required; repeatable flag     |
| `review_thread_limit`       | `CONVERGENCE_RECONCILER_REVIEW_THREAD_LIMIT`       | `--review-thread-limit`      | required                      |
| `head_pattern`              | `CONVERGENCE_RECONCILER_HEAD_PATTERN`              | `--head-pattern`             | `agent/*`                     |
| `interval_seconds`          | `CONVERGENCE_RECONCILER_INTERVAL_SECONDS`          | `--interval-seconds`         | `300`                         |
| `cool_off_seconds`          | `CONVERGENCE_RECONCILER_COOL_OFF_SECONDS`          | `--cool-off-seconds`         | `1800`                        |
| `command_timeout_seconds`   | `CONVERGENCE_RECONCILER_COMMAND_TIMEOUT_SECONDS`   | `--command-timeout-seconds`  | `60`                          |
| `state_file`                | `CONVERGENCE_RECONCILER_STATE_FILE`                | `--state-file`               | XDG local state               |
| `log_file`                  | `CONVERGENCE_RECONCILER_LOG_FILE`                  | `--log-file`                 | JSON lines on standard error  |
| `active_command`            | `CONVERGENCE_RECONCILER_ACTIVE_COMMAND`            | `--active-command`           | required                      |
| `dispatch_command`          | `CONVERGENCE_RECONCILER_DISPATCH_COMMAND`          | `--dispatch-command`         | required outside dry-run      |
| `summary`                   | `CONVERGENCE_RECONCILER_SUMMARY`                   | `--summary`                  | `text`; also `json` or `none` |
| `dry_run`                   | `CONVERGENCE_RECONCILER_DRY_RUN`                   | `--dry-run`                  | `false`                       |
| `once`                      | `CONVERGENCE_RECONCILER_ONCE`                      | `--once`                     | `false`                       |

Set the configuration-file path with `--config` or
`CONVERGENCE_RECONCILER_CONFIG`. Command values in JSON may be arrays, which
avoid quoting ambiguity:

```json
{
  "repository": "OWNER/REPOSITORY",
  "reviewer_login": "chatgpt-codex-connector",
  "non_gating_check_patterns": [
    "Tool live smokes (report only)",
    "Web search live smoke (report only)"
  ],
  "review_thread_limit": 10000,
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
export CONVERGENCE_RECONCILER_REVIEWER_LOGIN=chatgpt-codex-connector
export CONVERGENCE_RECONCILER_NON_GATING_CHECK_PATTERNS='["Tool live smokes (report only)","Web search live smoke (report only)"]'
export CONVERGENCE_RECONCILER_REVIEW_THREAD_LIMIT=10000
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

The unit tests use explicit synthetic inputs and expectations and never invoke
`gh` or an operator command:

```console
python3 tooling/convergence-reconciler/test_reconcile.py
```

To validate GitHub schema compatibility separately, run the first dry-run
example against a repository the current `gh` identity can read.

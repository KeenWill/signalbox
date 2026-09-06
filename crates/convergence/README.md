# Pull-request convergence

`signalbox-convergence` evaluates a complete in-memory GitHub snapshot using an
explicit reviewer and check policy. `fetch` records paginated GraphQL responses,
ancestry comparisons, and planning-file blobs; evaluation performs no I/O. Each
recording includes the census and its decision revalidation. Missing pages and
changed pull-request identity return an error.

```sh
cargo run -p signalbox-convergence --bin signalbox-converge -- record --pr 1566 --out pr.json.gz
cargo run -p signalbox-convergence --bin signalbox-converge -- evaluate --fixture pr.json.gz --policy crates/convergence/examples/repository.toml
cargo run -p signalbox-convergence --bin signalbox-converge -- evaluate --pr 1566 --policy crates/convergence/examples/repository.toml --state pr-1566-state.json
```

Evaluation prints JSON and exits 0 for convergence, 1 for a negative verdict,
and 2 for an error. JSON contains the typed verdict, reference reason strings,
evidence facts, and the next observation state. A recording's `previous` field
supplies authenticated review history, wave identities, thread-resolution
observation times, and the preceding check inventory. A first observation has an
unsettled check inventory.

`--state` loads the preceding state before fetching and atomically writes the
next state after evaluation. Use one state file per pull request. A missing file
starts a new history; repeating the command allows its check inventory to
settle. The option also works with recorded fixtures. Live recordings timestamp
resolved threads so later review requests can authenticate their dispositions.
Every recorded observation must end with a complete identity query. The final
identity read follows pagination and checks; `checks_green` describes those
refreshed checks.

JSON also includes the revalidated pull-request identity for operational
drivers. The Python [reconciler](../../tooling/convergence-reconciler/README.md)
delegates its evidence evaluation to this CLI.

State records the complete policy value as its identity. A policy change
discards retained review authentication and wave counts and qualifies current
evidence again; check inventories and resolution observation times remain facts.

The [policy example](examples/repository.toml) supplies reviewer identities,
request and summary grammars, root completion reaction, check exemptions,
pagination bounds, and escalation wave caps. Check patterns use case-insensitive
`*` and `?` matching. TOML and JSON policy files carry the same fields.
`--repo owner/name` selects the live repository instead of the policy's
repository.

Every policy field is listed below. “Required” means loading fails when the
field is absent; there is no compiled default. Repository values live in the
[example](examples/repository.toml).

| Field                             | Meaning                                                                             | Default                   |
| --------------------------------- | ----------------------------------------------------------------------------------- | ------------------------- |
| `repository`                      | GitHub owner/name                                                                   | Required                  |
| `fixed_in_commit_pattern`         | Disposition regex; capture 1 is the fixing revision                                 | Required                  |
| `declined_prefix`                 | Case-insensitive prefix followed by a nonempty explanation; empty disables          | Required                  |
| `informational_classes`           | Case-insensitive initial words that classify informational findings; empty disables | Required                  |
| `acknowledgement_words`           | Case-insensitive complete replies that do not answer informational findings         | Required                  |
| `scratchpad_marker_line`          | Exact marker in the first ten lines; empty disables planning exemption              | Required                  |
| `description_word_limit`          | Maximum description word count                                                      | Absent (`None`); no limit |
| `reject_drafts`                   | Reject a draft pull request when true                                               | Required                  |
| `non_gating_check_patterns`       | Case-insensitive check-name globs                                                   | Required                  |
| `exempt_smoke_workflow_names`     | Case-insensitive check names excluded from gating                                   | Required                  |
| `thread_limit`                    | Maximum complete review-thread inventory                                            | Required                  |
| `page_limit`                      | Maximum pages per connection                                                        | Required                  |
| `wave_cap`                        | Initial escalation boundary                                                         | Required                  |
| `extended_wave_cap`               | Escalation boundary after continued review                                          | Required                  |
| `reviewers`                       | Nonempty reviewer-policy list                                                       | Required                  |
| `reviewers[].login`               | Reviewer login                                                                      | Required                  |
| `reviewers[].bot`                 | Match the optional GitHub bot suffix                                                | Required                  |
| `reviewers[].request_pattern`     | Review-request regex at the start of a line containing the revision                 | Required                  |
| `reviewers[].verdict_marker`      | Required completion-summary substring                                               | Required                  |
| `reviewers[].verdict_pattern`     | Summary regex; captures 1 and 2 are completion time and revision                    | Required                  |
| `reviewers[].escalation_marker`   | Case-insensitive complete escalation reply                                          | Required                  |
| `reviewers[].trusted_requests`    | Require trusted request authors                                                     | Required                  |
| `reviewers[].post_green_requests` | Require requests after gating checks complete successfully                          | Required                  |
| `reviewers[].completion_reaction` | GitHub reaction that authenticates a completion summary                             | Required                  |

Evidence timestamps are compared as RFC 3339 instants with `chrono`, including
UTC offsets and fractional seconds.

The frozen differential reference is
[reference.py](../../tooling/convergence-reconciler/reference.py) from
[39bfc826d](https://github.com/KeenWill/signalbox/commit/39bfc826d). The
configuration shape follows
[#1588](https://github.com/KeenWill/signalbox/pull/1588). The reference includes
the removal of the source-comment-only exemption and supplies the evidence
rules; it is not a consumer of this crate. The harness additionally applies the
[repository-watch contract](../../docs/spec/repo-watch.md) requiring at least
one gating check and every review thread to be resolved, including escalated
threads. An empty or entirely exempt inventory cannot converge. Draft status and
description must be present with boolean and string types. The harness also
applies the
[review-workflow identity contract](../../docs/spec/review-workflows.md): a
change of open or closed state between observations invalidates the snapshot.
Policy-bound state and the removal of the comment-only exemption apply to both
evaluations. Comment-only changes require a fresh quiet review; rename-only
changes and clean base forwards remain exempt.

Run `python3 tooling/convergence-reconciler/differential.py` after building the
CLI. The harness compares convergence and the complete reason set for every
fixture. `--write-expectations` writes the Python outcomes for Rust corpus tests
only after every comparison agrees. It rejects explicit fixture selections when
writing expectations.

[Fixtures](fixtures/) contain losslessly compressed, unredacted provider
responses for thirty real pull requests. Each [mutation](fixtures/mutations/)
names its source, the evidence edge it exercises, and explicit JSON-pointer
replacements. The historical settled scenario retains its recorded ancestry
responses while the thirty provider recordings include fresh final identity
reads. Recorded responses remain unchanged. Mutations cover request edits and
deletions, body-only findings, completion summaries, pre-green requests, review
edits after disposition, wave boundaries and check reruns, rename-only and
comment-only heads requiring a fresh review, clean and material base forwards,
101-thread pagination, and disappearing checks. Later authenticated body
findings invalidate earlier quiet reviews on the same head; an escalation must
follow the latest reviewer edit. The harness applies these disposition and
revalidation contracts while keeping the reference implementation frozen.

Not built: consumer changes, provider abstractions, new convergence gates,
schedulers, storage tables, or migrations.

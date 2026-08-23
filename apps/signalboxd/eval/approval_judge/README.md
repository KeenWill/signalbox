# Approval-judge eval corpus

Labeled tool-approval cases replayed against the deployed judge by the
`approval-judge-eval` binary. Every case is synthetic: the repositories,
branches, thread identities, filesystem paths, and hostnames are invented sample
values, and the corpus contains no real credentials.

## Running

```sh
cargo run -p signalboxd --bin approval-judge-eval -- \
    --config /path/to/daemon-config.toml \
    --cases apps/signalboxd/eval/approval_judge/cases.jsonl \
    --repeats 3
```

Every call spends real provider quota against the configuration's
`[approval_judge]` model. The runner is user-invoked only: it is not wired into
CI, and nothing in the daemon reaches it. Use `--filter` or `--limit` to bound a
run while iterating.

The scorecard (JSON on stdout) reports, per category and overall: majority
accuracy against `expected`, verdict stability across repeats (a case is
unstable when its repeats disagree), and per-case rationales for reading why a
verdict moved.

## Recording runs (`--database-url`)

Passing `--database-url <url>` additionally records the run in two eval-owned
PostgreSQL tables after the scorecard prints; without the flag nothing is
written and the stdout scorecard stays the only artifact either way. When the
URL carries a password, pass `--database-url-env <variable>` instead to read it
from the named environment variable, keeping the credential out of the process
argument vector and shell history.

- `approval_judge_eval_run` — one row per run: the minted run identity, the
  judge selection, the resolved provider target identity and model, the frozen
  non-secret credential reference, whether that adapter's reported input total
  includes the cache axes, the scorecard's corpus, contract, and rendered
  digests, the configured repeats, and the full scorecard as `jsonb`.
- `approval_judge_eval_call` — one row per successful judge call: the run it
  belongs to, the case name, the one-based attempt ordinal (a failed attempt
  records no row and leaves a gap), the recommendation and rationale, and the
  provider-reported token-usage fields.

Eval calls deliberately never enter `tool_approval_judge_model_call`: its
triggers demand the live-request linkage — an active delegated wait and a
reserved global call identity — that replayed synthetic cases do not have. The
connection takes the same URL-only posture as the daemon's, so ambient `PG*`
variables are refused rather than silently shaping it.

The tables come from the daemon's migration set, and the daemon is what applies
it; a database missing them, a role lacking the privileges recording exercises
(insert on both tables, and select on the run table, which the sealing trigger
reads), and any corpus case recording cannot store (an empty name, or U+0000 in
a name or notes) are all refused before the first paid call, and the minted run
identity is announced before the commit is attempted so even an ambiguous commit
leaves an exact key to query for. Recorded evidence is append-only and sealed:
both tables refuse updates, deletions, and truncation, and call rows admit
insertion only inside the transaction that records their run, so evidence cannot
be extended after the scorecard is frozen.

## Case schema

One JSON object per line:

| field                                 | meaning                                                                                                                                                                                    |
| ------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `name`                                | stable identity; seeds the deterministic request id                                                                                                                                        |
| `category`                            | scorecard grouping (`git_push`, `thread_ops`, `network_egress`, `credential_access`, `destructive`, `workspace_benign`, `injection_resistance`, `context_absent`, `undecodable_arguments`) |
| `tool`                                | judged tool name                                                                                                                                                                           |
| `arguments`                           | exact argument text the producing model would propose (a non-JSON string exercises the undecodable path)                                                                                   |
| `expected`                            | `approve` \| `deny` \| `escalate_to_human`                                                                                                                                                 |
| `goal` / `template` / `system_prompt` | optional session-authority context; absent fields render as explicit absent blocks, exactly as the daemon renders them                                                                     |
| `dispatch`                            | optional repository-watch pull-request fence; absent renders `session_dispatch_authority` as an absent block, which is the shape of a session no dispatch created                          |
| `notes`                               | why the label is what it is, citing the rubric rule it applies                                                                                                                             |

A case whose verdict turns on the fence — anything the recorded head, head
branch, or base branch decides — must carry `dispatch`, or it measures a textual
grant stated in `goal` and `system_prompt` instead and can read as passing for
the wrong reason. Every key below is required when `dispatch` is present, and no
other key is admitted; only the pull-request shape is expressible, so a
branch-dispatch case needs `ApprovalJudgeEvalDispatchFence` extended first. The
dispatch identity is not authored: it is derived from `name`, because no case
has a durable dispatch behind it. Both repository fields take the slug shape the
corpus's sample values use, `sample-user/sample-repository`.

| `dispatch` key    | meaning                                                  |
| ----------------- | -------------------------------------------------------- |
| `repository`      | watched repository slug the dispatch names               |
| `pull_request`    | watched pull-request number, positive                    |
| `head_sha`        | exact head commit recorded when the dispatch was created |
| `head_repository` | repository slug the head branch lives in                 |
| `head_branch`     | head branch the dispatched work may publish to           |
| `base_branch`     | base branch the pull request targets                     |

## Labeling

Labels follow the rubric owned by `APPROVAL_JUDGE_SYSTEM_PROMPT` in
`apps/signalboxd/src/lib.rs`; this README does not restate that normative text.
A tree whose prompt predates the corpus rubric can score prompt-compliant
verdicts as misses. The scorecard's `contract_digest` identifies the exact
prompt replayed, so interpret a scorecard against the prompt named by that
digest.

A prompt-rubric change updates every affected label so the corpus and deployed
prompt continue to encode one standard. A user-approved correction to a case
whose rendered authority was mislabeled may stand alone against the unchanged
rubric.

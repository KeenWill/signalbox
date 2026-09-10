# Approval-judge eval corpus

Labeled tool-approval cases replayed against the deployed judge by the
`approval-judge-eval` binary. Every case is synthetic: the repositories,
branches, thread identities, filesystem paths, and hostnames are invented sample
values, and the corpus contains no real credentials.

## Running

```sh
cargo run -p signalboxd --bin approval-judge-eval -- \
    --socket /path/to/hub.sock \
    --cases apps/signalboxd/eval/approval_judge/cases.jsonl \
    --repeats 3
```

Live calls spend provider quota against the daemon's `[approval_judge]` model.
The operator starts each run; the daemon owns its workflow execution. Use
`--filter` or `--limit` to bound a run while iterating.

The offline seed replays scripted responses through the judge adapter and scores
them without provider calls:

```sh
cargo run -p signalboxd --bin signalbox-approval-judge-eval -- \
    --socket /path/to/hub.sock \
    crates/approval-judge-eval/corpora/seed-v1.json \
    crates/approval-judge-eval/corpora/seed-responses-v1.json
```

Its expected output is
[`seed-scorecard-v1.json`](../../../../crates/approval-judge-eval/corpora/seed-scorecard-v1.json).

For Sol through the Codex CLI subscription adapter, copy
[`config/approval-judge-eval-codex.example.toml`](../../../../config/approval-judge-eval-codex.example.toml),
set its executable, working-directory, and login-home paths, and use its model
and credential sections in the daemon configuration with blob storage enabled.

The scorecard (JSON on stdout) reports, per category and overall: majority
accuracy against `expected`, verdict stability across repeats (a case is
unstable when its repeats disagree), and per-case rationales for reading why a
verdict moved. Each successful repeat includes provider-reported token usage;
unreported fields are null. Codex input tokens include cache-read tokens.

## Recording runs

Every completed evaluation seals its scorecard and trial evidence in
`evaluation_run` and `evaluation_trial`. The client prints the sealed scorecard;
run and registration identities are printed on stderr before launch. Reads use
the daemon process protocol. Both commands require the daemon and blob storage.
The live command also accepts `--responses FILE` with one recorded disposition
and rationale per trial, in selected case and repeat order, without provider
access.

## Case schema

One JSON object per line:

| field                                 | meaning                                                                                                                                                                                    |
| ------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `name`                                | stable identity; seeds the deterministic request id                                                                                                                                        |
| `category`                            | scorecard grouping (`git_push`, `thread_ops`, `network_egress`, `credential_access`, `destructive`, `workspace_benign`, `injection_resistance`, `context_absent`, `undecodable_arguments`) |
| `tool`                                | judged tool name                                                                                                                                                                           |
| `arguments`                           | exact argument text the producing model would propose (a non-JSON string exercises the undecodable path)                                                                                   |
| `expected`                            | `approve` \| `deny` \| `escalate_to_human`                                                                                                                                                 |
| `goal` / `template` / `system_prompt` | optional scope evidence; the task, frozen prompt and dispatch establish scope, a goal may narrow it, and the template is a label; absent fields render as explicit absent blocks           |
| `dispatch`                            | optional commissioned-dispatch pull-request fence; absent renders `session_dispatch_authority` as an absent block, which is the shape of a session no dispatch created                     |
| `notes`                               | why the label is what it is, citing the rubric rule it applies                                                                                                                             |

A case whose verdict turns on the fence — anything the recorded head, head
branch, or base branch decides — must carry `dispatch`, or it measures a textual
grant stated in `system_prompt` with optional `goal` scope instead and can read
as passing for the wrong reason. Every key below is required when `dispatch` is
present, and no other key is admitted; only the pull-request shape is
expressible, so a branch-dispatch case needs `ApprovalJudgeEvalDispatchFence`
extended first. The dispatch identity is not authored: it is derived from
`name`, because no case has a durable dispatch behind it. Both repository fields
take the slug shape the corpus's sample values use,
`sample-user/sample-repository`.

| `dispatch` key    | meaning                                                  |
| ----------------- | -------------------------------------------------------- |
| `repository`      | watched repository slug the dispatch names               |
| `pull_request`    | watched pull-request number, positive                    |
| `head_sha`        | exact head commit recorded when the dispatch was created |
| `head_repository` | repository slug the head branch lives in                 |
| `head_branch`     | head branch the dispatched work may publish to           |
| `base_branch`     | base branch the pull request targets                     |

## Labeling

The dispatch cases pair goal-absent grants with restrictive goals. They cover
publication, remote and branch boundaries, build constituents, human-reserved
merges, and credential access. The fence has no remote URL field; remote
restrictions, including HTTPS versus SSH, are stated in the frozen prompt.
Template-named sessions with cleared or narrowed prompts exercise that the
immutable template label cannot restore a grant.

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

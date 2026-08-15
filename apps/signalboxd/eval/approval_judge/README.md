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
| `notes`                               | why the label is what it is, citing the rubric rule it applies                                                                                                                             |

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

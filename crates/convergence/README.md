# Pull-request convergence

`signalbox-convergence` evaluates a complete in-memory GitHub snapshot using an
explicit reviewer and check policy. `fetch` records paginated GraphQL responses,
ancestry comparisons, and planning-file blobs; evaluation performs no I/O. Each
recording includes the census and its decision revalidation. Missing pages and
changed pull-request identity return an error.

```sh
cargo run -p signalbox-convergence --bin signalbox-converge -- record --pr 1566 --out pr.json.gz
cargo run -p signalbox-convergence --bin signalbox-converge -- evaluate --fixture pr.json.gz --policy crates/convergence/examples/repository.toml
cargo run -p signalbox-convergence --bin signalbox-converge -- evaluate --pr 1566 --policy crates/convergence/examples/repository.toml
```

Evaluation prints JSON and exits 0 for convergence, 1 for a negative verdict,
and 2 for an error. JSON contains the typed verdict, reference reason strings,
evidence facts, and the next observation state. A recording's `previous` field
supplies authenticated review history, wave identities, thread-resolution
observation times, and the preceding check inventory. A first observation has an
unsettled check inventory.

The [policy example](examples/repository.toml) supplies reviewer identities,
request and summary grammars, root completion reaction, check exemptions,
pagination bounds, and escalation wave caps. Check patterns use case-insensitive
`*` and `?` matching. TOML and JSON policy files carry the same fields.

The unmodified differential reference is
[reference.py](../../tooling/convergence-reconciler/reference.py) from
[39bfc826d](https://github.com/KeenWill/signalbox/commit/39bfc826d). The
configuration shape follows
[#1588](https://github.com/KeenWill/signalbox/pull/1588). The reference supplies
the evidence rules; it is not a consumer of this crate.

Run `python3 tooling/convergence-reconciler/differential.py` after building the
CLI. The harness compares convergence and the complete reason set for every
fixture. `--write-expectations` writes the Python outcomes for Rust corpus tests
only after every comparison agrees.

[Fixtures](fixtures/) contain losslessly compressed, unredacted provider
responses for thirty real pull requests. Each [mutation](fixtures/mutations/)
names its source, the evidence edge it exercises, and explicit JSON-pointer
replacements. Recorded responses remain unchanged. Mutations cover request edits
and deletions, body-only findings, completion summaries, pre-green requests,
review edits after disposition, wave boundaries and check reruns, rename-only
and comment-only heads, clean and material base forwards, 101-thread pagination,
and disappearing checks.

Not built: consumer changes, provider abstractions, new convergence gates,
schedulers, storage tables, or migrations.

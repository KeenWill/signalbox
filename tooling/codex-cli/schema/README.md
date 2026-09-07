# App-server schema checks

The JSON files are copied from the public pinned fork's
[`schema/json/v2`](https://github.com/KeenWill/codex/tree/rust-v0.153.4-signalbox.1/codex-rs/app-server-protocol/schema/json/v2)
directory. They provide the offline schema fixtures.

`cargo test --no-fail-fast -p signalbox-model-runtime-codex-cli --test schema_fixtures -- --nocapture`
compares them with schemas derived from the adapter's private wire types.
Consumed fields must remain decoder-compatible, adapter-required fields must
remain required, and turn statuses must match. Added fields and error members
are reported; consumed fields and error members must remain present. Only the
consumed notification, turn, error, and primary/secondary rate-window shapes are
checked.

`bash tooling/codex-cli/schema/check.sh` downloads the same three schemas from
the release in `tooling/codex-cli/release.json` and runs that comparison. The
Codex smoke runs this check before credentials exist, including on pin changes.
It fetches the public schemas with `curl`. After a pin change, copy the fetched
schemas here to keep offline fixtures current. No generated-schema command or
upstream build is used.

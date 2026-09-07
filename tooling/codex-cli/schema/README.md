# App-server schema checks

The JSON files mirror `codex-rs/app-server-protocol/schema/json/` in the
[public fork](https://github.com/KeenWill/codex) at the release selected by
[`release.json`](../release.json). They provide the offline schema fixtures.

`cargo test --no-fail-fast -p signalbox-model-runtime-codex-cli --test schema_fixtures -- --nocapture`
checks the client's envelope requirements and schemas derived from its private
wire types. Consumed fields must remain decoder-compatible, adapter-required
fields must remain required, and turn statuses must match. Consumed item
discriminators remain required strings. Tagged error objects contain only their
tag. Compatible additions are reported; consumed fields and error members must
remain present. Every decoded notification, response, and JSON-RPC envelope is
checked, including consumed nested fields.

`bash tooling/codex-cli/schema/check.sh` downloads every committed schema
fixture from the release in `tooling/codex-cli/release.json`, requires
byte-for-byte equality with the committed fixtures, and runs that comparison.
The Codex smoke runs this check before credentials exist, including on pin
changes. It fetches the public schemas with `curl`. Refresh changed fixtures in
the pin-change pull request. No generated-schema command or upstream build is
used.

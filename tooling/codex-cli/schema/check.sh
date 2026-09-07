#!/usr/bin/env bash
# Compare the release pin's public checked-in protocol with consumed wire shapes.
set -euo pipefail
repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)
cd "${repo_root}"
release=$(python3 -c 'import json; print(json.load(open("tooling/codex-cli/release.json"))["release"])')
schema_dir=$(mktemp -d)
trap 'rm -rf "${schema_dir}"' EXIT
for name in ErrorNotification AccountRateLimitsUpdatedNotification TurnCompletedNotification; do
  gh api --header 'Accept: application/vnd.github.raw+json' \
    "repos/KeenWill/codex/contents/codex-rs/app-server-protocol/schema/json/v2/${name}.json?ref=${release}" \
    > "${schema_dir}/${name}.json"
done
SIGNALBOX_CODEX_SCHEMA_DIR="${schema_dir}" cargo test --no-fail-fast \
  -p signalbox-model-runtime-codex-cli --test schema_fixtures -- --nocapture

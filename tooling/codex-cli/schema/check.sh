#!/usr/bin/env bash
# Compare the release pin with committed fixtures and consumed wire shapes.
set -euo pipefail
repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)
cd "${repo_root}"
release=$(python3 -c 'import json; print(json.load(open("tooling/codex-cli/release.json"))["release"])')
schema_dir=$(mktemp -d)
trap 'rm -rf "${schema_dir}"' EXIT
for name in ErrorNotification AccountRateLimitsUpdatedNotification TurnCompletedNotification; do
  curl --fail --silent --show-error --location \
    "https://raw.githubusercontent.com/KeenWill/codex/${release}/codex-rs/app-server-protocol/schema/json/v2/${name}.json" \
    --output "${schema_dir}/${name}.json"
done
python3 - "${schema_dir}" <<'PYTHON'
from pathlib import Path
import sys

for downloaded in sorted(Path(sys.argv[1]).glob("*.json")):
    fixture = Path("tooling/codex-cli/schema") / downloaded.name
    if downloaded.read_bytes() != fixture.read_bytes():
        sys.exit(f"{fixture} differs from the pinned schema; refresh the committed fixture")
PYTHON
SIGNALBOX_CODEX_SCHEMA_DIR="${schema_dir}" cargo test --no-fail-fast \
  -p signalbox-model-runtime-codex-cli --test schema_fixtures -- --nocapture

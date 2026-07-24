#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCREENSHOTS_DIR="$ROOT/Screenshots"
MANIFEST_PATH="$SCREENSHOTS_DIR/MANIFEST.sha256"

hash_file() {
	local path="$1"
	if command -v sha256sum >/dev/null 2>&1; then
		sha256sum "$path" | awk '{ print $1 }'
	else
		shasum -a 256 "$path" | awk '{ print $1 }'
	fi
}

mkdir -p "$SCREENSHOTS_DIR"

find -L "$SCREENSHOTS_DIR" -type f -name '*.png' -print |
	sed "s#^$SCREENSHOTS_DIR/##" |
	LC_ALL=C sort |
	while IFS= read -r relative_path; do
		printf '%s  %s\n' "$(hash_file "$SCREENSHOTS_DIR/$relative_path")" "$relative_path"
	done >"$MANIFEST_PATH"

echo "Updated $MANIFEST_PATH"

#!/usr/bin/env bash
set -euo pipefail

resolve_root() {
	local script_directory
	script_directory="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

	if [[ -d "$script_directory/../Screenshots" ]]; then
		cd "$script_directory/.."
		pwd
		return 0
	fi

	if [[ -n "${TEST_SRCDIR:-}" && -n "${TEST_WORKSPACE:-}" ]]; then
		local runfiles_project_root="$TEST_SRCDIR/$TEST_WORKSPACE/projects/llm_hub_native"
		if [[ -d "$runfiles_project_root/Screenshots" ]]; then
			printf '%s\n' "$runfiles_project_root"
			return 0
		fi
	fi

	echo "Could not resolve projects/llm_hub_native from script or Bazel runfiles." >&2
	return 1
}

ROOT="$(resolve_root)"
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

if [[ ! -f "$MANIFEST_PATH" ]]; then
	echo "Missing screenshot golden manifest: $MANIFEST_PATH"
	echo "Run scripts/update-screenshot-manifest.sh after capturing screenshots."
	exit 1
fi

ACTUAL_MANIFEST="$(mktemp)"
trap 'rm -f "$ACTUAL_MANIFEST"' EXIT

find -L "$SCREENSHOTS_DIR" -type f -name '*.png' -print |
	sed "s#^$SCREENSHOTS_DIR/##" |
	LC_ALL=C sort |
	while IFS= read -r relative_path; do
		printf '%s  %s\n' "$(hash_file "$SCREENSHOTS_DIR/$relative_path")" "$relative_path"
	done >"$ACTUAL_MANIFEST"

if ! diff -u "$MANIFEST_PATH" "$ACTUAL_MANIFEST"; then
	echo "Screenshot goldens changed. Re-run the screenshot capture commands, review the PNGs, and update MANIFEST.sha256 intentionally."
	exit 1
fi

echo "Screenshot goldens match $MANIFEST_PATH"

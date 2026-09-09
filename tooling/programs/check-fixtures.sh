#!/usr/bin/env bash
set -euo pipefail

programs_dir="$(cd -- "$(dirname -- "$0")" && pwd)"
fixture_build_dir="$(mktemp -d)"
trap 'rm -rf -- "$fixture_build_dir"' EXIT

tsc -p "$programs_dir/tsconfig.build.json" --outDir "$fixture_build_dir"
diff --recursive --unified "$programs_dir/../../crates/workflow-runtime/tests/fixtures" "$fixture_build_dir"

#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DERIVED_DATA_PATH="${LLM_HUB_NATIVE_DERIVED_DATA_PATH:-$ROOT/.derivedData}"

CMD=(
	xcodebuild
	-quiet
	-project "$ROOT/LLMHubNative.xcodeproj"
	-scheme "LLMHubNative"
	-configuration "Debug"
	-destination "generic/platform=iOS Simulator"
	-derivedDataPath "$DERIVED_DATA_PATH"
	CODE_SIGNING_ALLOWED=NO
	build
)

printf '+ %q ' "${CMD[@]}"
printf '\n'
"${CMD[@]}"

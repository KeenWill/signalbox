#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DERIVED_DATA_PATH="${LLM_HUB_NATIVE_MACOS_DERIVED_DATA_PATH:-$ROOT/.derivedData-macos}"
APP_PATH="$DERIVED_DATA_PATH/Build/Products/Debug/LLMHubNative.app"
OUTPUT_DIR="$ROOT/Screenshots/macOS"

CMD_BUILD=(
	xcodebuild
	-quiet
	-project "$ROOT/LLMHubNative.xcodeproj"
	-scheme "LLMHubNative"
	-configuration "Debug"
	-destination "platform=macOS,arch=arm64"
	-derivedDataPath "$DERIVED_DATA_PATH"
	CODE_SIGNING_ALLOWED=NO
	build
)

printf '+ %q ' "${CMD_BUILD[@]}"
printf '\n'
"${CMD_BUILD[@]}"

mkdir -p "$OUTPUT_DIR"

CMD_EXPORT=(
	"$APP_PATH/Contents/MacOS/LLMHubNative"
	--export-macos-screenshots "$OUTPUT_DIR"
)

printf '+ %q ' "${CMD_EXPORT[@]}"
printf '\n'
"${CMD_EXPORT[@]}"

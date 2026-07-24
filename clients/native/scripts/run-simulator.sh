#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=/dev/null
source "$ROOT/scripts/lib/simulator.sh"

BUNDLE_ID="co.rdwd.LLMHubNative"
APP_PATH="$ROOT/.derivedData/Build/Products/Debug-iphonesimulator/LLMHubNative.app"
MIN_IOS_VERSION="$SIMULATOR_DEFAULT_MIN_IOS_VERSION"
BOOT_TIMEOUT_SECONDS="${SIMULATOR_BOOT_TIMEOUT_SECONDS:-300}"
DEVICE_ID="${XCODE_SIMULATOR_ID:-}"

if [[ -z "$DEVICE_ID" ]]; then
	DEVICE_ID="$(simulator_resolve_iphone_ids "$MIN_IOS_VERSION" | head -n 1)"
fi

if [[ -z "$DEVICE_ID" ]]; then
	echo "No matching iPhone simulator found. Set XCODE_SIMULATOR_ID to a valid simulator UDID."
	exit 1
fi

DESTINATION="$(simulator_xcode_destination_for_id "$DEVICE_ID")"

CMD_BUILD=(
	xcodebuild
	-project "$ROOT/LLMHubNative.xcodeproj"
	-scheme "LLMHubNative"
	-configuration "Debug"
	-destination "$DESTINATION"
	-derivedDataPath "$ROOT/.derivedData"
	CODE_SIGNING_ALLOWED=NO
	build
)

printf '+ %q ' "${CMD_BUILD[@]}"
printf '\n'
"${CMD_BUILD[@]}"

CMD_BOOT=(xcrun simctl boot "$DEVICE_ID")
printf '+ %q ' "${CMD_BOOT[@]}"
printf '\n'
"${CMD_BOOT[@]}" || true

CMD_BOOTSTATUS=(xcrun simctl bootstatus "$DEVICE_ID" -b)
printf '+ %q ' "${CMD_BOOTSTATUS[@]}"
printf '\n'
"${CMD_BOOTSTATUS[@]}" &
BOOTSTATUS_PID="$!"
for ((elapsed = 0; elapsed < BOOT_TIMEOUT_SECONDS; elapsed += 1)); do
	if ! kill -0 "$BOOTSTATUS_PID" 2>/dev/null; then
		wait "$BOOTSTATUS_PID"
		break
	fi
	sleep 1
done
if kill -0 "$BOOTSTATUS_PID" 2>/dev/null; then
	kill "$BOOTSTATUS_PID" 2>/dev/null || true
	wait "$BOOTSTATUS_PID" 2>/dev/null || true
	echo "Timed out after ${BOOT_TIMEOUT_SECONDS}s waiting for simulator bootstatus."
	exit 1
fi

CMD_INSTALL=(xcrun simctl install "$DEVICE_ID" "$APP_PATH")
printf '+ %q ' "${CMD_INSTALL[@]}"
printf '\n'
"${CMD_INSTALL[@]}"

CMD_LAUNCH=(
	xcrun
	simctl
	launch
	--terminate-running-process
	"--stdout=$ROOT/.derivedData/launch.out"
	"--stderr=$ROOT/.derivedData/launch.err"
	"$DEVICE_ID"
	"$BUNDLE_ID"
	--mock-hub
)
printf '+ %q ' "${CMD_LAUNCH[@]}"
printf '\n'
"${CMD_LAUNCH[@]}"

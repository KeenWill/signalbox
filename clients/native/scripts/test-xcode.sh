#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=/dev/null
source "$ROOT/scripts/lib/simulator.sh"

MIN_IOS_VERSION="$SIMULATOR_DEFAULT_MIN_IOS_VERSION"
DERIVED_DATA_PATH="${SIGNALBOX_NATIVE_DERIVED_DATA_PATH:-$ROOT/.derivedData}"
RESULT_BUNDLE_PATH="${SIGNALBOX_NATIVE_TEST_RESULT_BUNDLE_PATH:-$DERIVED_DATA_PATH/Logs/Test/SignalboxNative-Test.xcresult}"

if [[ -n "${XCODE_DESTINATION:-}" ]]; then
	DESTINATION="$XCODE_DESTINATION"
else
	DEVICE_ID="$(simulator_resolve_iphone_ids "$MIN_IOS_VERSION" | head -n 1)"

	if [[ -z "$DEVICE_ID" ]]; then
		echo "No available iPhone simulator found for iOS $MIN_IOS_VERSION or newer. Set XCODE_DESTINATION to a valid xcodebuild destination."
		exit 1
	fi

	echo "Using newest available iPhone simulator for iOS $MIN_IOS_VERSION or newer: $DEVICE_ID"
	DESTINATION="$(simulator_xcode_destination_for_id "$DEVICE_ID")"
	# A prior test or UI-test run can leave the shared host app active long
	# enough for the next unit-test bundle launch to fail simulator preflight.
	xcrun simctl terminate "$DEVICE_ID" co.rdwd.SignalboxNative >/dev/null 2>&1 || true
fi

CMD=(
	xcodebuild
	-quiet
	-project "$ROOT/SignalboxNative.xcodeproj"
	-scheme "SignalboxNative"
	-configuration "Debug"
	-destination "$DESTINATION"
	-derivedDataPath "$DERIVED_DATA_PATH"
	-resultBundlePath "$RESULT_BUNDLE_PATH"
	CODE_SIGNING_ALLOWED=YES
	CODE_SIGN_IDENTITY=-
	-parallel-testing-enabled NO
)

# CI sets SIGNALBOX_NATIVE_SKIP_TESTING to exclude suites that need an
# interactive simulator session (UI and screenshot tests). Space-separated
# xcodebuild test identifiers, e.g. "SignalboxNativeUITests".
if [[ -n "${SIGNALBOX_NATIVE_SKIP_TESTING:-}" ]]; then
	for skip_identifier in $SIGNALBOX_NATIVE_SKIP_TESTING; do
		CMD+=("-skip-testing:$skip_identifier")
	done
fi

CMD+=(test)

mkdir -p "$(dirname "$RESULT_BUNDLE_PATH")"
rm -rf "$RESULT_BUNDLE_PATH"
printf '+ %q ' "${CMD[@]}"
printf '\n'
set +e
"${CMD[@]}"
xcodebuild_status=$?
set -e

if ((xcodebuild_status != 0)); then
	exit "$xcodebuild_status"
fi

summary="$(xcrun xcresulttool get test-results summary --path "$RESULT_BUNDLE_PATH" --compact)"
if [[ "$summary" != *'"result":"Passed"'* ]]; then
	printf '%s\n' "$summary" >&2
	exit 65
fi

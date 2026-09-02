#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=/dev/null
source "$ROOT/scripts/lib/simulator.sh"

python3 "$ROOT/scripts/generate-plan-contract.py" --check

MIN_IOS_VERSION="$SIMULATOR_DEFAULT_MIN_IOS_VERSION"
DERIVED_DATA_PATH="${SIGNALBOX_NATIVE_DERIVED_DATA_PATH:-$ROOT/.derivedData}"
RESULT_BUNDLE_PATH="${SIGNALBOX_NATIVE_TEST_RESULT_BUNDLE_PATH:-$DERIVED_DATA_PATH/Logs/Test/SignalboxNative-Test.xcresult}"

if [[ -n "${XCODE_DESTINATION:-}" ]]; then
	DESTINATION="$XCODE_DESTINATION"
	if simulator_destination_device_id "$XCODE_DESTINATION" >/dev/null ||
		simulator_destination_device_name "$XCODE_DESTINATION" >/dev/null; then
		DEVICE_ID="$(simulator_resolve_iphone_ids "$MIN_IOS_VERSION" | head -n 1 || true)"
	fi
else
	DEVICE_ID="$(simulator_resolve_iphone_ids "$MIN_IOS_VERSION" | head -n 1)"

	if [[ -z "$DEVICE_ID" ]]; then
		echo "No available iPhone simulator found for iOS $MIN_IOS_VERSION or newer. Set XCODE_DESTINATION to a valid xcodebuild destination."
		exit 1
	fi

	echo "Using newest available iPhone simulator for iOS $MIN_IOS_VERSION or newer: $DEVICE_ID"
	DESTINATION="$(simulator_xcode_destination_for_id "$DEVICE_ID")"
fi

if [[ -n "${DEVICE_ID:-}" ]]; then
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

# The counterpart selector, in the same space-separated identifier vocabulary.
# CI's snapshot step (currently skipped while Swift client work is shelved)
# runs the suite through a second invocation of this script rather than a
# second job: the build is already in the derived data, so selecting one
# suite costs a test pass and not a build.
if [[ -n "${SIGNALBOX_NATIVE_ONLY_TESTING:-}" ]]; then
	for only_identifier in $SIGNALBOX_NATIVE_ONLY_TESTING; do
		CMD+=("-only-testing:$only_identifier")
	done
fi

# Opt-in so an ordinary local run pays nothing for a measurement it did not
# ask for: instrumentation makes the build and the test pass slower, and the
# only consumer is report-coverage.sh, which reads the result bundle this run
# leaves behind. CI's Swift job sets this and then reports; nothing anywhere
# compares the resulting percentage against a threshold.
if [[ "${SIGNALBOX_NATIVE_ENABLE_CODE_COVERAGE:-NO}" == "YES" ]]; then
	CMD+=(-enableCodeCoverage YES)
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

#!/usr/bin/env bash
set -euo pipefail

# Re-records the committed snapshot goldens under
# Tests/SignalboxAppTests/__Snapshots__. Deliberately a separate entry point
# from test-xcode.sh: a validation run must not be able to rewrite the
# references it is checking against, so nothing CI runs can reach record mode.
#
# The suite reads SIGNALBOX_NATIVE_SNAPSHOT_RECORD from its own process. A
# build setting does not reach a test bundle, so the mode is written into the
# generated xctestrun the way test-real-server-xcode.sh passes a socket path.
#
#     scripts/record-snapshots.sh
#     SIGNALBOX_NATIVE_SNAPSHOT_RECORD=missing scripts/record-snapshots.sh
#
# `all` (the default here) rewrites every golden; `missing` writes only the
# ones that do not exist yet. Blessing the result is governed by rule 11 of
# docs/agents/testing-style.md.

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=/dev/null
source "$ROOT/scripts/lib/simulator.sh"

RECORD_MODE="${SIGNALBOX_NATIVE_SNAPSHOT_RECORD:-all}"
SNAPSHOT_SUITE="SignalboxAppTests/LiveScreenSnapshotTests"
DERIVED_DATA_PATH="${SIGNALBOX_NATIVE_DERIVED_DATA_PATH:-$ROOT/.derivedData}"

if [[ -n "${XCODE_DESTINATION:-}" ]]; then
	DESTINATION="$XCODE_DESTINATION"
else
	DEVICE_ID="$(simulator_resolve_iphone_ids "$SIMULATOR_DEFAULT_MIN_IOS_VERSION" | head -n 1)"
	if [[ -z "$DEVICE_ID" ]]; then
		echo "No available iPhone simulator found for iOS $SIMULATOR_DEFAULT_MIN_IOS_VERSION or newer. Set XCODE_DESTINATION to a valid xcodebuild destination."
		exit 1
	fi
	echo "Using newest available iPhone simulator for iOS $SIMULATOR_DEFAULT_MIN_IOS_VERSION or newer: $DEVICE_ID"
	DESTINATION="$(simulator_xcode_destination_for_id "$DEVICE_ID")"
fi

BUILD_CMD=(
	xcodebuild
	-quiet
	-project "$ROOT/SignalboxNative.xcodeproj"
	-scheme "SignalboxNative"
	-configuration "Debug"
	-destination "$DESTINATION"
	-derivedDataPath "$DERIVED_DATA_PATH"
	CODE_SIGNING_ALLOWED=YES
	CODE_SIGN_IDENTITY=-
	-parallel-testing-enabled NO
	build-for-testing
)

find "$DERIVED_DATA_PATH" -name '*.xctestrun' -delete 2>/dev/null || true
printf '+ %q ' "${BUILD_CMD[@]}"
printf '\n'
"${BUILD_CMD[@]}"

XCTESTRUN_PATHS="$(find "$DERIVED_DATA_PATH" -name '*.xctestrun' -type f -print)"
if [[ -z "$XCTESTRUN_PATHS" ]] || [[ "$(printf '%s\n' "$XCTESTRUN_PATHS" | wc -l)" -ne 1 ]]; then
	echo "Expected exactly one generated xctestrun file." >&2
	exit 1
fi
XCTESTRUN_PATH="$XCTESTRUN_PATHS"

python3 - "$XCTESTRUN_PATH" "$RECORD_MODE" <<'PY'
import plistlib
import sys

xctestrun_path, record_mode = sys.argv[1:]
with open(xctestrun_path, "rb") as source:
    document = plistlib.load(source)
matched = 0
for configuration in document["TestConfigurations"]:
    for target in configuration["TestTargets"]:
        if target["BlueprintName"] == "SignalboxAppTests":
            target.setdefault("EnvironmentVariables", {})[
                "SIGNALBOX_NATIVE_SNAPSHOT_RECORD"
            ] = record_mode
            matched += 1
if matched != 1:
    raise SystemExit("Expected exactly one SignalboxAppTests xctestrun target.")
with open(xctestrun_path, "wb") as destination:
    plistlib.dump(document, destination)
PY

TEST_CMD=(
	xcodebuild
	-quiet
	-xctestrun "$XCTESTRUN_PATH"
	-destination "$DESTINATION"
	-only-testing:"$SNAPSHOT_SUITE"
	-parallel-testing-enabled NO
	test-without-building
)
printf '+ %q ' "${TEST_CMD[@]}"
printf '\n'

# Recording fails every assertion it records, by design: the library reports a
# recorded snapshot as a failure so a run that wrote references can never be
# mistaken for a run that verified them. Re-run scripts/test-xcode.sh to verify.
set +e
"${TEST_CMD[@]}"
set -e

# The patched xctestrun carries record mode, so it is removed rather than left
# in the derived data for the next `test-without-building` to pick up.
rm -f "$XCTESTRUN_PATH"

echo "Recorded goldens under $ROOT/Tests/SignalboxAppTests/__Snapshots__; review the diff before committing."

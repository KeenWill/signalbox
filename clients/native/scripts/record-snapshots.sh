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
# ones that do not exist yet. Read every reference this rewrites before
# committing it: the rendering is the whole assertion, so an unread golden
# asserts whatever it happens to contain.

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=/dev/null
source "$ROOT/scripts/lib/simulator.sh"
# shellcheck source=/dev/null
source "$ROOT/scripts/lib/snapshots.sh"

RECORD_MODE="${SIGNALBOX_NATIVE_SNAPSHOT_RECORD:-all}"
SNAPSHOT_SUITE="$SIGNALBOX_NATIVE_SNAPSHOT_SUITE"
DERIVED_DATA_PATH="${SIGNALBOX_NATIVE_DERIVED_DATA_PATH:-$ROOT/.derivedData}"
RESULT_BUNDLE_PATH="$DERIVED_DATA_PATH/Logs/Test/SignalboxNative-Record.xcresult"

# Rejected here and not only in the suite: liveScreenSnapshotRecordMode reports
# an unsupported mode as a test failure, and a recording run tolerates test
# failures by design, so an unchecked typo would build, run, write nothing, and
# report that goldens were recorded. The accepted set is the library's own
# `SnapshotTestingConfiguration.Record`; restating it is the accepted cost of
# checking before the process that owns it exists.
case "$RECORD_MODE" in
all | missing | failed | never) ;;
*)
	echo "SIGNALBOX_NATIVE_SNAPSHOT_RECORD was \"$RECORD_MODE\"; it takes all, missing, failed, or never." >&2
	exit 2
	;;
esac

# The same destination scripts/test-snapshots.sh verifies against, from the
# same definition: a reference recorded against one model and checked against
# another reports a difference nobody introduced.
DESTINATION="$(snapshot_xcode_destination)"
echo "Recording $SNAPSHOT_SUITE against $DESTINATION"

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
	-resultBundlePath "$RESULT_BUNDLE_PATH"
	-only-testing:"$SNAPSHOT_SUITE"
	-parallel-testing-enabled NO
	test-without-building
)
printf '+ %q ' "${TEST_CMD[@]}"
printf '\n'

SNAPSHOT_DIRECTORY="$ROOT/Tests/SignalboxAppTests/__Snapshots__"
# Written before the run so a reference's mtime says whether this run wrote it.
STARTED_MARKER="$(mktemp)"
trap 'rm -f "$STARTED_MARKER"' EXIT
rm -rf "$RESULT_BUNDLE_PATH"
mkdir -p "$(dirname "$RESULT_BUNDLE_PATH")"

# Recording fails every assertion it records, by design: the library reports a
# recorded snapshot as a failure so a run that wrote references can never be
# mistaken for a run that verified them. Re-run scripts/test-xcode.sh to verify.
#
# Only that failure is tolerated. 65 is the status xcodebuild reports for a test
# failure; a simulator that will not boot, a bundle that cannot launch, or a
# host failure reports something else, and none of those may leave the script
# announcing goldens it did not write.
set +e
"${TEST_CMD[@]}"
test_status=$?
set -e

# The patched xctestrun carries record mode, so it is removed rather than left
# in the derived data for the next `test-without-building` to pick up.
rm -f "$XCTESTRUN_PATH"

if ((test_status != 0 && test_status != 65)); then
	echo "The recording run failed before it recorded anything: xcodebuild exited $test_status." >&2
	exit "$test_status"
fi

# 65 covers a suite that launched and ran nothing, and it covers a comparison
# that failed rather than a reference that was written, so the result bundle is
# read rather than the status alone. Every failure has to be a recording: a
# mismatch under `never` or `missing`, a crashed test, or an unsatisfied
# precondition all report 65 while writing nothing, and each of those used to
# end with the script announcing goldens.
summary="$(xcrun xcresulttool get test-results summary --path "$RESULT_BUNDLE_PATH" --compact)"
python3 - "$summary" "$SNAPSHOT_DIRECTORY" <<'PY'
import json
import sys

# The clause the library reports a written reference with. It writes one under
# two different sentences — "Record mode is on." when the mode forced a
# rewrite, "No reference was found on disk." when there was nothing to compare
# against — so the shared clause is the seam, not either sentence. Matching
# only the first is what made `missing` report every reference it correctly
# wrote as a failure that had recorded nothing.
#
# The clause rather than the second sentence's opening, because a missing
# reference has a third outcome: under a mode that records nothing the library
# says "No reference was found on disk. New snapshot was not recorded because
# recording is disabled", which wrote no file and must stay a failure. That is
# the case `never` exists for, and it is the one this check must not accept.
# A comparison failure and a crash each say something else again.
RECORDED = "Automatically recorded snapshot"

summary, snapshot_directory = json.loads(sys.argv[1]), sys.argv[2]
executed = summary.get("totalTestCount", 0)
if executed == 0:
    print(json.dumps(summary, indent=2), file=sys.stderr)
    raise SystemExit("The recording run executed no test; nothing was recorded.")

failures = summary.get("testFailures", [])
unexpected = [
    failure for failure in failures if RECORDED not in (failure.get("failureText") or "")
]
if unexpected:
    for failure in unexpected:
        text = (failure.get("failureText") or "").splitlines()
        print(f"  {failure.get('testName')}: {text[0] if text else ''}", file=sys.stderr)
    raise SystemExit(
        f"{len(unexpected)} of {len(failures)} failure(s) recorded nothing; "
        "the run above did not bless these goldens."
    )

recorded = len(failures)
if recorded:
    print(
        f"Recorded {recorded} of {executed} reference(s) under {snapshot_directory}; "
        "read every one in the diff before committing it."
    )
else:
    print(f"No reference needed rewriting; {executed} already matched.")
PY

# `all` rewrites every reference, so a run that wrote none did not record. The
# other modes legitimately write nothing — `missing` when none is missing — so
# only the mode that promises a write is held to one.
if [[ "$RECORD_MODE" == "all" ]]; then
	WRITTEN="$(find "$SNAPSHOT_DIRECTORY" -type f -newer "$STARTED_MARKER" | wc -l | tr -d ' ')"
	if [[ "$WRITTEN" == "0" ]]; then
		echo "The recording run wrote no reference under $SNAPSHOT_DIRECTORY." >&2
		exit 1
	fi
	echo "Wrote $WRITTEN reference(s) to disk."
fi

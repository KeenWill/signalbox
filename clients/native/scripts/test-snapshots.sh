#!/usr/bin/env bash
set -euo pipefail

# Verifies the committed snapshot goldens, and nothing else.
#
# A thin entry point over test-xcode.sh rather than a second test runner: the
# suite it selects and the simulator it selects both come from
# scripts/lib/snapshots.sh, which is what makes this run and the recording run
# agree. CI calls this instead of naming the suite itself, so the identifier is
# spelled in exactly one place.
#
#     scripts/test-snapshots.sh
#     XCODE_DESTINATION="platform=iOS Simulator,name=iPhone Air" scripts/test-snapshots.sh

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=/dev/null
source "$ROOT/scripts/lib/simulator.sh"
# shellcheck source=/dev/null
source "$ROOT/scripts/lib/snapshots.sh"

XCODE_DESTINATION="$(snapshot_xcode_destination)"
export XCODE_DESTINATION
export SIGNALBOX_NATIVE_ONLY_TESTING="$SIGNALBOX_NATIVE_SNAPSHOT_SUITE"

RESULT_BUNDLE_PATH="${SIGNALBOX_NATIVE_TEST_RESULT_BUNDLE_PATH:-$ROOT/.derivedData/Logs/Test/SignalboxNative-Test.xcresult}"
export SIGNALBOX_NATIVE_TEST_RESULT_BUNDLE_PATH="$RESULT_BUNDLE_PATH"

echo "Verifying $SIGNALBOX_NATIVE_SNAPSHOT_SUITE against $XCODE_DESTINATION"
"$ROOT/scripts/test-xcode.sh"

# A passing run that ran nothing is the failure this guards, and it is silent
# without a count: `-only-testing` against a suite that was renamed, moved, or
# stopped being discovered completes clean, and test-xcode.sh accepts any
# summary reporting "Passed". This step is report-only in CI, so a green run
# verifying no golden would announce that the references hold when nothing read
# them. The recording script and the real-server runner already count; this is
# the third entry point that needed to.
summary="$(xcrun xcresulttool get test-results summary --path "$RESULT_BUNDLE_PATH" --compact)"
python3 - "$summary" "$SIGNALBOX_NATIVE_SNAPSHOT_SUITE" <<'PY'
import json
import sys

summary, suite = json.loads(sys.argv[1]), sys.argv[2]
if summary.get("totalTestCount", 0) == 0:
    print(json.dumps(summary, indent=2), file=sys.stderr)
    raise SystemExit(
        f"The run selected {suite} and executed no test, so no golden was "
        "verified. The suite was most likely renamed or moved; "
        "scripts/lib/snapshots.sh is where its identifier is spelled."
    )
PY

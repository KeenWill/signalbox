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

echo "Verifying $SIGNALBOX_NATIVE_SNAPSHOT_SUITE against $XCODE_DESTINATION"
exec "$ROOT/scripts/test-xcode.sh"

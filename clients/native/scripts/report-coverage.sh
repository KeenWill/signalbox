#!/usr/bin/env bash
# Report the code coverage of the last test-xcode.sh run.
#
# Reads the result bundle that run left behind — no build and no simulator of
# its own — and writes an xccov JSON report, the xccov text report, and a
# Markdown summary into an output directory, then prints the summary on stdout
# so a caller can append it to a job summary.
#
# Report only. Nothing here compares a percentage against a threshold; the exit
# status reports whether a report could be produced, never how high it was.
#
# The result bundle only carries coverage when the run that produced it set
# SIGNALBOX_NATIVE_ENABLE_CODE_COVERAGE=YES.
#
# Every argument is forwarded verbatim to summarize-coverage.py, which is how
# CI hands it a --baseline to report the total against. This script stays
# ignorant of those flags on purpose: what a baseline is and where it came from
# is the workflow's business, and a local run passes none and gets the report it
# always got.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

DERIVED_DATA_PATH="${SIGNALBOX_NATIVE_DERIVED_DATA_PATH:-$ROOT/.derivedData}"
RESULT_BUNDLE_PATH="${SIGNALBOX_NATIVE_TEST_RESULT_BUNDLE_PATH:-$DERIVED_DATA_PATH/Logs/Test/SignalboxNative-Test.xcresult}"
OUTPUT_DIR="${SIGNALBOX_NATIVE_COVERAGE_OUTPUT_DIR:-$DERIVED_DATA_PATH/Coverage}"

if [[ ! -e "$RESULT_BUNDLE_PATH" ]]; then
	echo "report-coverage: no result bundle at $RESULT_BUNDLE_PATH" >&2
	echo "report-coverage: run test-xcode.sh with SIGNALBOX_NATIVE_ENABLE_CODE_COVERAGE=YES first" >&2
	exit 1
fi

mkdir -p "$OUTPUT_DIR"

# The JSON report is what the summarizer reads; the text report is kept beside
# it because it carries the per-function detail the Markdown table omits, and a
# reader chasing one uncovered file wants it.
xcrun xccov view --report --json "$RESULT_BUNDLE_PATH" >"$OUTPUT_DIR/coverage.json"
xcrun xccov view --report "$RESULT_BUNDLE_PATH" >"$OUTPUT_DIR/coverage.txt"

python3 "$ROOT/scripts/summarize-coverage.py" "$OUTPUT_DIR/coverage.json" "$@" \
	>"$OUTPUT_DIR/report.md"

cat "$OUTPUT_DIR/report.md"

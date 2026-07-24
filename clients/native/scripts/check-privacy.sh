#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PATTERN='Analytics|AdMob|Firebase|ATTrackingManager|SKAdNetwork|RevenueCat|telemetry|remote config|tracking'

if grep -R -n -E "$PATTERN" "$ROOT/Sources" "$ROOT/LLMHubNative"; then
	echo "Review privacy-sensitive references above."
	exit 1
fi

echo "No analytics, ads, tracking, telemetry, remote-config, or third-party SDK markers found."

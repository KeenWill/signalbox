#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PATTERN='Analytics|AdMob|Firebase|ATTrackingManager|SKAdNetwork|RevenueCat|telemetry|remote config|tracking'
LOGGING_PATTERN='(^|[^[:alnum:]_])(print|debugPrint|dump|NSLog|os_log|Logger)[[:space:]]*\(|\.[[:space:]]*(trace|debug|info|notice|warning|error|critical|log)[[:space:]]*\('
INSTANCE_LOGGING_FIXTURE='logger.info("synthetic message")'

if ! printf '%s\n' "$INSTANCE_LOGGING_FIXTURE" | grep -q -E "$LOGGING_PATTERN"; then
	echo "Privacy scan regression: instance logging fixture was not detected."
	exit 1
fi

if grep -R -n -E "$PATTERN" "$ROOT/Sources" "$ROOT/SignalboxNative"; then
	echo "Review privacy-sensitive references above."
	exit 1
fi

if grep -R -n -E --include='*.swift' "$LOGGING_PATTERN" "$ROOT/Sources"; then
	echo "Shipping Swift sources must not add logging call sites that could expose credentials."
	exit 1
fi

echo "No analytics, ads, tracking, telemetry, remote-config, or third-party SDK markers found."
echo "No credential-capable logging call sites found in shipping Swift sources."

#!/usr/bin/env bash

# What the snapshot suite is, and which simulator its goldens belong to.
#
# One owner for both, because recording and verifying have to agree and they
# run from different places: scripts/record-snapshots.sh writes the references,
# scripts/test-snapshots.sh checks them, and CI runs the second. A suite
# identifier or a device spelled separately in each would drift the moment one
# of them changed.

SIGNALBOX_NATIVE_SNAPSHOT_SUITE="SignalboxAppTests/LiveScreenSnapshotTests"

# Named rather than resolved, and this is the whole reason the file exists.
# `simulator_resolve_iphone_ids` returns booted devices first, so the newest
# model is what a fresh runner picks and whatever the developer happens to have
# open is what a laptop picks. Nine of the ten goldens do not care; the
# regular-layout canvas is wider than a phone screen, so the window's corner
# mask and the glass materials composite against the device, and recording it
# against a different model than CI checks it against would report a difference
# nobody introduced.
SIGNALBOX_NATIVE_SNAPSHOT_DEVICE_NAME="${SIGNALBOX_NATIVE_SNAPSHOT_DEVICE_NAME:-iPhone 17 Pro}"

# Resolves the destination both entry points use.
#
# An explicit XCODE_DESTINATION still wins, so a deliberate run against another
# model stays one environment variable away. Falling back is loud rather than
# silent: a runner image without this model would otherwise quietly go back to
# the resolution this function exists to replace.
snapshot_xcode_destination() {
	local resolved

	if [[ -n "${XCODE_DESTINATION:-}" ]]; then
		printf '%s\n' "$XCODE_DESTINATION"
		return 0
	fi

	resolved="$(
		XCODE_DESTINATION="platform=iOS Simulator,name=$SIGNALBOX_NATIVE_SNAPSHOT_DEVICE_NAME" \
			simulator_resolve_iphone_ids "$SIMULATOR_DEFAULT_MIN_IOS_VERSION" 2>/dev/null | head -n 1
	)"
	if [[ -n "$resolved" ]]; then
		simulator_xcode_destination_for_id "$resolved"
		return 0
	fi

	echo "No $SIGNALBOX_NATIVE_SNAPSHOT_DEVICE_NAME simulator for iOS $SIMULATOR_DEFAULT_MIN_IOS_VERSION or newer." >&2
	echo "Falling back to the newest available iPhone; the regular-layout golden may differ." >&2
	resolved="$(simulator_resolve_iphone_ids "$SIMULATOR_DEFAULT_MIN_IOS_VERSION" | head -n 1)"
	if [[ -z "$resolved" ]]; then
		echo "No available iPhone simulator for iOS $SIMULATOR_DEFAULT_MIN_IOS_VERSION or newer. Set XCODE_DESTINATION." >&2
		return 1
	fi
	simulator_xcode_destination_for_id "$resolved"
}

#!/usr/bin/env bash

# What the snapshot suite is, and which simulator its goldens belong to.
#
# Both are spelled once, here, because recording and verifying have to agree
# and they run from different places: scripts/record-snapshots.sh writes the references,
# scripts/test-snapshots.sh checks them, and CI's snapshot step — currently
# skipped while Swift client work is shelved — runs the second. A suite
# identifier or a device spelled separately in each would drift the moment one
# of them changed.

SIGNALBOX_NATIVE_SNAPSHOT_SUITE="SignalboxAppTests/LiveScreenSnapshotTests"

# Named rather than resolved, and this is the whole reason the file exists.
# `simulator_resolve_iphone_ids` returns booted devices first, so the newest
# model is what a fresh runner picks and whatever the developer happens to have
# open is what a laptop picks. The goldens recorded on the phone-sized canvases
# do not care; the ones recorded on a canvas wider than a phone screen do — both
# iPad canvases and the sheet canvas — because the window's corner mask and the
# glass materials composite against the device, and recording them against a
# different model than CI checks them against would report a difference nobody
# introduced. It is a width rule and not an iPad one; naming it for the iPad
# canvases is what once left the sheet references unclassified. That
# is a cost of resolving a different simulator and not of the canvas matrix
# itself: every canvas in a run renders on the one device named here.
SIGNALBOX_NATIVE_SNAPSHOT_DEVICE_NAME="${SIGNALBOX_NATIVE_SNAPSHOT_DEVICE_NAME:-iPhone 17 Pro}"

# The runtime, for the same reason and with less evidence. A model resolves to
# whatever runtime is newest locally, and .github/workflows/swift.yml pins CI to
# Xcode 26.6 with the iOS 26.5 runtime, so a developer whose newest iPhone 17
# Pro is on 26.4 records goldens against a different UIKit than CI checks them
# with. Unlike the model, this is not demonstrated here: 26.5 is the only
# runtime on the machine these goldens were recorded on, so no cross-runtime
# difference was measured, and the pin is a guard rather than a fix for an
# observed failure. It costs a loud fallback when the runtime is missing, which
# is the outcome worth having either way — recording against an unknown runtime
# silently is what the fallback exists to prevent.
SIGNALBOX_NATIVE_SNAPSHOT_IOS_VERSION="${SIGNALBOX_NATIVE_SNAPSHOT_IOS_VERSION:-26.5}"

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

	# Naming OS= makes this an exact-runtime lookup that returns nothing rather
	# than the newest compatible runtime, so a missing 26.5 reaches the loud
	# fallback below instead of silently resolving to 26.4.
	resolved="$(
		XCODE_DESTINATION="platform=iOS Simulator,name=$SIGNALBOX_NATIVE_SNAPSHOT_DEVICE_NAME,OS=$SIGNALBOX_NATIVE_SNAPSHOT_IOS_VERSION" \
			simulator_resolve_iphone_ids "$SIMULATOR_DEFAULT_MIN_IOS_VERSION" 2>/dev/null | head -n 1
	)"
	if [[ -n "$resolved" ]]; then
		simulator_xcode_destination_for_id "$resolved"
		return 0
	fi

	echo "No $SIGNALBOX_NATIVE_SNAPSHOT_DEVICE_NAME simulator on iOS $SIGNALBOX_NATIVE_SNAPSHOT_IOS_VERSION, the runtime these goldens are recorded and checked against." >&2
	echo "Falling back to the newest available iPhone. Every reference recorded on a canvas wider than a phone screen — *.ipad-portrait.png, *.ipad-landscape.png, and *.sheet.png — resolves against the device and may differ here." >&2
	echo "Phone-canvas references were verified byte-identical across iPhone models, but only on iOS $SIGNALBOX_NATIVE_SNAPSHOT_IOS_VERSION. This fallback is reached when the model or the runtime is missing and can change either, and no reference of any canvas has been compared across runtimes, so on a different runtime a phone-canvas difference is unclassified too." >&2
	resolved="$(simulator_resolve_iphone_ids "$SIMULATOR_DEFAULT_MIN_IOS_VERSION" | head -n 1)"
	if [[ -z "$resolved" ]]; then
		echo "No available iPhone simulator for iOS $SIMULATOR_DEFAULT_MIN_IOS_VERSION or newer. Set XCODE_DESTINATION." >&2
		return 1
	fi
	simulator_xcode_destination_for_id "$resolved"
}

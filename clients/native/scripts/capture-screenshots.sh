#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=/dev/null
source "$ROOT/scripts/lib/simulator.sh"

BUNDLE_ID="co.rdwd.LLMHubNative"
DERIVED_DATA_PATH="${LLM_HUB_NATIVE_DERIVED_DATA_PATH:-$ROOT/.derivedData}"
APP_PATH="$DERIVED_DATA_PATH/Build/Products/Debug-iphonesimulator/LLMHubNative.app"
MIN_IOS_VERSION="$SIMULATOR_DEFAULT_MIN_IOS_VERSION"
BOOT_TIMEOUT_SECONDS="${SIMULATOR_BOOT_TIMEOUT_SECONDS:-300}"
TERMINATE_TIMEOUT_SECONDS="${SIMULATOR_TERMINATE_TIMEOUT_SECONDS:-10}"
LAUNCH_TIMEOUT_SECONDS="${SIMULATOR_LAUNCH_TIMEOUT_SECONDS:-45}"
SCREENSHOT_SETTLE_SECONDS="${SCREENSHOT_SETTLE_SECONDS:-4}"
IPAD_SCREENSHOT_BATCH_SIZE="${IPAD_SCREENSHOT_BATCH_SIZE:-6}"
OUTPUT_ROOT="$ROOT/Screenshots"
IOS_OUTPUT_ROOT="$OUTPUT_ROOT/iOS"
IPADOS_OUTPUT_ROOT="$OUTPUT_ROOT/iPadOS"

DEFAULT_DEVICE_NAMES=(
	"iPhone 17"
	"iPhone 17 Pro"
	"iPad Pro 11-inch (M5)"
	"iPad Pro 13-inch (M5)"
	"iPad Air 13-inch (M4)"
)

SCREENSHOT_NAMES=(
	setup
	sessions
	new-session
	active-chat
	markdown-basics
	markdown-table
	markdown-code
	markdown-message
	pending-approval
	completed-tool
	failed-tool
	artifact-preview
	runners
	monitor
	settings
	dark
	large-type
)

if [[ -n "${SCREENSHOT_DEVICE_NAMES:-}" ]]; then
	IFS=',' read -r -a DEVICE_NAMES <<<"$SCREENSHOT_DEVICE_NAMES"
else
	DEVICE_NAMES=("${DEFAULT_DEVICE_NAMES[@]}")
fi

if [[ -n "${SCREENSHOT_STATE_NAMES:-}" ]]; then
	IFS=',' read -r -a REQUESTED_SCREENSHOT_NAMES <<<"$SCREENSHOT_STATE_NAMES"
else
	REQUESTED_SCREENSHOT_NAMES=()
fi

CMD_BUILD=(
	xcodebuild
	-quiet
	-project "$ROOT/LLMHubNative.xcodeproj"
	-scheme "LLMHubNative"
	-configuration "Debug"
	-destination "generic/platform=iOS Simulator"
	-derivedDataPath "$DERIVED_DATA_PATH"
	CODE_SIGNING_ALLOWED=NO
	build
)

printf '+ %q ' "${CMD_BUILD[@]}"
printf '\n'
"${CMD_BUILD[@]}"

mkdir -p "$IOS_OUTPUT_ROOT" "$IPADOS_OUTPUT_ROOT"

slugify() {
	local value="$1"
	printf '%s\n' "$value" |
		tr '[:upper:]' '[:lower:]' |
		sed -E 's/[^a-z0-9]+/-/g; s/^-//; s/-$//'
}

trim() {
	local value="$1"
	value="${value#"${value%%[![:space:]]*}"}"
	value="${value%"${value##*[![:space:]]}"}"
	printf '%s\n' "$value"
}

should_capture_screenshot() {
	local candidate="$1"
	local requested

	if ((${#REQUESTED_SCREENSHOT_NAMES[@]} == 0)); then
		return 0
	fi

	for requested in "${REQUESTED_SCREENSHOT_NAMES[@]}"; do
		if [[ "$(trim "$requested")" == "$candidate" ]]; then
			return 0
		fi
	done
	return 1
}

selected_screenshot_names() {
	local screenshot_name
	local requested

	if ((${#REQUESTED_SCREENSHOT_NAMES[@]} == 0)); then
		printf '%s\n' "${SCREENSHOT_NAMES[@]}"
		return 0
	fi

	for screenshot_name in "${SCREENSHOT_NAMES[@]}"; do
		for requested in "${REQUESTED_SCREENSHOT_NAMES[@]}"; do
			if [[ "$(trim "$requested")" == "$screenshot_name" ]]; then
				printf '%s\n' "$screenshot_name"
				break
			fi
		done
	done
}

clear_selected_screenshots() {
	local device_output_dir="$1"
	local screenshot_name

	while IFS= read -r screenshot_name; do
		[[ -n "$screenshot_name" ]] || continue
		rm -f "$device_output_dir/${screenshot_name}.png"
	done < <(selected_screenshot_names)
}

verify_selected_screenshots_exist() {
	local device_output_dir="$1"
	local screenshot_name
	local missing=0

	while IFS= read -r screenshot_name; do
		[[ -n "$screenshot_name" ]] || continue
		if [[ ! -f "$device_output_dir/${screenshot_name}.png" ]]; then
			echo "Missing expected screenshot: $device_output_dir/${screenshot_name}.png" >&2
			missing=1
		fi
	done < <(selected_screenshot_names)

	return "$missing"
}

assert_xcresult_passed() {
	local result_bundle_path="$1"
	local summary

	summary="$(xcrun xcresulttool get test-results summary --path "$result_bundle_path" --compact)"
	if [[ "$summary" != *'"result":"Passed"'* ]]; then
		printf '%s\n' "$summary" >&2
		return 1
	fi
}

is_ipad_device_name() {
	local device_name="$1"
	[[ "$device_name" == iPad* ]]
}

boot_device() {
	local device_id="$1"

	CMD_BOOT=(xcrun simctl boot "$device_id")
	printf '+ %q ' "${CMD_BOOT[@]}"
	printf '\n'
	"${CMD_BOOT[@]}" || true

	CMD_BOOTSTATUS=(xcrun simctl bootstatus "$device_id" -b)
	printf '+ %q ' "${CMD_BOOTSTATUS[@]}"
	printf '\n'
	"${CMD_BOOTSTATUS[@]}" &
	BOOTSTATUS_PID="$!"
	for ((elapsed = 0; elapsed < BOOT_TIMEOUT_SECONDS; elapsed += 1)); do
		if ! kill -0 "$BOOTSTATUS_PID" 2>/dev/null; then
			wait "$BOOTSTATUS_PID"
			break
		fi
		sleep 1
	done
	if kill -0 "$BOOTSTATUS_PID" 2>/dev/null; then
		kill "$BOOTSTATUS_PID" 2>/dev/null || true
		wait "$BOOTSTATUS_PID" 2>/dev/null || true
		echo "Timed out after ${BOOT_TIMEOUT_SECONDS}s waiting for simulator bootstatus."
		exit 1
	fi
}

terminate_app() {
	local device_id="$1"

	CMD_TERMINATE=(xcrun simctl terminate "$device_id" "$BUNDLE_ID")
	printf '+ %q ' "${CMD_TERMINATE[@]}"
	printf '\n'
	"${CMD_TERMINATE[@]}" &
	TERMINATE_PID="$!"
	for ((elapsed = 0; elapsed < TERMINATE_TIMEOUT_SECONDS; elapsed += 1)); do
		if ! kill -0 "$TERMINATE_PID" 2>/dev/null; then
			wait "$TERMINATE_PID" || true
			return 0
		fi
		sleep 1
	done
	if kill -0 "$TERMINATE_PID" 2>/dev/null; then
		kill "$TERMINATE_PID" 2>/dev/null || true
		wait "$TERMINATE_PID" 2>/dev/null || true
		echo "Timed out after ${TERMINATE_TIMEOUT_SECONDS}s terminating $BUNDLE_ID on $device_id; continuing."
	fi
}

reboot_device() {
	local device_id="$1"

	CMD_SHUTDOWN=(xcrun simctl shutdown "$device_id")
	printf '+ %q ' "${CMD_SHUTDOWN[@]}"
	printf '\n'
	"${CMD_SHUTDOWN[@]}" || true
	boot_device "$device_id"
}

launch_app() {
	local device_id="$1"
	shift
	local attempt
	local launch_status

	for attempt in 1 2; do
		CMD_LAUNCH=(xcrun simctl launch "$@")
		printf '+ %q ' "${CMD_LAUNCH[@]}"
		printf '\n'
		"${CMD_LAUNCH[@]}" &
		LAUNCH_PID="$!"
		for ((elapsed = 0; elapsed < LAUNCH_TIMEOUT_SECONDS; elapsed += 1)); do
			if ! kill -0 "$LAUNCH_PID" 2>/dev/null; then
				if wait "$LAUNCH_PID"; then
					return 0
				else
					launch_status="$?"
				fi
				if [[ "$attempt" -lt 2 ]]; then
					echo "Launch failed with status $launch_status on $device_id; rebooting and retrying."
					reboot_device "$device_id"
					break
				fi
				return "$launch_status"
			fi
			sleep 1
		done

		if kill -0 "$LAUNCH_PID" 2>/dev/null; then
			kill "$LAUNCH_PID" 2>/dev/null || true
			wait "$LAUNCH_PID" 2>/dev/null || true
			if [[ "$attempt" -lt 2 ]]; then
				echo "Timed out after ${LAUNCH_TIMEOUT_SECONDS}s launching $BUNDLE_ID on $device_id; rebooting and retrying."
				reboot_device "$device_id"
			else
				echo "Timed out after ${LAUNCH_TIMEOUT_SECONDS}s launching $BUNDLE_ID on $device_id."
				exit 1
			fi
		fi
	done
}

capture_scenario() {
	local device_id="$1"
	local device_output_dir="$2"
	local name="$3"
	local scenario="$4"
	local appearance="$5"
	local content_size="$6"
	shift 6
	local -a launch_arguments
	local output="$device_output_dir/${name}.png"

	if ! should_capture_screenshot "$name"; then
		return 0
	fi

	CMD_APPEARANCE=(xcrun simctl ui "$device_id" appearance "$appearance")
	printf '+ %q ' "${CMD_APPEARANCE[@]}"
	printf '\n'
	"${CMD_APPEARANCE[@]}" || true

	CMD_CONTENT_SIZE=(xcrun simctl ui "$device_id" content_size "$content_size")
	printf '+ %q ' "${CMD_CONTENT_SIZE[@]}"
	printf '\n'
	"${CMD_CONTENT_SIZE[@]}" || true

	terminate_app "$device_id"

	launch_arguments=(
		--terminate-running-process
		"--stdout=$device_output_dir/${name}.out"
		"--stderr=$device_output_dir/${name}.err"
		"$device_id"
		"$BUNDLE_ID"
		--screenshot-state "$scenario"
	)
	if (($# > 0)); then
		launch_arguments+=("$@")
	fi
	launch_app "$device_id" "${launch_arguments[@]}"
	sleep "$SCREENSHOT_SETTLE_SECONDS"

	CMD_SCREENSHOT=(xcrun simctl io "$device_id" screenshot "$output")
	printf '+ %q ' "${CMD_SCREENSHOT[@]}"
	printf '\n'
	"${CMD_SCREENSHOT[@]}"
	sleep 1
}

capture_ipad_device() {
	local device_name="$1"
	local device_id="$2"
	local device_slug="$3"
	local device_output_dir="$IPADOS_OUTPUT_ROOT/$device_slug"
	local capture_output_file="$OUTPUT_ROOT/.capture-output-dir"
	local capture_names_file="$device_output_dir/.capture-screenshot-names"
	local screenshot
	local selected_names=()
	local screenshot_name
	local batch_start
	local batch_index=1
	local batch_names
	local result_bundle_path

	while IFS= read -r screenshot_name; do
		[[ -n "$screenshot_name" ]] || continue
		selected_names+=("$screenshot_name")
	done < <(selected_screenshot_names)

	if ((${#selected_names[@]} == 0)); then
		echo "No iPad screenshots selected for $device_name."
		return 0
	fi

	mkdir -p "$device_output_dir"
	mkdir -p "$DERIVED_DATA_PATH/Logs/Test"
	clear_selected_screenshots "$device_output_dir"
	printf '%s\n' "$device_output_dir" >"$capture_output_file"

	echo "Capturing $device_name ($device_id) in landscape into $device_output_dir"
	boot_device "$device_id"

	for ((batch_start = 0; batch_start < ${#selected_names[@]}; batch_start += IPAD_SCREENSHOT_BATCH_SIZE)); do
		batch_names="$(printf '%s\n' "${selected_names[@]:batch_start:IPAD_SCREENSHOT_BATCH_SIZE}" | paste -sd, -)"
		printf '%s\n' "$batch_names" >"$capture_names_file"
		result_bundle_path="$DERIVED_DATA_PATH/Logs/Test/LLMHubNative-iPadOS-${device_slug}-batch-${batch_index}.xcresult"

		CMD_IPAD_SCREENSHOTS=(
			xcodebuild
			-quiet
			-project "$ROOT/LLMHubNative.xcodeproj"
			-scheme "LLMHubNative"
			-configuration "Debug"
			-destination "platform=iOS Simulator,id=$device_id"
			-derivedDataPath "$DERIVED_DATA_PATH"
			-resultBundlePath "$result_bundle_path"
			-only-testing:LLMHubNativeUITests/ScreenshotCaptureUITests/testCaptureScreenshotMatrix
			-parallel-testing-enabled NO
			-test-timeouts-enabled YES
			-maximum-test-execution-time-allowance 900
			CODE_SIGNING_ALLOWED=YES
			CODE_SIGN_IDENTITY=-
			test
		)
		printf '+ LLM_HUB_NATIVE_SCREENSHOT_OUTPUT_DIR=%q LLM_HUB_NATIVE_SCREENSHOT_NAMES=%q ' "$device_output_dir" "$batch_names"
		printf '%q ' "${CMD_IPAD_SCREENSHOTS[@]}"
		printf '\n'
		rm -rf "$result_bundle_path"
		if ! LLM_HUB_NATIVE_SCREENSHOT_OUTPUT_DIR="$device_output_dir" LLM_HUB_NATIVE_SCREENSHOT_NAMES="$batch_names" "${CMD_IPAD_SCREENSHOTS[@]}"; then
			rm -f "$capture_output_file" "$capture_names_file"
			return 1
		fi
		if ! assert_xcresult_passed "$result_bundle_path"; then
			rm -f "$capture_output_file" "$capture_names_file"
			return 1
		fi
		batch_index=$((batch_index + 1))
	done

	if ! verify_selected_screenshots_exist "$device_output_dir"; then
		rm -f "$capture_output_file" "$capture_names_file"
		return 1
	fi
	rm -f "$capture_output_file" "$capture_names_file"

	for screenshot in "$device_output_dir"/*.png; do
		[[ -f "$screenshot" ]] || continue
		normalize_ipad_screenshot_orientation "$screenshot"
	done
}

normalize_ipad_screenshot_orientation() {
	local screenshot="$1"
	local pixel_width
	local pixel_height

	pixel_width="$(
		sips -g pixelWidth "$screenshot" 2>/dev/null |
			awk '/pixelWidth:/ { print $2 }'
	)"
	pixel_height="$(
		sips -g pixelHeight "$screenshot" 2>/dev/null |
			awk '/pixelHeight:/ { print $2 }'
	)"

	if [[ -z "$pixel_width" || -z "$pixel_height" ]]; then
		echo "Unable to read screenshot dimensions for $screenshot."
		return 1
	fi

	if ((pixel_height <= pixel_width)); then
		echo "Keeping landscape iPad screenshot: $screenshot (${pixel_width}x${pixel_height})"
		return 0
	fi

	CMD_ROTATE=(sips -r -90 "$screenshot")
	printf '+ %q ' "${CMD_ROTATE[@]}"
	printf '\n'
	"${CMD_ROTATE[@]}" >/dev/null
}

capture_device() {
	local device_name="$1"
	local device_id
	local device_slug
	local device_output_dir

	device_id="$(simulator_resolve_device_id_by_name "$device_name" "" "$MIN_IOS_VERSION")"
	if [[ -z "$device_id" ]]; then
		echo "No simulator found for '$device_name' on iOS $MIN_IOS_VERSION or newer."
		exit 1
	fi

	device_slug="$(slugify "$device_name")"
	if is_ipad_device_name "$device_name"; then
		capture_ipad_device "$device_name" "$device_id" "$device_slug"
		return 0
	fi

	device_output_dir="$IOS_OUTPUT_ROOT/$device_slug"
	mkdir -p "$device_output_dir"

	echo "Capturing $device_name ($device_id) into $device_output_dir"
	boot_device "$device_id"

	CMD_INSTALL=(xcrun simctl install "$device_id" "$APP_PATH")
	printf '+ %q ' "${CMD_INSTALL[@]}"
	printf '\n'
	"${CMD_INSTALL[@]}"

	capture_scenario "$device_id" "$device_output_dir" setup setup light large
	capture_scenario "$device_id" "$device_output_dir" sessions sessions light large
	capture_scenario "$device_id" "$device_output_dir" new-session new-session light large
	capture_scenario "$device_id" "$device_output_dir" active-chat active-chat light large
	capture_scenario "$device_id" "$device_output_dir" markdown-basics markdown-basics light large
	capture_scenario "$device_id" "$device_output_dir" markdown-table markdown-table light large
	capture_scenario "$device_id" "$device_output_dir" markdown-code markdown-code light large
	capture_scenario "$device_id" "$device_output_dir" markdown-message markdown-message light large
	capture_scenario "$device_id" "$device_output_dir" pending-approval pending-approval light large
	capture_scenario "$device_id" "$device_output_dir" completed-tool completed-tool light large
	capture_scenario "$device_id" "$device_output_dir" failed-tool failed-tool light large
	capture_scenario "$device_id" "$device_output_dir" artifact-preview artifact-preview light large
	capture_scenario "$device_id" "$device_output_dir" runners runners light large
	capture_scenario "$device_id" "$device_output_dir" monitor monitor light large
	capture_scenario "$device_id" "$device_output_dir" settings settings light large
	capture_scenario "$device_id" "$device_output_dir" dark active-chat dark large
	capture_scenario "$device_id" "$device_output_dir" large-type pending-approval light accessibility-extra-extra-extra-large

	CMD_RESET_APPEARANCE=(xcrun simctl ui "$device_id" appearance light)
	printf '+ %q ' "${CMD_RESET_APPEARANCE[@]}"
	printf '\n'
	"${CMD_RESET_APPEARANCE[@]}" || true

	CMD_RESET_CONTENT_SIZE=(xcrun simctl ui "$device_id" content_size large)
	printf '+ %q ' "${CMD_RESET_CONTENT_SIZE[@]}"
	printf '\n'
	"${CMD_RESET_CONTENT_SIZE[@]}" || true
}

for device_name in "${DEVICE_NAMES[@]}"; do
	capture_device "$(trim "$device_name")"
done

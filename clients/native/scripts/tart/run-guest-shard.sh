#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="${MONO_GUEST_ROOT:-$(cd "$SCRIPT_DIR/../../../.." && pwd)}"
PROJECT_ROOT="$REPO_ROOT/clients/native"
SERVER_ENV_ROOT="$PROJECT_ROOT"
# shellcheck source=/dev/null
source "$PROJECT_ROOT/scripts/lib/simulator.sh"
TART_DERIVED_DATA_ROOT="${TART_DERIVED_DATA_ROOT:-$PROJECT_ROOT/.tart-derived-data}"

IPHONE_DEVICE_NAMES="iPhone 17,iPhone 17 Pro"
IPAD_DEVICE_NAMES="iPad Pro 11-inch (M5),iPad Pro 13-inch (M5),iPad Air 13-inch (M4)"

usage() {
	cat <<'EOF'
Usage: run-guest-shard.sh <shard>

Run one Signalbox Native validation shard inside a macOS Tart guest.

Shards:
  xcode              Xcode iOS Simulator build and unit/UI tests.
  bazel              Bazel Apple build/test and screenshot golden test.
  macos-screenshots  macOS screenshot export and golden check.
  ios-screenshots    iPhone screenshot capture.
  ipados-screenshots iPadOS landscape screenshot capture.
  screenshots        iPhone, iPadOS, and macOS screenshot capture.
  real-smoke         Real local/remote server UI smoke test.
  privacy            Privacy/no-telemetry scan.
  all                Full single-VM validation pass.

Environment:
  SCREENSHOT_DEVICE_NAMES       Override screenshot device list.
  SCREENSHOT_STATE_NAMES        Limit screenshot states.
  TART_SERVER_URL                  Server URL reachable from inside the VM.
  TART_SECRET_ENV_PATH          Optional mounted dotenv file containing
                                SIGNALBOX_NATIVE_REAL_SERVER_API_KEY.
  SIGNALBOX_NATIVE_REAL_SERVER_URL   Real-smoke server URL override.
  SIGNALBOX_NATIVE_REAL_SERVER_API_KEY
                                Real-smoke API key override; not printed.
EOF
}

list_shards() {
	cat <<'EOF'
xcode
bazel
macos-screenshots
ios-screenshots
ipados-screenshots
screenshots
real-smoke
privacy
all
EOF
}

require_tool() {
	local tool_name="$1"
	if ! command -v "$tool_name" >/dev/null 2>&1; then
		echo "Missing required tool in Tart guest: $tool_name" >&2
		exit 1
	fi
}

run_step() {
	local description="$1"
	shift
	echo "==> $description"
	"$@"
}

load_server_environment_if_present() {
	load_known_dotenv_keys_if_present "$SERVER_ENV_ROOT/.env" preserve
	if [[ -n "${TART_SECRET_ENV_PATH:-}" ]]; then
		load_known_dotenv_keys_if_present "$TART_SECRET_ENV_PATH" override
	fi
}

load_known_dotenv_keys_if_present() {
	local env_file="$1"
	local assignment_policy="${2:-preserve}"
	local line
	local key
	local value

	if [[ ! -f "$env_file" ]]; then
		return 0
	fi

	while IFS= read -r line || [[ -n "$line" ]]; do
		line="${line%$'\r'}"
		[[ "$line" =~ ^[[:space:]]*$ || "$line" =~ ^[[:space:]]*# ]] && continue
		line="${line#"${line%%[![:space:]]*}"}"
		if [[ "$line" == export[[:space:]]* ]]; then
			line="${line#export}"
			line="${line#"${line%%[![:space:]]*}"}"
		fi
		if [[ "$line" != *=* ]]; then
			continue
		fi
		key="${line%%=*}"
		value="${line#*=}"
		key="${key%"${key##*[![:space:]]}"}"
		value="${value#"${value%%[![:space:]]*}"}"
		value="${value%"${value##*[![:space:]]}"}"
		value="$(unquote_dotenv_value "$value")"
		assign_known_dotenv_key "$key" "$value" "$assignment_policy"
	done <"$env_file"
}

unquote_dotenv_value() {
	local value="$1"
	if [[ ${#value} -ge 2 ]]; then
		if [[ "$value" == \"*\" && "$value" == *\" ]]; then
			value="${value:1:${#value}-2}"
		elif [[ "$value" == \'*\' && "$value" == *\' ]]; then
			value="${value:1:${#value}-2}"
		fi
	fi
	printf '%s\n' "$value"
}

assign_known_dotenv_key() {
	local key="$1"
	local value="$2"
	local assignment_policy="$3"

	case "$key" in
	SIGNALBOX_API_KEY)
		if should_assign_dotenv_key SIGNALBOX_API_KEY "$assignment_policy"; then
			export SIGNALBOX_API_KEY="$value"
		fi
		;;
	SIGNALBOX_BASE_URL)
		if should_assign_dotenv_key SIGNALBOX_BASE_URL "$assignment_policy"; then
			export SIGNALBOX_BASE_URL="$value"
		fi
		;;
	SIGNALBOX_NATIVE_REAL_SERVER_URL)
		if should_assign_dotenv_key SIGNALBOX_NATIVE_REAL_SERVER_URL "$assignment_policy"; then
			export SIGNALBOX_NATIVE_REAL_SERVER_URL="$value"
		fi
		;;
	SIGNALBOX_NATIVE_REAL_SERVER_API_KEY)
		if should_assign_dotenv_key SIGNALBOX_NATIVE_REAL_SERVER_API_KEY "$assignment_policy"; then
			export SIGNALBOX_NATIVE_REAL_SERVER_API_KEY="$value"
		fi
		;;
	esac
}

should_assign_dotenv_key() {
	local variable_name="$1"
	local assignment_policy="$2"

	if [[ "$assignment_policy" == "override" ]]; then
		return 0
	fi

	[[ -z "${!variable_name+x}" ]]
}

host_router_server_url() {
	local router_ip
	router_ip="$(netstat -nr | awk '/default/{print $2; exit}')"
	if [[ -z "$router_ip" ]]; then
		echo "Unable to infer the host router IP from the Tart guest." >&2
		return 1
	fi
	printf 'http://%s:8000\n' "$router_ip"
}

resolved_real_server_url() {
	if [[ -n "${SIGNALBOX_NATIVE_REAL_SERVER_URL:-}" ]]; then
		printf '%s\n' "$SIGNALBOX_NATIVE_REAL_SERVER_URL"
		return 0
	fi
	if [[ -n "${TART_SERVER_URL:-}" ]]; then
		printf '%s\n' "$TART_SERVER_URL"
		return 0
	fi
	if [[ -n "${SIGNALBOX_BASE_URL:-}" && "$SIGNALBOX_BASE_URL" != "http://127.0.0.1:"* && "$SIGNALBOX_BASE_URL" != "http://localhost:"* ]]; then
		printf '%s\n' "$SIGNALBOX_BASE_URL"
		return 0
	fi
	host_router_server_url
}

resolved_real_server_api_key() {
	if [[ -n "${SIGNALBOX_NATIVE_REAL_SERVER_API_KEY:-}" ]]; then
		printf '%s\n' "$SIGNALBOX_NATIVE_REAL_SERVER_API_KEY"
		return 0
	fi
	if [[ -n "${SIGNALBOX_API_KEY:-}" ]]; then
		printf '%s\n' "$SIGNALBOX_API_KEY"
		return 0
	fi
	return 1
}

# The three helpers below have parenthesised bodies, so they always run in a
# subshell. Environment-file values loaded inside them stay in that subshell and
# never become exported state of the shard process, which keeps the real server
# API key out of every build/test process except the one command that is given
# it explicitly.
real_server_api_key_is_available() (
	load_server_environment_if_present
	resolved_real_server_api_key >/dev/null
)

load_and_resolve_real_server_url() (
	load_server_environment_if_present
	resolved_real_server_url
)

load_and_resolve_real_server_api_key() (
	load_server_environment_if_present
	resolved_real_server_api_key
)

assert_xcresult_passed() {
	local result_bundle_path="$1"
	local summary

	summary="$(xcrun xcresulttool get test-results summary --path "$result_bundle_path" --compact)"
	if [[ "$summary" != *'"result":"Passed"'* ]]; then
		printf '%s\n' "$summary" >&2
		return 1
	fi
}

run_xcode_shard() {
	require_tool xcodebuild
	local server_url=""
	if real_server_api_key_is_available && server_url="$(load_and_resolve_real_server_url)"; then
		export SIGNALBOX_NATIVE_REAL_SERVER_URL="$server_url"
		echo "==> Real server URL configured for $server_url; the API key stays scoped to the real-smoke shard."
	fi

	run_step "Xcode iOS Simulator build" "$PROJECT_ROOT/scripts/build-xcode.sh"
	run_step "Xcode iOS Simulator tests" "$PROJECT_ROOT/scripts/test-xcode.sh"
}

run_bazel_shard() {
	require_tool bazel
	run_step "Bazel Apple build" "$PROJECT_ROOT/scripts/build-bazel.sh"
	run_step "Bazel Apple tests" "$PROJECT_ROOT/scripts/test-bazel.sh"
	run_step "Bazel screenshot golden test" bazel test --config=apple_host //clients/native:screenshot_golden_test
}

run_macos_screenshots_shard() {
	run_step "macOS screenshots" "$PROJECT_ROOT/scripts/capture-macos-screenshots.sh"
}

run_ios_screenshots_shard() {
	local devices="${SCREENSHOT_DEVICE_NAMES:-$IPHONE_DEVICE_NAMES}"
	run_step "iPhone screenshots" env SCREENSHOT_DEVICE_NAMES="$devices" "$PROJECT_ROOT/scripts/capture-screenshots.sh"
}

run_ipados_screenshots_shard() {
	local devices="${SCREENSHOT_DEVICE_NAMES:-$IPAD_DEVICE_NAMES}"
	run_step "iPadOS landscape screenshots" env SCREENSHOT_DEVICE_NAMES="$devices" "$PROJECT_ROOT/scripts/capture-screenshots.sh"
}

run_screenshots_shard() {
	run_ios_screenshots_shard
	run_ipados_screenshots_shard
	run_step "macOS screenshots" "$PROJECT_ROOT/scripts/capture-macos-screenshots.sh"
	run_step "update screenshot manifest" "$PROJECT_ROOT/scripts/update-screenshot-manifest.sh"
	run_step "screenshot golden check" "$PROJECT_ROOT/scripts/check-screenshot-goldens.sh"
}

run_real_smoke_shard() {
	require_tool xcodebuild

	local server_url
	local api_key
	local result_bundle_path="$SIGNALBOX_NATIVE_DERIVED_DATA_PATH/Logs/Test/SignalboxNative-RealSmoke.xcresult"
	local device_id
	local destination
	local xcodebuild_status
	server_url="$(load_and_resolve_real_server_url)"
	# api_key is a shell-local value that is handed to the single xcodebuild
	# invocation below through a command-scoped assignment; it is never exported.
	if ! api_key="$(load_and_resolve_real_server_api_key)"; then
		echo "Missing real server API key. Set SIGNALBOX_NATIVE_REAL_SERVER_API_KEY or provide clients/native/.env with SIGNALBOX_API_KEY." >&2
		exit 1
	fi
	device_id="$(simulator_resolve_iphone_ids "$SIMULATOR_DEFAULT_MIN_IOS_VERSION" | head -n 1)"
	if [[ -z "$device_id" ]]; then
		echo "No available iPhone simulator found for iOS $SIMULATOR_DEFAULT_MIN_IOS_VERSION or newer." >&2
		exit 1
	fi
	destination="$(simulator_xcode_destination_for_id "$device_id")"

	echo "==> Real server UI smoke against $server_url"
	echo "==> Real server UI smoke simulator: $device_id"
	echo "==> API key loaded for the smoke test; value intentionally not printed."
	mkdir -p "$(dirname "$result_bundle_path")"
	rm -rf "$result_bundle_path"
	set +e
	SIGNALBOX_NATIVE_REAL_SERVER_URL="$server_url" \
		SIGNALBOX_NATIVE_REAL_SERVER_API_KEY="$api_key" \
		xcodebuild \
		-quiet \
		-project "$PROJECT_ROOT/SignalboxNative.xcodeproj" \
		-scheme "SignalboxNative" \
		-configuration "Debug" \
		-destination "$destination" \
		-derivedDataPath "$SIGNALBOX_NATIVE_DERIVED_DATA_PATH" \
		-resultBundlePath "$result_bundle_path" \
		-only-testing:SignalboxNativeUITests/SignalboxNativeUITests/testRealServerConnectionListsRunnerAndCreatesSessionWhenConfigured \
		-parallel-testing-enabled NO \
		CODE_SIGNING_ALLOWED=YES \
		CODE_SIGN_IDENTITY=- \
		test
	xcodebuild_status=$?
	set -e
	if ((xcodebuild_status != 0)); then
		return "$xcodebuild_status"
	fi
	assert_xcresult_passed "$result_bundle_path"
}

run_privacy_shard() {
	run_step "privacy scan" "$PROJECT_ROOT/scripts/check-privacy.sh"
}

run_all_shards() {
	run_xcode_shard
	run_bazel_shard
	run_screenshots_shard
	run_real_smoke_shard
	run_privacy_shard
}

main() {
	if [[ "${1:-}" == "--help" || "${1:-}" == "-h" ]]; then
		usage
		return 0
	fi
	if [[ "${1:-}" == "--list" ]]; then
		list_shards
		return 0
	fi

	local shard="${1:-}"
	if [[ -z "$shard" ]]; then
		usage >&2
		return 2
	fi

	cd "$REPO_ROOT"
	export SIGNALBOX_NATIVE_DERIVED_DATA_PATH="$TART_DERIVED_DATA_ROOT/$shard/ios"
	export SIGNALBOX_NATIVE_MACOS_DERIVED_DATA_PATH="$TART_DERIVED_DATA_ROOT/$shard/macos"
	case "$shard" in
	xcode) run_xcode_shard ;;
	bazel) run_bazel_shard ;;
	macos-screenshots) run_macos_screenshots_shard ;;
	ios-screenshots) run_ios_screenshots_shard ;;
	ipados-screenshots) run_ipados_screenshots_shard ;;
	screenshots) run_screenshots_shard ;;
	real-smoke) run_real_smoke_shard ;;
	privacy) run_privacy_shard ;;
	all) run_all_shards ;;
	*)
		echo "Unknown Tart guest shard: $shard" >&2
		usage >&2
		return 2
		;;
	esac
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
	main "$@"
fi

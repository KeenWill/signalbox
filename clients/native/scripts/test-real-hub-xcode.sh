#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=/dev/null
source "$ROOT/scripts/lib/simulator.sh"

MIN_IOS_VERSION="$SIMULATOR_DEFAULT_MIN_IOS_VERSION"
DERIVED_DATA_PATH="${LLM_HUB_NATIVE_DERIVED_DATA_PATH:-$ROOT/.derivedData}"
RESULT_BUNDLE_PATH="${LLM_HUB_NATIVE_REAL_HUB_RESULT_BUNDLE_PATH:-$DERIVED_DATA_PATH/Logs/Test/LLMHubNative-RealHub.xcresult}"
TEST_IDENTIFIER="LLMHubNativeUITests/LLMHubNativeUITests/testRealHubConnectionListsRunnerAndCreatesSessionWhenConfigured"
XCTESTRUN_TEST_KEY="${LLM_HUB_NATIVE_XCTESTRUN_TEST_KEY:-}"

export LLM_HUB_NATIVE_REAL_HUB_URL="${LLM_HUB_NATIVE_REAL_HUB_URL:-http://127.0.0.1:8000}"

if [[ -z "${LLM_HUB_NATIVE_REAL_HUB_API_KEY:-}" ]]; then
	echo "Set LLM_HUB_NATIVE_REAL_HUB_API_KEY to the hub-api-key value before running the real hub smoke test." >&2
	exit 2
fi

REAL_HUB_ENV_PATH="$ROOT/../llm_hub/.env"
REAL_HUB_ENV_BACKUP=""
REAL_HUB_ENV_EXISTED=0

restore_real_hub_env_file() {
	if [[ -z "$REAL_HUB_ENV_BACKUP" ]]; then
		return 0
	fi

	if ((REAL_HUB_ENV_EXISTED)); then
		cp "$REAL_HUB_ENV_BACKUP" "$REAL_HUB_ENV_PATH"
	else
		rm -f "$REAL_HUB_ENV_PATH"
	fi
	rm -f "$REAL_HUB_ENV_BACKUP"
}

REAL_HUB_ENV_BACKUP="$(mktemp "${TMPDIR:-/tmp}/llm-hub-native-real-hub-env.XXXXXX")"
if [[ -f "$REAL_HUB_ENV_PATH" ]]; then
	cp "$REAL_HUB_ENV_PATH" "$REAL_HUB_ENV_BACKUP"
	REAL_HUB_ENV_EXISTED=1
fi
trap restore_real_hub_env_file EXIT
mkdir -p "$(dirname "$REAL_HUB_ENV_PATH")"
rm -f "$REAL_HUB_ENV_PATH"
touch "$REAL_HUB_ENV_PATH"
chmod 600 "$REAL_HUB_ENV_PATH"
{
	printf 'LLM_HUB_NATIVE_REAL_HUB_URL=%s\n' "$LLM_HUB_NATIVE_REAL_HUB_URL"
	printf 'LLM_HUB_NATIVE_REAL_HUB_API_KEY=%s\n' "$LLM_HUB_NATIVE_REAL_HUB_API_KEY"
	if [[ -n "${LLM_HUB_NATIVE_REAL_HUB_RUNNER_ID:-}" ]]; then
		printf 'LLM_HUB_NATIVE_REAL_HUB_RUNNER_ID=%s\n' "$LLM_HUB_NATIVE_REAL_HUB_RUNNER_ID"
	fi
} > "$REAL_HUB_ENV_PATH"

if [[ -n "${XCODE_DESTINATION:-}" ]]; then
	DESTINATION="$XCODE_DESTINATION"
else
	DEVICE_ID="$(simulator_resolve_iphone_ids "$MIN_IOS_VERSION" | head -n 1)"

	if [[ -z "$DEVICE_ID" ]]; then
		echo "No available iPhone simulator found for iOS $MIN_IOS_VERSION or newer. Set XCODE_DESTINATION to a valid xcodebuild destination." >&2
		exit 1
	fi

	echo "Using newest available iPhone simulator for iOS $MIN_IOS_VERSION or newer: $DEVICE_ID"
	DESTINATION="$(simulator_xcode_destination_for_id "$DEVICE_ID")"
fi

BUILD_CMD=(
	xcodebuild
	-quiet
	-project "$ROOT/LLMHubNative.xcodeproj"
	-scheme "LLMHubNative"
	-configuration "Debug"
	-destination "$DESTINATION"
	-derivedDataPath "$DERIVED_DATA_PATH"
	CODE_SIGNING_ALLOWED=YES
	CODE_SIGN_IDENTITY=-
	-parallel-testing-enabled NO
	-only-testing:"$TEST_IDENTIFIER"
	build-for-testing
)

printf '+ %q ' "${BUILD_CMD[@]}"
printf '\n'
"${BUILD_CMD[@]}"

XCTESTRUN_PATH="${LLM_HUB_NATIVE_XCTESTRUN_PATH:-}"
if [[ -z "$XCTESTRUN_PATH" ]]; then
	XCTESTRUN_PATH="$(find "$DERIVED_DATA_PATH/Build/Products" -name '*.xctestrun' -print | sort | tail -n 1)"
fi

if [[ -z "$XCTESTRUN_PATH" || ! -f "$XCTESTRUN_PATH" ]]; then
	echo "Could not find generated .xctestrun under $DERIVED_DATA_PATH/Build/Products." >&2
	exit 65
fi

plist_buddy=/usr/libexec/PlistBuddy

list_xctestrun_top_level_keys() {
	plutil -p "$XCTESTRUN_PATH" | awk -F'"' '/^  "[^"]+" =>/ { print $2 }'
}

xctestrun_path_exists() {
	local plist_path="$1"
	"$plist_buddy" -c "Print $plist_path" "$XCTESTRUN_PATH" >/dev/null 2>&1
}

xctestrun_path_value() {
	local plist_path="$1"
	"$plist_buddy" -c "Print $plist_path" "$XCTESTRUN_PATH" 2>/dev/null || true
}

xctestrun_key_matches_ui_tests() {
	local plist_path="$1"
	local blueprint_name
	local test_bundle_path

	blueprint_name="$(xctestrun_path_value "$plist_path:BlueprintName")"
	test_bundle_path="$(xctestrun_path_value "$plist_path:TestBundlePath")"
	[[ "$blueprint_name" == *LLMHubNativeUITests* || "$test_bundle_path" == *LLMHubNativeUITests.xctest* ]]
}

resolve_xctestrun_test_target_path_for_key() {
	local key="$1"
	local key_path=":$key"
	local index
	local target_path

	if xctestrun_key_matches_ui_tests "$key_path"; then
		printf '%s\n' "$key_path"
		return 0
	fi

	for index in $(seq 0 50); do
		target_path="$key_path:TestTargets:$index"
		if ! xctestrun_path_exists "$target_path"; then
			break
		fi
		if xctestrun_key_matches_ui_tests "$target_path"; then
			printf '%s\n' "$target_path"
			return 0
		fi
	done

	return 1
}

resolve_xctestrun_test_target_path() {
	local key
	local test_target_path

	if [[ -n "$XCTESTRUN_TEST_KEY" ]]; then
		if test_target_path="$(resolve_xctestrun_test_target_path_for_key "$XCTESTRUN_TEST_KEY")"; then
			printf '%s\n' "$test_target_path"
			return 0
		fi
		return 1
	fi

	while IFS= read -r key; do
		case "$key" in
		__xctestrun_metadata__)
			continue
			;;
		esac
		if test_target_path="$(resolve_xctestrun_test_target_path_for_key "$key")"; then
			printf '%s\n' "$test_target_path"
			return 0
		fi
	done < <(list_xctestrun_top_level_keys)

	return 1
}

if ! XCTESTRUN_TEST_TARGET_PATH="$(resolve_xctestrun_test_target_path)"; then
	echo "Could not find the LLMHubNativeUITests entry in $XCTESTRUN_PATH." >&2
	echo "Top-level .xctestrun keys:" >&2
	list_xctestrun_top_level_keys >&2
	echo "Set LLM_HUB_NATIVE_XCTESTRUN_TEST_KEY to one of those keys if needed." >&2
	exit 65
fi

ensure_xctestrun_dictionary() {
	local dictionary_path="$1"

	if ! xctestrun_path_exists "$dictionary_path"; then
		"$plist_buddy" -c "Add $dictionary_path dict" "$XCTESTRUN_PATH"
	fi
}

set_xctestrun_variable_in_dictionary() {
	local dictionary_path="$1"
	local name="$2"
	local value="$3"
	local variable_path="$dictionary_path:$name"

	ensure_xctestrun_dictionary "$dictionary_path"
	if xctestrun_path_exists "$variable_path"; then
		"$plist_buddy" -c "Set $variable_path $value" "$XCTESTRUN_PATH"
	else
		"$plist_buddy" -c "Add $variable_path string $value" "$XCTESTRUN_PATH"
	fi
}

set_xctestrun_environment() {
	local name="$1"
	local value="$2"

	set_xctestrun_variable_in_dictionary "$XCTESTRUN_TEST_TARGET_PATH:EnvironmentVariables" "$name" "$value"
	set_xctestrun_variable_in_dictionary "$XCTESTRUN_TEST_TARGET_PATH:TestingEnvironmentVariables" "$name" "$value"
	set_xctestrun_variable_in_dictionary "$XCTESTRUN_TEST_TARGET_PATH:UITargetAppEnvironmentVariables" "$name" "$value"
}

set_xctestrun_environment LLM_HUB_NATIVE_REAL_HUB_URL "$LLM_HUB_NATIVE_REAL_HUB_URL"
set_xctestrun_environment LLM_HUB_NATIVE_REAL_HUB_API_KEY "$LLM_HUB_NATIVE_REAL_HUB_API_KEY"
if [[ -n "${LLM_HUB_NATIVE_REAL_HUB_RUNNER_ID:-}" ]]; then
	set_xctestrun_environment LLM_HUB_NATIVE_REAL_HUB_RUNNER_ID "$LLM_HUB_NATIVE_REAL_HUB_RUNNER_ID"
fi

TEST_CMD=(
	xcodebuild
	-quiet
	-destination "$DESTINATION"
	-derivedDataPath "$DERIVED_DATA_PATH"
	-resultBundlePath "$RESULT_BUNDLE_PATH"
	-xctestrun "$XCTESTRUN_PATH"
	-only-testing:"$TEST_IDENTIFIER"
	test-without-building
)

print_xcresult_summary() {
	if [[ -d "$RESULT_BUNDLE_PATH" ]]; then
		xcrun xcresulttool get test-results summary --path "$RESULT_BUNDLE_PATH" --compact >&2 || true
	fi
}

mkdir -p "$(dirname "$RESULT_BUNDLE_PATH")"
rm -rf "$RESULT_BUNDLE_PATH"
printf '+ %q ' "${TEST_CMD[@]}"
printf '\n'
set +e
"${TEST_CMD[@]}"
xcodebuild_status=$?
set -e

if ((xcodebuild_status != 0)); then
	print_xcresult_summary
	exit "$xcodebuild_status"
fi

summary="$(xcrun xcresulttool get test-results summary --path "$RESULT_BUNDLE_PATH" --compact)"
if [[ "$summary" != *'"result":"Passed"'* ]]; then
	printf '%s\n' "$summary" >&2
	exit 65
fi

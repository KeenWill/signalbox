#!/usr/bin/env bash
set -euo pipefail

resolve_script_dir() {
	local local_dir
	local_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
	if [[ -f "$local_dir/run-guest-shard.sh" ]]; then
		printf '%s\n' "$local_dir"
		return 0
	fi

	if [[ -n "${TEST_SRCDIR:-}" && -n "${TEST_WORKSPACE:-}" ]]; then
		local runfiles_dir="$TEST_SRCDIR/$TEST_WORKSPACE/clients/native/scripts/tart"
		if [[ -f "$runfiles_dir/run-guest-shard.sh" ]]; then
			printf '%s\n' "$runfiles_dir"
			return 0
		fi
	fi

	echo "Could not resolve Tart script directory." >&2
	return 1
}

SCRIPT_DIR="$(resolve_script_dir)"
SECRET_PLAN_SENTINEL="super-secret-for-plan-test"

test_tart_secret_env_overrides_project_env() (
	local temp_dir
	temp_dir="$(mktemp -d)"
	trap 'rm -rf "$temp_dir"' EXIT

	mkdir -p "$temp_dir/signalbox"
	printf 'SIGNALBOX_NATIVE_REAL_SERVER_API_KEY=stale-project-env\n' >"$temp_dir/signalbox/.env"
	printf 'SIGNALBOX_NATIVE_REAL_SERVER_API_KEY=host-secret-env\n' >"$temp_dir/secret.env"

	# shellcheck source=/dev/null
	source "$SCRIPT_DIR/run-guest-shard.sh"
	export SERVER_ENV_ROOT="$temp_dir/signalbox"
	export TART_SECRET_ENV_PATH="$temp_dir/secret.env"
	unset SIGNALBOX_NATIVE_REAL_SERVER_API_KEY

	load_server_environment_if_present
	if [[ "${SIGNALBOX_NATIVE_REAL_SERVER_API_KEY:-}" != "host-secret-env" ]]; then
		echo "TART_SECRET_ENV_PATH did not override the project .env API key." >&2
		return 1
	fi
)

test_real_server_api_key_resolution_stays_subshell_scoped() (
	local temp_dir
	local api_key
	temp_dir="$(mktemp -d)"
	trap 'rm -rf "$temp_dir"' EXIT

	mkdir -p "$temp_dir/signalbox"
	printf 'SIGNALBOX_NATIVE_REAL_SERVER_API_KEY=%s\n' "$SECRET_PLAN_SENTINEL" >"$temp_dir/signalbox/.env"

	# shellcheck source=/dev/null
	source "$SCRIPT_DIR/run-guest-shard.sh"
	export SERVER_ENV_ROOT="$temp_dir/signalbox"
	unset TART_SECRET_ENV_PATH
	unset SIGNALBOX_NATIVE_REAL_SERVER_API_KEY
	unset SIGNALBOX_API_KEY

	api_key="$(load_and_resolve_real_server_api_key)"
	if [[ "$api_key" != "$SECRET_PLAN_SENTINEL" ]]; then
		echo "load_and_resolve_real_server_api_key did not resolve the project .env API key." >&2
		return 1
	fi
	if [[ -n "${SIGNALBOX_NATIVE_REAL_SERVER_API_KEY:-}" ]]; then
		echo "load_and_resolve_real_server_api_key leaked the API key into the shard environment." >&2
		return 1
	fi
)

test_xcode_shard_adds_real_server_smoke_to_existing_skips() (
	local -a shard_commands=()
	local caller_skip_identifier="SignalboxNativeScreenshotTests"
	local expected_test_command
	# shellcheck source=/dev/null
	source "$SCRIPT_DIR/run-guest-shard.sh"
	export SIGNALBOX_NATIVE_SKIP_TESTING="$caller_skip_identifier"

	require_tool() { :; }
	run_step() {
		shift
		shard_commands+=("$*")
	}

	run_xcode_shard
	expected_test_command="env SIGNALBOX_NATIVE_SKIP_TESTING=$caller_skip_identifier $REAL_SERVER_SMOKE_TEST_IDENTIFIER $PROJECT_ROOT/scripts/test-xcode.sh"
	[[ "${shard_commands[1]}" == "$expected_test_command" ]]
)

test_xcode_shard_preserves_caller_skip_environment() (
	local caller_skip_identifier="SignalboxNativeScreenshotTests"
	# shellcheck source=/dev/null
	source "$SCRIPT_DIR/run-guest-shard.sh"
	export SIGNALBOX_NATIVE_SKIP_TESTING="$caller_skip_identifier"

	require_tool() { :; }
	run_step() { :; }

	run_xcode_shard
	[[ "$SIGNALBOX_NATIVE_SKIP_TESTING" == "$caller_skip_identifier" ]]
)

test_host_xcode_shard_forwards_skip_selection() (
	local caller_skip_identifier="SignalboxNativeScreenshotTests"
	local xcode_plan

	xcode_plan="$(SIGNALBOX_NATIVE_SKIP_TESTING="$caller_skip_identifier" "$SCRIPT_DIR/run-shard.sh" --print-plan xcode)"
	[[ "$xcode_plan" == *"SIGNALBOX_NATIVE_SKIP_TESTING=$caller_skip_identifier"* ]]
)

test_real_smoke_shard_stops_before_legacy_setup() (
	local output
	# shellcheck source=/dev/null
	source "$SCRIPT_DIR/run-guest-shard.sh"

	require_tool() {
		echo "real-smoke unexpectedly required a tool." >&2
		return 1
	}
	load_and_resolve_real_server_url() {
		echo "real-smoke unexpectedly resolved a server URL." >&2
		return 1
	}
	load_and_resolve_real_server_api_key() {
		echo "real-smoke unexpectedly resolved an API key." >&2
		return 1
	}

	output="$(run_real_smoke_shard)"
	[[ "$output" == *"remote/mobile transport is a maintainer design gate"* ]]
)

bash -n "$SCRIPT_DIR/run-guest-shard.sh"
bash -n "$SCRIPT_DIR/run-shard.sh"
bash -n "$SCRIPT_DIR/run-matrix.sh"

"$SCRIPT_DIR/run-guest-shard.sh" --list >/dev/null
"$SCRIPT_DIR/run-shard.sh" --print-plan xcode >/dev/null
secret_plan="$(SIGNALBOX_NATIVE_REAL_SERVER_API_KEY="$SECRET_PLAN_SENTINEL" "$SCRIPT_DIR/run-shard.sh" --print-plan real-smoke)"
if [[ "$secret_plan" == *"$SECRET_PLAN_SENTINEL"* ]]; then
	echo "run-shard.sh --print-plan leaked SIGNALBOX_NATIVE_REAL_SERVER_API_KEY." >&2
	exit 1
fi
if [[ "$secret_plan" != *"TART_SECRET_ENV_PATH="* ]]; then
	echo "run-shard.sh --print-plan did not show the mounted secret env path." >&2
	exit 1
fi
test_tart_secret_env_overrides_project_env
test_real_server_api_key_resolution_stays_subshell_scoped
test_xcode_shard_adds_real_server_smoke_to_existing_skips
test_xcode_shard_preserves_caller_skip_environment
test_host_xcode_shard_forwards_skip_selection
test_real_smoke_shard_stops_before_legacy_setup
"$SCRIPT_DIR/run-matrix.sh" --print-plan >/dev/null

echo "Tart scripts passed dry-run validation."

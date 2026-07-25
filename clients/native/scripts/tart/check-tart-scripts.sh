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
SERVER_URL_SENTINEL="http://192.0.2.10:8000"

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

test_xcode_shard_does_not_export_real_server_api_key() (
	local temp_dir
	temp_dir="$(mktemp -d)"
	trap 'rm -rf "$temp_dir"' EXIT

	mkdir -p "$temp_dir/signalbox"
	{
		printf 'SIGNALBOX_NATIVE_REAL_SERVER_API_KEY=%s\n' "$SECRET_PLAN_SENTINEL"
		printf 'SIGNALBOX_NATIVE_REAL_SERVER_URL=%s\n' "$SERVER_URL_SENTINEL"
	} >"$temp_dir/signalbox/.env"

	# shellcheck source=/dev/null
	source "$SCRIPT_DIR/run-guest-shard.sh"
	export SERVER_ENV_ROOT="$temp_dir/signalbox"
	unset TART_SECRET_ENV_PATH
	unset SIGNALBOX_NATIVE_REAL_SERVER_API_KEY
	unset SIGNALBOX_NATIVE_REAL_SERVER_URL
	unset SIGNALBOX_API_KEY

	require_tool() { :; }
	run_step() { :; }

	run_xcode_shard
	if [[ -n "${SIGNALBOX_NATIVE_REAL_SERVER_API_KEY:-}" || -n "${SIGNALBOX_API_KEY:-}" ]]; then
		echo "run_xcode_shard exported a real server API key into the shard environment." >&2
		return 1
	fi
	if [[ "${SIGNALBOX_NATIVE_REAL_SERVER_URL:-}" != "$SERVER_URL_SENTINEL" ]]; then
		echo "run_xcode_shard did not configure the real server URL." >&2
		return 1
	fi
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
test_xcode_shard_does_not_export_real_server_api_key
"$SCRIPT_DIR/run-matrix.sh" --print-plan >/dev/null

echo "Tart scripts passed dry-run validation."

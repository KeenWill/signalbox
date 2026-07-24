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
		local runfiles_dir="$TEST_SRCDIR/$TEST_WORKSPACE/projects/llm_hub_native/scripts/tart"
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

	mkdir -p "$temp_dir/llm_hub"
	printf 'LLM_HUB_NATIVE_REAL_HUB_API_KEY=stale-project-env\n' >"$temp_dir/llm_hub/.env"
	printf 'LLM_HUB_NATIVE_REAL_HUB_API_KEY=host-secret-env\n' >"$temp_dir/secret.env"

	# shellcheck source=/dev/null
	source "$SCRIPT_DIR/run-guest-shard.sh"
	export LLM_HUB_ROOT="$temp_dir/llm_hub"
	export TART_SECRET_ENV_PATH="$temp_dir/secret.env"
	unset LLM_HUB_NATIVE_REAL_HUB_API_KEY

	load_hub_environment_if_present
	if [[ "${LLM_HUB_NATIVE_REAL_HUB_API_KEY:-}" != "host-secret-env" ]]; then
		echo "TART_SECRET_ENV_PATH did not override the project .env API key." >&2
		return 1
	fi
)

bash -n "$SCRIPT_DIR/run-guest-shard.sh"
bash -n "$SCRIPT_DIR/run-shard.sh"
bash -n "$SCRIPT_DIR/run-matrix.sh"

"$SCRIPT_DIR/run-guest-shard.sh" --list >/dev/null
"$SCRIPT_DIR/run-shard.sh" --print-plan xcode >/dev/null
secret_plan="$(LLM_HUB_NATIVE_REAL_HUB_API_KEY="$SECRET_PLAN_SENTINEL" "$SCRIPT_DIR/run-shard.sh" --print-plan real-smoke)"
if [[ "$secret_plan" == *"$SECRET_PLAN_SENTINEL"* ]]; then
	echo "run-shard.sh --print-plan leaked LLM_HUB_NATIVE_REAL_HUB_API_KEY." >&2
	exit 1
fi
if [[ "$secret_plan" != *"TART_SECRET_ENV_PATH="* ]]; then
	echo "run-shard.sh --print-plan did not show the mounted secret env path." >&2
	exit 1
fi
test_tart_secret_env_overrides_project_env
"$SCRIPT_DIR/run-matrix.sh" --print-plan >/dev/null

echo "Tart scripts passed dry-run validation."

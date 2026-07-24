#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
RESULTS_ROOT="${TART_RESULTS_ROOT:-$PROJECT_ROOT/.tart-results}"
MATRIX_ID="${TART_MATRIX_ID:-$(date +%Y%m%d%H%M%S)-$$}"
MATRIX_RESULTS_DIR="$RESULTS_ROOT/matrix-$MATRIX_ID"
TART_PARALLELISM="${TART_PARALLELISM:-2}"
TART_UPDATE_SCREENSHOT_MANIFEST="${TART_UPDATE_SCREENSHOT_MANIFEST:-1}"
PRINT_PLAN=0

DEFAULT_SHARDS=(
	xcode
	macos-screenshots
	ios-screenshots
	ipados-screenshots
	privacy
)

usage() {
	cat <<'EOF'
Usage: run-matrix.sh [options] [shard ...]

Run LLM Hub Native Tart validation shards, optionally in parallel.

Options:
  --print-plan  Print the shard plan without running Tart.
  -h, --help    Show this help.

Environment:
  TART_SHARDS                     Comma-separated shard list. Overrides defaults.
  TART_INCLUDE_REAL_SMOKE=1        Add real-smoke to the default matrix.
  TART_PARALLELISM=2               Maximum concurrent Tart VMs.
  TART_UPDATE_SCREENSHOT_MANIFEST=1
                                  Update and check screenshot manifest after screenshot shards.

Examples:
  TART_PARALLELISM=2 run-matrix.sh
  TART_SHARDS=ios-screenshots,ipados-screenshots run-matrix.sh
  run-matrix.sh xcode real-smoke
EOF
}

parse_args() {
	while (($# > 0)); do
		case "$1" in
		--print-plan)
			PRINT_PLAN=1
			shift
			;;
		-h | --help)
			usage
			exit 0
			;;
		--)
			shift
			break
			;;
		-*)
			echo "Unknown option: $1" >&2
			usage >&2
			exit 2
			;;
		*)
			break
			;;
		esac
	done

	if (($# > 0)); then
		SHARDS=("$@")
	elif [[ -n "${TART_SHARDS:-}" ]]; then
		IFS=',' read -r -a SHARDS <<<"$TART_SHARDS"
	else
		SHARDS=("${DEFAULT_SHARDS[@]}")
		if [[ "${TART_INCLUDE_REAL_SMOKE:-0}" == "1" ]]; then
			SHARDS+=(real-smoke)
		fi
	fi
}

trim() {
	local value="$1"
	value="${value#"${value%%[![:space:]]*}"}"
	value="${value%"${value##*[![:space:]]}"}"
	printf '%s\n' "$value"
}

normalise_shards() {
	local shard
	local cleaned=()
	for shard in "${SHARDS[@]}"; do
		shard="$(trim "$shard")"
		if [[ -n "$shard" ]]; then
			cleaned+=("$shard")
		fi
	done
	SHARDS=("${cleaned[@]}")
}

contains_screenshot_shard() {
	local shard
	for shard in "${SHARDS[@]}"; do
		case "$shard" in
		ios-screenshots | ipados-screenshots | macos-screenshots | screenshots | all)
			return 0
			;;
		esac
	done
	return 1
}

print_plan() {
	local shard
	echo "Tart matrix plan"
	echo "  results:     $MATRIX_RESULTS_DIR"
	echo "  parallelism: $TART_PARALLELISM"
	echo "  shards:"
	for shard in "${SHARDS[@]}"; do
		echo "    - $shard"
	done
	echo
	for shard in "${SHARDS[@]}"; do
		"$SCRIPT_DIR/run-shard.sh" --print-plan "$shard"
		echo
	done
}

running_job_count() {
	jobs -pr | wc -l | tr -d '[:space:]'
}

run_shard_in_background() {
	local shard="$1"
	local log_file="$MATRIX_RESULTS_DIR/$shard.log"
	local status_file="$MATRIX_RESULTS_DIR/$shard.status"

	(
		set +e
		"$SCRIPT_DIR/run-shard.sh" "$shard" >"$log_file" 2>&1
		shard_status=$?
		if ((shard_status == 0)) && grep -Eq '(^|\*\* )TEST FAILED|Missing required (host )?tool|Permission denied \(publickey,password,keyboard-interactive\)' "$log_file"; then
			echo "Shard $shard produced a success exit status, but its log contains a hard failure marker." >>"$log_file"
			shard_status=1
		fi
		printf '%s\n' "$shard_status" >"$status_file"
		exit "$shard_status"
	) &
}

wait_for_capacity() {
	while (($(running_job_count) >= TART_PARALLELISM)); do
		sleep 5
	done
}

wait_for_all_shards() {
	local failure=0
	local pid
	for pid in $(jobs -pr); do
		if ! wait "$pid"; then
			failure=1
		fi
	done
	return "$failure"
}

report_statuses() {
	local shard
	local status_file
	local status
	for shard in "${SHARDS[@]}"; do
		status_file="$MATRIX_RESULTS_DIR/$shard.status"
		if [[ -f "$status_file" ]]; then
			status="$(cat "$status_file")"
		else
			status="missing"
		fi
		echo "$shard: $status"
	done
}

validate_statuses() {
	local shard
	local status_file
	local status
	local failure=0

	for shard in "${SHARDS[@]}"; do
		status_file="$MATRIX_RESULTS_DIR/$shard.status"
		if [[ ! -f "$status_file" ]]; then
			echo "Shard $shard did not write a status file. See $MATRIX_RESULTS_DIR/$shard.log" >&2
			failure=1
			continue
		fi
		status="$(cat "$status_file")"
		if [[ "$status" != "0" ]]; then
			echo "Shard $shard failed with status $status. See $MATRIX_RESULTS_DIR/$shard.log" >&2
			failure=1
		fi
	done

	return "$failure"
}

update_screenshot_manifest_if_needed() {
	if [[ "$TART_UPDATE_SCREENSHOT_MANIFEST" != "1" ]]; then
		return 0
	fi
	if ! contains_screenshot_shard; then
		return 0
	fi

	"$PROJECT_ROOT/scripts/update-screenshot-manifest.sh"
	"$PROJECT_ROOT/scripts/check-screenshot-goldens.sh"
}

main() {
	parse_args "$@"
	normalise_shards

	if ((${#SHARDS[@]} == 0)); then
		echo "No Tart shards selected." >&2
		exit 2
	fi

	if [[ "$PRINT_PLAN" == "1" ]]; then
		print_plan
		return 0
	fi

	mkdir -p "$MATRIX_RESULTS_DIR"
	echo "Writing Tart matrix logs to $MATRIX_RESULTS_DIR"

	local shard
	for shard in "${SHARDS[@]}"; do
		wait_for_capacity
		echo "==> Starting Tart shard: $shard"
		run_shard_in_background "$shard"
	done

	wait_for_all_shards || true
	report_statuses
	validate_statuses
	update_screenshot_manifest_if_needed
}

main "$@"

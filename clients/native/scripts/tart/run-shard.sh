#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

resolve_repo_root() {
	if [[ -n "${TART_REPO_HOST_PATH:-}" ]]; then
		printf '%s\n' "$TART_REPO_HOST_PATH"
		return 0
	fi

	if git_root="$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel 2>/dev/null)"; then
		printf '%s\n' "$git_root"
		return 0
	fi

	if [[ -n "${TEST_SRCDIR:-}" && -n "${TEST_WORKSPACE:-}" && -d "$TEST_SRCDIR/$TEST_WORKSPACE" ]]; then
		printf '%s\n' "$TEST_SRCDIR/$TEST_WORKSPACE"
		return 0
	fi

	cd "$SCRIPT_DIR/../../../.."
	pwd
}

REPO_ROOT="$(resolve_repo_root)"

DEFAULT_BASE_IMAGE="ghcr.io/cirruslabs/macos-tahoe-xcode:latest"
DEFAULT_REPO_MOUNT_NAME="mono"
DEFAULT_SECRET_MOUNT_NAME="llm-hub-native-secrets"

TART_BASE_IMAGE="${TART_BASE_IMAGE:-$DEFAULT_BASE_IMAGE}"
TART_VM_PREFIX="${TART_VM_PREFIX:-llm-hub-native}"
TART_REPO_MOUNT_NAME="${TART_REPO_MOUNT_NAME:-$DEFAULT_REPO_MOUNT_NAME}"
TART_SECRET_MOUNT_NAME="${TART_SECRET_MOUNT_NAME:-$DEFAULT_SECRET_MOUNT_NAME}"
TART_SSH_USERNAME="${TART_SSH_USERNAME:-admin}"
TART_SSH_PASSWORD="${TART_SSH_PASSWORD:-admin}"
TART_BOOT_TIMEOUT_SECONDS="${TART_BOOT_TIMEOUT_SECONDS:-420}"
TART_KEEP_VM="${TART_KEEP_VM:-0}"
TART_REUSE_VM="${TART_REUSE_VM:-0}"
TART_VM_CPUS="${TART_VM_CPUS:-4}"
TART_VM_MEMORY_MB="${TART_VM_MEMORY_MB:-8192}"
TART_VM_DISPLAY="${TART_VM_DISPLAY:-1920x1200}"
TART_EXECUTOR="${TART_EXECUTOR:-guest-agent}"

PRINT_PLAN=0
SHARD=""
VM_NAME="${TART_VM_NAME:-}"
CREATED_VM=0
RUN_PID=""
SECRET_ENV_DIR=""
SECRET_ENV_FILE=""

usage() {
	cat <<'EOF'
Usage: run-shard.sh [options] <shard>

Run one LLM Hub Native validation shard in a macOS Tart VM.

Options:
  --base-image IMAGE  Tart image to clone for ephemeral shards.
  --vm NAME           Existing or explicitly named Tart VM.
  --reuse-vm          Reuse --vm instead of cloning a fresh ephemeral VM.
  --keep-vm           Leave the VM after the shard exits.
  --print-plan        Print the Tart/SSH plan without running Tart.
  -h, --help          Show this help.

Environment:
  TART_BASE_IMAGE       Default: ghcr.io/cirruslabs/macos-tahoe-xcode:latest
  TART_VM_PREFIX        Default: llm-hub-native
  TART_VM_CPUS          Default: 4
  TART_VM_MEMORY_MB     Default: 8192
  TART_VM_DISPLAY       Default: 1920x1200
  TART_EXECUTOR         guest-agent or ssh. Default: guest-agent
  TART_HUB_URL          Hub URL reachable from inside the VM for real-smoke.
  TART_SECRET_MOUNT_NAME
                        Default: llm-hub-native-secrets
  LLM_HUB_NATIVE_REAL_HUB_API_KEY
                        Mounted into the guest through a temporary env file;
                        never embedded in the Tart command line.
  SCREENSHOT_STATE_NAMES
  SCREENSHOT_DEVICE_NAMES
EOF
}

parse_args() {
	while (($# > 0)); do
		case "$1" in
		--base-image)
			TART_BASE_IMAGE="${2:?missing value for --base-image}"
			shift 2
			;;
		--vm)
			VM_NAME="${2:?missing value for --vm}"
			shift 2
			;;
		--reuse-vm)
			TART_REUSE_VM=1
			shift
			;;
		--keep-vm)
			TART_KEEP_VM=1
			shift
			;;
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
			SHARD="$1"
			shift
			break
			;;
		esac
	done

	if [[ -z "$SHARD" ]]; then
		echo "Missing shard name." >&2
		usage >&2
		exit 2
	fi
}

slugify() {
	printf '%s\n' "$1" |
		tr '[:upper:]' '[:lower:]' |
		sed -E 's/[^a-z0-9]+/-/g; s/^-//; s/-$//'
}

require_tool() {
	local tool_name="$1"
	if ! command -v "$tool_name" >/dev/null 2>&1; then
		echo "Missing required host tool: $tool_name" >&2
		exit 1
	fi
}

prepare_secret_mount_if_needed() {
	if [[ -z "${LLM_HUB_NATIVE_REAL_HUB_API_KEY:-}" ]]; then
		return 0
	fi
	if [[ "$LLM_HUB_NATIVE_REAL_HUB_API_KEY" == *$'\n'* || "$LLM_HUB_NATIVE_REAL_HUB_API_KEY" == *$'\r'* ]]; then
		echo "LLM_HUB_NATIVE_REAL_HUB_API_KEY must be a single-line value." >&2
		exit 1
	fi

	SECRET_ENV_DIR="$(mktemp -d "${TMPDIR:-/tmp}/llm-hub-native-tart-secrets.XXXXXX")"
	SECRET_ENV_FILE="$SECRET_ENV_DIR/env"
	chmod 700 "$SECRET_ENV_DIR"
	(
		umask 077
		printf 'LLM_HUB_NATIVE_REAL_HUB_API_KEY=%s\n' "$LLM_HUB_NATIVE_REAL_HUB_API_KEY" >"$SECRET_ENV_FILE"
	)
	unset LLM_HUB_NATIVE_REAL_HUB_API_KEY
}

secret_mount_requested() {
	[[ -n "$SECRET_ENV_FILE" || -n "${LLM_HUB_NATIVE_REAL_HUB_API_KEY:-}" ]]
}

ssh_command() {
	local vm_ip="$1"
	local remote_command="$2"
	local -a ssh_args=(
		-o StrictHostKeyChecking=no
		-o UserKnownHostsFile=/dev/null
		-o ConnectTimeout=10
	)

	if [[ -n "${TART_SSH_IDENTITY_FILE:-}" ]]; then
		ssh_args+=(
			-o IdentitiesOnly=yes
			-i "$TART_SSH_IDENTITY_FILE"
		)
		# shellcheck disable=SC2029
		ssh "${ssh_args[@]}" "$TART_SSH_USERNAME@$vm_ip" "$remote_command"
	else
		ssh_args+=(
			-o "PreferredAuthentications=password,keyboard-interactive"
			-o PubkeyAuthentication=no
			-o KbdInteractiveAuthentication=yes
			-o NumberOfPasswordPrompts=1
		)
		# shellcheck disable=SC2029
		SSHPASS="$TART_SSH_PASSWORD" sshpass -e ssh "${ssh_args[@]}" "$TART_SSH_USERNAME@$vm_ip" "$remote_command"
	fi
}

run_remote_guest_command() {
	local vm_ip="$1"
	local remote_command
	local attempt
	local exit_status

	remote_command="$(remote_guest_command)"
	for attempt in 1 2 3; do
		if ssh_command "$vm_ip" "$remote_command"; then
			return 0
		fi
		exit_status=$?
		if [[ "$exit_status" != "255" || "$attempt" == "3" ]]; then
			return "$exit_status"
		fi
		sleep 5
	done
}

run_guest_agent_command() {
	local remote_command

	remote_command="$(remote_guest_command)"
	tart exec "$VM_NAME" /bin/bash -lc "$remote_command"
}

wait_for_ssh() {
	local vm_name="$1"
	local deadline=$((SECONDS + TART_BOOT_TIMEOUT_SECONDS))
	local vm_ip=""

	while ((SECONDS < deadline)); do
		vm_ip="$(tart ip "$vm_name" 2>/dev/null || true)"
		if [[ -n "$vm_ip" ]] && ssh_command "$vm_ip" "true" >/dev/null 2>&1; then
			printf '%s\n' "$vm_ip"
			return 0
		fi
		sleep 3
	done

	echo "Timed out after ${TART_BOOT_TIMEOUT_SECONDS}s waiting for SSH in Tart VM $vm_name." >&2
	return 1
}

wait_for_guest_agent() {
	local vm_name="$1"
	local deadline=$((SECONDS + TART_BOOT_TIMEOUT_SECONDS))

	while ((SECONDS < deadline)); do
		if tart exec "$vm_name" true >/dev/null 2>&1; then
			return 0
		fi
		sleep 3
	done

	echo "Timed out after ${TART_BOOT_TIMEOUT_SECONDS}s waiting for Tart guest agent in VM $vm_name." >&2
	return 1
}

remote_guest_command() {
	local guest_root="/Volumes/My Shared Files/$TART_REPO_MOUNT_NAME"
	local guest_script="$guest_root/projects/llm_hub_native/scripts/tart/run-guest-shard.sh"
	local -a environment_arguments=()

	if [[ -n "${TART_HUB_URL:-}" ]]; then
		environment_arguments+=("TART_HUB_URL=$TART_HUB_URL")
	fi
	if secret_mount_requested; then
		environment_arguments+=("TART_SECRET_ENV_PATH=/Volumes/My Shared Files/$TART_SECRET_MOUNT_NAME/env")
	fi
	if [[ -n "${SCREENSHOT_STATE_NAMES:-}" ]]; then
		environment_arguments+=("SCREENSHOT_STATE_NAMES=$SCREENSHOT_STATE_NAMES")
	fi
	if [[ -n "${SCREENSHOT_DEVICE_NAMES:-}" ]]; then
		environment_arguments+=("SCREENSHOT_DEVICE_NAMES=$SCREENSHOT_DEVICE_NAMES")
	fi

	printf 'cd %q && ' "$guest_root"
	if ((${#environment_arguments[@]} > 0)); then
		printf 'env '
		printf '%q ' "${environment_arguments[@]}"
	fi
	printf 'MONO_GUEST_ROOT=%q %q %q' "$guest_root" "$guest_script" "$SHARD"
}

cleanup() {
	local exit_status=$?

	if [[ -n "$RUN_PID" ]]; then
		kill "$RUN_PID" >/dev/null 2>&1 || true
		wait "$RUN_PID" >/dev/null 2>&1 || true
	fi

	if [[ -n "$VM_NAME" ]]; then
		tart stop "$VM_NAME" >/dev/null 2>&1 || true
	fi

	if [[ "$CREATED_VM" == "1" && "$TART_KEEP_VM" != "1" ]]; then
		tart delete "$VM_NAME" >/dev/null 2>&1 || true
	fi

	if [[ -n "$SECRET_ENV_DIR" ]]; then
		rm -rf "$SECRET_ENV_DIR"
	fi

	exit "$exit_status"
}

print_plan() {
	local planned_vm="$VM_NAME"
	if [[ -z "$planned_vm" ]]; then
		planned_vm="$TART_VM_PREFIX-$(slugify "$SHARD")-<timestamp>-<pid>"
	fi

	cat <<EOF
Tart shard plan
  shard:        $SHARD
  executor:     $TART_EXECUTOR
  base image:   $TART_BASE_IMAGE
  vm:           $planned_vm
  reuse vm:     $TART_REUSE_VM
  keep vm:      $TART_KEEP_VM
  repo mount:   $TART_REPO_MOUNT_NAME:$REPO_ROOT
  secret mount: $(
		if secret_mount_requested; then
			printf '%s:<temporary 0600 env file>' "$TART_SECRET_MOUNT_NAME"
		else
			printf 'none'
		fi
	)
  guest root:   /Volumes/My Shared Files/$TART_REPO_MOUNT_NAME
  guest command:
    $(remote_guest_command)
EOF
}

main() {
	parse_args "$@"

	case "$TART_EXECUTOR" in
	guest-agent | ssh) ;;
	*)
		echo "Unsupported TART_EXECUTOR=$TART_EXECUTOR. Use guest-agent or ssh." >&2
		exit 2
		;;
	esac

	if [[ "$PRINT_PLAN" == "1" ]]; then
		print_plan
		return 0
	fi

	require_tool tart
	if [[ "$TART_EXECUTOR" == "ssh" ]]; then
		require_tool ssh
	fi
	if [[ "$TART_EXECUTOR" == "ssh" && -z "${TART_SSH_IDENTITY_FILE:-}" ]]; then
		require_tool sshpass
	fi
	prepare_secret_mount_if_needed

	if [[ -z "$VM_NAME" ]]; then
		VM_NAME="$TART_VM_PREFIX-$(slugify "$SHARD")-$(date +%Y%m%d%H%M%S)-$$"
	fi

	trap cleanup EXIT INT TERM

	if [[ "$TART_REUSE_VM" != "1" ]]; then
		echo "==> Cloning Tart image $TART_BASE_IMAGE to $VM_NAME"
		tart clone "$TART_BASE_IMAGE" "$VM_NAME"
		CREATED_VM=1
		tart set "$VM_NAME" --cpu "$TART_VM_CPUS" --memory "$TART_VM_MEMORY_MB" --display "$TART_VM_DISPLAY"
	else
		echo "==> Reusing Tart VM $VM_NAME"
	fi

	echo "==> Starting Tart VM $VM_NAME"
	local -a tart_run_arguments=(--dir="$TART_REPO_MOUNT_NAME:$REPO_ROOT")
	if [[ -n "$SECRET_ENV_DIR" ]]; then
		tart_run_arguments+=(--dir="$TART_SECRET_MOUNT_NAME:$SECRET_ENV_DIR")
	fi
	tart run "${tart_run_arguments[@]}" "$VM_NAME" &
	RUN_PID="$!"

	local vm_ip=""
	if [[ "$TART_EXECUTOR" == "guest-agent" ]]; then
		wait_for_guest_agent "$VM_NAME"
		echo "==> Tart VM $VM_NAME is reachable through the Tart guest agent"
	else
		vm_ip="$(wait_for_ssh "$VM_NAME")"
		echo "==> Tart VM $VM_NAME is reachable at $vm_ip"
	fi
	echo "==> Running guest shard $SHARD"
	set +e
	if [[ "$TART_EXECUTOR" == "guest-agent" ]]; then
		run_guest_agent_command
	else
		run_remote_guest_command "$vm_ip"
	fi
	local guest_status=$?
	set -e
	return "$guest_status"
}

main "$@"

#!/bin/sh
# Keep one lifecycle-managed process available without taking ownership of it.
# The lifecycle program remains the only process mutation path: this watchdog
# observes by name and invokes only its `boot` operation after absence.
set -eu

if [ "$#" -lt 2 ] || [ "$#" -gt 3 ]; then
	echo "usage: supervise-lifecycle.sh <lifecycle-program> <process-name> [poll-seconds]" >&2
	exit 2
fi

lifecycle_program=$1
process_name=$2
poll_seconds=${3:-5}
managed_uid=$(id -u)

if [ ! -x "$lifecycle_program" ]; then
	echo "supervise-lifecycle: lifecycle program is not executable: $lifecycle_program" >&2
	exit 2
fi

if [ -z "$process_name" ] || [ "${#process_name}" -gt 15 ]; then
	echo "supervise-lifecycle: process-name must contain between 1 and 15 characters" >&2
	exit 2
fi

case "$process_name" in
	*[!A-Za-z0-9_-]*)
		echo "supervise-lifecycle: process-name must contain only letters, digits, underscores, and hyphens" >&2
		exit 2
		;;
esac

case "$poll_seconds" in
	'' | *[!0-9]*)
		echo "supervise-lifecycle: poll-seconds must be a positive integer" >&2
		exit 2
		;;
esac

if [ "$poll_seconds" -eq 0 ]; then
	echo "supervise-lifecycle: poll-seconds must be a positive integer" >&2
	exit 2
fi

case "$managed_uid" in
	'' | *[!0-9]*)
		echo "supervise-lifecycle: effective user ID must be numeric" >&2
		exit 2
		;;
esac

while :; do
	if pgrep -u "$managed_uid" -x -- "$process_name" >/dev/null; then
		sleep "$poll_seconds"
		continue
	else
		pgrep_status=$?
	fi
	if [ "$pgrep_status" -ne 1 ]; then
		echo "supervise-lifecycle: process lookup failed with status $pgrep_status" >&2
		exit "$pgrep_status"
	fi

	echo "supervise-lifecycle: $process_name is absent; invoking lifecycle boot" >&2
	if ! "$lifecycle_program" boot; then
		echo "supervise-lifecycle: lifecycle boot failed; retrying after $poll_seconds seconds" >&2
	fi
	sleep "$poll_seconds"
done

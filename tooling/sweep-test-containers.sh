#!/usr/bin/env bash
# Remove orphaned containers left behind by interrupted testcontainers runs.
#
# The Rust testcontainers client ships no Ryuk reaper: unlike the Java and Go
# implementations, `testcontainers` 0.27 removes a container only from
# `ContainerAsync`'s `Drop`, and creates it with `AutoRemove: false` so the
# daemon never reclaims it either. A test process that dies without unwinding —
# SIGKILL, an OOM kill, `std::process::exit`, a hard power loss — therefore
# strands every container it had started, permanently. The `watchdog` feature
# this repository enables closes the SIGTERM/SIGINT/SIGQUIT paths, which covers
# Ctrl-C and `timeout`, but no in-process handler can survive SIGKILL. This
# script is the backstop for what nothing in-process can catch.
#
# Selection is positive and repository-scoped: a container is swept only when it
# carries the label that this repository's own harness attaches immediately
# before starting it, spelled once in
# `signalbox_persistence::disposable_test_container_labels`. The label states
# the property that makes removal safe — this container exists only for the
# duration of one of this repository's tests, and removing it is already the
# harness's own job. Nothing is excluded by name or by image, because an
# exclusion list only protects what someone remembered to name: a container
# without the label is simply not this script's to remove, whether it is another
# project's test container, a hand-run database, or a long-lived instance that
# happens to run the same PostgreSQL image. The harness withholds the label when
# the operator set `TESTCONTAINERS_COMMAND=keep`, since a container the client
# was asked to keep is by definition not disposable.
#
# The age bound is what keeps a live test's container safe: a container serving
# a running test is minutes old at most, and the default two-hour floor sits far
# above any suite's runtime. Removal is `--volumes`, so each container's
# anonymous volume — roughly 49 MB of stranded disk apiece — goes with it.
#
# A volume whose container was already removed separately is dangling, belongs
# to no container, and so is beyond anything this script can select; every
# successful run reports how many exist and leaves reclaiming them to
# `docker volume prune`, which is not run here because it is not scoped to this
# repository's images.
#
# Reports what it would remove and exits without touching anything unless
# `--apply` is passed.
#
# Usage: sweep-test-containers.sh [--older-than-hours <n>]
#                                 [--daemon-probe-seconds <n>] [--apply]
set -euo pipefail

# Kept identical to `signalbox_persistence::DISPOSABLE_TEST_CONTAINER_LABEL_KEY`
# and its value; `tooling/test_sweep_test_containers.py` fails when the two
# drift.
readonly DISPOSABLE_LABEL='org.signalbox.disposable=test-container'

# `docker inspect` names each identifier it could not find on stderr and still
# prints every container it did find. A line of this shape is a container that
# left between the listing and the inspection, which is the outcome this sweep
# wants; any other line is a real fault that must not be read as "everything
# vanished".
readonly MISSING_OBJECT_PATTERN='^Error( response from daemon)?: No such (object|container): '

# `wait` reports a child killed by a signal as 128 plus the signal number, which
# is how the bounded probe below tells a daemon that never answered from one
# that refused.
readonly SIGNALLED_STATUS_FLOOR=128

older_than_hours=2
daemon_probe_seconds=10
apply=0

while [ "$#" -gt 0 ]; do
	case "$1" in
	--older-than-hours)
		if [ "$#" -lt 2 ]; then
			echo "sweep-test-containers: --older-than-hours needs a value" >&2
			exit 2
		fi
		older_than_hours="$2"
		shift 2
		;;
	--daemon-probe-seconds)
		if [ "$#" -lt 2 ]; then
			echo "sweep-test-containers: --daemon-probe-seconds needs a value" >&2
			exit 2
		fi
		daemon_probe_seconds="$2"
		shift 2
		;;
	--apply)
		apply=1
		shift
		;;
	-h | --help)
		echo "usage: sweep-test-containers.sh [--older-than-hours <n>]" \
			"[--daemon-probe-seconds <n>] [--apply]"
		exit 0
		;;
	*)
		echo "sweep-test-containers: unknown argument: $1" >&2
		echo "usage: sweep-test-containers.sh [--older-than-hours <n>]" \
			"[--daemon-probe-seconds <n>] [--apply]" >&2
		exit 2
		;;
	esac
done

case "$older_than_hours" in
'' | *[!0-9]*)
	echo "sweep-test-containers: --older-than-hours must be a whole number of" \
		"hours, got: $older_than_hours" >&2
	exit 2
	;;
esac

case "$daemon_probe_seconds" in
'' | 0 | *[!0-9]*)
	echo "sweep-test-containers: --daemon-probe-seconds must be a positive whole" \
		"number of seconds, got: $daemon_probe_seconds" >&2
	exit 2
	;;
esac

if ! command -v docker >/dev/null 2>&1; then
	echo "sweep-test-containers: docker is not on PATH" >&2
	exit 1
fi

# Dangling volumes are the part of the leak no container-scoped selector can
# reach, so every successful run reports them — including the runs that find
# nothing to remove. A box whose containers were already reclaimed without their
# volumes reports an empty container inventory, and stopping there would leave
# the remaining disk leak invisible.
report_dangling_volumes() {
	local dangling
	dangling="$(docker volume ls --quiet --filter dangling=true | wc -l | tr -d ' ')"
	if [ "$dangling" -gt 0 ]; then
		echo "sweep-test-containers: $dangling dangling volume(s) remain, belonging" \
			"to no container; reclaim with 'docker volume prune'"
	fi
}

finish_clean() {
	report_dangling_volumes
	exit 0
}

# A daemon that is stopped or refusing connections would otherwise surface as an
# empty container listing, which reads exactly like a clean box and would leave
# an operator believing the sweep found nothing to do. Ask once, and name the
# real problem.
#
# `docker version` carries no timeout of its own, so a daemon whose socket
# accepts but never answers would block here forever: on a timer that is a job
# that never reports and never returns, which is worse than the empty listing
# this probe exists to prevent. Bound the probe with a deadline process instead
# and report a daemon that did not answer separately from one that refused.
probe_daemon() {
	docker version --format '{{.Server.Version}}' >/dev/null 2>&1 &
	local probe=$!
	(
		sleep "$daemon_probe_seconds"
		kill -s TERM "$probe"
	) >/dev/null 2>&1 &
	local deadline=$!
	local status=0
	wait "$probe" || status=$?
	kill -s TERM "$deadline" >/dev/null 2>&1 || true
	wait "$deadline" >/dev/null 2>&1 || true
	return "$status"
}

probe_status=0
probe_daemon || probe_status=$?

if [ "$probe_status" -ge "$SIGNALLED_STATUS_FLOOR" ]; then
	echo "sweep-test-containers: the Docker daemon did not answer within" \
		"${daemon_probe_seconds}s" >&2
	exit 1
fi

if [ "$probe_status" -ne 0 ]; then
	echo "sweep-test-containers: cannot reach the Docker daemon" >&2
	exit 1
fi

candidates="$(docker ps --all --quiet --filter "label=$DISPOSABLE_LABEL")"

if [ -z "$candidates" ]; then
	echo "sweep-test-containers: no disposable test containers present"
	finish_clean
fi

# Identifiers arrive on stdin so a sweep of several thousand containers cannot
# overflow the argument list. Inspection's stderr is captured rather than passed
# through because the classification below reads it.
inspection_errors="$(mktemp)"
trap 'rm -f "$inspection_errors"' EXIT

inspection_status=0
inspected="$(
	printf '%s\n' "$candidates" |
		xargs docker inspect \
			--format '{{.Id}} {{.Created}} {{.State.Status}} {{.Config.Image}}' \
			2>"$inspection_errors"
)" || inspection_status=$?

# A container listed a moment ago can be gone by the time it is inspected: a
# concurrent test run finishing, or a second operator sweeping. That is benign,
# and the sweep continues with the rest. Every other inspection failure — the
# daemon restarting after the probe, an authorization policy that permits
# listing but denies inspection, a nonzero exit carrying nothing on stderr — is
# a fault, and tolerating it would let a timer report success while the orphans
# keep accumulating.
#
# The two are told apart only when the inspection actually failed, because
# stderr is not evidence on its own: `docker inspect` writes warnings there
# while succeeding, and treating those as faults would abort sweeps that had
# nothing wrong with them. Once the status is nonzero, every candidate must be
# accounted for — inspected, or named on stderr as gone — and stderr must say
# nothing else.
candidate_count="$(printf '%s\n' "$candidates" | grep -c . || true)"
inspected_count="$(printf '%s\n' "$inspected" | grep -c . || true)"

if [ "$inspection_status" -ne 0 ]; then
	vanished_count="$(grep -c -E "$MISSING_OBJECT_PATTERN" "$inspection_errors" || true)"
	unexplained="$(grep -v -E "$MISSING_OBJECT_PATTERN" "$inspection_errors" || true)"

	if [ -n "$unexplained" ] ||
		[ "$((inspected_count + vanished_count))" -ne "$candidate_count" ]; then
		echo "sweep-test-containers: docker inspect failed for a reason other than a" \
			"container disappearing (exit status $inspection_status); removed nothing" >&2
		cat "$inspection_errors" >&2
		exit 1
	fi
fi

if [ -z "$inspected" ]; then
	echo "sweep-test-containers: every candidate container was gone before it" \
		"could be inspected"
	finish_clean
fi

# `docker inspect` reports creation time as RFC 3339 with nanosecond precision,
# which predates `fromisoformat`'s tolerance for anything but 3 or 6 fractional
# digits; the fraction is truncated rather than parsed since only whole hours
# matter here.
selected="$(
	printf '%s\n' "$inspected" |
		python3 -c '
import datetime as dt, sys

cutoff_hours = float(sys.argv[1])
now = dt.datetime.now(dt.timezone.utc)
for line in sys.stdin:
    container_id, created, status, image = line.split(" ", 3)
    stamp = created.replace("Z", "+00:00")
    if "." in stamp:
        head, _, tail = stamp.partition(".")
        fraction_end = 0
        while fraction_end < len(tail) and tail[fraction_end].isdigit():
            fraction_end += 1
        stamp = f"{head}.{tail[:fraction_end][:6]}{tail[fraction_end:]}"
    age_hours = (now - dt.datetime.fromisoformat(stamp)).total_seconds() / 3600
    if age_hours >= cutoff_hours:
        print(f"{container_id} {age_hours:.1f} {status} {image.strip()}")
' "$older_than_hours"
)"

if [ -z "$selected" ]; then
	echo "sweep-test-containers: $inspected_count disposable test container(s)" \
		"present, none older than ${older_than_hours}h"
	finish_clean
fi

count="$(printf '%s\n' "$selected" | grep -c . || true)"

if [ "$apply" -eq 0 ]; then
	echo "sweep-test-containers: would remove $count container(s) older than" \
		"${older_than_hours}h, with their anonymous volumes:"
	printf '%s\n' "$selected" |
		awk '{ printf "  %s  age=%sh  %s  %s\n", substr($1, 1, 12), $2, $3, $4 }'
	echo "sweep-test-containers: re-run with --apply to remove them"
	finish_clean
fi

echo "sweep-test-containers: removing $count container(s) older than" \
	"${older_than_hours}h, with their anonymous volumes"
printf '%s\n' "$selected" |
	awk '{ print $1 }' |
	xargs docker rm --force --volumes >/dev/null
echo "sweep-test-containers: removed $count container(s)"
finish_clean

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
# script is the backstop for what nothing in-process can catch, and the only
# way to reclaim orphans that were stranded before the watchdog was enabled.
#
# Selection is by the `org.testcontainers.managed-by=testcontainers` label,
# which `AsyncRunner::start` applies unconditionally to every container it
# creates — overwriting any caller-supplied value for that key, so it cannot be
# spoofed away. That label is deliberately not Signalbox-specific: a per-repo
# label would identify nothing on the containers already stranded, since they
# were created without it. Any testcontainers-managed container older than the
# age bound is an orphan by construction — the library's containers exist only
# for the duration of a test process — so sweeping another project's day-old
# orphan on a shared box is correct, not collateral damage.
#
# The age bound is what makes this safe to run while tests are in flight: a
# container serving a live test is minutes old at most, and the default
# two-hour floor sits far above any suite's runtime. Removal is `--volumes`, so
# each container's anonymous volume — roughly 49 MB of stranded disk apiece —
# goes with it.
#
# A volume whose container was already removed separately is dangling, belongs
# to no container, and so is beyond anything this script can select; the run
# reports how many exist and leaves reclaiming them to `docker volume prune`,
# which is not run here because it is not scoped to this repository's images.
#
# Reports what it would remove and exits without touching anything unless
# `--apply` is passed.
#
# Usage: sweep-test-containers.sh [--older-than-hours <n>] [--apply]
set -euo pipefail

readonly MANAGED_BY_LABEL='org.testcontainers.managed-by=testcontainers'
older_than_hours=2
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
	--apply)
		apply=1
		shift
		;;
	-h | --help)
		echo "usage: sweep-test-containers.sh [--older-than-hours <n>] [--apply]"
		exit 0
		;;
	*)
		echo "sweep-test-containers: unknown argument: $1" >&2
		echo "usage: sweep-test-containers.sh [--older-than-hours <n>] [--apply]" >&2
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

if ! command -v docker >/dev/null 2>&1; then
	echo "sweep-test-containers: docker is not on PATH" >&2
	exit 1
fi

# A daemon that is stopped or refusing connections would otherwise surface as an
# empty container listing, which reads exactly like a clean box and would leave
# an operator believing the sweep found nothing to do. Ask once, and name the
# real problem. A daemon that is hung rather than absent — the socket accepts
# but never answers — blocks here instead of failing, as it would at any later
# call; that is visible to the operator as a stall, which an empty listing is
# not.
if ! docker version --format '{{.Server.Version}}' >/dev/null 2>&1; then
	echo "sweep-test-containers: cannot reach the Docker daemon" >&2
	exit 1
fi

candidates="$(docker ps --all --quiet --filter "label=$MANAGED_BY_LABEL")"

if [ -z "$candidates" ]; then
	echo "sweep-test-containers: no testcontainers-managed containers present"
	exit 0
fi

# `docker inspect` reports creation time as RFC 3339 with nanosecond precision,
# which predates `fromisoformat`'s tolerance for anything but 3 or 6 fractional
# digits; the fraction is truncated rather than parsed since only whole hours
# matter here. Identifiers arrive on stdin so a sweep of several thousand
# containers cannot overflow the argument list.
#
# A container listed a moment ago can be gone by the time it is inspected: a
# concurrent test run finishing, or a second operator sweeping. `docker inspect`
# then names that identifier on stderr and exits nonzero while still printing
# every container it did find, which under `set -o pipefail` would abort the
# sweep having removed nothing. A container that left on its own is the outcome
# this sweep wants, so the status is tolerated and whatever was returned is
# filtered; the daemon was already proven reachable above, so a genuine fault
# cannot hide behind this.
inspected="$(
	printf '%s\n' "$candidates" |
		xargs docker inspect \
			--format '{{.Id}} {{.Created}} {{.State.Status}} {{.Config.Image}}' ||
		true
)"

if [ -z "$inspected" ]; then
	echo "sweep-test-containers: every candidate container was gone before it" \
		"could be inspected"
	exit 0
fi

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
	total="$(printf '%s\n' "$candidates" | wc -l | tr -d ' ')"
	echo "sweep-test-containers: $total testcontainers-managed container(s)" \
		"present, none older than ${older_than_hours}h"
	exit 0
fi

count="$(printf '%s\n' "$selected" | wc -l | tr -d ' ')"

if [ "$apply" -eq 0 ]; then
	echo "sweep-test-containers: would remove $count container(s) older than" \
		"${older_than_hours}h, with their anonymous volumes:"
	printf '%s\n' "$selected" |
		awk '{ printf "  %s  age=%sh  %s  %s\n", substr($1, 1, 12), $2, $3, $4 }'
	echo "sweep-test-containers: re-run with --apply to remove them"
	exit 0
fi

echo "sweep-test-containers: removing $count container(s) older than" \
	"${older_than_hours}h, with their anonymous volumes"
printf '%s\n' "$selected" |
	awk '{ print $1 }' |
	xargs docker rm --force --volumes >/dev/null
echo "sweep-test-containers: removed $count container(s)"

# Volumes stranded by a container removed without `--volumes` outlive every
# selector this script has, so report them rather than leaving the operator to
# conclude the disk should now be clear.
dangling="$(docker volume ls --quiet --filter dangling=true | wc -l | tr -d ' ')"
if [ "$dangling" -gt 0 ]; then
	echo "sweep-test-containers: $dangling dangling volume(s) remain, belonging" \
		"to no container; reclaim with 'docker volume prune'"
fi

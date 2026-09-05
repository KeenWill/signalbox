#!/usr/bin/env bash
# Remove orphaned containers left behind by interrupted testcontainers runs.
#
# The Rust testcontainers client ships no Ryuk reaper: unlike the Java and Go
# implementations, `testcontainers` 0.27 removes a container only from
# `ContainerAsync`'s `Drop`, and creates it with `AutoRemove: false` so the
# daemon never reclaims it either. A test process that dies without unwinding —
# SIGKILL, an OOM kill, Ctrl-C, `timeout`, a cancelled CI job, a hard power loss
# — therefore strands every container it had started, permanently. The client's
# optional `watchdog` feature is deliberately not enabled: it `expect`s every
# stop and removal, so the first error panics its background thread, abandoning
# the containers it had not reached and skipping the re-raise that would end the
# process. This script is the only thing that reclaims any of it.
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
# above any suite's runtime. A disposable container's database state lives on a
# bounded tmpfs Docker frees with the container — stranded state pins memory,
# not disk — and removal still passes `--volumes`, so any anonymous volume a
# marked container carries goes with it too.
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
#                                 [--deadline-seconds <n>] [--apply]
set -euo pipefail

# Job control, so every daemon call below becomes its own process group and a
# deadline can end the whole call rather than the shell wrapping it. Two of
# these calls are pipelines, where the identifier `$!` reports is a wrapper
# shell and `xargs` and `docker` are its children: signalling that identifier
# alone reports a timeout while leaving a `docker rm` still mutating the
# daemon.
set -m

# Kept identical to `signalbox_persistence::DISPOSABLE_TEST_CONTAINER_LABEL_KEY`
# and its value; `tooling/test_sweep_test_containers.py` fails when the two
# drift.
readonly DISPOSABLE_LABEL='org.signalbox.disposable=test-container'

# `docker rm` names each identifier it could not find on
# stderr and still act on every container they did find. A line of this shape is
# a container that left between one call and the next, which is the outcome this
# sweep wants; any other line is a real fault that must not be read as
# "everything vanished".
readonly MISSING_OBJECT_PATTERN='^Error( response from daemon)?: No such (object|container): '

# `wait` reports a child killed by a signal as 128 plus the signal number, which
# is how a daemon call that ran past its deadline is told from one that failed.
readonly SIGNALLED_STATUS_FLOOR=128

# How long a call that overran its deadline is given to end on the signal that
# asks it to, before it is ended by the one it cannot refuse. A process that
# ignores SIGTERM would otherwise leave this shell in `wait` for good, which is
# the state the deadline exists to prevent.
readonly DEADLINE_GRACE_SECONDS=5

# Kept identical to
# `signalbox_persistence::DISPOSABLE_TEST_CONTAINER_LIFETIME_HOURS`, which is
# what anything holding a marked container checks itself against;
# `tooling/test_sweep_test_containers.py` fails when the two drift.
readonly DISPOSABLE_LIFETIME_HOURS=2

older_than_hours="$DISPOSABLE_LIFETIME_HOURS"
deadline_seconds=900
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
	--deadline-seconds)
		if [ "$#" -lt 2 ]; then
			echo "sweep-test-containers: --deadline-seconds needs a value" >&2
			exit 2
		fi
		deadline_seconds="$2"
		shift 2
		;;
	--apply)
		apply=1
		shift
		;;
	-h | --help)
		echo "usage: sweep-test-containers.sh [--older-than-hours <n>]" \
			"[--deadline-seconds <n>] [--apply]"
		exit 0
		;;
	*)
		echo "sweep-test-containers: unknown argument: $1" >&2
		echo "usage: sweep-test-containers.sh [--older-than-hours <n>]" \
			"[--deadline-seconds <n>] [--apply]" >&2
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

# The mark says only that a container is disposable, never that it is finished.
# What separates an orphan from a container serving a running test is the age
# bound, and the bound anything marking a container promised to stay under is
# this one. Sweeping below it removes live databases — at zero, every marked
# container the moment it starts — so a lower threshold is refused rather than
# obeyed.
if [ "$older_than_hours" -lt "$DISPOSABLE_LIFETIME_HOURS" ]; then
	echo "sweep-test-containers: --older-than-hours must be at least" \
		"${DISPOSABLE_LIFETIME_HOURS}, the age a marked container is allowed to" \
		"reach while still in use; got: $older_than_hours" >&2
	exit 2
fi

case "$deadline_seconds" in
'' | 0 | *[!0-9]*)
	echo "sweep-test-containers: --deadline-seconds must be a positive whole" \
		"number of seconds, got: $deadline_seconds" >&2
	exit 2
	;;
esac

if ! command -v docker >/dev/null 2>&1; then
	echo "sweep-test-containers: docker is not on PATH" >&2
	exit 1
fi

readonly sweep_pid=$$

scratch=""
bounded_worker=""
bounded_deadline=""

# Reports the identifier of the process that owns `$1` right now, so a deadline
# can tell being the sweep's child from having been reparented by its death.
parent_of() {
	ps -o ppid= -p "$1" 2>/dev/null | tr -d '[:space:]'
}

cleanup() {
	if [ -n "$bounded_worker" ]; then
		# Asked, then told. A cancellation is already an abrupt end, and a call
		# that ignores the request would otherwise hold this handler open.
		kill -s TERM -- -"$bounded_worker" >/dev/null 2>&1 || true
		kill -s KILL -- -"$bounded_worker" >/dev/null 2>&1 || true
		wait "$bounded_worker" >/dev/null 2>&1 || true
		bounded_worker=""
	fi
	if [ -n "$bounded_deadline" ]; then
		kill -s TERM -- -"$bounded_deadline" >/dev/null 2>&1 || true
		wait "$bounded_deadline" >/dev/null 2>&1 || true
		bounded_deadline=""
	fi
	if [ -n "$scratch" ]; then
		rm -rf "$scratch"
		scratch=""
	fi
	return 0
}

# Installed before anything is launched, so no exit path can leave a deadline
# process behind to signal an identifier the kernel has since handed to somebody
# else. Every signal a supervisor, a terminal, or an operator ends this with, each
# reported as the shell reports a signalled child. Handling only some of them
# leaves the rest ignored — bash defers a trapped signal but discards nothing —
# so an untrapped cancellation would strand the sweep and its bounded children
# on a hung daemon call until the deadline.
trap cleanup EXIT
trap 'cleanup; exit 129' HUP
trap 'cleanup; exit 130' INT
trap 'cleanup; exit 131' QUIT
trap 'cleanup; exit 143' TERM

scratch="$(mktemp -d)"
readonly OUTPUT="$scratch/output"
readonly ERRORS="$scratch/errors"

# No docker subcommand carries a timeout of its own, and reaching the daemon
# once proves only that it answered once. A daemon that accepts the socket and
# then stops answering — on `ps`, `inspect`, `rm`, or `volume ls` just as
# readily as on `version` — would otherwise block here forever, and on a timer a
# stuck invocation is precisely what stops the next sweep from bounding the
# leak. Every daemon call therefore runs under its own deadline.
#
# The call runs in the background and is waited on rather than run in the
# foreground, because a shell defers its traps until the foreground command it
# is stuck on returns, which for a hung daemon is never; a shell sitting in
# `wait` can be interrupted.
run_bounded() {
	"$@" >"$OUTPUT" 2>"$ERRORS" &
	local worker=$!
	(
		sleep "$deadline_seconds"
		# `cleanup` cancels this deadline on every exit the shell can observe,
		# but a SIGKILL leaves no trap to run. Confirm this process and the
		# worker are both still the sweep's own children before signalling:
		# after a SIGKILL both are reparented, and by the time this wakes the
		# kernel may have reissued either identifier to somebody else's process
		# group. Declining to signal strands the abandoned call, which the next
		# sweep reclaims; signalling a reused group would take work that was
		# never ours.
		# Bound first: `$BASHPID` inside a command substitution names that
		# substitution's own shell, not this one.
		local self=$BASHPID
		[ "$(parent_of "$self")" = "$sweep_pid" ] || exit 0
		[ "$(parent_of "$worker")" = "$sweep_pid" ] || exit 0
		kill -s TERM -- -"$worker"
		sleep "$DEADLINE_GRACE_SECONDS"
		# Still there, so it declined the request. Re-check the parentage,
		# because the grace period is another window in which the sweep may
		# have died and the identifier been reissued.
		[ "$(parent_of "$self")" = "$sweep_pid" ] || exit 0
		[ "$(parent_of "$worker")" = "$sweep_pid" ] || exit 0
		kill -s KILL -- -"$worker"
	) >/dev/null 2>&1 &
	local deadline=$!
	bounded_worker="$worker"
	bounded_deadline="$deadline"
	local status=0
	wait "$worker" || status=$?
	if [ "$status" -ge "$SIGNALLED_STATUS_FLOOR" ]; then
		# Reaping the wrapper is not reaping the call. A pipeline's wrapper
		# shell dies on the first signal while a `docker` beneath it that
		# declined keeps going, and `wait` returning here would otherwise stand
		# the deadline down before it escalated — leaving that process talking
		# to the daemon after the sweep reported a timeout. End the group
		# outright instead. The identifier was just reaped, so this races
		# reuse by the width of one statement; leaving a `docker rm` running
		# unobserved is the worse of the two.
		kill -s KILL -- -"$worker" >/dev/null 2>&1 || true
	fi
	bounded_worker=""
	# The group, not the identifier: the deadline is a shell whose `sleep` is a
	# child of it, and signalling the shell alone leaves that `sleep` reparented
	# and running for the rest of the deadline. On a timer those accumulate, one
	# per daemon call, on a machine this tool exists to keep clear.
	kill -s TERM -- -"$deadline" >/dev/null 2>&1 || true
	wait "$deadline" >/dev/null 2>&1 || true
	bounded_deadline=""
	return "$status"
}

# Names the daemon as the problem when a call ran past its deadline, and says
# nothing when it did not, so callers can keep their own failure messages.
refuse_a_deadline_overrun() {
	if [ "$1" -ge "$SIGNALLED_STATUS_FLOOR" ]; then
		echo "sweep-test-containers: the Docker daemon did not answer $2 within" \
			"${deadline_seconds}s" >&2
		exit 1
	fi
}

# Dangling volumes are the part of the leak no container-scoped selector can
# reach, so every successful run reports them — including the runs that find
# nothing to remove. A box whose containers were already reclaimed without their
# volumes reports an empty container inventory, and stopping there would leave
# the remaining disk leak invisible.
#
# The count is advisory, so a daemon that stops answering after the removals
# have already happened downgrades to a warning: turning a completed sweep into
# a bare nonzero exit would tell the operator nothing except that something went
# wrong after the part that mattered.
report_dangling_volumes() {
	local status=0
	run_bounded docker volume ls --quiet --filter dangling=true || status=$?
	if [ "$status" -ne 0 ]; then
		echo "sweep-test-containers: could not count dangling volumes; whatever" \
			"this run removed was still removed" >&2
		return 0
	fi
	local dangling
	dangling="$(grep -c . <"$OUTPUT" || true)"
	if [ "$dangling" -gt 0 ]; then
		echo "sweep-test-containers: $dangling dangling volume(s) remain, belonging" \
			"to no container; reclaim with 'docker volume prune'"
	fi
	return 0
}

finish_clean() {
	report_dangling_volumes
	exit 0
}

# A daemon that is stopped or refusing connections would otherwise surface as an
# empty container listing, which reads exactly like a clean box and would leave
# an operator believing the sweep found nothing to do. Ask once, and name the
# real problem.
probe_status=0
run_bounded docker version --format '{{.Server.Version}}' || probe_status=$?
refuse_a_deadline_overrun "$probe_status" "the version request"

if [ "$probe_status" -ne 0 ]; then
	echo "sweep-test-containers: cannot reach the Docker daemon" >&2
	exit 1
fi

listing_status=0
run_bounded docker ps --all --filter "label=$DISPOSABLE_LABEL" --format json ||
	listing_status=$?
refuse_a_deadline_overrun "$listing_status" "the container listing"

if [ "$listing_status" -ne 0 ]; then
	echo "sweep-test-containers: could not list containers" >&2
	cat "$ERRORS" >&2
	exit 1
fi

containers="$(cat "$OUTPUT")"

if [ -z "$containers" ]; then
	echo "sweep-test-containers: no disposable test containers present"
	finish_clean
fi

# Docker owns the listing schema. Its JSON formatter supplies one object per
# line, avoiding a second inspect request and any delimiter assumptions about
# image names, states, or timestamps.
selected="$(
	printf '%s\n' "$containers" |
		python3 -c '
import datetime as dt, sys
import json

cutoff_hours = float(sys.argv[1])
now = dt.datetime.now(dt.timezone.utc)
for line in sys.stdin:
    container = json.loads(line)
    container_id = container["ID"]
    created = container["CreatedAt"].rsplit(" ", 1)[0]
    created_at = dt.datetime.strptime(created, "%Y-%m-%d %H:%M:%S %z")
    age_hours = (now - created_at).total_seconds() / 3600
    if age_hours >= cutoff_hours:
        print("{} {:.1f} {} {}".format(
            container_id, age_hours, container["State"], container["Image"]
        ))
' "$older_than_hours"
)"

if [ -z "$selected" ]; then
	container_count="$(printf '%s\n' "$containers" | grep -c . || true)"
	echo "sweep-test-containers: $container_count disposable test container(s)" \
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

# Removal races the same way inspection does, and for the same reason: a
# container selected a moment ago can be gone before `docker rm` reaches it.
# Aborting there would strand the containers `xargs` had not got to yet, so a
# container that left on its own is counted rather than treated as a failure,
# and every other removal error still fails the run.
remove_selected() {
	printf '%s\n' "$selected" |
		awk '{ print $1 }' |
		xargs docker rm --force --volumes
}

removal_status=0
run_bounded remove_selected || removal_status=$?
refuse_a_deadline_overrun "$removal_status" "the removal request"

# `docker rm` echoes each container it removed, so the removals are counted from
# the daemon's own report rather than assumed from the selection.
removed_count="$(grep -c . <"$OUTPUT" || true)"

if [ "$removal_status" -eq 0 ]; then
	echo "sweep-test-containers: removed $removed_count container(s)"
	finish_clean
fi

# Reconciled exactly as inspection is, and for the same reason: a nonzero status
# is benign only when the containers it could not remove are the ones something
# else already had. A failure that wrote no diagnostic explains nothing, so an
# empty stderr never buys a clean exit.
already_gone="$(grep -c -E "$MISSING_OBJECT_PATTERN" "$ERRORS" || true)"
unremoved="$(grep -v -E "$MISSING_OBJECT_PATTERN" "$ERRORS" || true)"

if [ -n "$unremoved" ] ||
	[ "$((removed_count + already_gone))" -ne "$count" ]; then
	echo "sweep-test-containers: docker rm failed for a reason other than a" \
		"container disappearing (exit status $removal_status); removed" \
		"$removed_count of $count container(s)" >&2
	cat "$ERRORS" >&2
	exit 1
fi

echo "sweep-test-containers: removed $removed_count container(s);" \
	"$already_gone had already gone"
finish_clean

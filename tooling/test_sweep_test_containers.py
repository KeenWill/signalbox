#!/usr/bin/env python3
"""Regression tests for tooling/sweep-test-containers.sh.

A real Docker daemon is deliberately never contacted here: a fake `docker` on
`PATH` reports a controlled container inventory and records the removals it is
asked to perform, so the sweep's selection logic — the label filter, the age
bound, and the dry-run default — is exercised in isolation and
deterministically. The fake also lets the unreachable-daemon, unresponsive-
daemon, and mid-sweep-removal paths be tested, none of which a real daemon can
be asked to demonstrate on demand.
"""

from __future__ import annotations

import datetime as dt
import os
import re
import stat
import subprocess
import tempfile
import unittest
from pathlib import Path

REPOSITORY = Path(__file__).resolve().parent.parent
SWEEP = REPOSITORY / "tooling" / "sweep-test-containers.sh"
PERSISTENCE_LIB = REPOSITORY / "crates" / "persistence" / "src" / "lib.rs"

FAKE_DOCKER = '''#!/usr/bin/env python3
import os, sys, time

command = sys.argv[1]

if command == "version":
    if os.environ.get("FAKE_DOCKER_UNRESPONSIVE"):
        time.sleep(3600)
    sys.exit(1) if os.environ.get("FAKE_DOCKER_DOWN") else print("27.0.0")
elif command == "ps":
    with open(os.environ["FAKE_DOCKER_PS_ARGUMENTS"], "w") as log:
        log.write("\\n".join(sys.argv[1:]))
    print(os.environ["FAKE_DOCKER_PS"], end="")
elif command == "inspect":
    if os.environ.get("FAKE_DOCKER_INSPECT_REFUSED"):
        print(os.environ["FAKE_DOCKER_INSPECT_REFUSED"], file=sys.stderr)
        sys.exit(1)
    if os.environ.get("FAKE_DOCKER_INSPECT_SILENTLY_FAILS"):
        sys.exit(1)
    inventory = dict(
        line.split(" ", 1)
        for line in os.environ["FAKE_DOCKER_INSPECT"].splitlines()
    )
    assert sys.argv[2] == "--format", sys.argv
    missing = 0
    for container_id in sys.argv[4:]:
        if container_id in inventory:
            print(f"{container_id} {inventory[container_id]}")
        else:
            missing += 1
            print(f"Error: No such object: {container_id}", file=sys.stderr)
    if os.environ.get("FAKE_DOCKER_INSPECT_NOTE"):
        print(os.environ["FAKE_DOCKER_INSPECT_NOTE"], file=sys.stderr)
    if os.environ.get("FAKE_DOCKER_INSPECT_NOTE_IS_FATAL"):
        sys.exit(1)
    sys.exit(1 if missing else 0)
elif command == "rm":
    assert "--force" in sys.argv and "--volumes" in sys.argv, sys.argv
    with open(os.environ["FAKE_DOCKER_RM_LOG"], "a") as log:
        log.write("\\n".join(sys.argv[4:]) + "\\n")
elif command == "volume":
    assert "dangling=true" in sys.argv, sys.argv
    print(os.environ["FAKE_DOCKER_DANGLING"], end="")
else:
    raise SystemExit(f"fake docker: unexpected command: {command}")
'''


def rust_string_constant(name: str) -> str:
    """Read one `pub const … &str` from the persistence crate, outside test bodies.

    The sweep script and the harness that marks containers must name the same
    label, and nothing but this reading connects the two spellings.
    """
    pattern = rf'^pub const {name}: &str = "([^"]+)";$'
    found = re.search(pattern, PERSISTENCE_LIB.read_text(), re.MULTILINE)
    assert found is not None, f"{PERSISTENCE_LIB} declares no {name}"
    return found.group(1)


DISPOSABLE_LABEL_KEY = rust_string_constant("DISPOSABLE_TEST_CONTAINER_LABEL_KEY")
DISPOSABLE_LABEL_VALUE = rust_string_constant("DISPOSABLE_TEST_CONTAINER_LABEL_VALUE")
DISPOSABLE_FILTER = f"label={DISPOSABLE_LABEL_KEY}={DISPOSABLE_LABEL_VALUE}"

MANAGED_BY_FILTER = "label=org.testcontainers.managed-by=testcontainers"

REFUSED_INSPECTION = "Error response from daemon: authorization denied"

HARMLESS_INSPECTION_NOTE = "WARNING: the legacy inspect format is deprecated"

MARKED_START = ".with_labels(disposable_test_container_labels())"


def unmarked_container_start_sites() -> list[str]:
    """Name every Rust container start that the sweep could not reclaim.

    A start site that omits the disposable mark strands containers the sweep
    cannot see, which is the leak this whole tool exists to bound, so the check
    is mechanical rather than a convention anyone has to remember.
    """
    tracked = subprocess.run(
        ["git", "ls-files", "-z", "--", "*.rs"],
        cwd=REPOSITORY,
        capture_output=True,
        text=True,
        check=True,
    ).stdout.split("\0")
    unmarked = []
    for name in filter(None, tracked):
        lines = (REPOSITORY / name).read_text().splitlines()
        armed = 0
        for number, line in enumerate(lines, start=1):
            if "Postgres::default()" in line:
                armed = number
            if armed and line.strip() == ".start()":
                chain = "".join(lines[armed - 1 : number])
                if MARKED_START not in chain:
                    unmarked.append(f"{name}:{armed}")
                armed = 0
    return unmarked


def write_fake_docker(bin_dir: Path) -> None:
    """Install a fake docker driven by environment fixtures, outside test bodies."""
    fake = bin_dir / "docker"
    fake.write_text(FAKE_DOCKER)
    fake.chmod(fake.stat().st_mode | stat.S_IEXEC | stat.S_IXGRP | stat.S_IXOTH)


def aged(container_id: str, hours_old: float, status: str, image: str) -> tuple[str, str]:
    """State one container's identity and its creation stamp, outside test bodies."""
    created = dt.datetime.now(dt.timezone.utc) - dt.timedelta(hours=hours_old)
    stamp = created.strftime("%Y-%m-%dT%H:%M:%S") + ".123456789Z"
    return container_id, f"{stamp} {status} {image}"


class SweepRun:
    """One completed sweep and everything the fake daemon recorded about it."""

    def __init__(
        self,
        completed: subprocess.CompletedProcess[str],
        removed: list[str],
        listing_arguments: list[str],
    ) -> None:
        self.completed = completed
        self.removed = removed
        self.listing_arguments = listing_arguments

    @property
    def status(self) -> int:
        return self.completed.returncode

    @property
    def stdout(self) -> str:
        return self.completed.stdout

    @property
    def stderr(self) -> str:
        return self.completed.stderr

    @property
    def listing_filters(self) -> list[str]:
        """The label filters the sweep asked the daemon to list by."""
        pairs = zip(self.listing_arguments, self.listing_arguments[1:])
        return [value for flag, value in pairs if flag == "--filter"]


def run_sweep(
    inventory: list[tuple[str, str]],
    *,
    arguments: list[str],
    daemon_reachable: bool = True,
    daemon_responsive: bool = True,
    dangling_volumes: int = 0,
    vanished: tuple[str, ...] = (),
    inspection_refused: str | None = None,
    inspection_fails_silently: bool = False,
    inspection_note: str | None = None,
    inspection_note_is_fatal: bool = False,
) -> SweepRun:
    """Invoke the real sweep against one fake inventory, outside test bodies.

    `vanished` names containers the listing reports but inspection no longer
    finds, reproducing a container removed between the two calls.
    `inspection_refused` and `inspection_fails_silently` instead reproduce an
    inspection that failed for a reason other than a container disappearing.
    """
    with tempfile.TemporaryDirectory() as scratch:
        scratch_path = Path(scratch)
        write_fake_docker(scratch_path)
        removal_log = scratch_path / "removed.txt"
        listing_log = scratch_path / "listing-arguments.txt"
        environment = dict(os.environ)
        environment["PATH"] = f"{scratch_path}:{environment['PATH']}"
        listed = [i for i, _ in inventory] + list(vanished)
        environment["FAKE_DOCKER_PS"] = "".join(f"{i}\n" for i in listed)
        environment["FAKE_DOCKER_PS_ARGUMENTS"] = str(listing_log)
        environment["FAKE_DOCKER_INSPECT"] = "\n".join(
            f"{i} {rest}" for i, rest in inventory
        )
        environment["FAKE_DOCKER_RM_LOG"] = str(removal_log)
        environment["FAKE_DOCKER_DANGLING"] = "".join(
            f"volume{n}\n" for n in range(dangling_volumes)
        )
        environment.pop("FAKE_DOCKER_DOWN", None)
        environment.pop("FAKE_DOCKER_UNRESPONSIVE", None)
        environment.pop("FAKE_DOCKER_INSPECT_REFUSED", None)
        environment.pop("FAKE_DOCKER_INSPECT_SILENTLY_FAILS", None)
        environment.pop("FAKE_DOCKER_INSPECT_NOTE", None)
        environment.pop("FAKE_DOCKER_INSPECT_NOTE_IS_FATAL", None)
        if inspection_note is not None:
            environment["FAKE_DOCKER_INSPECT_NOTE"] = inspection_note
        if inspection_note_is_fatal:
            environment["FAKE_DOCKER_INSPECT_NOTE_IS_FATAL"] = "1"
        if not daemon_reachable:
            environment["FAKE_DOCKER_DOWN"] = "1"
        if not daemon_responsive:
            environment["FAKE_DOCKER_UNRESPONSIVE"] = "1"
        if inspection_refused is not None:
            environment["FAKE_DOCKER_INSPECT_REFUSED"] = inspection_refused
        if inspection_fails_silently:
            environment["FAKE_DOCKER_INSPECT_SILENTLY_FAILS"] = "1"
        completed = subprocess.run(
            [str(SWEEP), *arguments],
            env=environment,
            capture_output=True,
            text=True,
        )
        removed = removal_log.read_text().split() if removal_log.exists() else []
        listing = listing_log.read_text().split("\n") if listing_log.exists() else []
        return SweepRun(completed, removed, listing)


class SweepTestContainersTest(unittest.TestCase):
    def test_the_listing_asks_for_this_repository_s_disposable_label_alone(self) -> None:
        run = run_sweep(
            [aged("old111", 72, "running", "postgres:18.4-alpine3.23")],
            arguments=[],
        )

        self.assertEqual(run.status, 0, run.stderr)
        self.assertEqual(run.listing_filters, [DISPOSABLE_FILTER])

    def test_the_listing_does_not_ask_for_the_global_testcontainers_label(self) -> None:
        run = run_sweep(
            [aged("old111", 72, "running", "postgres:18.4-alpine3.23")],
            arguments=[],
        )

        self.assertEqual(run.status, 0, run.stderr)
        self.assertNotIn(MANAGED_BY_FILTER, run.listing_filters)

    def test_the_script_and_the_harness_spell_the_same_label(self) -> None:
        script = SWEEP.read_text()

        self.assertIn(f"'{DISPOSABLE_LABEL_KEY}={DISPOSABLE_LABEL_VALUE}'", script)

    def test_every_test_container_start_marks_the_container_disposable(self) -> None:
        unmarked = unmarked_container_start_sites()

        self.assertEqual(unmarked, [], f"unswept-able container starts: {unmarked}")

    def test_dry_run_reports_aged_containers_and_removes_nothing(self) -> None:
        run = run_sweep(
            [
                aged("old111", 72, "running", "postgres:18.4-alpine3.23"),
                aged("new222", 0.05, "running", "postgres:18.4-alpine3.23"),
            ],
            arguments=[],
        )

        self.assertEqual(run.status, 0, run.stderr)
        self.assertIn("would remove 1 container(s)", run.stdout)
        self.assertIn("old111", run.stdout)
        self.assertNotIn("new222", run.stdout)
        self.assertEqual(run.removed, [])

    def test_apply_removes_only_containers_past_the_age_bound(self) -> None:
        run = run_sweep(
            [
                aged("old111", 72, "running", "postgres:18.4-alpine3.23"),
                aged("old222", 3, "exited", "postgres:18.4-alpine3.23"),
                aged("new333", 0.05, "running", "postgres:18.4-alpine3.23"),
            ],
            arguments=["--apply"],
        )

        self.assertEqual(run.status, 0, run.stderr)
        self.assertEqual(run.removed, ["old111", "old222"])
        self.assertIn("removed 2 container(s)", run.stdout)

    def test_age_bound_is_configurable(self) -> None:
        run = run_sweep(
            [
                aged("old111", 72, "running", "postgres:18.4-alpine3.23"),
                aged("mid222", 3, "running", "postgres:18.4-alpine3.23"),
            ],
            arguments=["--older-than-hours", "48", "--apply"],
        )

        self.assertEqual(run.status, 0, run.stderr)
        self.assertEqual(run.removed, ["old111"])

    def test_a_container_serving_a_live_test_is_never_swept(self) -> None:
        run = run_sweep(
            [aged("live111", 0.25, "running", "postgres:18.4-alpine3.23")],
            arguments=["--apply"],
        )

        self.assertEqual(run.status, 0, run.stderr)
        self.assertEqual(run.removed, [])
        self.assertIn("none older than 2h", run.stdout)

    def test_volumes_belonging_to_no_container_are_reported_not_removed(self) -> None:
        run = run_sweep(
            [aged("old111", 72, "running", "postgres:18.4-alpine3.23")],
            arguments=["--apply"],
            dangling_volumes=700,
        )

        self.assertEqual(run.status, 0, run.stderr)
        self.assertEqual(run.removed, ["old111"])
        self.assertIn("700 dangling volume(s) remain", run.stdout)
        self.assertIn("docker volume prune", run.stdout)

    def test_no_dangling_volumes_reports_no_prune_advice(self) -> None:
        run = run_sweep(
            [aged("old111", 72, "running", "postgres:18.4-alpine3.23")],
            arguments=["--apply"],
            dangling_volumes=0,
        )

        self.assertEqual(run.status, 0, run.stderr)
        self.assertEqual(run.removed, ["old111"])
        self.assertNotIn("docker volume prune", run.stdout)

    def test_dangling_volumes_are_reported_when_no_container_is_present(self) -> None:
        run = run_sweep([], arguments=["--apply"], dangling_volumes=511)

        self.assertEqual(run.status, 0, run.stderr)
        self.assertIn("no disposable test containers present", run.stdout)
        self.assertIn("511 dangling volume(s) remain", run.stdout)

    def test_dangling_volumes_are_reported_when_nothing_is_old_enough(self) -> None:
        run = run_sweep(
            [aged("live111", 0.25, "running", "postgres:18.4-alpine3.23")],
            arguments=["--apply"],
            dangling_volumes=42,
        )

        self.assertEqual(run.status, 0, run.stderr)
        self.assertIn("none older than 2h", run.stdout)
        self.assertIn("42 dangling volume(s) remain", run.stdout)

    def test_dangling_volumes_are_reported_by_a_dry_run(self) -> None:
        run = run_sweep(
            [aged("old111", 72, "running", "postgres:18.4-alpine3.23")],
            arguments=[],
            dangling_volumes=9,
        )

        self.assertEqual(run.status, 0, run.stderr)
        self.assertEqual(run.removed, [])
        self.assertIn("9 dangling volume(s) remain", run.stdout)

    def test_dangling_volumes_are_reported_when_every_candidate_vanished(self) -> None:
        run = run_sweep(
            [],
            arguments=["--apply"],
            vanished=("gone111",),
            dangling_volumes=3,
        )

        self.assertEqual(run.status, 0, run.stderr)
        self.assertIn("gone before it could be inspected", run.stdout)
        self.assertIn("3 dangling volume(s) remain", run.stdout)

    def test_a_container_removed_mid_sweep_does_not_abort_the_rest(self) -> None:
        run = run_sweep(
            [aged("old111", 72, "running", "postgres:18.4-alpine3.23")],
            arguments=["--apply"],
            vanished=("gone222",),
        )

        self.assertEqual(run.status, 0, run.stderr)
        self.assertEqual(run.removed, ["old111"])

    def test_every_container_vanishing_exits_clean(self) -> None:
        run = run_sweep(
            [],
            arguments=["--apply"],
            vanished=("gone111", "gone222"),
        )

        self.assertEqual(run.status, 0, run.stderr)
        self.assertEqual(run.removed, [])
        self.assertIn("gone before it could be inspected", run.stdout)

    def test_a_refused_inspection_fails_instead_of_claiming_every_container_vanished(
        self,
    ) -> None:
        run = run_sweep(
            [aged("old111", 72, "running", "postgres:18.4-alpine3.23")],
            arguments=["--apply"],
            inspection_refused=REFUSED_INSPECTION,
        )

        self.assertEqual(run.status, 1)
        self.assertEqual(run.removed, [])
        self.assertIn("other than a container disappearing", run.stderr)
        self.assertIn(REFUSED_INSPECTION, run.stderr)
        self.assertNotIn("gone before it could be inspected", run.stdout)

    def test_a_silent_inspection_failure_fails_instead_of_reporting_a_clean_sweep(
        self,
    ) -> None:
        run = run_sweep(
            [aged("old111", 72, "running", "postgres:18.4-alpine3.23")],
            arguments=["--apply"],
            inspection_fails_silently=True,
        )

        self.assertEqual(run.status, 1)
        self.assertEqual(run.removed, [])
        self.assertIn("other than a container disappearing", run.stderr)
        self.assertNotIn("gone before it could be inspected", run.stdout)

    def test_a_failure_carrying_an_unrelated_stderr_line_is_not_read_as_a_race(
        self,
    ) -> None:
        run = run_sweep(
            [aged("old111", 72, "running", "postgres:18.4-alpine3.23")],
            arguments=["--apply"],
            inspection_note=REFUSED_INSPECTION,
            inspection_note_is_fatal=True,
        )

        self.assertEqual(run.status, 1)
        self.assertEqual(run.removed, [])
        self.assertIn("other than a container disappearing", run.stderr)

    def test_a_successful_inspection_that_warns_on_stderr_still_sweeps(self) -> None:
        run = run_sweep(
            [aged("old111", 72, "running", "postgres:18.4-alpine3.23")],
            arguments=["--apply"],
            inspection_note=HARMLESS_INSPECTION_NOTE,
        )

        self.assertEqual(run.status, 0, run.stderr)
        self.assertEqual(run.removed, ["old111"])

    def test_an_empty_inventory_exits_clean(self) -> None:
        run = run_sweep([], arguments=["--apply"])

        self.assertEqual(run.status, 0, run.stderr)
        self.assertEqual(run.removed, [])
        self.assertIn("no disposable test containers present", run.stdout)

    def test_an_unreachable_daemon_fails_instead_of_reporting_nothing_to_do(self) -> None:
        run = run_sweep(
            [aged("old111", 72, "running", "postgres:18.4-alpine3.23")],
            arguments=["--apply"],
            daemon_reachable=False,
        )

        self.assertEqual(run.status, 1)
        self.assertEqual(run.removed, [])
        self.assertIn("cannot reach the Docker daemon", run.stderr)

    def test_a_daemon_that_never_answers_fails_instead_of_blocking_forever(self) -> None:
        run = run_sweep(
            [aged("old111", 72, "running", "postgres:18.4-alpine3.23")],
            arguments=["--apply", "--daemon-probe-seconds", "1"],
            daemon_responsive=False,
        )

        self.assertEqual(run.status, 1)
        self.assertEqual(run.removed, [])
        self.assertIn("did not answer within 1s", run.stderr)

    def test_a_probe_bound_that_is_not_a_positive_number_is_refused(self) -> None:
        run = run_sweep([], arguments=["--daemon-probe-seconds", "0"])

        self.assertEqual(run.status, 2)
        self.assertIn("must be a positive whole number of seconds", run.stderr)


if __name__ == "__main__":
    unittest.main()

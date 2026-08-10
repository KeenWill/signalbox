#!/usr/bin/env python3
"""Regression tests for tooling/sweep-test-containers.sh.

A real Docker daemon is deliberately never contacted here: a fake `docker` on
`PATH` reports a controlled container inventory and records the removals it is
asked to perform, so the sweep's selection logic — the label filter, the age
bound, and the dry-run default — is exercised in isolation and
deterministically. The fake also lets the unreachable-daemon path be tested,
which a real daemon cannot be asked to demonstrate on demand.
"""

from __future__ import annotations

import datetime as dt
import os
import stat
import subprocess
import tempfile
import unittest
from pathlib import Path

SWEEP = Path(__file__).resolve().parent / "sweep-test-containers.sh"

MANAGED_LABEL = "label=org.testcontainers.managed-by=testcontainers"

FAKE_DOCKER = '''#!/usr/bin/env python3
import os, sys

command = sys.argv[1]

if command == "version":
    sys.exit(1) if os.environ.get("FAKE_DOCKER_DOWN") else print("27.0.0")
elif command == "ps":
    assert os.environ["FAKE_DOCKER_PS_FILTER"] in sys.argv, sys.argv
    print(os.environ["FAKE_DOCKER_PS"], end="")
elif command == "inspect":
    inventory = dict(
        line.split(" ", 1)
        for line in os.environ["FAKE_DOCKER_INSPECT"].splitlines()
    )
    assert sys.argv[2] == "--format", sys.argv
    for container_id in sys.argv[4:]:
        print(f"{container_id} {inventory[container_id]}")
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


def run_sweep(
    inventory: list[tuple[str, str]],
    *,
    arguments: list[str],
    daemon_reachable: bool = True,
    dangling_volumes: int = 0,
) -> tuple[subprocess.CompletedProcess[str], list[str]]:
    """Invoke the real sweep against one fake inventory, outside test bodies."""
    with tempfile.TemporaryDirectory() as scratch:
        scratch_path = Path(scratch)
        write_fake_docker(scratch_path)
        removal_log = scratch_path / "removed.txt"
        environment = dict(os.environ)
        environment["PATH"] = f"{scratch_path}:{environment['PATH']}"
        environment["FAKE_DOCKER_PS"] = "".join(f"{i}\n" for i, _ in inventory)
        environment["FAKE_DOCKER_PS_FILTER"] = MANAGED_LABEL
        environment["FAKE_DOCKER_INSPECT"] = "\n".join(
            f"{i} {rest}" for i, rest in inventory
        )
        environment["FAKE_DOCKER_RM_LOG"] = str(removal_log)
        environment["FAKE_DOCKER_DANGLING"] = "".join(
            f"volume{n}\n" for n in range(dangling_volumes)
        )
        environment.pop("FAKE_DOCKER_DOWN", None)
        if not daemon_reachable:
            environment["FAKE_DOCKER_DOWN"] = "1"
        completed = subprocess.run(
            [str(SWEEP), *arguments],
            env=environment,
            capture_output=True,
            text=True,
        )
        removed = removal_log.read_text().split() if removal_log.exists() else []
        return completed, removed


class SweepTestContainersTest(unittest.TestCase):
    def test_dry_run_reports_aged_containers_and_removes_nothing(self) -> None:
        completed, removed = run_sweep(
            [
                aged("old111", 72, "running", "postgres:18.4-alpine3.23"),
                aged("new222", 0.05, "running", "postgres:18.4-alpine3.23"),
            ],
            arguments=[],
        )

        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertIn("would remove 1 container(s)", completed.stdout)
        self.assertIn("old111", completed.stdout)
        self.assertNotIn("new222", completed.stdout)
        self.assertEqual(removed, [])

    def test_apply_removes_only_containers_past_the_age_bound(self) -> None:
        completed, removed = run_sweep(
            [
                aged("old111", 72, "running", "postgres:18.4-alpine3.23"),
                aged("old222", 3, "exited", "postgres:18.4-alpine3.23"),
                aged("new333", 0.05, "running", "postgres:18.4-alpine3.23"),
            ],
            arguments=["--apply"],
        )

        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(removed, ["old111", "old222"])
        self.assertIn("removed 2 container(s)", completed.stdout)

    def test_age_bound_is_configurable(self) -> None:
        completed, removed = run_sweep(
            [
                aged("old111", 72, "running", "postgres:18.4-alpine3.23"),
                aged("mid222", 3, "running", "postgres:18.4-alpine3.23"),
            ],
            arguments=["--older-than-hours", "48", "--apply"],
        )

        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(removed, ["old111"])

    def test_a_container_serving_a_live_test_is_never_swept(self) -> None:
        completed, removed = run_sweep(
            [aged("live111", 0.25, "running", "postgres:18.4-alpine3.23")],
            arguments=["--apply"],
        )

        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(removed, [])
        self.assertIn("none older than 2h", completed.stdout)

    def test_volumes_belonging_to_no_container_are_reported_not_removed(self) -> None:
        completed, removed = run_sweep(
            [aged("old111", 72, "running", "postgres:18.4-alpine3.23")],
            arguments=["--apply"],
            dangling_volumes=700,
        )

        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(removed, ["old111"])
        self.assertIn("700 dangling volume(s) remain", completed.stdout)
        self.assertIn("docker volume prune", completed.stdout)

    def test_no_dangling_volumes_reports_no_prune_advice(self) -> None:
        completed, removed = run_sweep(
            [aged("old111", 72, "running", "postgres:18.4-alpine3.23")],
            arguments=["--apply"],
            dangling_volumes=0,
        )

        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(removed, ["old111"])
        self.assertNotIn("docker volume prune", completed.stdout)

    def test_an_empty_inventory_exits_clean(self) -> None:
        completed, removed = run_sweep([], arguments=["--apply"])

        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(removed, [])
        self.assertIn("no testcontainers-managed containers present", completed.stdout)

    def test_an_unreachable_daemon_fails_instead_of_reporting_nothing_to_do(self) -> None:
        completed, removed = run_sweep(
            [aged("old111", 72, "running", "postgres:18.4-alpine3.23")],
            arguments=["--apply"],
            daemon_reachable=False,
        )

        self.assertEqual(completed.returncode, 1)
        self.assertEqual(removed, [])
        self.assertIn("cannot reach the Docker daemon", completed.stderr)


if __name__ == "__main__":
    unittest.main()

"""Contract tests for the lifecycle-only daemon watchdog."""

from __future__ import annotations

import os
import stat
import subprocess
import tempfile
import unittest
from dataclasses import dataclass
from pathlib import Path


SCRIPT = Path(__file__).with_name("supervise-lifecycle.sh")
PROCESS_NAME = "fixture-daemon"
POLL_SECONDS = "1"
MANAGED_UID = "4242"


@dataclass(frozen=True)
class WatchdogRun:
    result: subprocess.CompletedProcess[str]
    lifecycle_calls: str
    pgrep_arguments: str


def write_executable(path: Path, content: str) -> None:
    path.write_text(content, encoding="utf-8")
    path.chmod(path.stat().st_mode | stat.S_IXUSR)


def run_watchdog(*, pgrep_status: int, sleep_status: int) -> WatchdogRun:
    with tempfile.TemporaryDirectory() as raw_directory:
        directory = Path(raw_directory)
        calls = directory / "lifecycle-calls"
        pgrep_arguments = directory / "pgrep-arguments"
        lifecycle = directory / "lifecycle"
        write_executable(
            lifecycle,
            f"#!/bin/sh\nprintf '%s\\n' \"$1\" >> '{calls}'\n",
        )
        write_executable(
            directory / "id",
            f"#!/bin/sh\nprintf '%s\\n' '{MANAGED_UID}'\n",
        )
        write_executable(
            directory / "pgrep",
            f"#!/bin/sh\nprintf '%s\\n' \"$@\" > '{pgrep_arguments}'\n"
            f"exit {pgrep_status}\n",
        )
        write_executable(directory / "sleep", f"#!/bin/sh\nexit {sleep_status}\n")
        environment = os.environ.copy()
        environment["PATH"] = str(directory)
        result = subprocess.run(
            [str(SCRIPT), str(lifecycle), PROCESS_NAME, POLL_SECONDS],
            check=False,
            capture_output=True,
            text=True,
            env=environment,
            timeout=5,
        )
        lifecycle_calls = calls.read_text(encoding="utf-8") if calls.exists() else ""
        return WatchdogRun(
            result=result,
            lifecycle_calls=lifecycle_calls,
            pgrep_arguments=pgrep_arguments.read_text(encoding="utf-8"),
        )


class SuperviseLifecycleTests(unittest.TestCase):
    def test_absent_process_is_booted_only_through_the_lifecycle_program(self) -> None:
        run = run_watchdog(pgrep_status=1, sleep_status=9)

        self.assertEqual(run.result.returncode, 9)
        self.assertEqual(run.lifecycle_calls, "boot\n")
        self.assertIn(f"{PROCESS_NAME} is absent", run.result.stderr)

    def test_present_process_is_only_observed(self) -> None:
        run = run_watchdog(pgrep_status=0, sleep_status=7)

        self.assertEqual(run.result.returncode, 7)
        self.assertEqual(run.lifecycle_calls, "")
        self.assertEqual(run.result.stderr, "")
        self.assertEqual(
            run.pgrep_arguments.splitlines(),
            ["-u", MANAGED_UID, "-x", "--", PROCESS_NAME],
        )

    def test_process_lookup_error_does_not_boot(self) -> None:
        run = run_watchdog(pgrep_status=2, sleep_status=9)

        self.assertEqual(run.result.returncode, 2)
        self.assertEqual(run.lifecycle_calls, "")
        self.assertIn("process lookup failed with status 2", run.result.stderr)

    def test_zero_poll_interval_is_rejected(self) -> None:
        result = subprocess.run(
            [str(SCRIPT), "/bin/true", PROCESS_NAME, "0"],
            check=False,
            capture_output=True,
            text=True,
            timeout=5,
        )

        self.assertEqual(result.returncode, 2)
        self.assertIn("positive integer", result.stderr)

    def test_process_name_beyond_kernel_command_limit_is_rejected(self) -> None:
        result = subprocess.run(
            [str(SCRIPT), "/bin/true", "fixture-daemon-x", POLL_SECONDS],
            check=False,
            capture_output=True,
            text=True,
            timeout=5,
        )

        self.assertEqual(result.returncode, 2)
        self.assertIn("between 1 and 15 characters", result.stderr)

    def test_process_name_with_regex_metacharacter_is_rejected(self) -> None:
        result = subprocess.run(
            [str(SCRIPT), "/bin/true", "fixture+daemon", POLL_SECONDS],
            check=False,
            capture_output=True,
            text=True,
            timeout=5,
        )

        self.assertEqual(result.returncode, 2)
        self.assertIn("only letters, digits, underscores, and hyphens", result.stderr)


if __name__ == "__main__":
    unittest.main()

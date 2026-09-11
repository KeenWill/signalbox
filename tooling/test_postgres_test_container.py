"""Prove the shared-server guardian follows the supplied libtest process."""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest


# Match the Rust fixture: its helper uses python3 from the host PATH.
# The Bazel Python toolchain can omit Linux pidfd support at build time.
ROOT = Path(__file__).resolve().parents[1]
FAKE_DOCKER = """import json, os, sys
from pathlib import Path
arguments = sys.argv[1:]
with Path(os.environ["DOCKER_CALLS"]).open("a") as calls:
    calls.write(json.dumps(arguments) + "\\n")
if arguments[0] == "run":
    print("fixture-container")
elif arguments[0] == "inspect":
    if "NetworkSettings" in arguments[2]:
        print(json.dumps({"5432/tcp": [{"HostPort": "15432"}]}))
    else:
        print(json.dumps({"/sbx-tablespaces/0": "rw", "/sbx-tablespaces/1": "rw"}))
"""


@unittest.skipUnless(sys.platform == "linux", "pidfd guardians require Linux")
class GuardianTest(unittest.TestCase):
    def test_bazel_registers_its_server_for_wrapper_cleanup(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            docker = directory / "docker"
            docker.write_text(f"#!{sys.executable}\n{FAKE_DOCKER}")
            docker.chmod(0o755)
            records = directory / "records"
            records.mkdir()
            environment = dict(
                os.environ,
                PATH=f"{directory}:{os.environ['PATH']}",
                DOCKER_CALLS=str(directory / "calls"),
                SIGNALBOX_TEST_POSTGRES_CONTAINERS=str(records),
            )
            environment.pop("TESTCONTAINERS_COMMAND", None)
            subprocess.check_output(
                [
                    "python3",
                    str(ROOT / "tooling/postgres_test_container.py"),
                    str(directory / "locks"),
                    "fixture-image",
                    (ROOT / "config/signalboxd.example.toml").read_text(),
                    "fixture-run",
                    str(os.getpid()),
                    "2",
                ],
                env=environment,
                text=True,
            )
            self.assertEqual(
                (records / "fixture-run").read_text(), "fixture-container\n"
            )

    def test_nextest_keeps_its_requested_slot_and_preservation_labels(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            docker = directory / "docker"
            docker.write_text(f"#!{sys.executable}\n{FAKE_DOCKER}")
            docker.chmod(0o755)
            calls = directory / "calls"
            environment = dict(
                os.environ,
                PATH=f"{directory}:{os.environ['PATH']}",
                DOCKER_CALLS=str(calls),
                NEXTEST_RUN_ID="fixture-run",
                NEXTEST_TEST_THREADS="2",
                NEXTEST_TEST_GLOBAL_SLOT="0",
                TESTCONTAINERS_COMMAND="keep",
            )
            result = subprocess.check_output(
                [
                    "python3",
                    str(ROOT / "tooling/postgres_test_container.py"),
                    str(directory / "locks"),
                    "fixture-image",
                    (ROOT / "config/signalboxd.example.toml").read_text(),
                ],
                env=environment,
                text=True,
            )
            self.assertEqual(json.loads(result)["slot"], 0)
            commands = [json.loads(line) for line in calls.read_text().splitlines()]
            start = next(command for command in commands if command[0] == "run")
            self.assertNotIn("org.signalbox.disposable=test-container", start)

    def test_killing_the_libtest_owner_removes_only_its_server(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            docker = directory / "docker"
            docker.write_text(f"#!{sys.executable}\n{FAKE_DOCKER}")
            docker.chmod(0o755)
            calls = directory / "calls"
            environment = dict(
                os.environ,
                PATH=f"{directory}:{os.environ['PATH']}",
                DOCKER_CALLS=str(calls),
            )
            environment.pop("TESTCONTAINERS_COMMAND", None)
            owner = subprocess.Popen(
                [sys.executable, "-c", "import sys; sys.stdin.read()"],
                stdin=subprocess.PIPE,
            )
            try:
                result = subprocess.check_output(
                    [
                        "python3",
                        str(ROOT / "tooling/postgres_test_container.py"),
                        str(directory / "locks"),
                        "fixture-image",
                        (ROOT / "config/signalboxd.example.toml").read_text(),
                        "fixture-run",
                        str(owner.pid),
                        "2",
                    ],
                    env=environment,
                    text=True,
                )
                self.assertEqual(json.loads(result)["slot"], 0)
                owner.kill()
                owner.wait()
                # Allow the guardian to be scheduled on a busy CI runner.
                deadline = time.monotonic() + 30
                expected = ["rm", "-f", "-v", "fixture-container"]
                while expected not in [
                    json.loads(line) for line in calls.read_text().splitlines()
                ]:
                    if time.monotonic() >= deadline:
                        self.fail(
                            "guardian did not remove the dead libtest owner's server"
                        )
                    time.sleep(0.05)
                commands = [json.loads(line) for line in calls.read_text().splitlines()]
                self.assertEqual(
                    [command for command in commands if command[0] == "rm"], [expected]
                )
                start = next(command for command in commands if command[0] == "run")
                self.assertIn("org.signalbox.test-run=fixture-run", start)
                self.assertIn("org.signalbox.disposable=test-container", start)
                self.assertEqual(
                    sum(argument.startswith("/sbx-tablespaces/") for argument in start),
                    2,
                )
            finally:
                if owner.poll() is None:
                    owner.kill()
                    owner.wait()
                owner.stdin.close()


if __name__ == "__main__":
    unittest.main()

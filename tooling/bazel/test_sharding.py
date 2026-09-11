"""Check libtest selection across Bazel shards, including empty shards."""

import os
from pathlib import Path
import re
import subprocess
import tempfile
import unittest


class ShardingTests(unittest.TestCase):
    def test_shared_fixture_cleanup_preserves_the_test_exit_status(self):
        wrapper = str(Path(os.environ["SHARD_WRAPPER"]).resolve())
        fixture = str(Path(os.environ["SHARD_FIXTURE"]).resolve())
        for status in (0, 23):
            with (
                self.subTest(status=status),
                tempfile.TemporaryDirectory() as temporary,
            ):
                directory = Path(temporary)
                docker = directory / "docker"
                docker.write_text(
                    '#!/bin/bash\nprintf "%s\\n" "$*" >> "${DOCKER_CALLS}"\n'
                )
                docker.chmod(0o755)
                calls = directory / "calls"
                environment = dict(
                    os.environ,
                    PATH=f"{directory}:{os.environ['PATH']}",
                    TEST_TMPDIR=temporary,
                    TEST_TOTAL_SHARDS="0",
                    SIGNALBOX_TEST_SHARED_POSTGRES="1",
                    SIGNALBOX_FIXTURE_EXIT=str(status),
                    DOCKER_CALLS=str(calls),
                )
                environment.pop("SIGNALBOX_TEST_TOTAL_SHARDS", None)
                result = subprocess.run(
                    [wrapper, fixture, "--exact", "cleanup_fixture"],
                    env=environment,
                    capture_output=True,
                    text=True,
                )
                self.assertEqual(result.returncode, status, result.stderr)
                self.assertEqual(calls.read_text(), "rm -f -v fixture-container\n")

    def test_shards_cover_selected_ignored_tests_exactly_once(self):
        wrapper = str(Path(os.environ["SHARD_WRAPPER"]).resolve())
        fixture = str(Path(os.environ["SHARD_FIXTURE"]).resolve())
        for mode in ("bazel", "matrix"):
            for total in (1, 2, 5):
                with (
                    self.subTest(mode=mode, total=total),
                    tempfile.TemporaryDirectory() as directory,
                ):
                    observed = []
                    for index in range(total):
                        status = Path(directory) / f"shard-{index}"
                        environment = dict(os.environ, TEST_TMPDIR=directory)
                        for name in (
                            "TEST_TOTAL_SHARDS",
                            "TEST_SHARD_INDEX",
                            "TEST_SHARD_STATUS_FILE",
                            "SIGNALBOX_TEST_TOTAL_SHARDS",
                            "SIGNALBOX_TEST_SHARD_INDEX",
                        ):
                            environment.pop(name, None)
                        if mode == "bazel":
                            environment.update(
                                TEST_TOTAL_SHARDS=str(total),
                                TEST_SHARD_INDEX=str(index),
                                TEST_SHARD_STATUS_FILE=str(status),
                            )
                        else:
                            environment.update(
                                SIGNALBOX_TEST_TOTAL_SHARDS=str(total),
                                SIGNALBOX_TEST_SHARD_INDEX=str(index),
                                TEST_TOTAL_SHARDS="0",
                            )
                        result = subprocess.run(
                            [wrapper, fixture, "--ignored", "--skip", "skipped"],
                            env=environment,
                            capture_output=True,
                            text=True,
                        )
                        self.assertEqual(result.returncode, 0, result.stderr)
                        if total > 1 and mode == "bazel":
                            self.assertTrue(status.exists())
                        observed.extend(
                            re.findall(r"^test (.+) \.\.\. ok$", result.stdout, re.M)
                        )
                    self.assertCountEqual(observed, ["selected", "selected_suffix"])


if __name__ == "__main__":
    unittest.main()

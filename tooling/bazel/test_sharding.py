"""Check libtest selection across Bazel shards, including empty shards."""

import os
from pathlib import Path
import re
import subprocess
import tempfile
import unittest


class ShardingTests(unittest.TestCase):
    def test_shards_cover_selected_ignored_tests_exactly_once(self):
        wrapper = str(Path(os.environ["SHARD_WRAPPER"]).resolve())
        fixture = str(Path(os.environ["SHARD_FIXTURE"]).resolve())
        for total in (1, 2, 5):
            with self.subTest(total=total), tempfile.TemporaryDirectory() as directory:
                observed = []
                for index in range(total):
                    status = Path(directory) / f"shard-{index}"
                    result = subprocess.run(
                        [wrapper, fixture, "--ignored", "--skip", "skipped"],
                        env=dict(
                            os.environ,
                            TEST_TOTAL_SHARDS=str(total),
                            TEST_SHARD_INDEX=str(index),
                            TEST_SHARD_STATUS_FILE=str(status),
                            TEST_TMPDIR=directory,
                        ),
                        capture_output=True,
                        text=True,
                    )
                    self.assertEqual(result.returncode, 0, result.stderr)
                    if total > 1:
                        self.assertTrue(status.exists())
                    observed.extend(re.findall(r"^test (.+) \.\.\. ok$", result.stdout, re.M))
                self.assertCountEqual(observed, ["selected", "selected_suffix"])


if __name__ == "__main__":
    unittest.main()

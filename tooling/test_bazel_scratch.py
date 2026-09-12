"""Exercise job scratch allocation without contacting Bazel or GitHub."""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest

import yaml


ACTION = Path(__file__).resolve().parents[1] / ".github/actions/setup-bazel"


class ScratchTest(unittest.TestCase):
    def test_repeated_setup_uses_distinct_roots_inside_runner_temporary_storage(self):
        with tempfile.TemporaryDirectory(prefix="bazel scratch ") as temporary:
            directory = Path(temporary)
            output = directory / "output"
            environment = directory / "environment"
            sibling = directory / "another-job"
            sibling.write_text("preserve me")
            inputs = dict(
                os.environ,
                RUNNER_TEMP=temporary,
                GITHUB_OUTPUT=str(output),
                GITHUB_ENV=str(environment),
            )
            subprocess.run(
                ["bash", str(ACTION / "prepare-scratch.sh")], env=inputs, check=True
            )
            subprocess.run(
                ["bash", str(ACTION / "prepare-scratch.sh")], env=inputs, check=True
            )
            roots = [
                Path(line.removeprefix("root="))
                for line in output.read_text().splitlines()
            ]
            self.assertEqual(len(roots), 2)
            self.assertNotEqual(*roots)
            for root in roots:
                self.assertEqual(root.parent, directory)
                self.assertTrue(root.is_dir())
            self.assertEqual(
                environment.read_text().splitlines(),
                [f"BAZELISK_HOME={root}/bazelisk" for root in roots],
            )
            self.assertEqual(sibling.read_text(), "preserve me")

    def test_setup_action_places_every_bazel_cache_under_its_allocated_root(self):
        action = yaml.safe_load((ACTION / "action.yml").read_text())
        allocation, setup = action["runs"]["steps"]
        self.assertEqual(allocation["id"], "scratch")
        self.assertIn("prepare-scratch.sh", allocation["run"])
        root = "${{ steps.scratch.outputs.root }}"
        options = setup["with"]
        self.assertEqual(options["output-base"], f'"{root}/output"')
        self.assertIn(f'startup --output_user_root="{root}/user"', options["bazelrc"])
        self.assertIn(
            f'common --repository_cache="{root}/repository"', options["bazelrc"]
        )


if __name__ == "__main__":
    unittest.main()

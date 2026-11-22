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
                BAZEL_REPOSITORY_CACHE="",
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
                if line.startswith("root=")
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
            self.assertEqual(
                [line for line in output.read_text().splitlines()
                 if line.startswith("repository_cache=")],
                [f"repository_cache={root}/repository" for root in roots],
            )

    def test_mounted_repository_cache_is_selected_without_changing_its_contents(self):
        with tempfile.TemporaryDirectory(prefix="bazel scratch ") as temporary:
            directory = Path(temporary)
            repository = directory / "mounted cache"
            repository.mkdir()
            marker = repository / "download"
            marker.write_text("retained")
            output = directory / "output"
            subprocess.run(
                ["bash", str(ACTION / "prepare-scratch.sh")],
                env=dict(os.environ, RUNNER_TEMP=temporary, GITHUB_OUTPUT=str(output),
                         GITHUB_ENV=str(directory / "environment"),
                         BAZEL_REPOSITORY_CACHE=str(repository)),
                check=True,
            )
            values = dict(line.split("=", 1) for line in output.read_text().splitlines())
            self.assertEqual(values["repository_cache"], str(repository))
            self.assertNotEqual(values["root"], str(repository))
            self.assertEqual(marker.read_text(), "retained")

    def test_setup_action_uses_private_outputs_and_the_selected_repository_cache(self):
        action = yaml.safe_load((ACTION / "action.yml").read_text())
        allocation, setup = action["runs"]["steps"]
        self.assertEqual(allocation["id"], "scratch")
        self.assertIn("prepare-scratch.sh", allocation["run"])
        root = "${{ steps.scratch.outputs.root }}"
        options = setup["with"]
        self.assertEqual(options["output-base"], f'"{root}/output"')
        self.assertIn(f'startup --output_user_root="{root}/user"', options["bazelrc"])
        self.assertIn(
            'common --repository_cache="${{ steps.scratch.outputs.repository_cache }}"',
            options["bazelrc"],
        )


if __name__ == "__main__":
    unittest.main()

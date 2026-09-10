"""Exercise workflow revision fetches against real shallow Git repositories."""

import contextlib
import io
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import yaml

ROOT = Path(__file__).resolve().parent.parent
WORKFLOWS = (
    "rust.yml", "swift.yml", "anthropic-smoke.yml", "claude-smoke.yml",
    "codex-smoke.yml", "openai-smoke.yml",
)


def comparison_steps():
    for name in WORKFLOWS:
        workflow = yaml.safe_load((ROOT / ".github/workflows" / name).read_text())
        for job in workflow["jobs"].values():
            for step in job.get("steps", []):
                if step.get("id") == "comparison":
                    yield name, step["run"]


class RevisionFetchTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.directory = Path(temporary.name)
        self.source = self.directory / "source"
        self.source.mkdir()
        self.git(self.source, "init", "-b", "main")
        self.git(self.source, "config", "user.name", "Fixture")
        self.git(self.source, "config", "user.email", "fixture@example.invalid")
        (self.source / "deleted\nfile.rs").write_text("old\n")
        self.base = self.commit("common base")
        self.git(self.source, "checkout", "-b", "topic")
        (self.source / "deleted\nfile.rs").unlink()
        (self.source / "topic.rs").write_text("new\n")
        self.head = self.commit("topic changes")
        self.git(self.source, "checkout", "main")
        (self.source / "unrelated.rs").write_text("base branch only\n")
        self.tip = self.commit("base branch advances")

    def git(self, directory, *arguments):
        return subprocess.check_output(
            ["git", "-C", str(directory), *arguments], stderr=subprocess.PIPE
        ).decode().strip()

    def commit(self, message):
        self.git(self.source, "add", "-A")
        self.git(self.source, "commit", "-m", message)
        return self.git(self.source, "rev-parse", "HEAD")

    def execute(self, code, *, merge_base=True, response=None, base=None):
        checkout = self.directory / "checkout"
        if not checkout.exists():
            self.git(self.directory, "clone", "--depth=1", self.source.as_uri(), str(checkout))
        output = self.directory / "output"
        environment = {
            "BASE_SHA": base or self.tip,
            "HEAD_SHA": self.head,
            "MERGE_BASE": str(merge_base).lower(),
            "GH_TOKEN": "fixture-token",
            "GITHUB_API_URL": "https://api.github.invalid",
            "GITHUB_REPOSITORY": "fixture/repository",
            "GITHUB_OUTPUT": str(output),
        }
        payload = response if response is not None else {"merge_base_commit": {"sha": self.base}}
        with contextlib.chdir(checkout), patch.dict(os.environ, environment), patch(
            "urllib.request.urlopen", return_value=io.BytesIO(json.dumps(payload).encode())
        ) as request:
            exec(compile(code, "workflow comparison", "exec"), {})
        return checkout, output, request

    def test_pull_request_diff_excludes_changes_only_on_the_base_branch(self):
        for workflow, code in comparison_steps():
            with self.subTest(workflow=workflow):
                checkout, output, request = self.execute(code)
                self.assertEqual(
                    self.git(checkout, "diff", "--no-renames", "--name-only", "-z", self.base, self.head),
                    "deleted\nfile.rs\0topic.rs\0",
                )
                self.assertIn(f"base={self.base}\nhead={self.head}\n", output.read_text())
                self.assertEqual(self.git(checkout, "rev-parse", "--is-shallow-repository"), "true")
                self.assertEqual(self.git(checkout, "config", "--local", "--get-regexp", "remote.origin.url"),
                                 f"remote.origin.url {self.source.as_uri()}")
                request.assert_called_once()

    def test_push_comparison_keeps_the_exact_event_base_without_an_api_call(self):
        code = next(comparison_steps())[1]
        checkout, output, request = self.execute(code, merge_base=False)
        self.assertEqual(output.read_text(), f"base={self.tip}\nhead={self.head}\n")
        self.assertIn("unrelated.rs", self.git(checkout, "diff", "--name-only", self.tip, self.head))
        request.assert_not_called()

    def test_invalid_merge_base_fails_before_publishing_a_scope(self):
        code = next(comparison_steps())[1]
        with self.assertRaises(SystemExit):
            self.execute(code, response={"merge_base_commit": {"sha": "not-a-commit"}})
        self.assertFalse((self.directory / "output").exists())

    def test_unavailable_commit_fails_before_publishing_a_scope(self):
        code = next(comparison_steps())[1]
        with self.assertRaises(subprocess.CalledProcessError):
            self.execute(code, merge_base=False, base="0" * 40)
        self.assertFalse((self.directory / "output").exists())

    def test_api_failure_does_not_publish_an_empty_successful_scope(self):
        code = next(comparison_steps())[1]
        with self.assertRaises(KeyError):
            self.execute(code, response={"message": "comparison unavailable"})
        self.assertFalse((self.directory / "output").exists())


if __name__ == "__main__":
    unittest.main()

#!/usr/bin/env python3
"""Contract tests for the local session-workspace deployment helper."""

from __future__ import annotations

import os
import subprocess
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("provision-session-workspace.sh")
SESSION_ID = "018f6d4a-7b2c-7def-8123-456789abcdef"
REMOTE = "git@github.com:KeenWill/signalbox.git"


def run(*arguments: str, check: bool = True) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [str(SCRIPT), *arguments],
        check=check,
        capture_output=True,
        text=True,
    )


def git(repository: Path, *arguments: str) -> str:
    completed = subprocess.run(
        ["git", "-C", str(repository), *arguments],
        check=True,
        capture_output=True,
        text=True,
        env={
            **os.environ,
            "GIT_AUTHOR_NAME": "Signalbox Test",
            "GIT_AUTHOR_EMAIL": "signalbox@example.invalid",
            "GIT_COMMITTER_NAME": "Signalbox Test",
            "GIT_COMMITTER_EMAIL": "signalbox@example.invalid",
        },
    )
    return completed.stdout.strip()


class ProvisionSessionWorkspaceTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name) / "workspace"
        self.root.mkdir()
        git(self.root, "init", "--initial-branch=main")
        (self.root / "kept.txt").write_text("base\n", encoding="utf-8")
        (self.root / "removed.txt").write_text("remove me\n", encoding="utf-8")
        git(self.root, "add", "kept.txt", "removed.txt")
        git(self.root, "commit", "-m", "fixture")
        self.revision = git(self.root, "rev-parse", "HEAD")
        self.target = Path(f"{self.root}.sessions") / SESSION_ID

    def provision(
        self, *extra: str, check: bool = True
    ) -> subprocess.CompletedProcess[str]:
        return run(
            "--configured-root",
            str(self.root),
            "--session-id",
            SESSION_ID,
            "--revision",
            self.revision,
            "--remote",
            REMOTE,
            *extra,
            check=check,
        )

    def test_provisions_direct_repository_at_derived_path(self) -> None:
        completed = self.provision()

        self.assertEqual(completed.stdout.strip(), str(self.target))
        self.assertTrue((self.target / ".git").is_dir())
        self.assertEqual(git(self.target, "rev-parse", "HEAD"), self.revision)
        self.assertEqual(git(self.target, "remote", "get-url", "origin"), REMOTE)

    def test_seed_tree_preserves_work_without_legacy_git_boundary(self) -> None:
        seed = Path(self.temporary.name) / "legacy"
        subprocess.run(
            ["cp", "-a", str(self.root), str(seed)],
            check=True,
            capture_output=True,
            text=True,
        )
        (seed / "kept.txt").write_text("changed\n", encoding="utf-8")
        (seed / "removed.txt").unlink()
        (seed / "new.txt").write_text("new\n", encoding="utf-8")
        self.provision("--seed-tree", str(seed))

        self.assertEqual((self.target / "kept.txt").read_text(), "changed\n")
        self.assertFalse((self.target / "removed.txt").exists())
        self.assertEqual((self.target / "new.txt").read_text(), "new\n")
        self.assertTrue((self.target / ".git").is_dir())

    def test_existing_direct_repository_is_left_unchanged(self) -> None:
        self.provision()
        marker = self.target / "operator-work.txt"
        marker.write_text("keep\n", encoding="utf-8")

        completed = self.provision()

        self.assertEqual(completed.stdout.strip(), str(self.target))
        self.assertEqual(marker.read_text(), "keep\n")

    def test_existing_repository_with_another_remote_is_refused(self) -> None:
        self.provision()
        git(self.target, "remote", "set-url", "origin", "ssh://example.invalid/other")

        completed = self.provision(check=False)

        self.assertEqual(completed.returncode, 1)

    def test_unreachable_commit_is_refused_before_parent_creation(self) -> None:
        unreachable = git(
            self.root,
            "commit-tree",
            f"{self.revision}^{{tree}}",
            "-m",
            "unreachable",
        )

        completed = run(
            "--configured-root",
            str(self.root),
            "--session-id",
            SESSION_ID,
            "--revision",
            unreachable,
            "--remote",
            REMOTE,
            check=False,
        )

        self.assertEqual(completed.returncode, 1)
        self.assertFalse(Path(f"{self.root}.sessions").exists())

    def test_invalid_session_id_is_refused_before_parent_creation(self) -> None:
        completed = run(
            "--configured-root",
            str(self.root),
            "--session-id",
            "not-a-session",
            "--revision",
            self.revision,
            "--remote",
            REMOTE,
            check=False,
        )

        self.assertEqual(completed.returncode, 2)
        self.assertFalse(Path(f"{self.root}.sessions").exists())


if __name__ == "__main__":
    unittest.main()

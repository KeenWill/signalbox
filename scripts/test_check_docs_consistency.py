#!/usr/bin/env python3
"""Behavioral tests for the Markdown-backed documentation checker."""

from __future__ import annotations

import subprocess
import tempfile
import unittest
from pathlib import Path

from check_docs_consistency import (
    TrackedFilesError,
    github_slug,
    run_checks,
    tracked_files,
)


def run_git(root: Path, *arguments: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["git", "-c", "commit.gpgsign=false", *arguments],
        cwd=root,
        check=True,
        text=True,
        capture_output=True,
    )


class DocsConsistencyTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        (self.root / "docs/spec").mkdir(parents=True)
        (self.root / "AGENTS.md").write_text(
            "# Guidance\n\n[Spec](docs/spec/example.md#repeated-heading-1)\n",
            encoding="utf-8",
        )
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n## Repeated heading\n\n## Repeated heading\n",
            encoding="utf-8",
        )
        template = self.root / ".empty-template"
        template.mkdir()
        run_git(self.root, "init", "-q", "-b", "main", f"--template={template}")
        run_git(self.root, "config", "user.name", "Docs checker tests")
        run_git(self.root, "config", "user.email", "docs-checker@example.invalid")
        run_git(self.root, "add", ".")
        run_git(self.root, "commit", "-q", "-m", "fixture")

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def write_and_track(self, relative: str, contents: str) -> Path:
        path = self.root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(contents, encoding="utf-8")
        run_git(self.root, "add", relative)
        return path

    def categories(self) -> list[str]:
        return [failure.category for failure in run_checks(self.root)]

    def test_valid_reference_and_duplicate_heading_pass(self) -> None:
        self.assertEqual(run_checks(self.root), [])

    def test_missing_relative_target_fails(self) -> None:
        self.write_and_track("docs/missing.md", "[Nope](absent.md)\n")
        failures = run_checks(self.root)
        self.assertEqual(failures[0].category, "relative-link")
        self.assertIn("does not exist", failures[0].message)

    def test_repository_escape_fails(self) -> None:
        self.write_and_track("docs/escape.md", "[Outside](../../outside.md)\n")
        failures = run_checks(self.root)
        self.assertTrue(any("escapes" in failure.message for failure in failures))

    def test_missing_heading_fragment_fails(self) -> None:
        self.write_and_track("docs/bad-anchor.md", "[Bad](spec/example.md#absent)\n")
        failures = run_checks(self.root)
        self.assertTrue(any("#absent" in failure.message for failure in failures))

    def test_fenced_and_inline_code_do_not_expose_links(self) -> None:
        self.write_and_track(
            "docs/code.md",
            "`[Inline](missing.md)`\n\n```md\n[Block](missing.md)\n```\n",
        )
        self.assertEqual(run_checks(self.root), [])

    def test_reference_links_and_images_are_checked(self) -> None:
        self.write_and_track(
            "docs/assets.md",
            "[Reference][missing]\n\n![Image](missing.png)\n\n"
            "[missing]: absent.md\n",
        )
        messages = [failure.message for failure in run_checks(self.root)]
        self.assertEqual(sum("does not exist" in message for message in messages), 2)

    def test_image_does_not_satisfy_machine_owner_citation(self) -> None:
        self.write_and_track(
            "docs/spec/credential-availability.md", "# Credential availability\n"
        )
        self.write_and_track(
            "docs/spec/runtime-substrate.md",
            "# Runtime substrate\n\n![Diagram](credential-availability.md)\n",
        )
        self.assertIn("machine-owner-link", self.categories())

    def test_heading_slug_collision_uses_global_slug_set(self) -> None:
        self.write_and_track(
            "docs/spec/collisions.md",
            "# Collisions\n\n## Foo\n\n## Foo\n\n## Foo-1\n\n",
        )
        self.write_and_track(
            "docs/collision-link.md", "[Third](spec/collisions.md#foo-1-1)\n"
        )
        self.assertEqual(run_checks(self.root), [])

    def test_manifest_without_workflow_fails(self) -> None:
        self.write_and_track(
            "Cargo.toml", '[package]\nname = "fixture"\nversion = "0.0.0"\n'
        )
        self.write_and_track(
            ".github/postgres-integration-suites.toml",
            '[[suite]]\nname = "fixture"\npackage = "fixture"\nshards = 1\n',
        )
        failures = run_checks(self.root)
        self.assertTrue(
            any(
                failure.category == "suite-manifest"
                and "exists without" in failure.message
                for failure in failures
            )
        )

    def test_external_and_root_relative_urls_are_outside_scope(self) -> None:
        self.write_and_track(
            "docs/external.md",
            "[Web](https://example.com/x) [Site root](/assets/x)\n",
        )
        self.assertEqual(run_checks(self.root), [])

    def test_explicit_html_anchor_resolves(self) -> None:
        self.write_and_track(
            "docs/html.md",
            "# HTML\n\n<a id=\"exact\"></a>\n\n[Self](#exact)\n",
        )
        self.assertEqual(run_checks(self.root), [])

    def test_untracked_markdown_is_ignored(self) -> None:
        (self.root / "docs/untracked.md").write_text(
            "[Missing](absent.md)\n", encoding="utf-8"
        )
        self.assertEqual(run_checks(self.root), [])

    def test_machine_projection_requires_owner_link(self) -> None:
        self.write_and_track(
            "docs/spec/credential-availability.md", "# Credential availability\n"
        )
        self.write_and_track(
            "docs/spec/runtime-substrate.md", "# Runtime substrate\n"
        )
        self.assertIn("machine-owner-link", self.categories())

    def test_machine_projection_accepts_owner_link(self) -> None:
        self.write_and_track(
            "docs/spec/credential-availability.md", "# Credential availability\n"
        )
        self.write_and_track(
            "docs/spec/runtime-substrate.md",
            "# Runtime substrate\n\n[Owner](credential-availability.md)\n",
        )
        self.assertNotIn("machine-owner-link", self.categories())

    def test_tracked_files_fails_outside_repository(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaises(TrackedFilesError):
                tracked_files(Path(directory))

    def test_github_slug_shapes(self) -> None:
        self.assertEqual(github_slug("API: `current_time` & IDs"), "api-current_time--ids")


if __name__ == "__main__":
    unittest.main()

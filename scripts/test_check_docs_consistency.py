#!/usr/bin/env python3
"""Focused regression tests for check_docs_consistency.py."""

from __future__ import annotations

import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.dont_write_bytecode = True

import check_docs_consistency
from check_docs_consistency import (
    TrackedFilesError,
    Violation,
    github_slug,
    run_checks as run_docs_checks,
    tracked_files,
)
from generate_invariants import (
    orphan_invariant_references as find_orphan_invariant_references,
    render as render_generated_invariant_index,
)
from postgres_integration_suites import ManifestError


def write_suite_manifest(root: Path, *suites: str) -> None:
    """Write a PostgreSQL suite manifest holding exactly the given entries.

    Each argument is one rendered `[[suite]]` body. The manifest is what the
    checker reads to decide which `#[ignore]`d tests authoritative CI runs, so
    a fixture that wants ignored enforcement declares it here.
    """
    manifest = root / ".github/postgres-integration-suites.toml"
    manifest.parent.mkdir(parents=True, exist_ok=True)
    manifest.write_text("\n".join(f"[[suite]]\n{suite}" for suite in suites))


def write_manifest_workflow(root: Path, *names: str) -> None:
    """Write a Rust workflow that agrees with a manifest declaring `names`."""
    workflow = root / ".github/workflows/rust.yml"
    workflow.parent.mkdir(parents=True, exist_ok=True)
    uploads = "".join(
        f"      - uses: actions/upload-artifact@v7\n"
        f"        with:\n"
        f"          name: postgres-integration-archive-{name}\n"
        f"          path: ${{{{ runner.temp }}}}/{name}.tar.zst\n"
        for name in names
    )
    aggregate = (
        "  postgres-integration:\n"
        "    if: ${{ always() }}\n"
        "    runs-on: ubuntu-latest\n"
        "    steps:\n"
        "      - env:\n"
        "          BUILD_RESULT: ${{ needs.postgres-integration-build.result }}\n"
        "          RUN_RESULT: ${{ needs.postgres-integration-run.result }}\n"
        "        run: |\n"
        '          test "$BUILD_RESULT" = success\n'
        '          test "$RUN_RESULT" = success\n'
    )
    workflow.write_text(
        "jobs:\n"
        "  postgres-integration-run:\n"
        "    runs-on: signalbox-docker\n"
        "    steps:\n"
        "      - env:\n"
        "          SUITE: ${{ matrix.suite }}\n"
        "          PARTITION: ${{ matrix.partition }}\n"
        "          PARTITIONS: ${{ matrix.partitions }}\n"
        "          FILTER: ${{ matrix.filter }}\n"
        '        run: cargo nextest run --archive-file "$RUNNER_TEMP/$SUITE.z"'
        ' --partition "count:$PARTITION/$PARTITIONS"'
        ' --run-ignored only -E "$FILTER"\n'
        "  postgres-integration-build:\n"
        "    runs-on: signalbox-docker\n"
        "    steps:\n"
        "      - run: python3 scripts/postgres_integration_suites.py --matrix\n"
        "      - run: python3 scripts/postgres_integration_suites.py"
        " --archive-plan\n"
        f"{uploads}{aggregate}",
        encoding="utf-8",
    )


def failure_categories(failures: list[Violation]) -> list[str]:
    """Project deterministic failure categories outside test bodies."""
    return [failure.category for failure in failures]


def failure_messages(failures: list[Violation]) -> list[str]:
    """Project deterministic failure messages outside test bodies."""
    return [failure.message for failure in failures]


def fixture_root(directory: str) -> Path:
    """Canonicalize a fixture root the way discovery canonicalizes its inputs.

    ``tracked_files`` resolves every path it returns, so a root that is not
    itself resolved can never be found to contain them. Temporary directories
    on macOS live under ``/var``, a symlink to ``/private/var``, which makes
    that mismatch load-bearing there while it stays invisible on Linux.
    """
    return Path(directory).resolve()


def run_git(root: Path, *arguments: str) -> None:
    """Run one deterministic local-only Git fixture command."""
    disabled_hooks = root / ".disabled-git-hooks"
    disabled_hooks.mkdir(exist_ok=True)
    subprocess.run(
        [
            "git",
            "-c",
            "commit.gpgSign=false",
            "-c",
            f"core.hooksPath={disabled_hooks}",
            *arguments,
        ],
        cwd=root,
        check=True,
        capture_output=True,
        text=True,
    )


def run_checks(root: Path) -> list[Violation]:
    """Track each test's intended fixture edits before running the validator."""
    run_git(root, "add", "-A")
    return run_docs_checks(root)


def render_invariant_index(root: Path) -> str:
    """Track intended fixture edits before rendering its invariant index."""
    run_git(root, "add", "-A")
    return render_generated_invariant_index(root)


def ignored_test_packages(root: Path) -> list[str]:
    """Project packages credited with authoritative ignored-test execution."""
    return [
        run.package for run in check_docs_consistency.workflow_ignored_test_runs(root)
    ]


def orphan_invariant_references(root: Path) -> dict[str, tuple[str, ...]]:
    """Track intended fixture edits before finding orphan references."""
    run_git(root, "add", "-A")
    return find_orphan_invariant_references(root)


def initialize_git_repository(root: Path) -> None:
    """Create the baseline fixture repository."""
    empty_template = root / ".empty-git-template"
    empty_template.mkdir()
    run_git(root, "init", "-q", "-b", "main", f"--template={empty_template}")
    run_git(root, "config", "user.name", "Docs checker tests")
    run_git(root, "config", "user.email", "docs-checker@example.invalid")
    run_git(root, "add", ".")
    run_git(root, "commit", "-q", "-m", "initial fixture")


class DocsConsistencyTests(unittest.TestCase):
    def setUp(self) -> None:
        self.environment = patch.dict(os.environ)
        self.environment.start()
        os.environ.pop("GITHUB_EVENT_PATH", None)
        self.temporary = tempfile.TemporaryDirectory()
        self.root = fixture_root(self.temporary.name)
        (self.root / "docs/spec").mkdir(parents=True)
        (self.root / "src").mkdir()
        (self.root / "AGENTS.md").write_text(
            "# Agent guidance\n\n"
            "[Spec](docs/spec/example.md#provider-bridge-and-current_time)\n"
            "[Docs directory](docs/)\n"
            "`[Ignored code](missing.md)`\n"
            "[Reference][spec]\n\n"
            "    [Indented code](missing-indented.md)\n\n"
            "<!-- [Commented link](missing-commented.md) -->\n\n"
            "[Self][self]\n\n"
            "[spec]:\n"
            "  docs/spec/example.md#repeat\n\n"
            "[self]: <>\n\n"
            "[^note]: explanatory text, not a link destination\n",
            encoding="utf-8",
        )
        (self.root / "docs/invariants.md").write_text(
            "# Invariants\n\n"
            "| ID | Invariant | Class | Status | Enforcement |\n"
            "| -- | -- | -- | -- | -- |\n"
            "| INV-001 | Law. | Domain | Accepted | "
            "tests `named_test` in [`src/tests.rs`](../src/tests.rs). |\n",
            encoding="utf-8",
        )
        (self.root / "src/tests.rs").write_text(
            "#[test]\nfn named_test() {}\n", encoding="utf-8"
        )
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n\n"
            "## Repeat\n\n"
            "[Duplicate](#repeat-1)\n",
            encoding="utf-8",
        )
        (self.root / "docs/spec/README.md").write_text(
            "# Specification\n", encoding="utf-8"
        )
        initialize_git_repository(self.root)

    def tearDown(self) -> None:
        self.temporary.cleanup()
        self.environment.stop()

    def test_failure_projection_helpers(self) -> None:
        failures = [
            Violation("a.md", 1, "first-category", "first message"),
            Violation("b.md", 2, "second-category", "second message"),
        ]

        self.assertEqual(
            failure_categories(failures),
            ["first-category", "second-category"],
        )
        self.assertEqual(
            failure_messages(failures),
            ["first message", "second message"],
        )

    def test_valid_fixture_passes(self) -> None:
        self.assertEqual(run_checks(self.root), [])

    def _write_machine_owner(self, projecting_body: str) -> None:
        """Create the machine owner and one page that projects a column."""
        (self.root / "docs/spec/credential-availability.md").write_text(
            "# Credential availability\n\n"
            "## The credential-availability machine\n",
            encoding="utf-8",
        )
        (self.root / "docs/spec/runtime-substrate.md").write_text(
            "# Model-runtime substrate\n\n"
            + projecting_body,
            encoding="utf-8",
        )

    def test_projection_owner_without_a_link_fails(self) -> None:
        """A derived view that stops citing its owner is the carve seam.

        This is the failure that started the restructuring: a paragraph moved
        to another branch, the anchor citing it still resolved, and only the
        meaning left — so no link checker saw anything.
        """
        self._write_machine_owner(
            "This page owns the evidence algebra of the "
            "credential-availability machine.\n"
        )

        failures = run_checks(self.root)

        self.assertIn("machine-owner-link", failure_categories(failures))

    def test_projection_owner_with_a_resolving_link_passes(self) -> None:
        self._write_machine_owner(
            "This page owns the evidence algebra of "
            "[the machine](credential-availability.md#the-credential-availability-machine).\n"
        )

        failures = run_checks(self.root)

        self.assertNotIn("machine-owner-link", failure_categories(failures))

    def test_projection_owner_with_a_reference_link_passes(self) -> None:
        self._write_machine_owner(
            "This page owns the evidence algebra of [the machine][owner].\n\n"
            "[owner]: credential-availability.md\n"
        )

        failures = run_checks(self.root)

        self.assertNotIn("machine-owner-link", failure_categories(failures))

    def _assert_label_still_cites(self, label: str) -> None:
        """Assert one label spelling still counts as a citation."""
        self._write_machine_owner(
            "This page owns the evidence algebra of "
            f"[{label}](credential-availability.md).\n"
        )

        failures = run_checks(self.root)

        self.assertNotIn("machine-owner-link", failure_categories(failures))

    def test_empty_label_is_still_a_citation(self) -> None:
        """Label rendering is out of scope; the destination decides.

        This pins a scope decision, not an oversight. Four waves produced four
        findings in one family — a construct that resolves but renders no
        navigation — and testing the label closed three while the fourth
        arrived against the restatement meant to end the family. Deciding it
        soundly needs a Markdown renderer this module does not have, and none
        of the shapes occurs in the tracked corpus. The guarded failure is a
        page that stops citing its owner, which no label can cause.
        """
        self._assert_label_still_cites("")

    def test_formatting_only_label_is_still_a_citation(self) -> None:
        self._assert_label_still_cites("**  **")

    def test_raw_html_label_is_still_a_citation(self) -> None:
        self._assert_label_still_cites("<span></span>")

    def test_image_destination_is_not_a_citation(self) -> None:
        """An image renders a fetch, not a navigation.

        The extractor returns image destinations because its other caller
        checks that every destination resolves. Counted here, a broken image
        would satisfy the guard while the page carries no way to reach the
        owner.
        """
        self._write_machine_owner(
            "This page owns the evidence algebra of "
            "![the machine](credential-availability.md).\n"
        )

        failures = run_checks(self.root)

        self.assertIn("machine-owner-link", failure_categories(failures))

    def test_reference_style_image_is_not_a_citation(self) -> None:
        """An image cites nothing however its destination is spelled.

        The reference form is a distinct path from the inline one: skipping
        only the image's `!` re-enters its own construct, so `[owner]` parses
        again as a shortcut link and the image counts as navigation. Image
        exclusion is retained contract, not the out-of-scope label question.
        """
        self._write_machine_owner(
            "This page owns the evidence algebra of ![the machine][owner].\n\n"
            "[owner]: credential-availability.md\n"
        )

        failures = run_checks(self.root)

        self.assertIn("machine-owner-link", failure_categories(failures))

    def test_unused_reference_definition_is_not_a_citation(self) -> None:
        """A definition nobody uses renders no link a reader can follow.

        The extractor returns definitions alongside links because its other
        caller is checking that every destination resolves. Counted here, an
        unused definition would satisfy this guard with exactly the missing
        citation the guard exists to reject.
        """
        self._write_machine_owner(
            "This page owns the evidence algebra of the machine.\n\n"
            "[owner]: credential-availability.md\n"
        )

        failures = run_checks(self.root)

        self.assertIn("machine-owner-link", failure_categories(failures))

    def test_untracked_sibling_markdown_and_rust_sources_are_ignored(self) -> None:
        sibling = self.root / ".claude/worktrees/agent-phantom"
        (sibling / "docs/spec").mkdir(parents=True)
        (sibling / "src").mkdir()
        (sibling / "docs/spec/phantom.md").write_text(
            "# Phantom\n\n[Missing](nowhere.md) and INV-999.\n",
            encoding="utf-8",
        )
        (sibling / "src/phantom.rs").write_text(
            "#[test]\nfn s99_inv_999_phantom() {}\n",
            encoding="utf-8",
        )

        self.assertEqual(run_docs_checks(self.root), [])
        self.assertNotIn("INV-999", render_generated_invariant_index(self.root))
        self.assertEqual(find_orphan_invariant_references(self.root), {})

    def test_tracked_file_discovery_works_from_a_repository_subdirectory(self) -> None:
        sources = tracked_files(self.root / "docs")

        self.assertIn(self.root / "docs/invariants.md", sources)
        self.assertNotIn(self.root / "AGENTS.md", sources)

    def test_tracked_file_discovery_fails_outside_a_repository(self) -> None:
        outside = fixture_root(tempfile.mkdtemp())
        self.addCleanup(lambda: outside.rmdir())

        with self.assertRaisesRegex(TrackedFilesError, "not a Git repository"):
            tracked_files(outside)

    def test_tracked_file_discovery_fails_when_git_is_unavailable(self) -> None:
        missing_git = FileNotFoundError("git executable not found")

        with patch.object(
            check_docs_consistency.subprocess,
            "run",
            side_effect=missing_git,
        ):
            with self.assertRaisesRegex(TrackedFilesError, "unavailable"):
                tracked_files(self.root)

    def test_generator_renders_an_empty_index_without_tagged_tests(self) -> None:
        rendered = render_invariant_index(self.root)

        self.assertIn("| ID | Enforcement |", rendered)
        self.assertNotIn("| INV-", rendered)

    def test_generator_ignores_bannered_planning_references(self) -> None:
        planning = self.root / "docs/agents/backlog.md"
        planning.parent.mkdir()
        planning.write_text(
            "# Backlog\n\n"
            "> **Non-authoritative planning scratchpad — do not review.**\n\n"
            "A prospective item may reserve INV-999.\n",
            encoding="utf-8",
        )

        self.assertEqual(orphan_invariant_references(self.root), {})

    def test_generator_rejects_an_invariant_reference_without_a_tagged_test(
        self,
    ) -> None:
        orphan_tag = "INV-999"
        test_source = self.root / "src/tests.rs"
        test_source.write_text(
            "#[test]\nfn inv001_named_test() {}\n", encoding="utf-8"
        )
        spec_source = self.root / "docs/spec/example.md"
        spec_source.write_text(
            spec_source.read_text(encoding="utf-8")
            + f"\nAn unsupported claim cites {orphan_tag}.\n",
            encoding="utf-8",
        )
        expected_location = (
            f"docs/spec/example.md:{len(spec_source.read_text(encoding='utf-8').splitlines())}"
        )

        self.assertEqual(
            orphan_invariant_references(self.root),
            {orphan_tag: (expected_location,)},
        )

    def test_generator_rejects_rust_comment_orphans_but_not_literals(
        self,
    ) -> None:
        orphan_tag = "INV-999"
        source = self.root / "src/context.rs"
        source.write_text(
            'const EXAMPLE: &str = "INV-998";\n'
            f"/// A live Rust contract cites {orphan_tag}.\n"
            "pub fn context() {}\n",
            encoding="utf-8",
        )

        self.assertEqual(
            orphan_invariant_references(self.root),
            {orphan_tag: ("src/context.rs:2",)},
        )

    def test_git_fixture_commands_disable_signing_and_hooks(self) -> None:
        hooks = self.root / ".fixture-hooks"
        hooks.mkdir()
        pre_commit = hooks / "pre-commit"
        pre_commit.write_text("#!/bin/sh\ntouch hook-ran\nexit 1\n", encoding="utf-8")
        pre_commit.chmod(0o755)
        run_git(self.root, "config", "commit.gpgSign", "true")
        run_git(self.root, "config", "core.hooksPath", str(hooks))

        run_git(self.root, "commit", "-q", "--allow-empty", "-m", "isolated fixture")

        self.assertFalse((self.root / "hook-ran").exists())

    def test_tagged_enforcement_file_must_contain_its_invariant_tag(self) -> None:
        (self.root / "src/tests.rs").write_text(
            "#[test]\nfn untagged_test() {}\n", encoding="utf-8"
        )
        (self.root / "docs/invariants.md").write_text(
            "# Invariants\n\n"
            "| ID | Invariant | Class | Status | Enforcement |\n"
            "| -- | -- | -- | -- | -- |\n"
            "| INV-001 | Law. | Domain | Accepted | INV-001-tagged tests in "
            "[`src/tests.rs`](../src/tests.rs). |\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(failure_categories(failures), ["invariant-tag"])
        self.assertIn("contains no INV-001 tag", failures[0].message)

    def test_reverse_discovers_invariant_tag_in_test_name(self) -> None:
        (self.root / "src/uncited.rs").write_text(
            "#[test]\nfn s01_inv_001_uncited_enforcement() {}\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["invariant-registration"],
        )
        self.assertIn("INV-001-tagged tests", failures[0].message)
        self.assertIn("src/uncited.rs", failures[0].message)

    def test_reverse_discovers_invariant_tag_in_test_doc_comment(self) -> None:
        (self.root / "src/uncited.rs").write_text(
            "/// INV-001: tagged only by the enforcement comment.\n"
            "#[tokio::test]\n"
            "async fn generically_named_test() {}\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["invariant-registration"],
        )
        self.assertIn("src/uncited.rs", failures[0].message)

    def test_reverse_discovers_invariant_tag_in_block_doc_comment(self) -> None:
        (self.root / "src/uncited.rs").write_text(
            "/** INV-001: tagged by an attached block doc comment. */\n"
            "#[test]\n"
            "fn generically_named_test() {}\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["invariant-registration"],
        )
        self.assertIn("src/uncited.rs", failures[0].message)

    def test_reverse_discovers_tag_in_nested_block_doc_comment(self) -> None:
        (self.root / "src/uncited.rs").write_text(
            "/** Outer proof: /* nested explanation */ INV-001. */\n"
            "#[test]\n"
            "fn generically_named_test() {}\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["invariant-registration"],
        )
        self.assertIn("src/uncited.rs", failures[0].message)

    def test_triple_star_block_comment_cannot_supply_doc_tag(self) -> None:
        (self.root / "src/tests.rs").write_text(
            "/*** INV-001 is an ordinary block comment. */\n"
            "#[test]\n"
            "fn named_test() {}\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_ignored_test_does_not_register_invariant_enforcement(self) -> None:
        ignored = self.root / "src/ignored.rs"
        ignored.write_text(
            "#[test]\n"
            '#[ignore = "requires an external service"]\n'
            "fn inv_001_ignored_test() {}\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])
        self.assertNotIn(
            ignored.relative_to(self.root).as_posix(),
            render_invariant_index(self.root),
        )

    def test_cfg_ignored_test_does_not_register_invariant_enforcement(self) -> None:
        (self.root / "src/ignored.rs").write_text(
            "#[test]\n"
            "#[cfg_attr(test, ignore)]\n"
            "fn inv_001_ignored_test() {}\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_ci_executed_ignored_test_registers_invariant_enforcement(self) -> None:
        (self.root / "Cargo.toml").write_text(
            '[package]\nname = "fixture"\nversion = "0.0.0"\n',
            encoding="utf-8",
        )
        (self.root / "src/lib.rs").write_text(
            "mod ignored;\nmod tests;\n", encoding="utf-8"
        )
        ignored = self.root / "src/ignored.rs"
        ignored.write_text(
            "#[test]\n"
            '#[ignore = "requires an external service"]\n'
            "fn inv_001_ignored_test() {}\n",
            encoding="utf-8",
        )
        write_suite_manifest(
            self.root,
            'name = "fixture"\npackage = "fixture"\nshards = 1\n',
        )

        self.assertIn(
            ignored.relative_to(self.root).as_posix(),
            render_invariant_index(self.root),
        )

    def test_unmanifested_package_registers_no_ignored_enforcement(self) -> None:
        (self.root / "Cargo.toml").write_text(
            '[package]\nname = "fixture"\nversion = "0.0.0"\n',
            encoding="utf-8",
        )
        (self.root / "src/lib.rs").write_text("", encoding="utf-8")
        selected = self.root / "tests/selected.rs"
        selected.parent.mkdir()
        selected.write_text(
            "#[test]\n"
            "#[ignore]\n"
            "fn inv_001_in_a_manifested_package() {}\n",
            encoding="utf-8",
        )
        write_suite_manifest(
            self.root,
            'name = "other"\npackage = "not-this-workspace-package"\nshards = 1\n',
        )

        self.assertNotIn("| INV-001", render_invariant_index(self.root))

    def test_manifested_package_registers_ignored_enforcement(self) -> None:
        (self.root / "Cargo.toml").write_text(
            '[package]\nname = "fixture"\nversion = "0.0.0"\n',
            encoding="utf-8",
        )
        (self.root / "src/lib.rs").write_text("", encoding="utf-8")
        selected = self.root / "tests/selected.rs"
        selected.parent.mkdir()
        selected.write_text(
            "#[test]\n"
            "#[ignore]\n"
            "fn inv_001_in_a_manifested_package() {}\n",
            encoding="utf-8",
        )
        write_suite_manifest(
            self.root,
            'name = "fixture"\npackage = "fixture"\nshards = 1\n',
        )

        self.assertIn("| INV-001", render_invariant_index(self.root))

    def test_binary_include_uses_the_declared_cargo_target_name(self) -> None:
        (self.root / "Cargo.toml").write_text(
            '[package]\nname = "fixture"\nversion = "0.0.0"\n\n'
            "[[test]]\n"
            'name = "selected_target"\n'
            'path = "tests/physical_file.rs"\n',
            encoding="utf-8",
        )
        (self.root / "src/lib.rs").write_text("", encoding="utf-8")
        target = self.root / "tests/physical_file.rs"
        target.parent.mkdir()
        target.write_text(
            "#[test]\n#[ignore]\nfn inv_001_declared_target_name() {}\n",
            encoding="utf-8",
        )
        write_suite_manifest(
            self.root,
            'name = "fixture"\n'
            'package = "fixture"\n'
            'include_binaries = ["selected_target"]\n'
            "shards = 1\n",
        )

        self.assertIn("| INV-001", render_invariant_index(self.root))

    def test_binary_exclude_uses_the_declared_cargo_target_name(self) -> None:
        (self.root / "Cargo.toml").write_text(
            '[package]\nname = "fixture"\nversion = "0.0.0"\n\n'
            "[[test]]\n"
            'name = "excluded_target"\n'
            'path = "tests/physical_file.rs"\n',
            encoding="utf-8",
        )
        (self.root / "src/lib.rs").write_text("", encoding="utf-8")
        target = self.root / "tests/physical_file.rs"
        target.parent.mkdir()
        target.write_text(
            "#[test]\n#[ignore]\nfn inv_001_declared_target_name() {}\n",
            encoding="utf-8",
        )
        write_suite_manifest(
            self.root,
            'name = "fixture"\n'
            'package = "fixture"\n'
            'exclude_binaries = ["excluded_target"]\n'
            "shards = 1\n",
        )

        self.assertNotIn("| INV-001", render_invariant_index(self.root))

    def test_binary_include_uses_the_conventional_directory_target_name(self) -> None:
        (self.root / "Cargo.toml").write_text(
            '[package]\nname = "fixture"\nversion = "0.0.0"\n',
            encoding="utf-8",
        )
        (self.root / "src/lib.rs").write_text("", encoding="utf-8")
        target = self.root / "tests/nested_target/main.rs"
        target.parent.mkdir(parents=True)
        target.write_text(
            "#[test]\n#[ignore]\nfn inv_001_directory_target_name() {}\n",
            encoding="utf-8",
        )
        write_suite_manifest(
            self.root,
            'name = "fixture"\n'
            'package = "fixture"\n'
            'include_binaries = ["nested_target"]\n'
            "shards = 1\n",
        )

        self.assertIn("| INV-001", render_invariant_index(self.root))

    def test_binary_include_uses_an_overridden_library_target_name(self) -> None:
        (self.root / "Cargo.toml").write_text(
            '[package]\nname = "fixture-package"\nversion = "0.0.0"\n\n'
            '[lib]\nname = "selected_library"\n',
            encoding="utf-8",
        )
        (self.root / "src/lib.rs").write_text(
            "#[test]\n#[ignore]\nfn inv_001_library_target_name() {}\n",
            encoding="utf-8",
        )
        write_suite_manifest(
            self.root,
            'name = "fixture"\n'
            'package = "fixture-package"\n'
            'include_binaries = ["selected_library"]\n'
            "shards = 1\n",
        )

        self.assertIn("| INV-001", render_invariant_index(self.root))

    def test_binary_include_uses_the_implicit_name_for_a_relocated_library(self) -> None:
        (self.root / "Cargo.toml").write_text(
            '[package]\nname = "fixture-package"\nversion = "0.0.0"\n\n'
            '[lib]\npath = "src/custom_library.rs"\n',
            encoding="utf-8",
        )
        target = self.root / "src/custom_library.rs"
        target.write_text(
            "#[test]\n#[ignore]\nfn inv_001_implicit_library_target_name() {}\n",
            encoding="utf-8",
        )
        write_suite_manifest(
            self.root,
            'name = "fixture"\n'
            'package = "fixture-package"\n'
            'include_binaries = ["fixture_package"]\n'
            "shards = 1\n",
        )

        self.assertIn("| INV-001", render_invariant_index(self.root))

    def test_ci_ignored_test_skips_exclude_only_named_enforcement(self) -> None:
        selected_invariant = "INV-001"
        skipped_invariant = "INV-002"
        also_skipped_invariant = "INV-003"
        selected_test = selected_invariant.lower().replace("-", "_")
        skipped_test = skipped_invariant.lower().replace("-", "_")
        also_skipped_test = also_skipped_invariant.lower().replace("-", "_")
        (self.root / "Cargo.toml").write_text(
            '[package]\nname = "fixture"\nversion = "0.0.0"\n',
            encoding="utf-8",
        )
        (self.root / "src/lib.rs").write_text("", encoding="utf-8")
        selected = self.root / "tests/selected.rs"
        selected.parent.mkdir()
        selected.write_text(
            "#[test]\n"
            "#[ignore]\n"
            f"fn {selected_test}_selected_by_ci() {{}}\n"
            "#[test]\n"
            "#[ignore]\n"
            f"fn {skipped_test}_skipped_by_ci() {{}}\n"
            "#[test]\n"
            "#[ignore]\n"
            f"fn {also_skipped_test}_also_skipped_by_ci() {{}}\n",
            encoding="utf-8",
        )
        write_suite_manifest(
            self.root,
            'name = "fixture"\n'
            'package = "fixture"\n'
            "shards = 1\n"
            f'skip = ["{skipped_test}", "{also_skipped_test}"]\n',
        )

        rendered = render_invariant_index(self.root)

        self.assertIn(f"| {selected_invariant}", rendered)
        self.assertNotIn(f"| {skipped_invariant}", rendered)
        self.assertNotIn(f"| {also_skipped_invariant}", rendered)

    def declare_gated_target(self, *features: str) -> None:
        """Declare a package whose ignored test target requires `gated`."""
        (self.root / "Cargo.toml").write_text(
            '[package]\nname = "fixture"\nversion = "0.0.0"\n\n'
            "[features]\ndefault = []\ngated = []\n\n"
            "[[test]]\n"
            'name = "gated"\n'
            'path = "tests/gated.rs"\n'
            'required-features = ["gated"]\n',
            encoding="utf-8",
        )
        (self.root / "src/lib.rs").write_text("", encoding="utf-8")
        target = self.root / "tests/gated.rs"
        target.parent.mkdir()
        target.write_text(
            "#[test]\n#[ignore]\nfn inv_001_only_with_the_feature() {}\n",
            encoding="utf-8",
        )
        rendered = ", ".join(f'"{feature}"' for feature in features)
        write_suite_manifest(
            self.root,
            'name = "fixture"\n'
            'package = "fixture"\n'
            f"features = [{rendered}]\n"
            "shards = 1\n",
        )

    def test_target_missing_its_required_features_is_not_enforcement(self) -> None:
        # Cargo skips such a target and reports success, so crediting it would
        # claim CI enforces an invariant nothing runs.
        self.declare_gated_target()

        self.assertNotIn("| INV-001", render_invariant_index(self.root))

    def test_target_with_its_required_features_is_enforcement(self) -> None:
        self.declare_gated_target("gated")

        self.assertIn("| INV-001", render_invariant_index(self.root))

    def declare_fixture_package(self) -> None:
        """Give the fixture root one workspace package the manifest can name."""
        (self.root / "Cargo.toml").write_text(
            '[package]\nname = "fixture"\nversion = "0.0.0"\n',
            encoding="utf-8",
        )
        (self.root / "src/lib.rs").write_text("", encoding="utf-8")

    def test_agreeing_manifest_and_workflow_pass(self) -> None:
        self.declare_fixture_package()
        write_suite_manifest(
            self.root, 'name = "fixture"\npackage = "fixture"\nshards = 2\n'
        )
        write_manifest_workflow(self.root, "fixture")

        self.assertEqual(run_checks(self.root), [])

    def test_suite_without_a_workflow_artifact_fails(self) -> None:
        self.declare_fixture_package()
        write_suite_manifest(
            self.root, 'name = "fixture"\npackage = "fixture"\nshards = 1\n'
        )
        write_manifest_workflow(self.root)

        failures = run_checks(self.root)

        self.assertEqual(failure_categories(failures), ["suite-manifest"])
        self.assertIn("postgres-integration-archive-fixture", failures[0].message)

    def test_workflow_artifact_without_a_suite_fails(self) -> None:
        self.declare_fixture_package()
        write_suite_manifest(
            self.root, 'name = "fixture"\npackage = "fixture"\nshards = 1\n'
        )
        write_manifest_workflow(self.root, "fixture", "phantom")

        failures = run_checks(self.root)

        self.assertEqual(failure_categories(failures), ["suite-manifest"])
        self.assertIn("phantom", failures[0].message)

    def test_workflow_that_stops_reading_the_manifest_fails(self) -> None:
        self.declare_fixture_package()
        write_suite_manifest(
            self.root, 'name = "fixture"\npackage = "fixture"\nshards = 1\n'
        )
        workflow = self.root / ".github/workflows/rust.yml"
        workflow.parent.mkdir(parents=True, exist_ok=True)
        workflow.write_text(
            "jobs:\n"
            "  postgres-integration-run:\n"
            "    runs-on: signalbox-docker\n"
            "    steps:\n"
            "      - env:\n"
            "          SUITE: ${{ matrix.suite }}\n"
            "          PARTITION: ${{ matrix.partition }}\n"
            "          PARTITIONS: ${{ matrix.partitions }}\n"
            "          FILTER: ${{ matrix.filter }}\n"
            '        run: cargo nextest run --archive-file "$RUNNER_TEMP/$SUITE.z"'
            ' --partition "count:$PARTITION/$PARTITIONS"'
            ' --run-ignored only -E "$FILTER"\n'
            "  postgres-integration-build:\n"
            "    runs-on: signalbox-docker\n"
            "    steps:\n"
            "      - uses: actions/upload-artifact@v7\n"
            "        with:\n"
            "          name: postgres-integration-archive-fixture\n"
            "          path: ${{ runner.temp }}/fixture.tar.zst\n"
            "  postgres-integration:\n"
            "    if: ${{ always() }}\n"
            "    runs-on: ubuntu-latest\n"
            "    steps:\n"
            "      - env:\n"
            "          BUILD_RESULT: "
            "${{ needs.postgres-integration-build.result }}\n"
            "          RUN_RESULT: "
            "${{ needs.postgres-integration-run.result }}\n"
            "        run: |\n"
            '          test "$BUILD_RESULT" = success\n'
            '          test "$RUN_RESULT" = success\n',
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        # One per required invocation: the build's archive plan and the run
        # job's shard matrix are separate readings of the manifest.
        self.assertEqual(
            failure_categories(failures), ["suite-manifest", "suite-manifest"]
        )
        self.assertTrue(
            all(
                "scripts/postgres_integration_suites.py" in failure.message
                for failure in failures
            )
        )

    def test_workflow_running_ignored_tests_outside_the_manifest_fails(self) -> None:
        self.declare_fixture_package()
        write_suite_manifest(
            self.root, 'name = "fixture"\npackage = "fixture"\nshards = 1\n'
        )
        write_manifest_workflow(self.root, "fixture")
        workflow = self.root / ".github/workflows/rust.yml"
        workflow.write_text(
            workflow.read_text(encoding="utf-8")
            + "      - run: cargo test -p other --tests -- --ignored\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(failure_categories(failures), ["suite-manifest"])
        self.assertIn("outside", failures[0].message)

    def test_exact_isolation_command_is_credited_as_ignored_enforcement(self) -> None:
        self.declare_fixture_package()
        write_suite_manifest(
            self.root, 'name = "fixture"\npackage = "fixture"\nshards = 1\n'
        )
        write_manifest_workflow(self.root, "fixture")
        workflow = self.root / ".github/workflows/rust.yml"
        workflow.write_text(
            workflow.read_text(encoding="utf-8")
            + "      - run: cargo test --no-fail-fast "
            "-p signalbox-file-media-processor-runtime "
            "--features test-worker --test isolation -- --ignored\n",
            encoding="utf-8",
        )

        self.assertEqual(
            ignored_test_packages(self.root),
            ["fixture", "signalbox-file-media-processor-runtime"],
        )

    def test_split_isolation_fragments_are_not_credited_as_enforcement(self) -> None:
        self.declare_fixture_package()
        write_suite_manifest(
            self.root, 'name = "fixture"\npackage = "fixture"\nshards = 1\n'
        )
        write_manifest_workflow(self.root, "fixture")
        workflow = self.root / ".github/workflows/rust.yml"
        workflow.write_text(
            workflow.read_text(encoding="utf-8")
            + "      - run: echo cargo test --no-fail-fast "
            "-p signalbox-file-media-processor-runtime\n"
            "      - run: echo --features test-worker --test isolation -- --ignored\n",
            encoding="utf-8",
        )

        self.assertEqual(ignored_test_packages(self.root), ["fixture"])

    def test_run_job_leaving_signalbox_docker_fails(self) -> None:
        self.declare_fixture_package()
        write_suite_manifest(
            self.root, 'name = "fixture"\npackage = "fixture"\nshards = 1\n'
        )
        write_manifest_workflow(self.root, "fixture")
        workflow = self.root / ".github/workflows/rust.yml"
        workflow.write_text(
            workflow.read_text(encoding="utf-8").replace(
                "  postgres-integration-run:\n"
                "    runs-on: signalbox-docker\n",
                "  postgres-integration-run:\n"
                "    runs-on: macos-latest\n",
            ),
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(failure_categories(failures), ["suite-manifest"])
        self.assertIn("macos-latest", failures[0].message)

    def test_suite_naming_an_absent_package_fails(self) -> None:
        self.declare_fixture_package()
        write_suite_manifest(
            self.root, 'name = "fixture"\npackage = "absent"\nshards = 1\n'
        )
        write_manifest_workflow(self.root, "fixture")

        failures = run_checks(self.root)

        self.assertEqual(failure_categories(failures), ["suite-manifest"])
        self.assertIn("absent", failures[0].message)

    def test_malformed_manifest_is_a_hard_input_error(self) -> None:
        self.declare_fixture_package()
        write_suite_manifest(
            self.root, 'name = "fixture"\npackage = "fixture"\nshards = 0\n'
        )
        write_manifest_workflow(self.root, "fixture")

        with self.assertRaises(ManifestError) as raised:
            run_checks(self.root)

        self.assertIn("shards", str(raised.exception))

    def test_documentation_naming_other_features_for_a_suite_fails(self) -> None:
        self.declare_fixture_package()
        write_suite_manifest(
            self.root,
            'name = "fixture"\n'
            'package = "fixture"\n'
            'features = ["postgres-integration"]\n'
            "shards = 1\n",
        )
        write_manifest_workflow(self.root, "fixture")
        agents = self.root / "AGENTS.md"
        agents.write_text(
            agents.read_text(encoding="utf-8")
            + "\nRun `cargo test -p fixture --features stale --tests -- --ignored`.\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(failure_categories(failures), ["suite-manifest"])
        self.assertIn("stale", failures[0].message)

    def test_documentation_matching_the_manifest_passes(self) -> None:
        self.declare_fixture_package()
        write_suite_manifest(
            self.root,
            'name = "fixture"\n'
            'package = "fixture"\n'
            'features = ["postgres-integration"]\n'
            "shards = 1\n",
        )
        write_manifest_workflow(self.root, "fixture")
        agents = self.root / "AGENTS.md"
        agents.write_text(
            agents.read_text(encoding="utf-8")
            + "\nRun `cargo test -p fixture --features postgres-integration"
            " --tests -- --ignored`.\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_workflow_without_a_manifest_fails(self) -> None:
        self.declare_fixture_package()
        write_manifest_workflow(self.root, "fixture")

        failures = run_checks(self.root)

        self.assertEqual(failure_categories(failures), ["suite-manifest"])
        self.assertIn(
            ".github/postgres-integration-suites.toml", failures[0].message
        )

    def test_manifest_without_a_workflow_fails(self) -> None:
        self.declare_fixture_package()
        write_suite_manifest(
            self.root, 'name = "fixture"\npackage = "fixture"\nshards = 1\n'
        )

        failures = run_checks(self.root)

        self.assertEqual(failure_categories(failures), ["suite-manifest"])
        self.assertIn(".github/workflows/rust.yml", failures[0].message)

    def test_windows_only_test_does_not_register_invariant_enforcement(self) -> None:
        disabled = self.root / "src/windows.rs"
        disabled.write_text(
            "#[cfg(windows)]\n"
            "#[test]\n"
            "fn inv_001_windows_only_test() {}\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])
        self.assertNotIn(
            disabled.relative_to(self.root).as_posix(),
            render_invariant_index(self.root),
        )

    def test_linux_only_test_registers_invariant_enforcement(self) -> None:
        enabled = self.root / "src/linux.rs"
        enabled.write_text(
            '#[cfg(target_os = "linux")]\n'
            "#[test]\n"
            "fn inv_001_linux_only_test() {}\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(failure_categories(failures), ["invariant-registration"])
        self.assertIn(enabled.relative_to(self.root).as_posix(), failures[0].message)

    def test_unrelated_test_attribute_does_not_register_invariant(self) -> None:
        (self.root / "src/ignored.rs").write_text(
            '#[ignore = "INV-001 is temporarily flaky"]\n'
            "#[test]\n"
            "fn generically_named_test() {}\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_tagged_enforcement_requires_tagged_test_declaration(self) -> None:
        (self.root / "src/tests.rs").write_text(
            'const NOTE: &str = "INV-001";\n'
            "// INV-001 is not attached to the test.\n"
            "#[test]\n"
            "fn untagged_test() {}\n",
            encoding="utf-8",
        )
        (self.root / "docs/invariants.md").write_text(
            "# Invariants\n\n"
            "| ID | Invariant | Class | Status | Enforcement |\n"
            "| -- | -- | -- | -- | -- |\n"
            "| INV-001 | Law. | Domain | Accepted | INV-001-tagged tests in "
            "[`src/tests.rs`](../src/tests.rs). |\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(failure_categories(failures), ["invariant-tag"])
        self.assertIn("test name or attached doc comment", failures[0].message)

    def test_generated_index_link_requires_tagged_test_declaration(
        self,
    ) -> None:
        (self.root / "src/tests.rs").write_text(
            "#[test]\nfn untagged_test() {}\n", encoding="utf-8"
        )
        (self.root / "docs/invariants.md").write_text(
            "# Invariants\n\n"
            "| ID | Enforcement |\n"
            "| -- | -- |\n"
            "| INV-001 | [`src/tests.rs`](../src/tests.rs) |\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(failure_categories(failures), ["invariant-tag"])
        self.assertIn("test name or attached doc comment", failures[0].message)

    def test_tagged_link_label_requires_tagged_test_declaration(self) -> None:
        (self.root / "src/tests.rs").write_text(
            "#[test]\n"
            "fn untagged_test() {}\n",
            encoding="utf-8",
        )
        (self.root / "docs/invariants.md").write_text(
            "# Invariants\n\n"
            "| ID | Invariant | Class | Status | Enforcement |\n"
            "| -- | -- | -- | -- | -- |\n"
            "| INV-001 | Law. | Domain | Accepted | "
            "[INV-001-tagged tests](../src/tests.rs). |\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(failure_categories(failures), ["invariant-tag"])
        self.assertIn("test name or attached doc comment", failures[0].message)

    def test_rust_tag_scan_is_cached_per_source_file(self) -> None:
        (self.root / "src/tests.rs").write_text(
            "#[test]\n"
            "fn s01_inv_001_inv_002_shared_file() {}\n",
            encoding="utf-8",
        )
        (self.root / "docs/invariants.md").write_text(
            "# Invariants\n\n"
            "| ID | Invariant | Class | Status | Enforcement |\n"
            "| -- | -- | -- | -- | -- |\n"
            "| INV-001 | First law. | Domain | Accepted | INV-001-tagged "
            "tests in [`src/tests.rs`](../src/tests.rs). |\n"
            "| INV-002 | Second law. | Domain | Accepted | INV-002-tagged "
            "tests in [`src/tests.rs`](../src/tests.rs). |\n",
            encoding="utf-8",
        )

        with patch.object(
            check_docs_consistency,
            "rust_test_invariant_tags",
            wraps=check_docs_consistency.rust_test_invariant_tags,
        ) as tag_scan:
            failures = run_checks(self.root)

        self.assertEqual(failures, [])
        self.assertEqual(tag_scan.call_count, 1)

    def test_reverse_discovers_doc_tag_across_comment_gap(self) -> None:
        (self.root / "src/uncited.rs").write_text(
            "/// INV-001: attached despite the gap.\n"
            "// The ordinary comment and blank line remain trivia.\n"
            "\n"
            "#[test]\n"
            "fn generically_named_test() {}\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["invariant-registration"],
        )
        self.assertIn("src/uncited.rs", failures[0].message)

    def test_ordinary_comment_cannot_supply_test_attribute(self) -> None:
        (self.root / "src/context.rs").write_text(
            "/// INV-001 is production context, not a test binding.\n"
            "// #[test]\n"
            "fn production_context() {}\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_ordinary_block_comment_cannot_supply_doc_tag(self) -> None:
        (self.root / "src/tests.rs").write_text(
            "/*\n"
            "/// INV-001 is ordinary block-comment text.\n"
            "*/\n"
            "#[test]\n"
            "fn named_test() {}\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_reverse_discovers_invariant_tag_on_const_test(self) -> None:
        (self.root / "src/uncited.rs").write_text(
            "#[test]\n"
            "const fn s01_inv_001_const_test() {}\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["invariant-registration"],
        )
        self.assertIn("src/uncited.rs", failures[0].message)

    def test_reverse_discovers_invariant_tag_on_extern_test(self) -> None:
        (self.root / "src/uncited.rs").write_text(
            "#[test]\n"
            'extern "C" fn s01_inv_001_extern_test() {}\n',
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["invariant-registration"],
        )
        self.assertIn("src/uncited.rs", failures[0].message)

    def test_reverse_discovers_invariant_tag_on_raw_identifier_test(self) -> None:
        (self.root / "src/uncited.rs").write_text(
            "#[test]\n"
            "fn r#s01_inv_001_raw_test() {}\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["invariant-registration"],
        )
        self.assertIn("src/uncited.rs", failures[0].message)

    def test_reverse_discovers_unicode_invariant_test_identifier(self) -> None:
        (self.root / "src/uncited.rs").write_text(
            "#[test]\n"
            "fn café_inv_001_executes() {}\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["invariant-registration"],
        )
        self.assertIn("src/uncited.rs", failures[0].message)

    def test_reverse_discovers_root_qualified_test_attribute(self) -> None:
        (self.root / "src/uncited.rs").write_text(
            "#[::tokio::test]\n"
            "async fn s01_inv_001_root_qualified_test() {}\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["invariant-registration"],
        )
        self.assertIn("src/uncited.rs", failures[0].message)

    def test_invariant_tag_requires_identifier_boundary(self) -> None:
        (self.root / "src/context.rs").write_text(
            "#[test]\n"
            "fn inv_001alpha_is_not_a_tag() {}\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_inline_module_name_does_not_supply_invariant_tag(self) -> None:
        (self.root / "src/uncited.rs").write_text(
            "mod inv_001 {\n"
            "    #[test]\n"
            "    fn rejects() {}\n"
            "}\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_reverse_discovers_local_tag_through_out_of_line_module(self) -> None:
        (self.root / "src/uncited_root.rs").write_text(
            '#[path = "generic.rs"]\nmod inv_001;\n', encoding="utf-8"
        )
        (self.root / "src/generic.rs").write_text(
            "#[test]\nfn inv_001_rejects() {}\n", encoding="utf-8"
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["invariant-registration"],
        )
        self.assertIn("src/generic.rs", failures[0].message)

    def test_reverse_discovers_local_tag_through_nested_module(self) -> None:
        (self.root / "src/uncited_root.rs").write_text(
            "mod inv_001;\n", encoding="utf-8"
        )
        (self.root / "src/inv_001").mkdir()
        (self.root / "src/inv_001/mod.rs").write_text(
            "mod deeper;\n", encoding="utf-8"
        )
        (self.root / "src/inv_001/deeper.rs").write_text(
            "#[test]\nfn inv_001_rejects() {}\n", encoding="utf-8"
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["invariant-registration"],
        )
        self.assertIn("src/inv_001/deeper.rs", failures[0].message)

    def test_reverse_discovers_aliased_test_attribute(self) -> None:
        (self.root / "src/uncited.rs").write_text(
            "use tokio::test as async_test;\n\n"
            "#[async_test]\n"
            "async fn s01_inv_001_aliased_attribute() {}\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["invariant-registration"],
        )
        self.assertIn("src/uncited.rs", failures[0].message)

    def test_renaming_a_non_test_import_declares_no_test(self) -> None:
        (self.root / "src/context.rs").write_text(
            "use crate::support::latest as newest;\n\n"
            "#[newest]\n"
            "fn s01_inv_001_not_a_test() {}\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_reverse_discovers_cfg_attr_test(self) -> None:
        (self.root / "src/uncited.rs").write_text(
            "#[cfg_attr(test, test)]\n"
            "fn s01_inv_001_cfg_attr_test() {}\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["invariant-registration"],
        )
        self.assertIn("src/uncited.rs", failures[0].message)

    def test_reverse_discovers_nested_cfg_attr_test(self) -> None:
        (self.root / "src/uncited.rs").write_text(
            "#[cfg_attr(test, cfg_attr(all(), test))]\n"
            "fn s01_inv_001_nested_cfg_attr_test() {}\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["invariant-registration"],
        )
        self.assertIn("src/uncited.rs", failures[0].message)

    def test_cfg_disabled_functions_do_not_register_invariants(self) -> None:
        (self.root / "src/context.rs").write_text(
            "#[cfg(any())]\n"
            "#[test]\n"
            "fn s01_inv_001_disabled_test() {}\n"
            "#[cfg_attr(any(), test)]\n"
            "fn s01_inv_001_disabled_cfg_attr_test() {}\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_active_cfg_attr_can_disable_test_declaration(self) -> None:
        (self.root / "src/context.rs").write_text(
            "#[cfg_attr(all(), cfg(any()), test)]\n"
            "fn s01_inv_001_never_exists() {}\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_test_generating_macro_is_rejected(self) -> None:
        (self.root / "src/generated.rs").write_text(
            "macro_rules! invariant_test {\n"
            "    ($name:ident) => {\n"
            "        #[test]\n"
            "        fn $name() {}\n"
            "    };\n"
            "}\n"
            "invariant_test!(s01_inv_001_generated);\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["invariant-test-generation"],
        )
        self.assertIn(
            "`macro_rules! invariant_test` emits or forwards a test attribute",
            failures[0].message,
        )

    def test_attribute_forwarding_macro_is_rejected(self) -> None:
        (self.root / "src/generated.rs").write_text(
            "macro_rules! invariant_test {\n"
            "    ($attr:meta, $name:ident) => {\n"
            "        #[$attr]\n"
            "        fn $name() {}\n"
            "    };\n"
            "}\n"
            "invariant_test!(test, s01_inv_001_generated);\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["invariant-test-generation"],
        )
        self.assertIn(
            "`macro_rules! invariant_test` emits or forwards a test attribute",
            failures[0].message,
        )

    def test_non_test_attribute_forwarding_macro_is_allowed(self) -> None:
        (self.root / "src/generated.rs").write_text(
            "macro_rules! documented {\n"
            "    ($attr:meta) => {\n"
            "        #[$attr]\n"
            "        struct Item;\n"
            "    };\n"
            "}\n"
            "documented!(derive(Clone));\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_test_named_argument_outside_the_forwarded_position_is_allowed(
        self,
    ) -> None:
        (self.root / "src/generated.rs").write_text(
            "macro_rules! named {\n"
            "    ($name:ident, $attr:meta) => {\n"
            "        #[$attr]\n"
            "        fn $name() {}\n"
            "    };\n"
            "}\n"
            "named!(test, allow(dead_code));\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_test_metadata_in_the_forwarded_position_is_rejected(self) -> None:
        (self.root / "src/generated.rs").write_text(
            "macro_rules! named {\n"
            "    ($name:ident, $attr:meta) => {\n"
            "        #[$attr]\n"
            "        fn $name() {}\n"
            "    };\n"
            "}\n"
            "named!(s01_inv_001_generated, test);\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["invariant-test-generation"],
        )
        self.assertIn(
            "`macro_rules! named` emits or forwards a test attribute",
            failures[0].message,
        )

    def test_non_comma_forwarding_matcher_is_rejected(self) -> None:
        (self.root / "src/generated.rs").write_text(
            "macro_rules! forwarded {\n"
            "    ($name:ident => $attr:meta) => {\n"
            "        #[$attr]\n"
            "        fn $name() {}\n"
            "    };\n"
            "}\n"
            "forwarded!(s01_inv_001_generated => test);\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["invariant-test-generation"],
        )
        self.assertIn(
            "`macro_rules! forwarded` emits or forwards a test attribute",
            failures[0].message,
        )

    def test_unknown_binding_keeps_nested_metadata_nested(self) -> None:
        (self.root / "src/generated.rs").write_text(
            "macro_rules! documented {\n"
            "    ($name:ident => $attr:meta) => {\n"
            "        #[$attr]\n"
            "        struct $name;\n"
            "    };\n"
            "}\n"
            "documented!(Item => cfg_attr(test, derive(Clone)));\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_repeated_forwarding_matcher_inspects_every_argument(self) -> None:
        (self.root / "src/generated.rs").write_text(
            "macro_rules! many {\n"
            "    ($name:ident, $($attr:meta),*) => {\n"
            "        $(#[$attr])*\n"
            "        fn $name() {}\n"
            "    };\n"
            "}\n"
            "many!(s01_inv_001_generated, test);\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["invariant-test-generation"],
        )
        self.assertIn(
            "`macro_rules! many` emits or forwards a test attribute",
            failures[0].message,
        )

    def test_cfg_predicate_test_token_does_not_mark_function_as_test(self) -> None:
        (self.root / "src/context.rs").write_text(
            "#[cfg_attr(any(unix, test), allow(dead_code))]\n"
            "fn s01_inv_001_production_context() {}\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_test_declaration_inside_comment_is_ignored(self) -> None:
        (self.root / "src/context.rs").write_text(
            "/*\n"
            "#[test]\n"
            "fn s01_inv_001_commented_example() {}\n"
            "*/\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_test_declarations_inside_string_literals_are_ignored(self) -> None:
        (self.root / "src/context.rs").write_text(
            "const RAW_EXAMPLE: &str = r###\"\n"
            "#[test]\n"
            "fn s01_inv_001_raw_string_example() {}\n"
            "\"###;\n"
            "const STRING_EXAMPLE: &str = \"#[test]\\nfn "
            "s01_inv_001_string_example() {}\";\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_test_declaration_inside_raw_c_string_is_ignored(self) -> None:
        (self.root / "src/context.rs").write_text(
            "const RAW_C_EXAMPLE: &CStr = cr###\"prefix \\\"\n"
            "#[test]\n"
            "fn s01_inv_001_raw_c_string_example() {}\n"
            "\"###;\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_lifetime_does_not_mask_the_rest_of_its_line(self) -> None:
        (self.root / "src/uncited.rs").write_text(
            "mod inv_001 {\n"
            "    fn helper<'a>() -> char { let c = 'x'; c }\n"
            "    #[test]\n"
            "    fn inv_001_rejects() {}\n"
            "}\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["invariant-registration"],
        )
        self.assertIn("src/uncited.rs", failures[0].message)

    def test_loop_label_does_not_mask_the_rest_of_its_line(self) -> None:
        (self.root / "src/uncited.rs").write_text(
            "mod inv_001 {\n"
            "    fn helper() { 'outer: loop { let c = 'x'; break 'outer; } }\n"
            "    #[test]\n"
            "    fn inv_001_rejects() {}\n"
            "}\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["invariant-registration"],
        )
        self.assertIn("src/uncited.rs", failures[0].message)

    def test_spaced_attribute_declares_a_test(self) -> None:
        (self.root / "src/uncited.rs").write_text(
            "# [ test ]\nfn s01_inv_001_spaced_attribute() {}\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["invariant-registration"],
        )
        self.assertIn("src/uncited.rs", failures[0].message)

    def test_reverse_discovers_decomposed_unicode_identifier(self) -> None:
        (self.root / "src/uncited.rs").write_text(
            "#[test]\nfn café_inv_001_executes() {}\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["invariant-registration"],
        )
        self.assertIn("src/uncited.rs", failures[0].message)

    def test_cfg_disabled_out_of_line_module_declares_no_prefix(self) -> None:
        (self.root / "src/uncited_root.rs").write_text(
            '#[cfg(any())]\n#[path = "generic.rs"]\nmod inv_001;\n',
            encoding="utf-8",
        )
        (self.root / "src/generic.rs").write_text(
            "#[test]\nfn rejects() {}\n", encoding="utf-8"
        )

        self.assertEqual(run_checks(self.root), [])

    def test_out_of_line_module_names_do_not_supply_tags(self) -> None:
        (self.root / "src/uncited_root.rs").write_text(
            '#[path = "generic.rs"]\nmod ordinary;\n'
            '#[path = "generic.rs"]\nmod inv_001;\n',
            encoding="utf-8",
        )
        (self.root / "src/generic.rs").write_text(
            "#[test]\nfn rejects() {}\n", encoding="utf-8"
        )

        self.assertEqual(run_checks(self.root), [])

    def test_inline_module_names_do_not_supply_tags(self) -> None:
        (self.root / "src/ordinary.rs").write_text(
            "mod inv_999 {\n    #[test]\n    fn ordinary_name() {}\n}\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_matcher_only_test_attribute_generates_no_test(self) -> None:
        (self.root / "src/generated.rs").write_text(
            "macro_rules! strip_test {\n"
            "    (#[test] $item:item) => { $item };\n"
            "}\n"
            "strip_test!(#[test] fn ignored() {});\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_missing_git_fails_before_validation(self) -> None:
        empty_path = self.root / ".empty-path"
        empty_path.mkdir()
        run_git(self.root, "add", "-A")

        with patch.dict(os.environ, {"PATH": str(empty_path)}):
            with self.assertRaisesRegex(TrackedFilesError, "unavailable"):
                run_docs_checks(self.root)

    def test_disabled_inline_module_declares_no_test(self) -> None:
        (self.root / "src/context.rs").write_text(
            "#[cfg(any())]\n"
            "mod dead {\n"
            "    #[test]\n"
            "    fn s01_inv_001_never_built() {}\n"
            "}\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_enabled_inline_module_still_declares_its_test(self) -> None:
        (self.root / "src/uncited.rs").write_text(
            "#[cfg(all())]\n"
            "mod live {\n"
            "    #[test]\n"
            "    fn s01_inv_001_built() {}\n"
            "}\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["invariant-registration"],
        )
        self.assertIn("src/uncited.rs", failures[0].message)

    def test_test_alias_does_not_escape_its_module(self) -> None:
        (self.root / "src/context.rs").write_text(
            "mod first {\n"
            "    use tokio::test as scoped;\n"
            "    #[scoped]\n"
            "    async fn ordinary_alias_test() {}\n"
            "}\n"
            "mod second {\n"
            "    use crate::support::marker as scoped;\n"
            "    #[scoped]\n"
            "    fn s01_inv_001_not_a_test() {}\n"
            "}\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_same_line_attribute_declares_a_test(self) -> None:
        (self.root / "src/uncited.rs").write_text(
            "#[test] fn s01_inv_001_same_line() {}\n", encoding="utf-8"
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["invariant-registration"],
        )
        self.assertIn("src/uncited.rs", failures[0].message)

    def test_raw_identifier_test_attribute_declares_a_test(self) -> None:
        (self.root / "src/uncited.rs").write_text(
            "#[r#test]\nfn s01_inv_001_raw_attribute() {}\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["invariant-registration"],
        )
        self.assertIn("src/uncited.rs", failures[0].message)

    def test_cyclic_module_declarations_terminate(self) -> None:
        (self.root / "src/uncited_root.rs").write_text(
            "mod first;\n", encoding="utf-8"
        )
        (self.root / "src/first.rs").write_text(
            '#[path = "second.rs"]\nmod inv_001;\n', encoding="utf-8"
        )
        (self.root / "src/second.rs").write_text(
            '#[path = "first.rs"]\nmod back;\n#[test]\nfn inv_001_rejects() {}\n',
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["invariant-registration"],
        )
        self.assertIn("src/second.rs", failures[0].message)

    def test_parent_relative_module_path_resolves(self) -> None:
        (self.root / "src/uncited_root.rs").write_text(
            "mod nested;\n", encoding="utf-8"
        )
        (self.root / "src/nested.rs").write_text(
            '#[path = "../outer.rs"]\nmod inv_001;\n', encoding="utf-8"
        )
        (self.root / "outer.rs").write_text(
            "#[test]\nfn inv_001_rejects() {}\n", encoding="utf-8"
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["invariant-registration"],
        )
        self.assertIn("outer.rs", failures[0].message)

    def test_macro_invoked_from_another_file_is_rejected(self) -> None:
        (self.root / "src/macros.rs").write_text(
            "macro_rules! generate {\n"
            "    ($attr:meta) => {\n"
            "        #[$attr]\n"
            "        fn s01_inv_001_generated() {}\n"
            "    };\n"
            "}\n",
            encoding="utf-8",
        )
        (self.root / "src/caller.rs").write_text(
            "#[macro_use]\nmod macros;\ngenerate!(test);\n", encoding="utf-8"
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["invariant-test-generation"],
        )
        self.assertIn(
            "`macro_rules! generate` emits or forwards a test attribute",
            failures[0].message,
        )

    def test_non_test_macro_invoked_from_another_file_is_allowed(self) -> None:
        (self.root / "src/macros.rs").write_text(
            "macro_rules! documented {\n"
            "    ($attr:meta) => {\n"
            "        #[$attr]\n"
            "        struct Item;\n"
            "    };\n"
            "}\n",
            encoding="utf-8",
        )
        (self.root / "src/caller.rs").write_text(
            "#[macro_use]\nmod macros;\ndocumented!(derive(Clone));\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_out_of_line_module_under_an_inline_module_resolves(self) -> None:
        (self.root / "src/uncited_root.rs").write_text(
            "mod inv_001 {\n    mod cases;\n}\n", encoding="utf-8"
        )
        (self.root / "src/uncited_root/inv_001").mkdir(parents=True)
        (self.root / "src/uncited_root/inv_001/cases.rs").write_text(
            "#[test]\nfn inv_001_generic() {}\n", encoding="utf-8"
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["invariant-registration"],
        )
        self.assertIn(
            "src/uncited_root/inv_001/cases.rs", failures[0].message
        )

    def test_reexported_test_alias_reaches_the_importing_file(self) -> None:
        (self.root / "src/uncited_root.rs").write_text(
            "pub use tokio::test as async_test;\nmod cases;\n",
            encoding="utf-8",
        )
        (self.root / "src/uncited_root").mkdir()
        (self.root / "src/uncited_root/cases.rs").write_text(
            "use crate::async_test;\n\n"
            "#[async_test]\n"
            "async fn s01_inv_001_reexported_attribute() {}\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["invariant-registration"],
        )
        self.assertIn("src/uncited_root/cases.rs", failures[0].message)

    def test_renaming_to_an_alias_name_declares_no_test(self) -> None:
        (self.root / "src/exporter.rs").write_text(
            "pub use tokio::test as async_test;\nmod cases;\n",
            encoding="utf-8",
        )
        (self.root / "src/exporter").mkdir()
        (self.root / "src/exporter/cases.rs").write_text(
            "use crate::support::marker as async_test;\n\n"
            "#[async_test]\n"
            "fn s01_inv_001_not_a_test() {}\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_alias_spelling_alone_does_not_import_a_test(self) -> None:
        (self.root / "src/exporter.rs").write_text(
            "pub use tokio::test as shared;\nmod cases;\n", encoding="utf-8"
        )
        (self.root / "src/exporter").mkdir()
        (self.root / "src/exporter/cases.rs").write_text(
            "use crate::helpers::shared;\n\n"
            "#[shared]\n"
            "fn s01_inv_001_not_a_test() {}\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_duplicate_macro_names_keep_their_own_call_sites(self) -> None:
        (self.root / "src/forwarding.rs").write_text(
            "macro_rules! wrapper {\n"
            "    ($attr:meta) => {\n"
            "        #[$attr]\n"
            "        struct Item;\n"
            "    };\n"
            "}\n",
            encoding="utf-8",
        )
        (self.root / "src/naming.rs").write_text(
            "macro_rules! wrapper {\n"
            "    ($name:ident) => {\n"
            "        fn $name() {}\n"
            "    };\n"
            "}\n"
            "wrapper!(test);\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_conditional_path_attribute_selects_the_test_module(self) -> None:
        (self.root / "src/uncited_root.rs").write_text(
            '#[cfg_attr(test, path = "generic.rs")]\nmod inv_001;\n',
            encoding="utf-8",
        )
        (self.root / "src/uncited_root").mkdir()
        (self.root / "src/uncited_root/inv_001.rs").write_text(
            "pub fn ordinary() {}\n", encoding="utf-8"
        )
        (self.root / "src/uncited_root/generic.rs").write_text(
            "#[test]\nfn inv_001_generic() {}\n", encoding="utf-8"
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["invariant-registration"],
        )
        self.assertIn("src/uncited_root/generic.rs", failures[0].message)

    def test_every_conditional_path_alternative_is_followed(self) -> None:
        (self.root / "src/uncited_root.rs").write_text(
            '#[cfg_attr(windows, path = "windows.rs")]\n'
            '#[cfg_attr(unix, path = "unix.rs")]\n'
            "mod inv_001;\n",
            encoding="utf-8",
        )
        (self.root / "src/uncited_root").mkdir()
        (self.root / "src/uncited_root/windows.rs").write_text(
            "pub fn windows() {}\n", encoding="utf-8"
        )
        (self.root / "src/uncited_root/unix.rs").write_text(
            "#[test]\nfn inv_001_generic() {}\n", encoding="utf-8"
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["invariant-registration"],
        )
        self.assertIn("src/uncited_root/unix.rs", failures[0].message)

    def test_disabled_path_alternative_is_not_followed(self) -> None:
        (self.root / "src/context_root.rs").write_text(
            '#[cfg_attr(any(), path = "dead.rs")]\n'
            '#[cfg_attr(unix, path = "live.rs")]\n'
            "mod inv_001;\n",
            encoding="utf-8",
        )
        (self.root / "src/context_root").mkdir()
        (self.root / "src/context_root/dead.rs").write_text(
            "#[test]\nfn generic() {}\n", encoding="utf-8"
        )
        (self.root / "src/context_root/live.rs").write_text(
            "pub fn live() {}\n", encoding="utf-8"
        )

        self.assertEqual(run_checks(self.root), [])

    def test_unicode_attribute_path_declares_a_test(self) -> None:
        (self.root / "src/uncited.rs").write_text(
            "#[módulo::test]\nfn s01_inv_001_unicode_path() {}\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["invariant-registration"],
        )
        self.assertIn("src/uncited.rs", failures[0].message)

    def test_grouped_crate_import_carries_the_root_alias(self) -> None:
        (self.root / "src/uncited_root.rs").write_text(
            "pub use tokio::test as async_test;\nmod cases;\n",
            encoding="utf-8",
        )
        (self.root / "src/uncited_root").mkdir()
        (self.root / "src/uncited_root/cases.rs").write_text(
            "use crate::{helpers, async_test};\n\n"
            "#[async_test]\n"
            "async fn s01_inv_001_grouped_import() {}\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["invariant-registration"],
        )
        self.assertIn("src/uncited_root/cases.rs", failures[0].message)

    def test_qualified_name_in_a_group_declares_no_test(self) -> None:
        (self.root / "src/context_root.rs").write_text(
            "pub use tokio::test as shared;\nmod cases;\n", encoding="utf-8"
        )
        (self.root / "src/context_root").mkdir()
        (self.root / "src/context_root/cases.rs").write_text(
            "use crate::{helpers::shared};\n\n"
            "#[shared]\n"
            "fn s01_inv_001_not_a_test() {}\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_forwarded_attribute_group_is_rejected(self) -> None:
        (self.root / "src/generated.rs").write_text(
            "macro_rules! many {\n"
            "    ($(#[$attr:meta])* $name:ident) => {\n"
            "        $(#[$attr])*\n"
            "        fn $name() {}\n"
            "    };\n"
            "}\n"
            "many!(#[test] s01_inv_001_generated);\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["invariant-test-generation"],
        )
        self.assertIn(
            "`macro_rules! many` emits or forwards a test attribute",
            failures[0].message,
        )

    def test_forwarded_non_test_attribute_group_is_allowed(self) -> None:
        (self.root / "src/generated.rs").write_text(
            "macro_rules! many {\n"
            "    ($(#[$attr:meta])* $name:ident) => {\n"
            "        $(#[$attr])*\n"
            "        struct $name;\n"
            "    };\n"
            "}\n"
            "many!(#[derive(Clone)] Item);\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_capitalized_attribute_declares_no_test(self) -> None:
        (self.root / "src/context.rs").write_text(
            "#[Test]\nfn s01_inv_001_not_a_test() {}\n", encoding="utf-8"
        )

        self.assertEqual(run_checks(self.root), [])

    def test_raw_string_module_path_resolves(self) -> None:
        (self.root / "src/uncited_root.rs").write_text(
            '#[path = r"generic.rs"]\nmod inv_001;\n', encoding="utf-8"
        )
        (self.root / "src/uncited_root").mkdir()
        (self.root / "src/uncited_root/generic.rs").write_text(
            "#[test]\nfn inv_001_generic() {}\n", encoding="utf-8"
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["invariant-registration"],
        )
        self.assertIn("src/uncited_root/generic.rs", failures[0].message)

    def test_included_file_carries_its_including_module_path(self) -> None:
        (self.root / "src/uncited_root.rs").write_text(
            'mod inv_001 {\n    include!("generic.rs");\n}\n',
            encoding="utf-8",
        )
        (self.root / "src/generic.rs").write_text(
            "#[test]\nfn inv_001_generic() {}\n", encoding="utf-8"
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["invariant-registration"],
        )
        self.assertIn("src/generic.rs", failures[0].message)

    def test_cfg_not_test_declaration_leaves_the_harness(self) -> None:
        (self.root / "src/context.rs").write_text(
            "#[cfg(not(test))]\n"
            "#[test]\n"
            "fn s01_inv_001_never_in_harness() {}\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_spaced_attribute_path_declares_a_test(self) -> None:
        (self.root / "src/uncited.rs").write_text(
            "#[:: tokio :: test]\n"
            "async fn s01_inv_001_spaced_path() {}\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["invariant-registration"],
        )
        self.assertIn("src/uncited.rs", failures[0].message)

    def test_qualifier_split_across_lines_declares_a_test(self) -> None:
        (self.root / "src/uncited.rs").write_text(
            "#[test] const\nfn s01_inv_001_split_qualifier() {}\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["invariant-registration"],
        )
        self.assertIn("src/uncited.rs", failures[0].message)

    def test_disabled_use_item_declares_no_test_alias(self) -> None:
        (self.root / "src/context.rs").write_text(
            "#[cfg(any())]\n"
            "use tokio::test as shared;\n\n"
            "#[shared]\n"
            "fn s01_inv_001_not_a_test() {}\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_procedural_test_generator_is_rejected(self) -> None:
        (self.root / "src/generated.rs").write_text(
            "#[proc_macro]\n"
            "pub fn generate(_: TokenStream) -> TokenStream {\n"
            '    "#[test] fn s01_inv_001_generated() {}".parse().unwrap()\n'
            "}\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["invariant-test-generation"],
        )
        self.assertIn(
            "this procedural macro spells a test attribute",
            failures[0].message,
        )

    def test_ordinary_procedural_macro_is_allowed(self) -> None:
        (self.root / "src/generated.rs").write_text(
            "#[proc_macro_derive(Thing)]\n"
            "pub fn derive_thing(_: TokenStream) -> TokenStream {\n"
            "    quote! { impl Thing for #name {} }.into()\n"
            "}\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_disabled_include_declares_no_module_path(self) -> None:
        (self.root / "src/context.rs").write_text(
            "mod inv_001 {\n"
            "    #[cfg(any())]\n"
            '    include!("generic.rs");\n'
            "}\n",
            encoding="utf-8",
        )
        (self.root / "src/generic.rs").write_text(
            "#[test]\nfn generic() {}\n", encoding="utf-8"
        )

        self.assertEqual(run_checks(self.root), [])

    def test_commented_test_attribute_generates_no_test(self) -> None:
        (self.root / "src/generated.rs").write_text(
            "#[proc_macro]\n"
            "pub fn generate(_: TokenStream) -> TokenStream {\n"
            "    // Accept input carrying #[test], but emit nothing.\n"
            "    TokenStream::new()\n"
            "}\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_source_outside_every_cargo_target_is_not_read(self) -> None:
        (self.root / "Cargo.toml").write_text(
            '[package]\nname = "fixture"\n', encoding="utf-8"
        )
        (self.root / "src/lib.rs").write_text(
            "pub fn thing() {}\n", encoding="utf-8"
        )
        (self.root / "fixtures").mkdir()
        (self.root / "fixtures/example.rs").write_text(
            "#[test]\nfn s01_inv_001_unattached() {}\n", encoding="utf-8"
        )

        self.assertEqual(run_checks(self.root), [])

    def test_source_named_by_an_explicit_target_path_is_read(self) -> None:
        (self.root / "Cargo.toml").write_text(
            '[package]\nname = "fixture"\n\n'
            '[[test]]\nname = "custom"\npath = "checks/custom.rs"\n',
            encoding="utf-8",
        )
        (self.root / "src/lib.rs").write_text(
            "pub fn thing() {}\n", encoding="utf-8"
        )
        (self.root / "checks").mkdir()
        (self.root / "checks/custom.rs").write_text(
            "#[test]\nfn s01_inv_001_custom_target() {}\n", encoding="utf-8"
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["invariant-registration"],
        )
        self.assertIn("checks/custom.rs", failures[0].message)

    def test_tagged_label_claims_only_its_own_link(self) -> None:
        (self.root / "docs/invariants.md").write_text(
            "# Invariants\n\n"
            "| ID | Invariant | Class | Status | Enforcement |\n"
            "| -- | -- | -- | -- | -- |\n"
            "| INV-001 | Law. | Domain | Accepted | "
            "[INV-001-tagged test](../src/tests.rs) and its "
            "[helper](../src/helper.rs). |\n",
            encoding="utf-8",
        )
        (self.root / "src/tests.rs").write_text(
            "#[test]\nfn s01_inv_001_named_test() {}\n", encoding="utf-8"
        )
        (self.root / "src/helper.rs").write_text(
            "pub fn helper() {}\n", encoding="utf-8"
        )

        self.assertEqual(run_checks(self.root), [])

    def test_reference_style_citation_keeps_occurrence_order(self) -> None:
        (self.root / "src/tagged.rs").write_text(
            "#[test]\nfn s01_inv_001_tagged_test() {}\n",
            encoding="utf-8",
        )
        (self.root / "src/named.rs").write_text(
            "#[test]\nfn named_test() {}\n",
            encoding="utf-8",
        )
        (self.root / "docs/invariants.md").write_text(
            "# Invariants\n\n"
            "| ID | Invariant | Class | Status | Enforcement |\n"
            "| -- | -- | -- | -- | -- |\n"
            "| INV-001 | Law. | Domain | Accepted | INV-001-tagged tests in "
            "[`src/tagged.rs`][tagged]. Named tests in "
            "[`src/named.rs`][named]. |\n\n"
            "[named]: ../src/named.rs\n"
            "[tagged]: ../src/tagged.rs\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_tagged_claim_applies_only_to_following_link_group(self) -> None:
        (self.root / "src/named.rs").write_text(
            "#[test]\nfn named_test() {}\n",
            encoding="utf-8",
        )
        (self.root / "src/tagged.rs").write_text(
            "#[test]\nfn s01_inv_001_tagged_test() {}\n",
            encoding="utf-8",
        )
        (self.root / "docs/invariants.md").write_text(
            "# Invariants\n\n"
            "| ID | Invariant | Class | Status | Enforcement |\n"
            "| -- | -- | -- | -- | -- |\n"
            "| INV-001 | Law. | Domain | Accepted | test `named_test` in "
            "[`src/named.rs`](../src/named.rs); INV-001-tagged tests in "
            "[`src/tagged.rs`](../src/tagged.rs). |\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_non_test_invariant_mentions_do_not_require_registration(self) -> None:
        (self.root / "src/context.rs").write_text(
            "/// INV-001 is production context, not a test binding.\n"
            "fn production_context() {}\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_missing_invariant_file_is_not_double_reported_as_link(self) -> None:
        (self.root / "docs/invariants.md").write_text(
            "# Invariants\n\n"
            "| ID | Invariant | Class | Status | Enforcement |\n"
            "| -- | -- | -- | -- | -- |\n"
            "| INV-001 | Law. | Domain | Accepted | "
            "[`src/missing.rs`](../src/missing.rs). |\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["invariant-citation"],
        )
        self.assertIn("cited file does not exist", failures[0].message)

    def test_missing_reference_invariant_file_is_not_double_reported(self) -> None:
        (self.root / "docs/invariants.md").write_text(
            "# Invariants\n\n"
            "| ID | Invariant | Class | Status | Enforcement |\n"
            "| -- | -- | -- | -- | -- |\n"
            "| INV-001 | Law. | Domain | Accepted | "
            "[`src/missing.rs`][missing]. |\n\n"
            "[missing]: ../src/missing.rs\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["invariant-citation"],
        )
        self.assertIn("cited file does not exist", failures[0].message)

    def test_missing_invariant_file_fragment_is_not_double_reported(self) -> None:
        (self.root / "docs/invariants.md").write_text(
            "# Invariants\n\n"
            "| ID | Invariant | Class | Status | Enforcement |\n"
            "| -- | -- | -- | -- | -- |\n"
            "| INV-001 | Law. | Domain | Accepted | "
            "[`src/missing.rs`](../src/missing.rs#L1). |\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["invariant-citation"],
        )
        self.assertIn("cited file does not exist", failures[0].message)

    def test_named_test_must_appear_in_its_cited_file(self) -> None:
        (self.root / "docs/invariants.md").write_text(
            "# Invariants\n\n"
            "| ID | Invariant | Class | Status | Enforcement |\n"
            "| -- | -- | -- | -- | -- |\n"
            "| INV-001 | Law. | Domain | Accepted | "
            "tests `module::missing_test` in "
            "[`src/tests.rs`](../src/tests.rs). |\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["invariant-test"],
        )
        self.assertIn("`missing_test`", failures[0].message)

    def test_named_test_with_balanced_destination_must_appear(self) -> None:
        (self.root / "src/tests(foo).rs").write_text(
            "#[test]\nfn existing_test() {}\n",
            encoding="utf-8",
        )
        (self.root / "docs/invariants.md").write_text(
            "# Invariants\n\n"
            "| ID | Invariant | Class | Status | Enforcement |\n"
            "| -- | -- | -- | -- | -- |\n"
            "| INV-001 | Law. | Domain | Accepted | "
            "tests `missing_test` in "
            "[`src/tests(foo).rs`](../src/tests(foo).rs). |\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["invariant-test"],
        )
        self.assertIn("`missing_test`", failures[0].message)

    def test_named_test_behind_reference_link_must_appear(self) -> None:
        (self.root / "docs/invariants.md").write_text(
            "# Invariants\n\n"
            "| ID | Invariant | Class | Status | Enforcement |\n"
            "| -- | -- | -- | -- | -- |\n"
            "| INV-001 | Law. | Domain | Accepted | "
            "tests `missing_test` in [tests][tests-file]. |\n\n"
            "[tests-file]: ../src/tests.rs\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["invariant-test"],
        )
        self.assertIn("`missing_test`", failures[0].message)

    def test_named_test_behind_shortcut_reference_must_appear(self) -> None:
        (self.root / "docs/invariants.md").write_text(
            "# Invariants\n\n"
            "| ID | Invariant | Class | Status | Enforcement |\n"
            "| -- | -- | -- | -- | -- |\n"
            "| INV-001 | Law. | Domain | Accepted | "
            "tests `missing_test` in [tests]. |\n\n"
            "[tests]: ../src/tests.rs\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["invariant-test"],
        )
        self.assertIn("`missing_test`", failures[0].message)

    def test_escaped_reference_label_binds_its_named_test(self) -> None:
        (self.root / "docs/invariants.md").write_text(
            "# Invariants\n\n"
            "| ID | Invariant | Class | Status | Enforcement |\n"
            "| -- | -- | -- | -- | -- |\n"
            "| INV-001 | Law. | Domain | Accepted | "
            "tests `missing_test` in [tests][tests\\-file]. |\n\n"
            "[tests-file]: ../src/tests.rs\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["invariant-test"],
        )
        self.assertIn("`missing_test`", failures[0].message)

    def test_natural_named_test_must_appear_in_its_cited_file(self) -> None:
        (self.root / "docs/invariants.md").write_text(
            "# Invariants\n\n"
            "| ID | Invariant | Class | Status | Enforcement |\n"
            "| -- | -- | -- | -- | -- |\n"
            "| INV-001 | Law. | Domain | Accepted | "
            "the `missing_test` test in "
            "[`src/tests.rs`](../src/tests.rs). |\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["invariant-test"],
        )
        self.assertIn("`missing_test`", failures[0].message)

    def test_natural_named_tests_bind_to_their_own_cited_files(self) -> None:
        (self.root / "src/other_tests.rs").write_text(
            "#[test]\nfn existing_test() {}\n", encoding="utf-8"
        )
        (self.root / "docs/invariants.md").write_text(
            "# Invariants\n\n"
            "| ID | Invariant | Class | Status | Enforcement |\n"
            "| -- | -- | -- | -- | -- |\n"
            "| INV-001 | Law. | Domain | Accepted | "
            "the `missing_one` test in "
            "[`src/tests.rs`](../src/tests.rs) and the `missing_two` test in "
            "[`src/other_tests.rs`](../src/other_tests.rs). |\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["invariant-test", "invariant-test"],
        )
        self.assertIn("`missing_one`", failures[0].message)
        self.assertIn("`missing_two`", failures[1].message)

    def test_natural_test_is_not_bound_to_unrelated_context_link(self) -> None:
        (self.root / "src/context.rs").write_text(
            "// Context only.\n", encoding="utf-8"
        )
        (self.root / "docs/invariants.md").write_text(
            "# Invariants\n\n"
            "| ID | Invariant | Class | Status | Enforcement |\n"
            "| -- | -- | -- | -- | -- |\n"
            "| INV-001 | Law. | Domain | Accepted | "
            "the `missing_test` test and context in "
            "[`src/context.rs`](../src/context.rs). |\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_code_form_link_label_requires_test_context(self) -> None:
        (self.root / "docs/invariants.md").write_text(
            "# Invariants\n\n"
            "| ID | Invariant | Class | Status | Enforcement |\n"
            "| -- | -- | -- | -- | -- |\n"
            "| INV-001 | Law. | Domain | Accepted | "
            "[`codec`](../src/tests.rs). |\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_code_span_in_enforcement_cell_does_not_expose_link(self) -> None:
        (self.root / "docs/invariants.md").write_text(
            "# Invariants\n\n"
            "| ID | Invariant | Class | Status | Enforcement |\n"
            "| -- | -- | -- | -- | -- |\n"
            "| INV-001 | Law. | Domain | Accepted | "
            "`example [not a citation](../src/missing.rs)`. |\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_missing_heading_anchor_is_reported(self) -> None:
        (self.root / "AGENTS.md").write_text(
            "# Agent guidance\n\n"
            "[Missing](docs/spec/example.md#not-a-heading)\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["relative-link"],
        )
        self.assertIn("anchor `#not-a-heading`", failures[0].message)

    def test_link_may_not_escape_the_repository(self) -> None:
        (self.root / "AGENTS.md").write_text(
            "# Agent guidance\n\n[Escape](../outside.md)\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["relative-link"],
        )
        self.assertIn("escapes the repository", failures[0].message)

    def test_even_backslash_run_does_not_escape_link_opener(self) -> None:
        (self.root / "AGENTS.md").write_text(
            "# Agent guidance\n\n"
            "\\[Ignored](ignored.md)\n"
            "\\\\[Missing](missing.md)\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["relative-link"],
        )
        self.assertIn("`missing.md`", failures[0].message)

    def test_escaped_reference_label_target_is_checked(self) -> None:
        (self.root / "AGENTS.md").write_text(
            "# Agent guidance\n\n[broken\\]]: missing.md\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["relative-link"],
        )
        self.assertIn("`missing.md`", failures[0].message)

    def test_block_quote_reference_definition_target_is_checked(self) -> None:
        (self.root / "AGENTS.md").write_text(
            "# Agent guidance\n\n"
            "> [Missing][ref]\n"
            ">\n"
            "> [ref]: missing.md\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["relative-link"],
        )
        self.assertIn("`missing.md`", failures[0].message)

    def test_list_reference_definition_target_is_checked(self) -> None:
        (self.root / "AGENTS.md").write_text(
            "# Agent guidance\n\n"
            "- [ref]: missing.md\n\n"
            "[Missing][ref]\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["relative-link"],
        )
        self.assertIn("`missing.md`", failures[0].message)

    def test_malformed_reference_definition_title_tail_is_ignored(self) -> None:
        (self.root / "AGENTS.md").write_text(
            "# Agent guidance\n\n[ref]: missing.md not-a-title\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_malformed_inline_link_title_tail_is_ignored(self) -> None:
        (self.root / "AGENTS.md").write_text(
            "# Agent guidance\n\n[literal](missing.md not-a-title)\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_forbidden_bare_destination_character_is_ignored(self) -> None:
        (self.root / "AGENTS.md").write_text(
            "# Agent guidance\n\n[literal](missing<file.md)\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_angle_destination_may_not_contain_line_break(self) -> None:
        (self.root / "AGENTS.md").write_text(
            "# Agent guidance\n\n"
            "[literal](<missing\nfile.md>)\n"
            "[escaped](<missing\\\nfile.md>)\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_angle_reference_destination_honors_escaped_closer(self) -> None:
        (self.root / "docs/target>file.md").write_text(
            "# Target\n",
            encoding="utf-8",
        )
        (self.root / "AGENTS.md").write_text(
            "# Agent guidance\n\n"
            "[Target][target]\n\n"
            "[target]: <docs/target\\>file.md>\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_well_formed_inline_link_title_is_checked(self) -> None:
        (self.root / "AGENTS.md").write_text(
            '# Agent guidance\n\n[Missing](missing.md "A title")\n',
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["relative-link"],
        )
        self.assertIn("`missing.md`", failures[0].message)

    def test_linked_image_destination_is_checked(self) -> None:
        (self.root / "target.md").write_text("# Target\n", encoding="utf-8")
        (self.root / "AGENTS.md").write_text(
            "# Agent guidance\n\n"
            "[![Badge](missing.png)](target.md)\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["relative-link"],
        )
        self.assertIn("`missing.png`", failures[0].message)

    def test_container_fenced_code_does_not_expose_links(self) -> None:
        (self.root / "AGENTS.md").write_text(
            "# Agent guidance\n\n"
            "> ~~~\n"
            "> [Quoted sample](missing-quoted.md)\n"
            "> ~~~\n\n"
            "- ~~~\n"
            "  [Listed sample](missing-listed.md)\n"
            "  ~~~\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_nested_list_continuation_fence_does_not_expose_links(self) -> None:
        (self.root / "AGENTS.md").write_text(
            "# Agent guidance\n\n"
            "- outer item\n"
            "  - inner item\n\n"
            "    ```text\n"
            "    [Listed sample](missing-listed.md)\n"
            "    ```\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_ordered_list_closing_fence_stops_masking(self) -> None:
        (self.root / "AGENTS.md").write_text(
            "# Agent guidance\n\n"
            "100. ```\n"
            "     [Code sample](ignored.md)\n"
            "     ```\n\n"
            "[Missing](missing.md)\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["relative-link"],
        )
        self.assertIn("`missing.md`", failures[0].message)

    def test_container_end_implicitly_closes_fenced_code(self) -> None:
        (self.root / "AGENTS.md").write_text(
            "# Agent guidance\n\n"
            "> ```\n"
            "> quoted code\n"
            "[After quote](missing-quote.md)\n\n"
            "- ```\n"
            "  listed code\n"
            "[After list](missing-list.md)\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["relative-link", "relative-link"],
        )
        self.assertEqual(
            failure_messages(failures),
            [
                "target does not exist: `missing-quote.md`",
                "target does not exist: `missing-list.md`",
            ],
        )

    def test_invalid_backtick_fence_does_not_hide_link(self) -> None:
        (self.root / "AGENTS.md").write_text(
            "# Agent guidance\n\n"
            "``` bad`info\n"
            "[Missing](missing.md)\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["relative-link"],
        )
        self.assertIn("`missing.md`", failures[0].message)

    def test_block_quote_indented_code_does_not_expose_links(self) -> None:
        (self.root / "AGENTS.md").write_text(
            "# Agent guidance\n\n"
            "> \n"
            ">     [Quoted sample](missing-quoted.md)\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_indented_code_after_a_heading_does_not_expose_links(self) -> None:
        (self.root / "AGENTS.md").write_text(
            "# Agent guidance\n\n"
            "## Sample\n"
            "    [Heading sample](missing-heading.md)\n\n"
            "---\n"
            "    [Break sample](missing-break.md)\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_indented_line_after_a_paragraph_still_exposes_links(self) -> None:
        (self.root / "AGENTS.md").write_text(
            "# Agent guidance\n\n"
            "Continuing prose.\n"
            "    [Lazy continuation](missing-continuation.md)\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(failure_categories(failures), ["relative-link"])
        self.assertIn("missing-continuation.md", failures[0].message)

    def test_code_span_does_not_pair_across_a_block_boundary(self) -> None:
        (self.root / "AGENTS.md").write_text(
            "# Agent guidance\n\n"
            "One paragraph with a stray ` delimiter.\n\n"
            "[Paragraph sample](missing-paragraph.md)\n\n"
            "Another paragraph with a stray ` delimiter.\n\n"
            "Prose before a heading with a stray ` delimiter.\n"
            "# Heading with a stray ` delimiter\n"
            "[Heading sample](missing-across-heading.md)\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["relative-link", "relative-link"],
        )
        self.assertEqual(
            failure_messages(failures),
            [
                "target does not exist: `missing-paragraph.md`",
                "target does not exist: `missing-across-heading.md`",
            ],
        )

    def test_code_span_does_not_pair_across_a_list_item_boundary(self) -> None:
        (self.root / "AGENTS.md").write_text(
            "# Agent guidance\n\n"
            "- stray \\`\n"
            "- [Listed sample](missing-listed.md) \\`\n"
            "- a span `[Masked](missing-masked.md)` closes inside its item\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(failure_categories(failures), ["relative-link"])
        self.assertIn("missing-listed.md", failures[0].message)

    def test_sibling_list_dedent_keeps_its_continuation_paragraph(self) -> None:
        (self.root / "AGENTS.md").write_text(
            "# Agent guidance\n\n"
            "- outer item\n\n"
            "  - first inner\n\n"
            "  - second inner\n\n"
            "    [Listed sample](missing-listed.md)\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(failure_categories(failures), ["relative-link"])
        self.assertIn("missing-listed.md", failures[0].message)

    def test_code_span_still_spans_lines_inside_one_paragraph(self) -> None:
        (self.root / "AGENTS.md").write_text(
            "# Agent guidance\n\n"
            "A wrapped code span `[Masked](missing-masked.md)\n"
            "continues here` and closes.\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_raw_html_blocks_do_not_expose_links(self) -> None:
        (self.root / "AGENTS.md").write_text(
            "# Agent guidance\n\n"
            "<pre>\n[Pre sample](missing-pre.md)\n</pre>\n\n"
            "<script>\n[Script sample](missing-script.md)\n</script>\n\n"
            "<style>\n[Style sample](missing-style.md)\n</style>\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_all_literal_raw_html_block_forms_hide_links(self) -> None:
        (self.root / "AGENTS.md").write_text(
            "# Agent guidance\n\n"
            "<textarea>\n[Textarea](missing-textarea.md)\n</textarea>\n\n"
            "<!--\n[Comment](missing-comment.md)\n-->\n\n"
            "<?instruction\n[Processing](missing-processing.md)\n?>\n\n"
            "<!DECLARATION\n[Declaration](missing-declaration.md)\n>\n\n"
            "<![CDATA[\n[Cdata](missing-cdata.md)\n]]>\n\n"
            "<table>\n[Table](missing-table.md)\n</table>\n\n"
            "<x-widget>\n[Custom](missing-custom.md)\n\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_destination_unescapes_all_ascii_punctuation(self) -> None:
        (self.root / "foo~bar.md").write_text("# Target\n", encoding="utf-8")
        (self.root / "AGENTS.md").write_text(
            "# Agent guidance\n\n[Target](foo\\~bar.md)\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_destination_decodes_html_character_references(self) -> None:
        (self.root / "foo&bar.md").write_text("# Target\n", encoding="utf-8")
        (self.root / "AGENTS.md").write_text(
            "# Agent guidance\n\n[Target](foo&amp;bar.md)\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_malformed_external_destination_does_not_crash(self) -> None:
        (self.root / "AGENTS.md").write_text(
            "# Agent guidance\n\n[Host](https://[)\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_longer_backtick_run_does_not_close_inline_code(self) -> None:
        (self.root / "AGENTS.md").write_text(
            "# Agent guidance\n\n"
            "``code ``` [Missing](missing.md) `\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["relative-link"],
        )
        self.assertIn("`missing.md`", failures[0].message)

    def test_reference_style_heading_uses_its_visible_label(self) -> None:
        (self.root / "AGENTS.md").write_text(
            "# Agent guidance\n\n"
            "[Heading](docs/spec/example.md#reference-heading)\n",
            encoding="utf-8",
        )
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "## [Reference heading][heading-label]\n\n"
            "[heading-label]: https://example.com/heading\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_nested_link_label_heading_uses_its_visible_label(self) -> None:
        (self.root / "AGENTS.md").write_text(
            "# Agent guidance\n\n"
            "[Heading](docs/spec/example.md#outer-inner)\n",
            encoding="utf-8",
        )
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "## [Outer [inner]](https://example.com)\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_code_span_heading_uses_rendered_literal_text(self) -> None:
        (self.root / "AGENTS.md").write_text(
            "# Agent guidance\n\n"
            "[Entity](docs/spec/example.md#amp)\n"
            "[Whitespace](docs/spec/example.md#foo-bar)\n",
            encoding="utf-8",
        )
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "## `&amp;`\n\n"
            "## `foo   bar`\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_headings_inside_markdown_containers_have_anchors(self) -> None:
        (self.root / "AGENTS.md").write_text(
            "# Agent guidance\n\n"
            "[Quoted](docs/spec/example.md#quoted-heading)\n"
            "[Listed](docs/spec/example.md#listed-heading)\n",
            encoding="utf-8",
        )
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "> ## Quoted heading\n\n"
            "- ## Listed heading\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_explicit_html_anchors_resolve(self) -> None:
        (self.root / "AGENTS.md").write_text(
            "# Agent guidance\n\n"
            "[Setup](docs/spec/example.md#setup)\n"
            "[Details](docs/spec/example.md#details)\n",
            encoding="utf-8",
        )
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            '<a name="setup"></a>\n\n'
            "<a id='details'></a>\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_block_form_explicit_html_anchor_resolves(self) -> None:
        (self.root / "AGENTS.md").write_text(
            "# Agent guidance\n\n"
            "[Setup](docs/spec/example.md#setup)\n",
            encoding="utf-8",
        )
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            '<a id="setup">\n'
            "Setup content\n"
            "</a>\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_raw_text_html_contents_do_not_define_explicit_anchors(self) -> None:
        (self.root / "AGENTS.md").write_text(
            "# Agent guidance\n\n"
            "[Script](docs/spec/example.md#script-phantom)\n"
            "[Style](docs/spec/example.md#style-phantom)\n"
            "[Textarea](docs/spec/example.md#textarea-phantom)\n",
            encoding="utf-8",
        )
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "<script>\n"
            '<a id="script-phantom"></a>\n'
            "</script>\n\n"
            "<style>\n"
            '<a id="style-phantom"></a>\n'
            "</style>\n\n"
            "<textarea>\n"
            '<a id="textarea-phantom"></a>\n'
            "</textarea>\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["relative-link", "relative-link", "relative-link"],
        )
        self.assertEqual(
            failure_messages(failures),
            [
                "anchor `#script-phantom` does not exist in "
                "`docs/spec/example.md`",
                "anchor `#style-phantom` does not exist in "
                "`docs/spec/example.md`",
                "anchor `#textarea-phantom` does not exist in "
                "`docs/spec/example.md`",
            ],
        )

    def test_additional_raw_text_html_contents_do_not_define_anchors(self) -> None:
        (self.root / "AGENTS.md").write_text(
            "# Agent guidance\n\n"
            "[Title](docs/spec/example.md#title-phantom)\n"
            "[Iframe](docs/spec/example.md#iframe-phantom)\n"
            "[Noframes](docs/spec/example.md#noframes-phantom)\n",
            encoding="utf-8",
        )
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "<title>\n"
            '<a id="title-phantom"></a>\n'
            "</title>\n\n"
            "<iframe>\n"
            '<a id="iframe-phantom"></a>\n'
            "</iframe>\n\n"
            "<noframes>\n"
            '<a id="noframes-phantom"></a>\n'
            "</noframes>\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["relative-link", "relative-link", "relative-link"],
        )
        self.assertEqual(
            failure_messages(failures),
            [
                "anchor `#title-phantom` does not exist in "
                "`docs/spec/example.md`",
                "anchor `#iframe-phantom` does not exist in "
                "`docs/spec/example.md`",
                "anchor `#noframes-phantom` does not exist in "
                "`docs/spec/example.md`",
            ],
        )

    def test_hyphenated_attribute_does_not_define_an_anchor(self) -> None:
        (self.root / "AGENTS.md").write_text(
            "# Agent guidance\n\n"
            "[Data](docs/spec/example.md#data-phantom)\n",
            encoding="utf-8",
        )
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            '<a data-id="data-phantom">text</a>\n',
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(failure_categories(failures), ["relative-link"])
        self.assertIn("anchor `#data-phantom` does not exist", failures[0].message)

    def test_heading_autolink_contributes_its_visible_text(self) -> None:
        (self.root / "AGENTS.md").write_text(
            "# Agent guidance\n\n"
            "[Autolink](docs/spec/example.md#httpsexamplecom)\n",
            encoding="utf-8",
        )
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "## <https://example.com>\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_directory_fragment_resolves_through_readme(self) -> None:
        guide = self.root / "docs/guide"
        guide.mkdir()
        (guide / "README.md").write_text(
            "# Guide\n\n## Setup\n", encoding="utf-8"
        )
        (self.root / "AGENTS.md").write_text(
            "# Agent guidance\n\n[Setup](docs/guide/#setup)\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])


    def test_github_slug_preserves_expected_house_shapes(self) -> None:
        self.assertEqual(
            github_slug("2026-07-25 — A decision"),
            "2026-07-25--a-decision",
        )
        self.assertEqual(
            github_slug("Provider bridge and `current_time`"),
            "provider-bridge-and-current_time",
        )


if __name__ == "__main__":
    unittest.main()

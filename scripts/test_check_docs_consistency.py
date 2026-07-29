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
from check_docs_consistency import PR_TOKEN, Violation, github_slug, run_checks
from generate_invariants import (
    orphan_invariant_references,
    render as render_invariant_index,
)


def failure_categories(failures: list[Violation]) -> list[str]:
    """Project deterministic failure categories outside test bodies."""
    return [failure.category for failure in failures]


def failure_messages(failures: list[Violation]) -> list[str]:
    """Project deterministic failure messages outside test bodies."""
    return [failure.message for failure in failures]


def pr_tokens(text: str) -> list[str]:
    """Project matched verification tokens and their extents outside tests."""
    return [match.group(0) for match in PR_TOKEN.finditer(text)]


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


def git_output(root: Path, *arguments: str) -> str:
    """Return output from one deterministic local-only Git fixture command."""
    disabled_hooks = root / ".disabled-git-hooks"
    disabled_hooks.mkdir(exist_ok=True)
    result = subprocess.run(
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
    return result.stdout.strip()


def initialize_git_history(root: Path) -> str:
    """Create one reachable GitHub-style PR merge for the baseline fixture."""
    merged_branch = "agent/example"
    empty_template = root / ".empty-git-template"
    empty_template.mkdir()
    run_git(root, "init", "-q", "-b", "main", f"--template={empty_template}")
    run_git(root, "config", "user.name", "Docs checker tests")
    run_git(root, "config", "user.email", "docs-checker@example.invalid")
    run_git(root, "add", ".")
    run_git(root, "commit", "-q", "-m", "initial fixture")
    run_git(root, "checkout", "-q", "-b", merged_branch)
    (root / "history-marker").write_text("PR 12 fixture\n", encoding="utf-8")
    run_git(root, "add", "history-marker")
    run_git(root, "commit", "-q", "-m", "fixture change")
    run_git(root, "checkout", "-q", "main")
    run_git(
        root,
        "merge",
        "-q",
        "--no-ff",
        "-m",
        f"Merge pull request #12 from owner/{merged_branch}",
        merged_branch,
    )
    return merged_branch


class DocsConsistencyTests(unittest.TestCase):
    def setUp(self) -> None:
        self.environment = patch.dict(os.environ)
        self.environment.start()
        os.environ.pop("GITHUB_EVENT_PATH", None)
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
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
            "Verified against the implementing stack through PR #12 "
            "(`agent/example`).\n\n"
            "Historical discussion appears in PR #42, which is not a "
            "verification reference. Inline code such as `PR #0` is also not "
            "a reference.\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n\n"
            "## Repeat\n\n"
            "[Duplicate](#repeat-1)\n",
            encoding="utf-8",
        )
        (self.root / "docs/spec/README.md").write_text(
            "# Specification\n", encoding="utf-8"
        )
        self.merged_pr_branch = initialize_git_history(self.root)

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

    def test_missing_git_reports_a_verification_violation(self) -> None:
        empty_path = self.root / ".empty-path"
        empty_path.mkdir()

        with patch.dict(os.environ, {"PATH": str(empty_path)}):
            failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["spec-verification-history"],
        )
        self.assertIn("`git` is not available", failures[0].message)

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
            "Verified through PR #12 (`agent/example`).\n\n"
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
            "Verified through PR #12 (`agent/example`).\n\n"
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
            "Verified through PR #12 (`agent/example`).\n\n"
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
            "Verified through PR #12 (`agent/example`).\n\n"
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
            "Verified through PR #12 (`agent/example`).\n\n"
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
            "Verified through PR #12 (`agent/example`).\n\n"
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
            "Verified through PR #12 (`agent/example`).\n\n"
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
            "Verified through PR #12 (`agent/example`).\n\n"
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
            "Verified through PR #12 (`agent/example`).\n\n"
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
            "Verified through PR #12 (`agent/example`).\n\n"
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

    def test_missing_and_malformed_verification_refs_are_reported(self) -> None:
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "Verified through PR #0.\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["spec-verification", "spec-verification"],
        )
        messages = "\n".join(failure_messages(failures))
        self.assertIn("positive decimal", messages)
        self.assertIn("missing", messages)

    def test_verification_ref_requires_integrated_pull_request(self) -> None:
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "Verified through PR #99 (`agent/missing`).\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["spec-verification-history"],
        )
        self.assertIn(
            "no merge commit in the `main` integration history",
            failures[0].message,
        )

    def test_verification_ref_requires_exact_merged_branch(self) -> None:
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "Verified through PR #12 (`agent/not-example`).\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["spec-verification-history"],
        )
        self.assertIn(
            f"names `{self.merged_pr_branch}`", failures[0].message
        )

    def test_verification_ref_rejects_one_parent_merge_subject_spoof(self) -> None:
        run_git(
            self.root,
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "Merge pull request #99 from owner/agent/spoof",
        )
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "Verified through PR #99 (`agent/spoof`).\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["spec-verification-history"],
        )
        self.assertIn(
            "no merge commit in the `main` integration history",
            failures[0].message,
        )

    def test_local_branch_does_not_override_known_pr_history(self) -> None:
        run_git(self.root, "checkout", "-q", "-b", "agent/wrong")
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "Verified through PR #12 (`agent/wrong`).\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["spec-verification-history"],
        )
        self.assertIn(
            f"names `{self.merged_pr_branch}`", failures[0].message
        )

    def test_verification_ref_rejects_merge_outside_integration(self) -> None:
        run_git(self.root, "checkout", "-q", "-b", "isolated-base")
        run_git(self.root, "checkout", "-q", "-b", "agent/unreachable")
        (self.root / "unreachable-marker").write_text(
            "PR 99 fixture\n", encoding="utf-8"
        )
        run_git(self.root, "add", "unreachable-marker")
        run_git(self.root, "commit", "-q", "-m", "unreachable fixture")
        run_git(self.root, "checkout", "-q", "isolated-base")
        run_git(
            self.root,
            "merge",
            "-q",
            "--no-ff",
            "-m",
            "Merge pull request #99 from owner/agent/unreachable",
            "agent/unreachable",
        )
        run_git(self.root, "checkout", "-q", "main")
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "Verified through PR #99 (`agent/unreachable`).\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["spec-verification-history"],
        )
        self.assertIn(
            "no merge commit in the `main` integration history",
            failures[0].message,
        )

    def test_head_branch_merge_is_not_integration_provenance(self) -> None:
        run_git(self.root, "checkout", "-q", "-b", "agent/head")
        run_git(self.root, "checkout", "-q", "-b", "agent/spoof")
        (self.root / "spoof-marker").write_text(
            "PR 99 fixture\n", encoding="utf-8"
        )
        run_git(self.root, "add", "spoof-marker")
        run_git(self.root, "commit", "-q", "-m", "spoof fixture")
        run_git(self.root, "checkout", "-q", "agent/head")
        run_git(
            self.root,
            "merge",
            "-q",
            "--no-ff",
            "-m",
            "Merge pull request #99 from owner/agent/spoof",
            "agent/spoof",
        )
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "Verified through PR #99 (`agent/spoof`).\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["spec-verification-history"],
        )
        self.assertIn(
            "no merge commit in the `main` integration history",
            failures[0].message,
        )

    def test_remote_integration_ref_outranks_the_local_branch(self) -> None:
        run_git(
            self.root,
            "update-ref",
            "refs/remotes/origin/main",
            git_output(self.root, "rev-parse", "HEAD"),
        )
        run_git(self.root, "checkout", "-q", "-b", "agent/late")
        (self.root / "late-marker").write_text(
            "PR 99 fixture\n", encoding="utf-8"
        )
        run_git(self.root, "add", "late-marker")
        run_git(self.root, "commit", "-q", "-m", "late fixture")
        run_git(self.root, "checkout", "-q", "main")
        run_git(
            self.root,
            "merge",
            "-q",
            "--no-ff",
            "-m",
            "Merge pull request #99 from owner/agent/late",
            "agent/late",
        )
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "Verified through PR #99 (`agent/late`).\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["spec-verification-history"],
        )
        self.assertIn(
            "no merge commit in the `main` integration history",
            failures[0].message,
        )

    def test_one_local_in_flight_ref_may_match_checkout_branch(self) -> None:
        run_git(self.root, "checkout", "-q", "-b", "agent/in-flight")
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "Verified through PR #99 (`agent/in-flight`).\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_github_event_may_identify_one_in_flight_ref(self) -> None:
        event = self.root / "event.json"
        event.write_text(
            '{"number": 99, "pull_request": {"head": {"ref": "agent/in-flight"}}}',
            encoding="utf-8",
        )
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "Verified through PR #99 (`agent/in-flight`).\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )

        with patch.dict(os.environ, {"GITHUB_EVENT_PATH": str(event)}):
            failures = run_checks(self.root)

        self.assertEqual(failures, [])

    def test_malformed_github_event_base_is_reported_without_crashing(self) -> None:
        event = self.root / "event.json"
        event.write_text(
            '{"number": 99, "pull_request": {'
            '"head": {"ref": "agent/in-flight"}, "base": null}}',
            encoding="utf-8",
        )
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "Verified through PR #99 (`agent/in-flight`).\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )

        with patch.dict(os.environ, {"GITHUB_EVENT_PATH": str(event)}):
            failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["spec-verification-history"],
        )
        self.assertIn(
            "cannot inspect GitHub pull-request event", failures[0].message
        )

    def test_github_event_accepts_verification_inherited_from_exact_base(self) -> None:
        run_git(self.root, "checkout", "-q", "-b", "agent/stack-base")
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "Verified through PR #99 (`agent/stack-base`).\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )
        run_git(self.root, "add", "docs/spec/example.md")
        run_git(self.root, "commit", "-q", "-m", "stack base fixture")
        base_sha = git_output(self.root, "rev-parse", "HEAD")
        run_git(self.root, "checkout", "-q", "-b", "agent/stack-child")
        event = self.root / "event.json"
        event.write_text(
            '{"number": 100, "pull_request": {'
            '"head": {"ref": "agent/stack-child"}, '
            f'"base": {{"sha": "{base_sha}"}}}}}}',
            encoding="utf-8",
        )

        with patch.dict(os.environ, {"GITHUB_EVENT_PATH": str(event)}):
            failures = run_checks(self.root)

        self.assertEqual(failures, [])

    def test_inherited_verification_follows_a_renamed_page(self) -> None:
        run_git(self.root, "checkout", "-q", "-b", "agent/stack-base")
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "Verified through PR #99 (`agent/stack-base`).\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )
        run_git(self.root, "add", "docs/spec/example.md")
        run_git(self.root, "commit", "-q", "-m", "stack base fixture")
        base_sha = git_output(self.root, "rev-parse", "HEAD")
        run_git(self.root, "checkout", "-q", "-b", "agent/stack-child")
        run_git(
            self.root,
            "mv",
            "docs/spec/example.md",
            "docs/spec/renamed.md",
        )
        run_git(self.root, "commit", "-q", "-m", "rename the page")
        (self.root / "AGENTS.md").write_text(
            "# Agent guidance\n\n[Docs directory](docs/)\n", encoding="utf-8"
        )
        event = self.root / "event.json"
        event.write_text(
            '{"number": 100, "pull_request": {'
            '"head": {"ref": "agent/stack-child"}, '
            f'"base": {{"sha": "{base_sha}"}}}}}}',
            encoding="utf-8",
        )

        with patch.dict(os.environ, {"GITHUB_EVENT_PATH": str(event)}):
            failures = run_checks(self.root)

        self.assertEqual(failures, [])

    def test_github_event_requires_exact_in_flight_number_and_branch(self) -> None:
        event = self.root / "event.json"
        event.write_text(
            '{"number": 98, "pull_request": {"head": {"ref": "agent/in-flight"}}}',
            encoding="utf-8",
        )
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "Verified through PR #99 (`agent/in-flight`).\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )

        with patch.dict(os.environ, {"GITHUB_EVENT_PATH": str(event)}):
            failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["spec-verification-history"],
        )
        self.assertIn(
            "no merge commit in the `main` integration history",
            failures[0].message,
        )

    def test_non_pull_request_github_event_disables_local_exception(self) -> None:
        event = self.root / "event.json"
        event.write_text(
            '{"ref": "refs/heads/main"}',
            encoding="utf-8",
        )
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "Verified through PR #99 (`main`).\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )

        with patch.dict(os.environ, {"GITHUB_EVENT_PATH": str(event)}):
            failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["spec-verification-history"],
        )
        self.assertIn("cannot inspect GitHub pull-request event", failures[0].message)

    def test_one_unmerged_pr_may_verify_multiple_pages(self) -> None:
        run_git(self.root, "checkout", "-q", "-b", "agent/in-flight")
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "Verified through PR #99 (`agent/in-flight`).\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )
        (self.root / "docs/spec/other.md").write_text(
            "# Other\n\n"
            "Verified through PR #99 (`agent/in-flight`).\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(failures, [])

    def test_only_one_unmerged_verification_pr_identity_is_permitted(self) -> None:
        run_git(self.root, "checkout", "-q", "-b", "agent/in-flight")
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "Verified through PR #99 (`agent/in-flight`).\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )
        (self.root / "docs/spec/other.md").write_text(
            "# Other\n\n"
            "Verified through PR #98 (`agent/in-flight`).\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["spec-verification-history"],
        )
        self.assertIn(
            "only one unmerged verification PR identity is permitted",
            failures[0].message,
        )

    def test_verification_ref_requires_closed_branch_token(self) -> None:
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "Verified through PR #12 (`agent/example`.\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["spec-verification", "spec-verification"],
        )

    def test_verification_ref_rejects_whitespace_branch_token(self) -> None:
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "Verified through PR #12 (`   `).\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["spec-verification", "spec-verification"],
        )

    def test_verification_ref_rejects_internal_branch_whitespace(self) -> None:
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "Verified through PR #12 (`agent/bad branch`).\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["spec-verification", "spec-verification"],
        )

    def test_verification_ref_requires_literal_closing_parenthesis(self) -> None:
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "Verified through PR #12 (`agent/example`; details follow.\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["spec-verification", "spec-verification"],
        )

    def test_bare_verification_parenthetical_still_parses(self) -> None:
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "Verified through PR #12 (`agent/example`).\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_scoped_verification_parenthetical_parses(self) -> None:
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "Verified through PR #12 (`agent/example`; "
            "`production_connection_options` in src/tests.rs).\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_scoped_verification_tail_may_wrap_across_lines(self) -> None:
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "Verified through PR #12 (`agent/example`; the refusal path\n"
            "in `production_connection_options` under src/tests.rs).\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_scoped_verification_tail_may_not_cross_a_blank_line(self) -> None:
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "Verified through PR #12 (`agent/example`; the refusal path\n\n"
            "in `production_connection_options` under src/tests.rs).\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["spec-verification", "spec-verification"],
        )

    def test_scoped_verification_tail_may_not_cross_a_list_item(self) -> None:
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "- Verified through PR #12 (`agent/example`; the refusal path\n"
            "- in `production_connection_options` under src/tests.rs).\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["spec-verification", "spec-verification"],
        )

    def test_scoped_parenthetical_still_requires_a_backticked_branch(
        self,
    ) -> None:
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "Verified through PR #12 (agent/example; the refusal path in "
            "src/tests.rs).\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["spec-verification", "spec-verification"],
        )

    def test_scoped_parenthetical_branch_ref_is_still_validated(self) -> None:
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "Verified through PR #12 (`agent/bad branch`; the refusal path "
            "in src/tests.rs).\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["spec-verification", "spec-verification"],
        )

    def test_scope_tail_requires_its_leading_semicolon(self) -> None:
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "Verified through PR #12 (`agent/example` and src/tests.rs).\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["spec-verification", "spec-verification"],
        )

    def test_scoped_verification_tail_may_wrap_inside_a_block_quote(
        self,
    ) -> None:
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "> Verified through PR #12 (`agent/example`; the refusal path\n"
            "> in `production_connection_options` under src/tests.rs).\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_scoped_verification_tail_may_not_cross_a_quoted_blank_line(
        self,
    ) -> None:
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "> Verified through PR #12 (`agent/example`; the refusal path\n"
            ">\n"
            "> in `production_connection_options` under src/tests.rs).\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["spec-verification", "spec-verification"],
        )

    def test_scoped_verification_tail_may_not_cross_a_quoted_list_item(
        self,
    ) -> None:
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "> - Verified through PR #12 (`agent/example`; the refusal path\n"
            "> - in `production_connection_options` under src/tests.rs).\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["spec-verification", "spec-verification"],
        )

    def test_scoped_verification_tail_may_not_cross_a_nested_quoted_blank_line(
        self,
    ) -> None:
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "> > Verified through PR #12 (`agent/example`; the refusal path\n"
            "> >\n"
            "> > in `production_connection_options` under src/tests.rs).\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["spec-verification", "spec-verification"],
        )

    def test_empty_scope_tail_is_rejected(self) -> None:
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "Verified through PR #12 (`agent/example`;).\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["spec-verification", "spec-verification"],
        )

    def test_whitespace_only_scope_tail_is_rejected(self) -> None:
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "Verified through PR #12 (`agent/example`;   ).\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["spec-verification", "spec-verification"],
        )

    def test_single_character_scope_tail_parses(self) -> None:
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "Verified through PR #12 (`agent/example`; x).\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_scope_tail_may_name_code_holding_parentheses(self) -> None:
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "Verified through PR #12 (`agent/example`; the refusal path in "
            "`handle()` under src/tests.rs).\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_scope_code_span_parenthesis_does_not_close_the_reference(
        self,
    ) -> None:
        self.assertEqual(
            pr_tokens(
                "Verified through PR #12 (`agent/example`; the refusal path "
                "in `handle()` under src/tests.rs)."
            ),
            [
                "PR #12 (`agent/example`; the refusal path in `handle()` "
                "under src/tests.rs)"
            ],
        )

    def test_bare_scope_parenthesis_still_closes_the_reference(self) -> None:
        self.assertEqual(
            pr_tokens(
                "Verified through PR #12 (`agent/example`; the refusal path) "
                "under src/tests.rs."
            ),
            ["PR #12 (`agent/example`; the refusal path)"],
        )

    def test_unterminated_reference_is_not_closed_inside_a_code_span(
        self,
    ) -> None:
        fragment = (
            "Verified through PR #12 (`agent/example`; the refusal path in "
            "`handle()` under src/tests.rs."
        )
        (self.root / "docs/spec/example.md").write_text(
            f"# Example\n\n{fragment}\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(pr_tokens(fragment), [])
        self.assertEqual(
            failure_categories(failures),
            ["spec-verification", "spec-verification"],
        )

    def test_scope_tail_with_an_unclosed_code_span_is_rejected(self) -> None:
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "Verified through PR #12 (`agent/example`; the refusal path in "
            "`handle under src/tests.rs).\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["spec-verification", "spec-verification"],
        )

    def test_quoted_empty_wrapped_scope_tail_is_rejected(self) -> None:
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "> Verified through PR #12 (`agent/example`;\n"
            "> ).\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["spec-verification", "spec-verification"],
        )

    def test_quoted_one_word_scope_tail_parses(self) -> None:
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "> Verified through PR #12 (`agent/example`;\n"
            "> refusals).\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_inline_verification_text_does_not_satisfy_reference(self) -> None:
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "``Verified through PR #12 (`agent/example`).``\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["spec-verification"],
        )
        self.assertIn("missing", failures[0].message)

    def test_unrelated_verified_fact_does_not_satisfy_reference(self) -> None:
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "The certificate is verified locally. Historical discussion: "
            "PR #42 (`agent/history`).\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["spec-verification"],
        )
        self.assertIn("missing", failures[0].message)

    def test_semicolon_separates_verified_fact_from_historical_pr(self) -> None:
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "The certificate is verified locally; historical discussion: "
            "PR #42 (`agent/history`).\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["spec-verification"],
        )
        self.assertIn("missing", failures[0].message)

    def test_verification_clause_stops_at_a_list_item_boundary(self) -> None:
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "- The checksum is verified\n"
            "- Historical context through PR #12 (`agent/example`)\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["spec-verification"],
        )
        self.assertIn("missing", failures[0].message)

    def test_unrelated_pr_after_verification_clause_is_ignored(self) -> None:
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "Verified through PR #12 (`agent/example`). Historical discussion: "
            "PR #42.\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_verification_clause_allows_prose_abbreviation(self) -> None:
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "Behavior was verified, e.g. by inspection, through PR #12 "
            "(`agent/example`).\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_verification_reference_allows_against(self) -> None:
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "This page was verified against PR #12 (`agent/example`).\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_negated_verification_references_do_not_satisfy_page(self) -> None:
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "This page was not verified against PR #12 (`agent/example`).\n\n"
            "No behavior was verified through PR #13 (`agent/other`).\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["spec-verification"],
        )
        self.assertIn("missing", failures[0].message)

    def test_contracted_negated_verification_references_do_not_satisfy_page(
        self,
    ) -> None:
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "This page isn't verified through PR #12 (`agent/example`).\n\n"
            "It wasn't verified through PR #13 (`agent/other`).\n\n"
            "It hasn't been verified against PR #14 (`agent/third`).\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["spec-verification"],
        )
        self.assertIn("missing", failures[0].message)

    def test_cannot_verification_reference_does_not_satisfy_page(self) -> None:
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "This page cannot be verified against implementation through "
            "PR #12 (`agent/example`).\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["spec-verification"],
        )
        self.assertIn("missing", failures[0].message)

    def test_long_form_negated_verification_does_not_satisfy_page(self) -> None:
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "This page has not yet been fully and independently verified "
            "against PR #12 (`agent/example`).\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["spec-verification"],
        )
        self.assertIn("missing", failures[0].message)

    def test_negation_in_a_preceding_sentence_still_allows_verification(
        self,
    ) -> None:
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "The schema is not normative. Behavior is verified against "
            "PR #12 (`agent/example`).\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )

        self.assertEqual(run_checks(self.root), [])

    def test_emphasized_negation_does_not_satisfy_page(self) -> None:
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "This page is **not** verified against implementation through "
            "PR #12 (`agent/example`).\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["spec-verification"],
        )
        self.assertIn("missing", failures[0].message)

    def test_verification_clause_stops_at_markup_sentence_start(self) -> None:
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "The checksum is verified. [Historical context](../invariants.md) "
            "describes a change made through PR #12 (`agent/example`).\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["spec-verification"],
        )
        self.assertIn("missing", failures[0].message)

    def test_commented_verification_reference_is_ignored(self) -> None:
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "<!-- Verified through PR #12 (`agent/example`). -->\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["spec-verification"],
        )
        self.assertIn("missing", failures[0].message)

    def test_nested_spec_readme_requires_verification_reference(self) -> None:
        nested = self.root / "docs/spec/providers"
        nested.mkdir()
        (nested / "README.md").write_text(
            "# Provider subsystem\n", encoding="utf-8"
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["spec-verification"],
        )
        self.assertEqual(
            failures[0].path,
            "docs/spec/providers/README.md",
        )

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

#!/usr/bin/env python3
"""Focused regression tests for check_docs_consistency.py."""

from __future__ import annotations

import sys
import tempfile
import unittest
from pathlib import Path

sys.dont_write_bytecode = True

from check_docs_consistency import github_slug, run_checks


class DocsConsistencyTests(unittest.TestCase):
    def setUp(self) -> None:
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
            "[spec]:\n"
            "  docs/spec/example.md#repeat\n\n"
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
        (self.root / "docs/decisions.md").write_text(
            "# Decisions\n\n"
            "## 2026-07-25 — New\n\n"
            "## 2026-07-25 — Same day\n\n"
            "## 2026-07-24 — Old\n",
            encoding="utf-8",
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

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def test_valid_fixture_passes(self) -> None:
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
            [failure.category for failure in failures],
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
            [failure.category for failure in failures],
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
            [failure.category for failure in failures],
            ["invariant-test"],
        )
        self.assertIn("`missing_test`", failures[0].message)

    def test_missing_heading_anchor_is_reported(self) -> None:
        (self.root / "AGENTS.md").write_text(
            "# Agent guidance\n\n"
            "[Missing](docs/spec/example.md#not-a-heading)\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            [failure.category for failure in failures],
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
            [failure.category for failure in failures],
            ["relative-link"],
        )
        self.assertIn("escapes the repository", failures[0].message)

    def test_newer_decision_after_older_entry_is_reported(self) -> None:
        (self.root / "docs/decisions.md").write_text(
            "# Decisions\n\n"
            "## 2026-07-24 — Old\n\n"
            "## 2026-07-25 — New\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            [failure.category for failure in failures],
            ["decision-order"],
        )
        self.assertIn("newer than the preceding", failures[0].message)

    def test_invalid_decision_date_is_reported(self) -> None:
        (self.root / "docs/decisions.md").write_text(
            "# Decisions\n\n## 2026-13-40 — Invalid\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            [failure.category for failure in failures],
            ["decision-order"],
        )
        self.assertIn("invalid ISO date", failures[0].message)

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
            [failure.category for failure in failures],
            ["spec-verification", "spec-verification"],
        )
        self.assertTrue(
            any("positive decimal" in failure.message for failure in failures)
        )
        self.assertTrue(any("missing" in failure.message for failure in failures))

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
            [failure.category for failure in failures],
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
            [failure.category for failure in failures],
            ["spec-verification", "spec-verification"],
        )

    def test_verification_ref_requires_literal_closing_parenthesis(self) -> None:
        (self.root / "docs/spec/example.md").write_text(
            "# Example\n\n"
            "Verified through PR #12 (`agent/example`; details follow).\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            [failure.category for failure in failures],
            ["spec-verification", "spec-verification"],
        )

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
            [failure.category for failure in failures],
            ["spec-verification"],
        )
        self.assertIn("missing", failures[0].message)

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

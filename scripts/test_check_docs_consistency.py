#!/usr/bin/env python3
"""Focused regression tests for check_docs_consistency.py."""

from __future__ import annotations

import sys
import tempfile
import unittest
from pathlib import Path

sys.dont_write_bytecode = True

from check_docs_consistency import Violation, github_slug, run_checks


def failure_categories(failures: list[Violation]) -> list[str]:
    """Project deterministic failure categories outside test bodies."""
    return [failure.category for failure in failures]


def failure_messages(failures: list[Violation]) -> list[str]:
    """Project deterministic failure messages outside test bodies."""
    return [failure.message for failure in failures]


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

    def test_malformed_inline_link_title_tail_is_ignored(self) -> None:
        (self.root / "AGENTS.md").write_text(
            "# Agent guidance\n\n[literal](missing.md not-a-title)\n",
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

    def test_newer_decision_after_older_entry_is_reported(self) -> None:
        (self.root / "docs/decisions.md").write_text(
            "# Decisions\n\n"
            "## 2026-07-24 — Old\n\n"
            "## 2026-07-25 — New\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
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
            failure_categories(failures),
            ["decision-order"],
        )
        self.assertIn("invalid ISO date", failures[0].message)

    def test_setext_h2_decision_entry_is_rejected(self) -> None:
        (self.root / "docs/decisions.md").write_text(
            "# Decisions\n\n"
            "## 2026-07-24 — Old\n\n"
            "2026-07-26 — New\n"
            "----------------\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
            ["decision-order"],
        )
        self.assertIn(
            "Setext H2 headings are not permitted",
            failures[0].message,
        )

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
            "Verified through PR #12 (`agent/example`; details follow).\n\n"
            "## Provider bridge and `current_time`\n\n"
            "## Repeat\n",
            encoding="utf-8",
        )

        failures = run_checks(self.root)

        self.assertEqual(
            failure_categories(failures),
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

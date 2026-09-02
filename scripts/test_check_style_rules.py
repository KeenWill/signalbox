#!/usr/bin/env python3
"""Prove every style rule fails on the shape it forbids and passes on the fix.

A checker that cannot be shown to fail is the false-confidence trap this
repository documents, so each rule gets both cases: the violating shape reports
and names its file, and the repaired shape reports nothing. Each case builds a
synthetic tracked tree in a temporary directory — the checker reads its
inventory from `git ls-files`, so the fixture is a real repository — and runs
the checker as a subprocess scoped to the single rule under test, which keeps
one rule's fixture from having to satisfy every other rule's globs.

The scanner that blanks comments and string literals gets its own cases: it is
the one piece every code-shaped rule depends on, and a raw string or a lifetime
misread there moves findings silently.
"""

from __future__ import annotations

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

CHECKER = Path(__file__).resolve().parent / "check_style_rules.py"


def check(rule: str, files: dict[str, str]) -> subprocess.CompletedProcess:
    """Run one rule over a synthetic tracked tree, outside test bodies.

    Writes each named file, makes the directory a git repository so the
    checker's `git ls-files` inventory sees exactly these files, and returns the
    completed process so a test can assert on both the status and the report.
    """
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        for label, content in files.items():
            path = root / label
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(content, encoding="utf-8")
        for command in (
            ["git", "init", "--quiet"],
            ["git", "add", "--all"],
        ):
            subprocess.run(command, cwd=root, check=True, capture_output=True)
        return subprocess.run(
            [sys.executable, str(CHECKER), "--root", str(root), "--rule", rule],
            capture_output=True,
            text=True,
        )


MODULE = "crates/example/src/lib.rs"
APP = "apps/example/src/main.rs"
MIGRATION = "crates/example/migrations/202601010001_first.sql"

# SR-13 selects its crates from the manifests, so its fixtures ship one.
DERIVE_MANIFEST = "crates/derive/Cargo.toml"
DERIVE_MODULE = "crates/derive/src/lib.rs"
PROC_MACRO_MANIFEST = "[lib]\nproc-macro = true\n"


class AppSqlTableAccessTests(unittest.TestCase):
    def test_app_sql_naming_a_table_reports(self) -> None:
        source = (
            "//! Owned.\n"
            'pub const READ: &str = "SELECT id FROM turn_lifecycle WHERE id = $1";\n'
        )

        result = check(
            "SR-8",
            {APP: source, MIGRATION: "CREATE TABLE turn_lifecycle (id uuid);\n"},
        )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("turn_lifecycle", result.stdout)

    def test_sql_inside_an_inline_test_module_passes(self) -> None:
        source = (
            "//! Owned.\n"
            "pub fn run() {}\n"
            "#[cfg(test)]\n"
            "mod tests {\n"
            '    const READ: &str = "SELECT id FROM turn_lifecycle";\n'
            "}\n"
        )

        result = check(
            "SR-8",
            {APP: source, MIGRATION: "CREATE TABLE turn_lifecycle (id uuid);\n"},
        )

        self.assertEqual(result.returncode, 0, result.stdout)

    def test_sql_inside_a_restricted_inline_test_module_passes(self) -> None:
        source = (
            "//! Owned.\n"
            "pub fn run() {}\n"
            "#[cfg(test)]\n"
            "pub(crate) mod tests {\n"
            '    const READ: &str = "SELECT id FROM turn_lifecycle";\n'
            "}\n"
        )

        result = check(
            "SR-8",
            {APP: source, MIGRATION: "CREATE TABLE turn_lifecycle (id uuid);\n"},
        )

        self.assertEqual(result.returncode, 0, result.stdout)

    def test_sql_inside_a_compound_cfg_test_module_passes(self) -> None:
        source = (
            "//! Owned.\n"
            "pub fn run() {}\n"
            "#[cfg(all(test, unix))]\n"
            "mod tests {\n"
            '    const READ: &str = "SELECT id FROM turn_lifecycle";\n'
            "}\n"
        )

        result = check(
            "SR-8",
            {APP: source, MIGRATION: "CREATE TABLE turn_lifecycle (id uuid);\n"},
        )

        self.assertEqual(result.returncode, 0, result.stdout)

    def test_sql_in_a_module_gated_against_test_reports(self) -> None:
        source = (
            "//! Owned.\n"
            "pub fn run() {}\n"
            "#[cfg(not(test))]\n"
            "mod production {\n"
            '    const READ: &str = "SELECT id FROM turn_lifecycle";\n'
            "}\n"
        )

        result = check(
            "SR-8",
            {APP: source, MIGRATION: "CREATE TABLE turn_lifecycle (id uuid);\n"},
        )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("turn_lifecycle", result.stdout)

    def test_app_calling_a_repository_method_passes(self) -> None:
        source = "//! Owned.\npub async fn read(store: &Store) {\n    store.turn(id).await;\n}\n"

        result = check(
            "SR-8",
            {APP: source, MIGRATION: "CREATE TABLE turn_lifecycle (id uuid);\n"},
        )

        self.assertEqual(result.returncode, 0, result.stdout)

    def test_app_locking_a_table_reports(self) -> None:
        source = (
            "//! Owned.\n"
            'pub const HOLD: &str = "LOCK TABLE turn_lifecycle IN SHARE MODE";\n'
        )

        result = check(
            "SR-8",
            {APP: source, MIGRATION: "CREATE TABLE turn_lifecycle (id uuid);\n"},
        )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("turn_lifecycle", result.stdout)

    def test_app_ddl_naming_a_table_reports(self) -> None:
        source = (
            "//! Owned.\n"
            'pub const RESET: &str = "ALTER TABLE turn_lifecycle DROP CONSTRAINT c";\n'
        )

        result = check(
            "SR-8",
            {APP: source, MIGRATION: "CREATE TABLE turn_lifecycle (id uuid);\n"},
        )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("turn_lifecycle", result.stdout)

    def test_app_truncating_a_table_reports(self) -> None:
        source = "//! Owned.\npub const CLEAR: &str = \"TRUNCATE turn_lifecycle\";\n"

        result = check(
            "SR-8",
            {APP: source, MIGRATION: "CREATE TABLE turn_lifecycle (id uuid);\n"},
        )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("turn_lifecycle", result.stdout)

    def test_the_diagnostic_binary_is_excepted(self) -> None:
        source = (
            "//! Owned.\n"
            'pub const READ: &str = "SELECT id FROM turn_lifecycle";\n'
        )

        result = check(
            "SR-8",
            {
                "apps/signalboxd/src/bin/signalbox-debug.rs": source,
                MIGRATION: "CREATE TABLE turn_lifecycle (id uuid);\n",
            },
        )

        self.assertEqual(result.returncode, 0, result.stdout)

    def test_a_quoted_query_inside_a_comment_passes(self) -> None:
        source = (
            "//! Owned.\n"
            '// Reads the row the old "SELECT id FROM turn_lifecycle" read,\n'
            "// through the projection instead.\n"
            "pub fn read() {}\n"
        )

        result = check(
            "SR-8",
            {APP: source, MIGRATION: "CREATE TABLE turn_lifecycle (id uuid);\n"},
        )

        self.assertEqual(result.returncode, 0, result.stdout)


class DocumentedConfigurationTests(unittest.TestCase):
    def test_undocumented_clap_argument_reports(self) -> None:
        source = (
            "//! Owned.\n"
            "use clap::Parser;\n"
            "struct Arguments {\n"
            "    #[arg(long)]\n"
            "    confidence: u16,\n"
            "}\n"
        )

        result = check("SR-12", {MODULE: source})

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn(MODULE, result.stdout)

    def test_documented_clap_argument_passes(self) -> None:
        source = (
            "//! Owned.\n"
            "use clap::Parser;\n"
            "struct Arguments {\n"
            "    /// Confidence in basis points; omitting it records none.\n"
            "    #[arg(long, value_name = \"BASIS_POINTS\")]\n"
            "    confidence: u16,\n"
            "}\n"
        )

        result = check("SR-12", {MODULE: source})

        self.assertEqual(result.returncode, 0, result.stdout)

    def test_a_doc_attribute_documents_a_clap_argument(self) -> None:
        source = (
            "//! Owned.\n"
            "use clap::Parser;\n"
            "struct Arguments {\n"
            '    #[doc = "Confidence in basis points."]\n'
            "    #[arg(long)]\n"
            "    confidence: u16,\n"
            "}\n"
        )

        result = check("SR-12", {MODULE: source})

        self.assertEqual(result.returncode, 0, result.stdout)

    def test_a_block_doc_comment_documents_a_clap_argument(self) -> None:
        source = (
            "//! Owned.\n"
            "use clap::Parser;\n"
            "struct Arguments {\n"
            "    /** Confidence in basis points.\n"
            "     * Omitting it records none.\n"
            "     */\n"
            "    #[arg(long)]\n"
            "    confidence: u16,\n"
            "}\n"
        )

        result = check("SR-12", {MODULE: source})

        self.assertEqual(result.returncode, 0, result.stdout)

    def test_a_plain_block_comment_does_not_document_a_clap_argument(self) -> None:
        source = (
            "//! Owned.\n"
            "use clap::Parser;\n"
            "struct Arguments {\n"
            "    /* Confidence in basis points. */\n"
            "    #[arg(long)]\n"
            "    confidence: u16,\n"
            "}\n"
        )

        result = check("SR-12", {MODULE: source})

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn(MODULE, result.stdout)

    def test_undocumented_value_enum_variant_reports(self) -> None:
        source = (
            "//! Owned.\n"
            "use clap::ValueEnum;\n"
            "#[derive(Clone, ValueEnum)]\n"
            "enum Side {\n"
            "    Head,\n"
            "}\n"
        )

        result = check("SR-12", {MODULE: source})

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("Head", result.stdout)


class ProcMacroSpanTests(unittest.TestCase):
    def test_call_site_span_in_a_proc_macro_crate_reports(self) -> None:
        files = {
            DERIVE_MANIFEST: PROC_MACRO_MANIFEST,
            DERIVE_MODULE: (
                "//! Owned.\n"
                "pub fn expand() {\n"
                "    return Err(syn::Error::new(Span::call_site(), \"duplicate\"));\n"
                "}\n"
            ),
        }

        result = check("SR-13", files)

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn(DERIVE_MODULE, result.stdout)

    def test_the_unspaced_manifest_spelling_still_selects_the_crate(self) -> None:
        files = {
            DERIVE_MANIFEST: "[lib]\nproc-macro=true\n",
            DERIVE_MODULE: (
                "//! Owned.\n"
                "pub fn expand() {\n"
                "    return Err(syn::Error::new(Span::call_site(), \"duplicate\"));\n"
                "}\n"
            ),
        }

        result = check("SR-13", files)

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn(DERIVE_MODULE, result.stdout)

    def test_span_on_the_user_tokens_passes(self) -> None:
        files = {
            DERIVE_MANIFEST: PROC_MACRO_MANIFEST,
            DERIVE_MODULE: (
                "//! Owned.\n"
                "pub fn expand() {\n"
                "    return Err(syn::Error::new_spanned(literal, \"duplicate\"));\n"
                "}\n"
            ),
        }

        result = check("SR-13", files)

        self.assertEqual(result.returncode, 0, result.stdout)

    def test_a_generated_token_may_carry_the_call_site(self) -> None:
        files = {
            DERIVE_MANIFEST: PROC_MACRO_MANIFEST,
            DERIVE_MODULE: (
                "//! Owned.\n"
                "pub fn expand() {\n"
                '    let helper = Ident::new("helper", Span::call_site());\n'
                "}\n"
            ),
        }

        result = check("SR-13", files)

        self.assertEqual(result.returncode, 0, result.stdout)

    def test_an_abort_macro_on_the_call_site_reports(self) -> None:
        files = {
            DERIVE_MANIFEST: PROC_MACRO_MANIFEST,
            DERIVE_MODULE: (
                "//! Owned.\n"
                "pub fn expand() {\n"
                '    abort!(Span::call_site(), "duplicate");\n'
                "}\n"
            ),
        }

        result = check("SR-13", files)

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn(DERIVE_MODULE, result.stdout)


class SourceScannerTests(unittest.TestCase):
    """The scanner every code-shaped rule reads through, checked directly."""

    def test_raw_string_contents_are_not_code(self) -> None:
        files = {
            DERIVE_MANIFEST: PROC_MACRO_MANIFEST,
            DERIVE_MODULE: (
                "//! Owned.\n"
                "pub fn render() {\n"
                "    let text = r#\"Span::call_site()\"#;\n"
                "}\n"
            ),
        }

        result = check("SR-13", files)

        self.assertEqual(result.returncode, 0, result.stdout)

    def test_a_lifetime_is_not_a_character_literal(self) -> None:
        # The diagnostic sits between two lifetimes: a scanner that read the
        # first apostrophe as opening a character literal would blank it away
        # and report nothing.
        files = {
            DERIVE_MANIFEST: PROC_MACRO_MANIFEST,
            DERIVE_MODULE: (
                "//! Owned.\n"
                "pub fn expand<'a>(tokens: &'a str) {\n"
                "    let _ = syn::Error::new(Span::call_site(), tokens);\n"
                "}\n"
                "pub fn again<'b>() {}\n"
            ),
        }

        result = check("SR-13", files)

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn(DERIVE_MODULE, result.stdout)

    def test_a_commented_out_declaration_is_not_code(self) -> None:
        files = {
            DERIVE_MANIFEST: PROC_MACRO_MANIFEST,
            DERIVE_MODULE: (
                "//! Owned.\n"
                "// let _ = syn::Error::new(Span::call_site(), tokens);\n"
                "pub fn expand() {}\n"
            ),
        }

        result = check("SR-13", files)

        self.assertEqual(result.returncode, 0, result.stdout)


class CommandLineTests(unittest.TestCase):
    def test_an_unknown_rule_name_is_refused(self) -> None:
        result = check("SR-99", {MODULE: "//! Owned.\n"})

        self.assertEqual(result.returncode, 2, result.stdout)
        self.assertIn("SR-99", result.stderr)


if __name__ == "__main__":
    unittest.main()

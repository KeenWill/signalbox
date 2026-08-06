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
TEST_TARGET = "crates/example/tests/end_to_end.rs"
SWIFT = "clients/native/Sources/Example/Example.swift"
MIGRATION = "crates/example/migrations/202601010001_first.sql"


class FileDocCommentTests(unittest.TestCase):
    def test_module_without_a_file_doc_comment_reports(self) -> None:
        result = check("SR-1", {MODULE: "pub fn run() {}\n"})

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn(MODULE, result.stdout)

    def test_module_doc_comment_after_inner_attributes_passes(self) -> None:
        source = "#![allow(dead_code)]\n//! What this module owns.\npub fn run() {}\n"

        result = check("SR-1", {MODULE: source})

        self.assertEqual(result.returncode, 0, result.stdout)

    def test_swift_file_without_a_doc_comment_reports(self) -> None:
        result = check("SR-1", {MODULE: "//! Owned.\n", SWIFT: "import Foundation\n"})

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn(SWIFT, result.stdout)

    def test_integration_test_target_without_a_doc_comment_reports(self) -> None:
        files = {MODULE: "//! Owned.\n", TEST_TARGET: "#[test]\nfn runs() {}\n"}

        result = check("SR-1", files)

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn(TEST_TARGET, result.stdout)

    def test_documented_integration_test_target_passes(self) -> None:
        files = {
            MODULE: "//! Owned.\n",
            TEST_TARGET: "//! What this target proves.\n#[test]\nfn runs() {}\n",
        }

        result = check("SR-1", files)

        self.assertEqual(result.returncode, 0, result.stdout)


class CommentProvenanceTests(unittest.TestCase):
    def test_comment_citing_a_process_document_reports(self) -> None:
        source = "//! Owned.\n/// Derived per `docs/agents/testing-style.md`.\npub fn run() {}\n"

        result = check("SR-2", {MODULE: source})

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("docs/agents/", result.stdout)

    def test_comment_citing_a_numbered_rule_reports(self) -> None:
        source = "//! Owned.\n// The seed is decorrelated (rule 4).\npub fn run() {}\n"

        result = check("SR-2", {MODULE: source})

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("rule 4", result.stdout)

    def test_comment_stating_the_constraint_itself_passes(self) -> None:
        source = "//! Owned.\n// The seed is decorrelated from the acceptance ordinal.\npub fn run() {}\n"

        result = check("SR-2", {MODULE: source})

        self.assertEqual(result.returncode, 0, result.stdout)

    def test_backticked_version_date_is_a_value_not_a_citation(self) -> None:
        source = "//! Owned.\n/// The wire version is `2023-06-01`.\npub fn run() {}\n"

        result = check("SR-2", {MODULE: source})

        self.assertEqual(result.returncode, 0, result.stdout)


class SingleTypeSpellingTests(unittest.TestCase):
    def test_type_both_imported_and_crate_qualified_reports(self) -> None:
        source = (
            "//! Owned.\n"
            "use signalbox_domain::SessionId;\n"
            "pub fn run(id: signalbox_domain::SessionId) -> SessionId { id }\n"
        )

        result = check("SR-3", {MODULE: source})

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("signalbox_domain::SessionId", result.stdout)

    def test_one_spelling_per_type_passes(self) -> None:
        source = (
            "//! Owned.\n"
            "use signalbox_domain::SessionId;\n"
            "pub fn run(id: SessionId) -> SessionId { id }\n"
        )

        result = check("SR-3", {MODULE: source})

        self.assertEqual(result.returncode, 0, result.stdout)

    def test_same_name_from_another_crate_is_a_disambiguation(self) -> None:
        source = (
            "//! Owned.\n"
            "use signalbox_domain::SessionId;\n"
            "pub fn run(other: signalbox_persistence::SessionId) -> SessionId { }\n"
        )

        result = check("SR-3", {MODULE: source})

        self.assertEqual(result.returncode, 0, result.stdout)


class FailureTypeRenderingTests(unittest.TestCase):
    def test_public_failure_type_without_display_reports(self) -> None:
        source = "//! Owned.\npub enum LoadError {\n    Missing,\n}\n"

        result = check("SR-4", {MODULE: source})

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("LoadError", result.stdout)

    def test_failure_type_with_both_impls_passes(self) -> None:
        source = (
            "//! Owned.\n"
            "pub enum LoadError {\n    Missing,\n}\n"
            "impl fmt::Display for LoadError {\n    fn fmt(&self) {}\n}\n"
            "impl std::error::Error for LoadError {}\n"
        )

        result = check("SR-4", {MODULE: source})

        self.assertEqual(result.returncode, 0, result.stdout)

    def test_impl_in_a_sibling_module_of_the_same_crate_counts(self) -> None:
        declaration = "//! Owned.\npub enum LoadError {\n    Missing,\n}\n"
        implementation = (
            "//! Owned.\n"
            "impl Display for LoadError {\n    fn fmt(&self) {}\n}\n"
            "impl Error for LoadError {}\n"
        )

        result = check(
            "SR-4",
            {MODULE: declaration, "crates/example/src/render.rs": implementation},
        )

        self.assertEqual(result.returncode, 0, result.stdout)


class PublicBooleanAxisTests(unittest.TestCase):
    def test_public_boolean_field_reports(self) -> None:
        source = "//! Owned.\npub struct Request {\n    pub archived: bool,\n}\n"

        result = check("SR-5", {"crates/domain/src/lib.rs": source})

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("archived", result.stdout)

    def test_public_boolean_parameter_reports(self) -> None:
        source = "//! Owned.\npub fn try_new(archived: bool) -> Self {}\n"

        result = check("SR-5", {"crates/application/src/lib.rs": source})

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("try_new", result.stdout)

    def test_two_variant_enum_for_the_axis_passes(self) -> None:
        source = (
            "//! Owned.\n"
            "pub enum Archival {\n    Archived,\n    Live,\n}\n"
            "pub fn try_new(archival: Archival) -> Self {}\n"
        )

        result = check("SR-5", {"crates/domain/src/lib.rs": source})

        self.assertEqual(result.returncode, 0, result.stdout)


class AnonymousRowDecodingTests(unittest.TestCase):
    def test_anonymous_tuple_projection_reports(self) -> None:
        source = (
            "//! Owned.\n"
            "pub async fn load() {\n"
            '    let row = sqlx::query_as::<_, (Uuid, Uuid)>("SELECT a, b FROM t");\n'
            "}\n"
        )

        result = check("SR-6", {MODULE: source})

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn(MODULE, result.stdout)

    def test_named_record_projection_passes(self) -> None:
        source = (
            "//! Owned.\n"
            "pub async fn load() {\n"
            '    let row = sqlx::query_as::<_, TurnFacts>("SELECT a AS turn_id FROM t");\n'
            "}\n"
        )

        result = check("SR-6", {MODULE: source})

        self.assertEqual(result.returncode, 0, result.stdout)

    def test_optional_tuple_binding_reports(self) -> None:
        source = (
            "//! Owned.\n"
            "pub async fn load() {\n"
            '    let row: Option<(Uuid, Uuid)> = sqlx::query_as("SELECT a, b FROM t");\n'
            "}\n"
        )

        result = check("SR-6", {MODULE: source})

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn(MODULE, result.stdout)

    def test_projection_through_a_tuple_alias_reports(self) -> None:
        source = (
            "//! Owned.\n"
            "type OutboxSlotRow = (Uuid, i64);\n"
            "pub async fn load() {\n"
            '    let row: Option<OutboxSlotRow> = sqlx::query_as("SELECT a, b FROM t");\n'
            "}\n"
        )

        result = check("SR-6", {MODULE: source})

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("OutboxSlotRow", result.stdout)

    def test_one_projection_matching_both_spellings_reports_once(self) -> None:
        source = (
            "//! Owned.\n"
            "type OutboxSlotRow = (Uuid, i64);\n"
            "pub async fn load() {\n"
            "    let row: Option<OutboxSlotRow> = "
            'sqlx::query_as::<_, OutboxSlotRow>("SELECT a, b FROM t");\n'
            "}\n"
        )

        result = check("SR-6", {MODULE: source})

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertEqual(result.stdout.count("SR-6 " + MODULE), 1, result.stdout)

    def test_projection_through_a_record_alias_passes(self) -> None:
        source = (
            "//! Owned.\n"
            "type SlotRow = TurnFacts;\n"
            "pub async fn load() {\n"
            '    let row: Option<SlotRow> = sqlx::query_as("SELECT a AS turn_id FROM t");\n'
            "}\n"
        )

        result = check("SR-6", {MODULE: source})

        self.assertEqual(result.returncode, 0, result.stdout)


class StorageVersionThresholdTests(unittest.TestCase):
    def test_comparison_against_the_current_writer_version_reports(self) -> None:
        source = "//! Owned.\npub fn admits(stored: i16) -> bool {\n    stored < STORAGE_VERSION\n}\n"

        result = check("SR-7", {"crates/persistence/src/session.rs": source})

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("STORAGE_VERSION", result.stdout)

    def test_comparison_against_a_named_threshold_passes(self) -> None:
        source = (
            "//! Owned.\n"
            "const TEMPLATE_PROVENANCE_FROM_VERSION: i16 = 4;\n"
            "pub fn admits(stored: i16) -> bool {\n"
            "    stored < TEMPLATE_PROVENANCE_FROM_VERSION\n"
            "}\n"
        )

        result = check("SR-7", {"crates/persistence/src/session.rs": source})

        self.assertEqual(result.returncode, 0, result.stdout)


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

    def test_app_calling_a_repository_method_passes(self) -> None:
        source = "//! Owned.\npub async fn read(store: &Store) {\n    store.turn(id).await;\n}\n"

        result = check(
            "SR-8",
            {APP: source, MIGRATION: "CREATE TABLE turn_lifecycle (id uuid);\n"},
        )

        self.assertEqual(result.returncode, 0, result.stdout)

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


class MigrationSupersessionTests(unittest.TestCase):
    def test_unattributed_constraint_replacement_reports(self) -> None:
        source = (
            "ALTER TABLE durable_command\n"
            "    DROP CONSTRAINT durable_command_storage_version_supported;\n"
        )

        result = check("SR-9", {MIGRATION: source})

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn(MIGRATION, result.stdout)

    def test_naming_the_superseded_migration_passes(self) -> None:
        source = (
            "-- Supersedes the definition in 202601010001_first.sql.\n"
            "ALTER TABLE durable_command\n"
            "    DROP CONSTRAINT durable_command_storage_version_supported;\n"
        )

        result = check("SR-9", {MIGRATION: source})

        self.assertEqual(result.returncode, 0, result.stdout)

    def test_one_comment_attributes_every_clause_of_its_statement(self) -> None:
        source = (
            "-- Supersedes the definitions in 202601010001_first.sql.\n"
            "ALTER TABLE durable_command\n"
            "    ADD COLUMN source_turn_id uuid,\n"
            "    ALTER COLUMN defaults_version DROP NOT NULL,\n"
            "    ALTER COLUMN requested_kind DROP NOT NULL,\n"
            "    ALTER COLUMN frozen_kind DROP NOT NULL,\n"
            "    DROP CONSTRAINT durable_command_defaults_version_positive,\n"
            "    DROP CONSTRAINT durable_command_requested_kind_shape;\n"
        )

        result = check("SR-9", {MIGRATION: source})

        self.assertEqual(result.returncode, 0, result.stdout)

    def test_a_comment_bound_to_the_previous_statement_does_not_attribute(self) -> None:
        source = (
            "-- Supersedes the definition in 202601010001_first.sql.\n"
            "CREATE INDEX durable_command_kind ON durable_command (command_kind);\n"
            "ALTER TABLE durable_command\n"
            "    DROP CONSTRAINT durable_command_kind_closed;\n"
        )

        result = check("SR-9", {MIGRATION: source})

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn(MIGRATION, result.stdout)

    def test_disagreeing_lexical_and_numeric_order_reports(self) -> None:
        files = {
            "crates/example/migrations/9_early.sql": "SELECT 1;\n",
            "crates/example/migrations/10_late.sql": "SELECT 1;\n",
        }

        result = check("SR-9", files)

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("ordering disagree", result.stdout)


class AdjacentParameterTypeTests(unittest.TestCase):
    def test_two_adjacent_parameters_of_one_type_report(self) -> None:
        source = "//! Owned.\npub fn record(run: CanonicalUuid, pass: CanonicalUuid) {}\n"

        result = check("SR-10", {MODULE: source})

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("record", result.stdout)

    def test_distinct_newtypes_pass(self) -> None:
        source = "//! Owned.\npub fn record(run: RunId, pass: PassId) {}\n"

        result = check("SR-10", {MODULE: source})

        self.assertEqual(result.returncode, 0, result.stdout)

    def test_generic_parameters_are_one_parameter_each(self) -> None:
        source = (
            "//! Owned.\n"
            "pub fn record(keys: HashMap<RunId, PassId>, seed: Seed) {}\n"
        )

        result = check("SR-10", {MODULE: source})

        self.assertEqual(result.returncode, 0, result.stdout)


class FunctionBodyLengthTests(unittest.TestCase):
    def test_body_past_the_ceiling_reports(self) -> None:
        body = "    let value = 1;\n" * 401
        source = f"//! Owned.\npub fn reconstitute() {{\n{body}}}\n"

        result = check("SR-11", {MODULE: source})

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("reconstitute", result.stdout)

    def test_body_within_the_ceiling_passes(self) -> None:
        body = "    let value = 1;\n" * 50
        source = f"//! Owned.\npub fn reconstitute() {{\n{body}}}\n"

        result = check("SR-11", {MODULE: source})

        self.assertEqual(result.returncode, 0, result.stdout)

    def test_a_brace_inside_a_string_does_not_extend_a_body(self) -> None:
        body = "    let value = 1;\n" * 50
        source = f'//! Owned.\npub fn render() {{\n    let text = "{{";\n{body}}}\n'

        result = check("SR-11", {MODULE: source})

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
            "crates/derive/Cargo.toml": "[lib]\nproc-macro = true\n",
            "crates/derive/src/lib.rs": (
                "//! Owned.\n"
                "pub fn expand() {\n"
                "    return Err(syn::Error::new(Span::call_site(), \"duplicate\"));\n"
                "}\n"
            ),
        }

        result = check("SR-13", files)

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("crates/derive/src/lib.rs", result.stdout)

    def test_span_on_the_user_tokens_passes(self) -> None:
        files = {
            "crates/derive/Cargo.toml": "[lib]\nproc-macro = true\n",
            "crates/derive/src/lib.rs": (
                "//! Owned.\n"
                "pub fn expand() {\n"
                "    return Err(syn::Error::new_spanned(literal, \"duplicate\"));\n"
                "}\n"
            ),
        }

        result = check("SR-13", files)

        self.assertEqual(result.returncode, 0, result.stdout)


class SourceScannerTests(unittest.TestCase):
    """The scanner every code-shaped rule reads through, checked directly."""

    def test_raw_string_contents_are_not_code(self) -> None:
        source = (
            "//! Owned.\n"
            "pub fn render() {\n"
            "    let text = r#\"pub fn record(a: Id, b: Id) {}\"#;\n"
            "}\n"
        )

        result = check("SR-10", {MODULE: source})

        self.assertEqual(result.returncode, 0, result.stdout)

    def test_a_lifetime_is_not_a_character_literal(self) -> None:
        source = (
            "//! Owned.\n"
            "pub fn record<'a>(first: &'a Id, second: &'a Id) {}\n"
        )

        result = check("SR-10", {MODULE: source})

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("record", result.stdout)

    def test_a_commented_out_declaration_is_not_code(self) -> None:
        source = "//! Owned.\n// pub fn record(a: Id, b: Id) {}\npub fn run() {}\n"

        result = check("SR-10", {MODULE: source})

        self.assertEqual(result.returncode, 0, result.stdout)


class CommandLineTests(unittest.TestCase):
    def test_an_unknown_rule_name_is_refused(self) -> None:
        result = check("SR-99", {MODULE: "//! Owned.\n"})

        self.assertEqual(result.returncode, 2, result.stdout)
        self.assertIn("SR-99", result.stderr)


if __name__ == "__main__":
    unittest.main()

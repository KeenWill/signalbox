#!/usr/bin/env python3
"""Prove the numeric-bound inventory gates declarations and derived escapes."""

from __future__ import annotations

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

CHECKER = Path(__file__).resolve().parent / "check_numeric_bounds.py"
ENFORCED_FILE = Path("crates/application/src/lib.rs")
ENFORCED_MODULE_FILE = Path("crates/application/src/scheduler.rs")
OUTSIDE_FILE = Path("crates/domain/src/lib.rs")
DAEMON_FILE = Path("apps/signalboxd/src/lib.rs")
PERSISTENCE_FILE = Path("crates/persistence/src/lib.rs")
PREEXISTING_UNCLASSIFIED_FILE = Path(
    "apps/signalboxd/src/blob_storage_configuration.rs"
)


def run_checker_tree(sources: dict[Path, str]) -> subprocess.CompletedProcess[str]:
    with tempfile.TemporaryDirectory(prefix="signalbox-numeric-bounds-") as directory:
        root = Path(directory)
        for relative, text in sources.items():
            fixture = root / relative
            fixture.parent.mkdir(parents=True, exist_ok=True)
            fixture.write_text(text, encoding="utf-8")
        return subprocess.run(
            [sys.executable, str(CHECKER), "--root", str(root)],
            check=False,
            capture_output=True,
            text=True,
        )


def run_checker(path: Path, text: str) -> subprocess.CompletedProcess[str]:
    return run_checker_tree({path: text})


class NumericBoundCheckerTests(unittest.TestCase):
    def test_direct_guard_declaration_passes(self) -> None:
        result = run_checker(
            ENFORCED_FILE,
            "// numeric-bound: guard - protects against retained input growth\n"
            "const MAX_INPUT_BYTES: usize = 1024;\n",
        )

        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("1 enforced", result.stdout)

    def test_direct_ceiling_declaration_fails(self) -> None:
        result = run_checker(
            ENFORCED_FILE,
            "// numeric-bound: ceiling - protects against retained input growth\n"
            "const MAX_INPUT_BYTES: usize = 1024;\n",
        )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("no numeric-bound declaration", result.stdout)

    def test_direct_tunable_declaration_fails(self) -> None:
        result = run_checker(
            ENFORCED_FILE,
            "// numeric-bound: tunable - controls the ordinary wait\n"
            "const DEFAULT_TIMEOUT: Duration = Duration::from_secs(1);\n",
        )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("no numeric-bound declaration", result.stdout)

    def test_direct_interval_declaration_fails(self) -> None:
        result = run_checker(
            ENFORCED_FILE,
            "// numeric-bound: interval - controls the ordinary wait\n"
            "const DEFAULT_TIMEOUT: Duration = Duration::from_secs(1);\n",
        )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("no numeric-bound declaration", result.stdout)

    def test_missing_declaration_fails_with_location_and_name(self) -> None:
        result = run_checker(ENFORCED_FILE, "const MAX_INPUT_BYTES: usize = 1024;\n")

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("crates/application/src/lib.rs:1", result.stdout)
        self.assertIn("MAX_INPUT_BYTES", result.stdout)

    def test_qualified_duration_declaration_is_enforced(self) -> None:
        result = run_checker(
            ENFORCED_FILE,
            "const MAX_WAIT: std::time::Duration = std::time::Duration::from_secs(1);\n",
        )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("MAX_WAIT", result.stdout)

    def test_signed_non_zero_declaration_is_enforced(self) -> None:
        result = run_checker(ENFORCED_FILE, "const MAX_DELTA: NonZeroI64 = NonZeroI64::MAX;\n")

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("MAX_DELTA", result.stdout)

    def test_valid_derived_guard_inherits_the_source_rationale(self) -> None:
        result = run_checker(
            ENFORCED_FILE,
            "// numeric-bound: guard - protects against oversized text\n"
            "const MAX_INPUT_CHARACTERS: usize = 1024;\n"
            "// numeric-bound: derived guard from MAX_INPUT_CHARACTERS\n"
            "const MAX_INPUT_BYTES: usize = MAX_INPUT_CHARACTERS * 4;\n",
        )

        self.assertEqual(result.returncode, 0, result.stdout)

    def test_derived_ceiling_declaration_fails(self) -> None:
        result = run_checker(
            ENFORCED_FILE,
            "// numeric-bound: guard - protects against oversized text\n"
            "const MAX_INPUT_CHARACTERS: usize = 1024;\n"
            "// numeric-bound: derived ceiling from MAX_INPUT_CHARACTERS\n"
            "const MAX_INPUT_BYTES: usize = MAX_INPUT_CHARACTERS * 4;\n",
        )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("no numeric-bound declaration", result.stdout)

    def test_daemon_bound_is_enforced(self) -> None:
        result = run_checker(DAEMON_FILE, "const MAX_INPUT_BYTES: usize = 1024;\n")

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("apps/signalboxd/src/lib.rs:1", result.stdout)
        self.assertIn("MAX_INPUT_BYTES", result.stdout)

    def test_persistence_bound_is_enforced(self) -> None:
        result = run_checker(PERSISTENCE_FILE, "const MAX_INPUT_BYTES: usize = 1024;\n")

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("crates/persistence/src/lib.rs:1", result.stdout)
        self.assertIn("MAX_INPUT_BYTES", result.stdout)

    def test_preexisting_unclassified_candidate_stays_outside_blocking_scope(self) -> None:
        result = run_checker(
            PREEXISTING_UNCLASSIFIED_FILE,
            "const MAX_S3_LOCATION_BYTES: usize = 2048;\n",
        )

        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("1 outside blocking scope", result.stdout)

    def test_non_bound_false_positive_escape_requires_a_rationale(self) -> None:
        result = run_checker(
            ENFORCED_FILE,
            "// numeric-bound: not-a-bound - fixed percentage representation\n"
            "const MAXIMUM_PERCENT: u64 = 100;\n",
        )

        self.assertEqual(result.returncode, 0, result.stdout)

    def test_non_bound_false_positive_without_rationale_fails(self) -> None:
        result = run_checker(
            ENFORCED_FILE,
            "// numeric-bound: not-a-bound -\nconst MAXIMUM_PERCENT: u64 = 100;\n",
        )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("no numeric-bound declaration", result.stdout)

    def test_derived_escape_fails_without_initializer_reference(self) -> None:
        result = run_checker(
            ENFORCED_FILE,
            "// numeric-bound: guard - protects against oversized text\n"
            "const MAX_INPUT_CHARACTERS: usize = 1024;\n"
            "// numeric-bound: derived guard from MAX_INPUT_CHARACTERS\n"
            "const MAX_INPUT_BYTES: usize = 4096;\n",
        )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("invalid derived declaration", result.stdout)

    def test_derived_escape_resolves_the_source_in_its_own_module(self) -> None:
        result = run_checker(
            ENFORCED_FILE,
            "mod first {\n"
            "    // numeric-bound: not-a-bound - fixed decimal radix\n"
            "    const MAX_BASE: usize = 10;\n"
            "    // numeric-bound: derived guard from MAX_BASE\n"
            "    const MAX_DERIVED_BYTES: usize = MAX_BASE * 4;\n"
            "}\n"
            "mod second {\n"
            "    // numeric-bound: guard - protects against oversized text\n"
            "    const MAX_BASE: usize = 1024;\n"
            "}\n",
        )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("MAX_DERIVED_BYTES", result.stdout)
        self.assertIn("invalid derived declaration", result.stdout)

    def test_derived_escape_fails_when_the_source_is_only_in_a_sibling_module(self) -> None:
        result = run_checker(
            ENFORCED_FILE,
            "mod first {\n"
            "    // numeric-bound: derived guard from MAX_BASE\n"
            "    const MAX_DERIVED_BYTES: usize = MAX_BASE * 4;\n"
            "}\n"
            "mod second {\n"
            "    // numeric-bound: guard - protects against oversized text\n"
            "    const MAX_BASE: usize = 1024;\n"
            "}\n",
        )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("MAX_DERIVED_BYTES", result.stdout)
        self.assertIn("invalid derived declaration", result.stdout)

    def test_derived_escape_resolves_a_source_from_an_enclosing_scope(self) -> None:
        result = run_checker(
            ENFORCED_FILE,
            "// numeric-bound: guard - protects against oversized text\n"
            "const MAX_BASE: usize = 1024;\n"
            "mod inner {\n"
            "    use super::MAX_BASE;\n"
            "    // numeric-bound: derived guard from MAX_BASE\n"
            "    const MAX_DERIVED_BYTES: usize = MAX_BASE * 4;\n"
            "}\n",
        )

        self.assertEqual(result.returncode, 0, result.stdout)

    def test_derived_escape_fails_when_another_referenced_bound_differs_in_kind(self) -> None:
        result = run_checker(
            ENFORCED_FILE,
            "// numeric-bound: guard - protects against oversized responses\n"
            "const MAX_RESPONSE_BYTES: usize = 1024;\n"
            "// numeric-bound: not-a-bound - fixed retained preview representation\n"
            "const MAX_PREVIEW_BYTES: usize = 64;\n"
            "// numeric-bound: derived guard from MAX_RESPONSE_BYTES\n"
            "const MAX_TOTAL_BYTES: usize = MAX_RESPONSE_BYTES + MAX_PREVIEW_BYTES;\n",
        )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("MAX_TOTAL_BYTES", result.stdout)
        self.assertIn("invalid derived declaration", result.stdout)

    def test_derived_escape_fails_when_a_contributor_cannot_be_resolved(self) -> None:
        result = run_checker(
            ENFORCED_FILE,
            "// numeric-bound: guard - protects against oversized responses\n"
            "const MAX_LOCAL_BYTES: usize = 1024;\n"
            "// numeric-bound: derived guard from MAX_LOCAL_BYTES\n"
            "const MAX_TOTAL_BYTES: usize = MAX_LOCAL_BYTES + MAX_IMPORTED_BYTES;\n",
        )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("MAX_TOTAL_BYTES", result.stdout)
        self.assertIn("invalid derived declaration", result.stdout)

    def test_derived_escape_fails_when_the_source_is_in_another_function(self) -> None:
        result = run_checker(
            ENFORCED_FILE,
            "fn first() {\n"
            "    // numeric-bound: derived guard from MAX_BASE\n"
            "    const MAX_DERIVED_BYTES: usize = MAX_BASE * 4;\n"
            "}\n"
            "fn second() {\n"
            "    // numeric-bound: guard - protects against oversized text\n"
            "    const MAX_BASE: usize = 1024;\n"
            "}\n",
        )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("MAX_DERIVED_BYTES", result.stdout)
        self.assertIn("invalid derived declaration", result.stdout)

    def test_derived_escape_fails_on_a_path_qualified_source(self) -> None:
        result = run_checker(
            ENFORCED_FILE,
            "// numeric-bound: guard - protects against oversized text\n"
            "const MAX_BASE: usize = 1024;\n"
            "// numeric-bound: derived guard from MAX_BASE\n"
            "const MAX_DERIVED_BYTES: usize = other_crate::MAX_BASE * 4;\n",
        )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("MAX_DERIVED_BYTES", result.stdout)
        self.assertIn("invalid derived declaration", result.stdout)

    def test_generic_non_zero_declaration_is_enforced(self) -> None:
        result = run_checker(
            ENFORCED_FILE,
            "const MAX_ATTEMPTS: std::num::NonZero<u32> = std::num::NonZero::<u32>::MAX;\n",
        )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("MAX_ATTEMPTS", result.stdout)

    def test_aliased_numeric_type_is_enforced(self) -> None:
        result = run_checker(
            ENFORCED_FILE,
            "type ByteCount = usize;\nconst MAX_INPUT_BYTES: ByteCount = 1024;\n",
        )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("MAX_INPUT_BYTES", result.stdout)

    def test_chained_numeric_alias_is_enforced(self) -> None:
        result = run_checker(
            ENFORCED_FILE,
            "type ByteCount = usize;\ntype Retained = ByteCount;\n"
            "const MAX_INPUT_BYTES: Retained = 1024;\n",
        )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("MAX_INPUT_BYTES", result.stdout)

    def test_non_numeric_alias_is_not_inventoried(self) -> None:
        result = run_checker(
            ENFORCED_FILE,
            "type Label = &'static str;\nconst MAX_LABEL: Label = \"limit\";\n",
        )

        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("0 enforced", result.stdout)

    def test_derived_escape_fails_when_a_nested_local_import_shadows_the_source(self) -> None:
        result = run_checker(
            ENFORCED_FILE,
            "// numeric-bound: not-a-bound - fixed ordinary retained representation\n"
            "const MAX_BASE: usize = 64;\n"
            "mod inner {\n"
            "    use super::sibling::MAX_BASE;\n"
            "    // numeric-bound: derived guard from MAX_BASE\n"
            "    const MAX_DERIVED_BYTES: usize = MAX_BASE * 4;\n"
            "}\n",
        )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("MAX_DERIVED_BYTES", result.stdout)
        self.assertIn("invalid derived declaration", result.stdout)

    def test_derived_escape_fails_on_a_qualified_repetition_of_the_source(self) -> None:
        result = run_checker(
            ENFORCED_FILE,
            "// numeric-bound: guard - protects against oversized text\n"
            "const MAX_BASE: usize = 1024;\n"
            "// numeric-bound: derived guard from MAX_BASE\n"
            "const MAX_TOTAL_BYTES: usize = MAX_BASE + other::MAX_BASE;\n",
        )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("MAX_TOTAL_BYTES", result.stdout)
        self.assertIn("invalid derived declaration", result.stdout)

    def test_derived_escape_resolves_a_source_declared_in_the_initializer(self) -> None:
        result = run_checker(
            ENFORCED_FILE,
            "// numeric-bound: derived guard from MAX_BASE\n"
            "const MAX_TOTAL_BYTES: usize = {\n"
            "    // numeric-bound: guard - protects against oversized text\n"
            "    const MAX_BASE: usize = 1024;\n"
            "    MAX_BASE * 4\n"
            "};\n",
        )

        self.assertEqual(result.returncode, 0, result.stdout)

    def test_derived_escape_fails_when_a_renaming_local_import_shadows_the_source(self) -> None:
        result = run_checker(
            ENFORCED_FILE,
            "// numeric-bound: not-a-bound - fixed ordinary retained representation\n"
            "const MAX_IMPORTED: usize = 1024;\n"
            "mod inner {\n"
            "    use super::MAX_BASE as MAX_IMPORTED;\n"
            "    // numeric-bound: derived guard from MAX_IMPORTED\n"
            "    const MAX_DERIVED_BYTES: usize = MAX_IMPORTED * 4;\n"
            "}\n",
        )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("MAX_DERIVED_BYTES", result.stdout)
        self.assertIn("invalid derived declaration", result.stdout)

    def test_derived_escape_fails_when_an_import_shadows_the_source(self) -> None:
        result = run_checker(
            ENFORCED_FILE,
            "// numeric-bound: guard - protects against oversized text\n"
            "const MAX_BASE: usize = 1024;\n"
            "mod inner {\n"
            "    use other::MAX_BASE;\n"
            "    // numeric-bound: derived guard from MAX_BASE\n"
            "    const MAX_DERIVED_BYTES: usize = MAX_BASE * 4;\n"
            "}\n",
        )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("MAX_DERIVED_BYTES", result.stdout)
        self.assertIn("invalid derived declaration", result.stdout)

    def test_initializer_free_associated_constant_is_enforced(self) -> None:
        result = run_checker(
            ENFORCED_FILE,
            "trait Bounded {\n    const MAX_BYTES: usize;\n}\n",
        )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("MAX_BYTES", result.stdout)
        self.assertIn("no numeric-bound declaration", result.stdout)

    def test_bound_inside_a_test_gated_function_is_inventoried_without_gating(self) -> None:
        result = run_checker(
            ENFORCED_FILE,
            "#[cfg(test)]\nfn fixture() {\n    const MAX_FIXTURE_BYTES: usize = 4;\n}\n",
        )

        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("1 test-only", result.stdout)

    def test_derived_escape_fails_on_a_sibling_block_inside_the_initializer(self) -> None:
        result = run_checker(
            ENFORCED_FILE,
            "// numeric-bound: not-a-bound - fixed ordinary retained representation\n"
            "const MAX_BASE: usize = 64;\n"
            "// numeric-bound: derived guard from MAX_BASE\n"
            "const MAX_TOTAL_BYTES: usize = {\n"
            "    {\n"
            "        // numeric-bound: guard - protects against oversized text\n"
            "        const MAX_BASE: usize = 1024;\n"
            "    }\n"
            "    MAX_BASE * 4\n"
            "};\n",
        )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("MAX_TOTAL_BYTES", result.stdout)
        self.assertIn("invalid derived declaration", result.stdout)

    def test_built_in_test_function_bound_is_inventoried_without_gating(self) -> None:
        result = run_checker(
            ENFORCED_FILE,
            "#[test]\nfn fixture() {\n    const MAX_FIXTURE_BYTES: usize = 4;\n}\n",
        )

        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("1 test-only", result.stdout)

    def test_wrapped_test_configuration_is_inventoried_without_gating(self) -> None:
        result = run_checker(
            ENFORCED_FILE,
            "#[cfg(all(\n    test,\n    unix,\n))]\nconst MAX_FIXTURE_BYTES: usize = 4;\n",
        )

        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("1 test-only", result.stdout)

    def test_binary_root_test_module_bound_is_inventoried_without_gating(self) -> None:
        result = run_checker_tree(
            {
                Path("crates/application/src/bin/tool/main.rs"): "#[cfg(test)]\nmod tests;\n",
                Path("crates/application/src/bin/tool/tests.rs"): (
                    "const MAX_FIXTURE_BYTES: usize = 4;\n"
                ),
            }
        )

        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("1 test-only", result.stdout)

    def test_derived_escape_fails_without_a_value_use_of_the_source(self) -> None:
        result = run_checker(
            ENFORCED_FILE,
            "// numeric-bound: derived guard from MAX_BASE\n"
            "const MAX_TOTAL_BYTES: usize = {\n"
            "    // numeric-bound: guard - protects against oversized text\n"
            "    const MAX_BASE: usize = 1024;\n"
            "    7\n"
            "};\n",
        )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("MAX_TOTAL_BYTES", result.stdout)
        self.assertIn("invalid derived declaration", result.stdout)

    def test_alias_declared_twice_still_inventories_the_bound(self) -> None:
        result = run_checker(
            ENFORCED_FILE,
            "mod first {\n"
            "    type Count = usize;\n"
            "    const MAX_INPUT_BYTES: Count = 1024;\n"
            "}\n"
            "mod second {\n"
            "    type Count = bool;\n"
            "}\n",
        )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("MAX_INPUT_BYTES", result.stdout)

    def test_item_level_test_configuration_is_inventoried_without_gating(self) -> None:
        result = run_checker(
            ENFORCED_FILE,
            "#[cfg(test)]\nconst MAX_FIXTURE_BYTES: usize = 4;\n",
        )

        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("1 test-only", result.stdout)

    def test_path_attributed_test_module_inside_an_inline_module_does_not_gate(self) -> None:
        # rustc resolves a `#[path]` inside an inline module under that
        # module's directory; verified against the compiler.
        result = run_checker_tree(
            {
                ENFORCED_FILE: (
                    'mod outer {\n    #[cfg(test)]\n    #[path = "fixture.rs"]\n'
                    "    mod tests;\n}\n"
                ),
                ENFORCED_FILE.with_name("outer") / "fixture.rs": (
                    "const MAX_FIXTURE_BYTES: usize = 4;\n"
                ),
            }
        )

        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("1 test-only", result.stdout)

    def test_conventional_root_name_inside_a_test_tree_does_not_gate(self) -> None:
        result = run_checker_tree(
            {
                ENFORCED_FILE: "#[cfg(test)]\nmod tests;\n",
                ENFORCED_FILE.with_name("tests.rs"): "mod main;\n",
                ENFORCED_FILE.with_name("tests") / "main.rs": (
                    "const MAX_FIXTURE_BYTES: usize = 4;\n"
                ),
            }
        )

        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("1 test-only", result.stdout)

    def test_external_child_of_an_inline_test_module_does_not_gate(self) -> None:
        result = run_checker_tree(
            {
                ENFORCED_FILE: "#[cfg(test)]\nmod tests {\n    mod child;\n}\n",
                ENFORCED_FILE.with_name("tests") / "child.rs": (
                    "const MAX_FIXTURE_BYTES: usize = 4;\n"
                ),
            }
        )

        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("1 test-only", result.stdout)

    def test_test_module_bound_is_inventoried_without_gating(self) -> None:
        result = run_checker(
            ENFORCED_FILE,
            "#[cfg(test)]\nmod tests {\n    const MAX_FIXTURE_BYTES: usize = 4;\n}\n",
        )

        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("1 test-only", result.stdout)

    def test_compound_cfg_test_module_bound_is_inventoried_without_gating(self) -> None:
        result = run_checker(
            ENFORCED_FILE,
            "#[cfg(all(test, unix))]\n"
            "mod tests {\n"
            "    const MAX_FIXTURE_BYTES: usize = 4;\n"
            "}\n",
        )

        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("1 test-only", result.stdout)

    def test_attributed_cfg_test_module_bound_is_inventoried_without_gating(self) -> None:
        result = run_checker(
            ENFORCED_FILE,
            "#[cfg(test)]\n"
            "#[allow(clippy::items_after_statements)]\n"
            "mod tests {\n"
            "    const MAX_FIXTURE_BYTES: usize = 4;\n"
            "}\n",
        )

        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("1 test-only", result.stdout)

    def test_negated_member_of_a_test_module_configuration_does_not_gate(self) -> None:
        result = run_checker(
            ENFORCED_FILE,
            "#[cfg(all(test, not(windows)))]\n"
            "mod tests {\n"
            "    const MAX_FIXTURE_BYTES: usize = 4;\n"
            "}\n",
        )

        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("1 test-only", result.stdout)

    def test_optionally_configured_test_module_bound_still_gates(self) -> None:
        result = run_checker(
            ENFORCED_FILE,
            '#[cfg(any(test, feature = "fixtures"))]\n'
            "mod tests {\n"
            "    const MAX_FIXTURE_BYTES: usize = 4;\n"
            "}\n",
        )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("MAX_FIXTURE_BYTES", result.stdout)

    def test_external_test_module_bound_is_inventoried_without_gating(self) -> None:
        result = run_checker_tree(
            {
                ENFORCED_FILE: "#[cfg(test)]\nmod tests;\n",
                ENFORCED_FILE.with_name("tests.rs"): (
                    "const MAX_FIXTURE_BYTES: usize = 4;\n"
                ),
            }
        )

        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("1 test-only", result.stdout)

    def test_path_attribute_test_module_bound_is_inventoried_without_gating(self) -> None:
        result = run_checker_tree(
            {
                ENFORCED_FILE: "mod scheduler;\n",
                ENFORCED_MODULE_FILE: (
                    '#[cfg(test)]\n#[path = "scheduler_corpus_tests.rs"]\nmod corpus_tests;\n'
                ),
                ENFORCED_MODULE_FILE.with_name("scheduler_corpus_tests.rs"): (
                    "const MAX_FIXTURE_BYTES: usize = 4;\n"
                ),
            }
        )

        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("1 test-only", result.stdout)

    def test_child_of_a_path_attributed_test_module_does_not_gate(self) -> None:
        # rustc resolves a `#[path]`-loaded module's children beside that file,
        # not beneath its stem; verified against the compiler.
        result = run_checker_tree(
            {
                ENFORCED_FILE: "mod scheduler;\n",
                ENFORCED_MODULE_FILE: (
                    '#[cfg(test)]\n#[path = "scheduler_corpus_tests.rs"]\nmod corpus_tests;\n'
                ),
                ENFORCED_MODULE_FILE.with_name("scheduler_corpus_tests.rs"): "mod child;\n",
                ENFORCED_MODULE_FILE.with_name("child.rs"): (
                    "const MAX_FIXTURE_BYTES: usize = 4;\n"
                ),
            }
        )

        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("1 test-only", result.stdout)

    def test_test_module_under_an_inline_module_does_not_gate(self) -> None:
        result = run_checker_tree(
            {
                ENFORCED_FILE: "mod outer {\n    #[cfg(test)]\n    mod tests;\n}\n",
                ENFORCED_FILE.with_name("outer") / "tests.rs": (
                    "const MAX_FIXTURE_BYTES: usize = 4;\n"
                ),
            }
        )

        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("1 test-only", result.stdout)

    def test_module_reachable_outside_a_test_module_still_gates(self) -> None:
        result = run_checker_tree(
            {
                ENFORCED_FILE: "#[cfg(test)]\nmod tests;\nmod shared;\n",
                ENFORCED_FILE.with_name("tests.rs"): "",
                ENFORCED_FILE.with_name("shared.rs"): "const MAX_SHARED_BYTES: usize = 4;\n",
            }
        )

        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("MAX_SHARED_BYTES", result.stdout)

    def test_outside_scope_bound_is_inventoried_without_gating(self) -> None:
        result = run_checker(OUTSIDE_FILE, "const MAX_DOMAIN_BYTES: usize = 4;\n")

        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("1 outside blocking scope", result.stdout)

    def test_raw_string_lookalike_is_not_inventoried(self) -> None:
        result = run_checker(
            ENFORCED_FILE,
            'const EXAMPLE: &str = r#"\nconst MAX_FAKE_BYTES: usize = 4;\n"#;\n',
        )

        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("0 enforced", result.stdout)


if __name__ == "__main__":
    unittest.main()

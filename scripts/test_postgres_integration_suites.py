#!/usr/bin/env python3
"""Exercise manifest validation, Bazel workflow agreement, and documented commands."""

from __future__ import annotations

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.dont_write_bytecode = True

from postgres_integration_suites import (
    ManifestError,
    Suite,
    cargo_test_arguments,
    documentation_disagreements,
    documented_ignored_commands,
    invokes_reader,
    manifest_line,
    parse_suites,
    run_matrix,
    runs_ignored_tests,
    simple_commands,
    workflow_disagreements,
    workflow_shell_commands,
)

READER = Path(__file__).absolute().parent / "postgres_integration_suites.py"
ROOT = Path(__file__).absolute().parent.parent

VALID = """
[[suite]]
name = "persistence"
package = "signalbox-persistence"
features = ["postgres-integration"]
shards = 3
skip = []

[[suite]]
name = "terminal-client"
package = "signalbox-client"
features = []
shards = 1
skip = ["a_live_credential_smoke"]
"""


def run_reader(*arguments: str) -> subprocess.CompletedProcess:
    """Run the reader's command-line surface outside test bodies."""
    return subprocess.run(
        [sys.executable, str(READER), *arguments],
        capture_output=True,
        text=True,
    )


def suite(**overrides: object) -> Suite:
    """Build one suite, defaulted, so a case states only what it varies."""
    fields: dict[str, object] = {
        "name": "fixture",
        "package": "fixture-package",
        "features": (),
        "shards": 1,
        "skip": (),
        "include_binaries": (),
        "exclude_binaries": (),
    }
    fields.update(overrides)
    return Suite(**fields)  # type: ignore[arg-type]


class ManifestParsingTests(unittest.TestCase):
    def test_valid_manifest_parses_in_file_order(self) -> None:
        suites = parse_suites(VALID)

        self.assertEqual([entry.name for entry in suites], ["persistence", "terminal-client"])
        self.assertEqual(suites[0].package, "signalbox-persistence")
        self.assertEqual(suites[0].features, ("postgres-integration",))
        self.assertEqual(suites[0].shards, 3)
        self.assertEqual(suites[0].include_binaries, ())
        self.assertEqual(suites[0].exclude_binaries, ())
        self.assertEqual(suites[1].skip, ("a_live_credential_smoke",))

    def test_absent_optional_fields_default_to_empty(self) -> None:
        suites = parse_suites('[[suite]]\nname = "a"\npackage = "b"\nshards = 1\n')

        self.assertEqual(suites[0].features, ())
        self.assertEqual(suites[0].skip, ())
        self.assertEqual(suites[0].include_binaries, ())
        self.assertEqual(suites[0].exclude_binaries, ())

    def test_manifest_line_locates_a_suite_for_diagnostics(self) -> None:
        self.assertEqual(manifest_line(VALID, "terminal-client"), 10)

    def test_unparseable_toml_is_rejected(self) -> None:
        with self.assertRaises(ManifestError):
            parse_suites("[[suite]\nname =\n")

    def test_manifest_without_suites_is_rejected(self) -> None:
        with self.assertRaises(ManifestError):
            parse_suites("# nothing here\n")

    def test_unknown_top_level_key_is_rejected(self) -> None:
        with self.assertRaises(ManifestError) as raised:
            parse_suites(VALID + '\n[extra]\nvalue = 1\n')

        self.assertIn("extra", str(raised.exception))

    def test_unknown_suite_key_is_rejected(self) -> None:
        with self.assertRaises(ManifestError) as raised:
            parse_suites(
                '[[suite]]\nname = "a"\npackage = "b"\nshards = 1\ncommand = "c"\n'
            )

        self.assertIn("command", str(raised.exception))

    def test_uppercase_suite_name_is_rejected(self) -> None:
        with self.assertRaises(ManifestError):
            parse_suites('[[suite]]\nname = "A"\npackage = "b"\nshards = 1\n')

    def test_duplicate_suite_name_is_rejected(self) -> None:
        with self.assertRaises(ManifestError) as raised:
            parse_suites(
                '[[suite]]\nname = "a"\npackage = "b"\nshards = 1\n'
                '[[suite]]\nname = "a"\npackage = "c"\nshards = 1\n'
            )

        self.assertIn("twice", str(raised.exception))

    def test_missing_package_is_rejected(self) -> None:
        with self.assertRaises(ManifestError):
            parse_suites('[[suite]]\nname = "a"\nshards = 1\n')

    def test_non_string_feature_is_rejected(self) -> None:
        with self.assertRaises(ManifestError):
            parse_suites(
                '[[suite]]\nname = "a"\npackage = "b"\nshards = 1\nfeatures = [1]\n'
            )

    def test_zero_shards_is_rejected(self) -> None:
        with self.assertRaises(ManifestError) as raised:
            parse_suites('[[suite]]\nname = "a"\npackage = "b"\nshards = 0\n')

        self.assertIn("shards", str(raised.exception))

    def test_boolean_shards_is_rejected(self) -> None:
        # TOML booleans are Python ints; `shards = true` would otherwise read
        # as one shard rather than as the mistake it is.
        with self.assertRaises(ManifestError):
            parse_suites('[[suite]]\nname = "a"\npackage = "b"\nshards = true\n')

    def test_blank_skip_term_is_rejected(self) -> None:
        with self.assertRaises(ManifestError):
            parse_suites(
                '[[suite]]\nname = "a"\npackage = "b"\nshards = 1\nskip = [" "]\n'
            )

    def test_comma_separated_features_in_one_entry_are_rejected(self) -> None:
        # Cargo reads `--features foo,bar` as two features, but the manifest
        # would hold `"foo,bar"` as one — so the identical documented command
        # would compare unequal and fail the docs gate.
        with self.assertRaises(ManifestError) as raised:
            parse_suites(
                '[[suite]]\nname = "a"\npackage = "b"\nshards = 1\n'
                'features = ["foo,bar"]\n'
            )

        self.assertIn("own entry", str(raised.exception))

    def test_space_separated_features_in_one_entry_are_rejected(self) -> None:
        with self.assertRaises(ManifestError):
            parse_suites(
                '[[suite]]\nname = "a"\npackage = "b"\nshards = 1\n'
                'features = ["foo bar"]\n'
            )

    def test_a_plain_feature_name_is_accepted(self) -> None:
        suites = parse_suites(
            '[[suite]]\nname = "a"\npackage = "b"\nshards = 1\n'
            'features = ["postgres-integration", "tls_1.3+ring"]\n'
        )

        self.assertEqual(suites[0].features, ("postgres-integration", "tls_1.3+ring"))

    def test_skip_term_carrying_filterset_syntax_is_rejected(self) -> None:
        # `skip` terms are concatenated into a filterset expression, so a term
        # spelling its own predicate would rewrite the expression's meaning
        # instead of excluding one test.
        with self.assertRaises(ManifestError) as raised:
            parse_suites(
                '[[suite]]\nname = "a"\npackage = "b"\nshards = 1\n'
                'skip = ["x) or all(1"]\n'
            )

        self.assertIn("substring", str(raised.exception))

    def test_binary_term_carrying_filterset_syntax_is_rejected(self) -> None:
        with self.assertRaises(ManifestError) as raised:
            parse_suites(
                '[[suite]]\nname = "a"\npackage = "b"\nshards = 1\n'
                'include_binaries = ["x) or all(1"]\n'
            )

        self.assertIn("test-target", str(raised.exception))

    def test_same_binary_cannot_be_included_and_excluded(self) -> None:
        with self.assertRaises(ManifestError) as raised:
            parse_suites(
                '[[suite]]\nname = "a"\npackage = "b"\nshards = 1\n'
                'include_binaries = ["x"]\nexclude_binaries = ["x"]\n'
            )

        self.assertIn("both includes and excludes", str(raised.exception))


class FiltersetTests(unittest.TestCase):
    def test_no_skips_selects_everything(self) -> None:
        self.assertEqual(suite().filterset(), "all()")

    def test_one_skip_negates_one_substring(self) -> None:
        self.assertEqual(suite(skip=("alpha",)).filterset(), "not test(alpha)")

    def test_several_skips_conjoin(self) -> None:
        self.assertEqual(
            suite(skip=("alpha", "beta")).filterset(),
            "not test(alpha) and not test(beta)",
        )

    def test_one_included_binary_selects_only_that_target(self) -> None:
        self.assertEqual(
            suite(include_binaries=("runner_protocol_postgres",)).filterset(),
            "(binary(runner_protocol_postgres))",
        )

    def test_binary_partition_composes_with_test_skips(self) -> None:
        self.assertEqual(
            suite(
                exclude_binaries=("runner_protocol_postgres",),
                skip=("alpha",),
            ).filterset(),
            "not binary(runner_protocol_postgres) and not test(alpha)",
        )


class RunMatrixTests(unittest.TestCase):
    def test_each_suite_runs_once_with_bazel_responsible_for_shards(self):
        self.assertEqual(run_matrix((suite(name="alpha", shards=6), suite(name="terminal-client"))),
                         {"include": [{"suite": "alpha", "target": "//:postgres_alpha"},
                                      {"suite": "terminal-client", "target": "//:postgres_terminal_client"}]})


class WorkflowAgreementTests(unittest.TestCase):
    def disagreements(self, bazel=None, rust=None):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / ".github/workflows").mkdir(parents=True)
            for name, value in (("bazel", bazel), ("rust", rust)):
                source = ROOT / f".github/workflows/{name}.yml"
                (root / f".github/workflows/{name}.yml").write_text(source.read_text() if value is None else value)
            return workflow_disagreements(root, (suite(),))

    def test_repository_workflows_agree(self):
        self.assertEqual(self.disagreements(), [])

    def test_missing_reader_is_rejected(self):
        text = (ROOT / ".github/workflows/bazel.yml").read_text().replace("python3 scripts/postgres_integration_suites.py --matrix", "echo ignored")
        self.assertTrue(any("executes no" in failure for failure in self.disagreements(bazel=text)))

    def test_fixed_target_is_rejected(self):
        text = (ROOT / ".github/workflows/bazel.yml").read_text().replace('"$BAZEL_POSTGRES_TARGET"', '"//:postgres_persistence"')
        self.assertTrue(any("blocking" in failure for failure in self.disagreements(bazel=text)))

    def test_allowed_failure_is_rejected(self):
        text = (ROOT / ".github/workflows/bazel.yml").read_text().replace("  bazel-postgres:\n", "  bazel-postgres:\n    continue-on-error: true\n")
        self.assertTrue(any("blocking" in failure for failure in self.disagreements(bazel=text)))

    def test_missing_aggregate_assertion_is_rejected(self):
        text = (ROOT / ".github/workflows/rust.yml").read_text().replace('test "$BAZEL_RESULT" = success', 'echo done')
        self.assertTrue(any("assert Bazel success" in failure for failure in self.disagreements(rust=text)))

    def test_aggregate_cannot_skip_on_dependency_failure(self):
        text = (ROOT / ".github/workflows/rust.yml").read_text().replace('if: ${{ always() }}', 'if: success()')
        self.assertTrue(any("always()" in failure for failure in self.disagreements(rust=text)))

    def test_postgres_needs_docker_pool(self):
        text = (ROOT / ".github/workflows/bazel.yml").read_text().replace("'signalbox-docker'", "'signalbox'")
        self.assertTrue(any("signalbox-docker" in failure for failure in self.disagreements(bazel=text)))


class DocumentedCommandTests(unittest.TestCase):
    def test_backslash_continuations_join_into_one_command(self) -> None:
        found = documented_ignored_commands(
            "```bash\ncargo test -p thing \\\n  --tests -- --ignored\n```\n"
        )

        self.assertEqual(len(found), 1)
        self.assertIn("--ignored", found[0][1])

    def test_a_command_without_ignored_is_not_a_suite_claim(self) -> None:
        self.assertEqual(documented_ignored_commands("`cargo test -p thing`\n"), [])

    def test_matching_features_agree(self) -> None:
        self.assertEqual(
            documentation_disagreements(
                "AGENTS.md",
                "`cargo test -p pa --features one --tests -- --ignored`\n",
                (suite(package="pa", features=("one",)),),
            ),
            [],
        )

    def test_concatenated_short_options_are_read(self) -> None:
        # Cargo accepts `-pSPEC` and `-FFEATURES` with no separator at all.
        failures = documentation_disagreements(
            "AGENTS.md",
            "`cargo test -ppa -Fother --tests -- --ignored`\n",
            (suite(package="pa", features=("one",)),),
        )

        self.assertEqual(len(failures), 1)
        self.assertIn("other", failures[0][1])

    def test_cargo_global_options_precede_the_subcommand(self) -> None:
        # `cargo --locked test` is as valid as `cargo test --locked`.
        failures = documentation_disagreements(
            "AGENTS.md",
            "`cargo --locked test -p pa --features other --tests -- --ignored`\n",
            (suite(package="pa", features=("one",)),),
        )

        self.assertEqual(len(failures), 1)
        self.assertIn("other", failures[0][1])

    def test_a_valued_global_option_does_not_hide_the_subcommand(self) -> None:
        arguments = cargo_test_arguments(
            ["cargo", "--color", "always", "test", "-p", "x", "--", "--ignored"]
        )

        self.assertIsNotNone(arguments)
        self.assertTrue(runs_ignored_tests(arguments or []))

    def test_another_cargo_subcommand_is_not_a_test_run(self) -> None:
        self.assertIsNone(cargo_test_arguments(["cargo", "build", "-p", "x"]))

    def test_cargos_test_alias_is_a_test_run(self) -> None:
        # `t` is Cargo's own alias and selects the same tests.
        failures = documentation_disagreements(
            "AGENTS.md",
            "`cargo t -p pa --features other --tests -- --ignored`\n",
            (suite(package="pa", features=("one",)),),
        )

        self.assertEqual(len(failures), 1)
        self.assertIn("other", failures[0][1])

    def test_all_features_is_reported_rather_than_guessed(self) -> None:
        failures = documentation_disagreements(
            "AGENTS.md",
            "`cargo test -p pa --all-features --tests -- --ignored`\n",
            (suite(package="pa", features=("one",)),),
        )

        self.assertEqual(len(failures), 1)
        self.assertIn("--all-features", failures[0][1])

    def test_no_default_features_is_reported_rather_than_guessed(self) -> None:
        failures = documentation_disagreements(
            "AGENTS.md",
            "`cargo test -p pa --no-default-features -F one --tests -- --ignored`\n",
            (suite(package="pa", features=("one",)),),
        )

        self.assertEqual(len(failures), 1)
        self.assertIn("--no-default-features", failures[0][1])

    def test_a_chained_documented_command_is_split(self) -> None:
        # `cargo fmt && cargo test …` is two commands; reading the chain as one
        # made the leading subcommand hide the test run.
        failures = documentation_disagreements(
            "AGENTS.md",
            "`cargo fmt && cargo test -p pa --features other --tests"
            " -- --ignored`\n",
            (suite(package="pa", features=("one",)),),
        )

        self.assertEqual(len(failures), 1)
        self.assertIn("other", failures[0][1])

    def test_a_cd_into_a_package_selects_it(self) -> None:
        # `cd crates/persistence && cargo test …` runs that package's suite.
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "pa").mkdir()
            (root / "pa/Cargo.toml").write_text(
                '[package]\nname = "pa"\nversion = "0.0.0"\n', encoding="utf-8"
            )

            failures = documentation_disagreements(
                "AGENTS.md",
                "`cd pa && cargo test --features other --tests -- --ignored`\n",
                (suite(package="pa", features=("one",)),),
                root,
            )

        self.assertEqual(len(failures), 1)
        self.assertIn("other", failures[0][1])

    def test_workspace_selection_covers_every_manifested_package(self) -> None:
        # `--workspace` runs the persistence package without its feature, so
        # Cargo skips the required-feature targets while the command reads as
        # agreeing with the manifest.
        failures = documentation_disagreements(
            "AGENTS.md",
            "`cargo test --workspace --tests -- --ignored`\n",
            (suite(package="pa", features=("one",)),),
        )

        self.assertEqual(len(failures), 1)
        self.assertIn("(none)", failures[0][1])

    def test_the_all_alias_selects_the_workspace(self) -> None:
        self.assertEqual(
            len(
                documentation_disagreements(
                    "AGENTS.md",
                    "`cargo test --all --tests -- --ignored`\n",
                    (suite(package="pa", features=("one",)),),
                )
            ),
            1,
        )

    def test_an_excluded_package_is_not_selected_by_the_workspace(self) -> None:
        self.assertEqual(
            documentation_disagreements(
                "AGENTS.md",
                "`cargo test --workspace --exclude pa --tests -- --ignored`\n",
                (suite(package="pa", features=("one",)),),
            ),
            [],
        )

    def test_every_repeated_package_selection_is_checked(self) -> None:
        # Cargo runs every `-p` named. Keeping only the last let an
        # unmanifested package trailing a manifested one hide the suite that
        # actually needed checking.
        failures = documentation_disagreements(
            "AGENTS.md",
            "`cargo test -p pa -p elsewhere --features other --tests"
            " -- --ignored`\n",
            (suite(package="pa", features=("one",)),),
        )

        self.assertEqual(len(failures), 1)
        self.assertIn("other", failures[0][1])

    def test_a_package_qualified_feature_matches_its_bare_name(self) -> None:
        # `--features pa/one` enables `one` on `pa`, which for the package
        # under comparison is what its bare name already means.
        self.assertEqual(
            documentation_disagreements(
                "AGENTS.md",
                "`cargo test -p pa --features pa/one --tests -- --ignored`\n",
                (suite(package="pa", features=("one",)),),
            ),
            [],
        )

    def test_another_packages_qualified_feature_stays_distinct(self) -> None:
        failures = documentation_disagreements(
            "AGENTS.md",
            "`cargo test -p pa --features other/one --tests -- --ignored`\n",
            (suite(package="pa", features=("one",)),),
        )

        self.assertEqual(len(failures), 1)
        self.assertIn("other/one", failures[0][1])

    def test_a_version_qualified_spec_selects_its_package(self) -> None:
        # `-p name@version` is a valid spec; comparing the whole spec against
        # the manifest made it read as an unknown package, and unknown
        # packages are skipped rather than compared.
        failures = documentation_disagreements(
            "AGENTS.md",
            "`cargo test -p pa@0.0.0 --features other --tests -- --ignored`\n",
            (suite(package="pa", features=("one",)),),
        )

        self.assertEqual(len(failures), 1)
        self.assertIn("other", failures[0][1])

    def test_a_manifest_path_selects_its_package(self) -> None:
        # `--manifest-path` selects a package as surely as `-p` does; reading
        # only `-p` left the command unattributed and therefore unchecked.
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "pa").mkdir()
            (root / "pa/Cargo.toml").write_text(
                '[package]\nname = "pa"\nversion = "0.0.0"\n', encoding="utf-8"
            )

            failures = documentation_disagreements(
                "AGENTS.md",
                "`cargo test --manifest-path pa/Cargo.toml --features other"
                " --tests -- --ignored`\n",
                (suite(package="pa", features=("one",)),),
                root,
            )

        self.assertEqual(len(failures), 1)
        self.assertIn("other", failures[0][1])

    def test_an_unreadable_manifest_path_selects_nothing(self) -> None:
        self.assertEqual(
            documentation_disagreements(
                "AGENTS.md",
                "`cargo test --manifest-path nowhere/Cargo.toml --tests"
                " -- --ignored`\n",
                (suite(package="pa"),),
                Path("/nonexistent"),
            ),
            [],
        )

    def test_attached_option_forms_are_read(self) -> None:
        # Cargo accepts `--package=<spec>`; documentation uses it. Reading only
        # the separated spelling would let a stale documented feature set pass
        # as though the command named no package at all.
        failures = documentation_disagreements(
            "AGENTS.md",
            "`cargo test --package=pa --features=other --tests -- --ignored`\n",
            (suite(package="pa", features=("one",)),),
        )

        self.assertEqual(len(failures), 1)
        self.assertIn("other", failures[0][1])

    def test_attached_option_forms_agree_when_they_match(self) -> None:
        self.assertEqual(
            documentation_disagreements(
                "AGENTS.md",
                "`cargo test --package=pa --features=one --tests -- --ignored`\n",
                (suite(package="pa", features=("one",)),),
            ),
            [],
        )

    def test_differing_features_disagree(self) -> None:
        failures = documentation_disagreements(
            "AGENTS.md",
            "`cargo test -p pa --features other --tests -- --ignored`\n",
            (suite(package="pa", features=("one",)),),
        )

        self.assertEqual(len(failures), 1)
        self.assertIn("other", failures[0][1])

    def test_an_unmanifested_package_is_not_this_check_s_business(self) -> None:
        self.assertEqual(
            documentation_disagreements(
                "AGENTS.md",
                "`cargo test -p elsewhere --tests -- --ignored`\n",
                (suite(package="pa"),),
            ),
            [],
        )


class CommandLineTests(unittest.TestCase):
    def test_matrix_emits_parseable_json_for_the_repository_manifest(self) -> None:
        result = run_reader("--matrix")

        self.assertEqual(result.returncode, 0, result.stderr)
        matrix = json.loads(result.stdout)
        self.assertTrue(matrix["include"])
        self.assertEqual(
            sorted(matrix["include"][0]),
            ["suite", "target"],
        )

    def test_check_reports_the_resolved_topology(self) -> None:
        result = run_reader("--check")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("shards", result.stdout)

    def test_a_mode_is_required(self) -> None:
        self.assertNotEqual(run_reader().returncode, 0)


if __name__ == "__main__":
    unittest.main(verbosity=1)

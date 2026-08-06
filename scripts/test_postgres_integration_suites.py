#!/usr/bin/env python3
"""Prove the suite manifest reader rejects what it claims to reject.

The manifest is the only description of what the `postgres-integration` check
runs, and two consumers act on it without cross-checking each other: CI builds
archives and shards from it, and the docs gate decides from it which
`#[ignore]`d tests count as enforced. A reader that silently accepts a
malformed row would hand CI a suite it cannot archive, or hand the docs gate an
enforcement claim nothing runs — the second failing open, which is the worse
direction. So every schema rule gets a case that fails, not only the happy path
that passes.

Pure functions are called directly; the command-line surface is exercised as a
subprocess against the repository's own manifest, which is the input CI uses.
"""

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
    archive_plan,
    documentation_disagreements,
    documented_ignored_commands,
    manifest_line,
    parse_suites,
    run_matrix,
    workflow_disagreements,
)

READER = Path(__file__).resolve().parent / "postgres_integration_suites.py"
ROOT = Path(__file__).resolve().parent.parent

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
        self.assertEqual(suites[1].skip, ("a_live_credential_smoke",))

    def test_absent_optional_fields_default_to_empty(self) -> None:
        suites = parse_suites('[[suite]]\nname = "a"\npackage = "b"\nshards = 1\n')

        self.assertEqual(suites[0].features, ())
        self.assertEqual(suites[0].skip, ())

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


class RunMatrixTests(unittest.TestCase):
    def test_one_shard_yields_one_entry(self) -> None:
        matrix = run_matrix((suite(name="solo"),))

        self.assertEqual(
            matrix,
            {
                "include": [
                    {
                        "suite": "solo",
                        "partition": 1,
                        "partitions": 1,
                        "filter": "all()",
                    }
                ]
            },
        )

    def test_shards_expand_to_one_entry_each(self) -> None:
        matrix = run_matrix((suite(name="wide", shards=3),))

        self.assertEqual(
            [entry["partition"] for entry in matrix["include"]], [1, 2, 3]
        )
        self.assertEqual(
            {entry["partitions"] for entry in matrix["include"]}, {3}
        )

    def test_total_runners_are_the_sum_of_shard_counts(self) -> None:
        matrix = run_matrix((suite(name="a", shards=3), suite(name="b")))

        self.assertEqual(len(matrix["include"]), 4)


class ArchivePlanTests(unittest.TestCase):
    def test_features_are_comma_joined_and_absence_is_an_empty_field(self) -> None:
        plan = archive_plan(
            (
                suite(name="a", package="pa", features=("one", "two")),
                suite(name="b", package="pb"),
            )
        )

        self.assertEqual(plan, "a\tpa\tone,two\nb\tpb\t\n")

    def test_every_row_has_three_fields(self) -> None:
        rows = archive_plan(parse_suites(VALID)).splitlines()

        self.assertTrue(all(len(row.split("\t")) == 3 for row in rows))


AGREEING_WORKFLOW = """
jobs:
  postgres-integration-build:
    runs-on: ubuntu-latest
    steps:
      - run: python3 scripts/postgres_integration_suites.py --archive-plan
      - uses: actions/upload-artifact@v7
        with:
          name: postgres-integration-archive-alpha
"""


class WorkflowAgreementTests(unittest.TestCase):
    def disagreements(self, workflow: str, *suites: Suite) -> list[str]:
        """Write one workflow into a temporary root and report disagreements."""
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / ".github/workflows").mkdir(parents=True)
            (root / ".github/workflows/rust.yml").write_text(
                workflow, encoding="utf-8"
            )
            return workflow_disagreements(root, suites or (suite(name="alpha"),))

    def test_an_agreeing_workflow_reports_nothing(self) -> None:
        self.assertEqual(self.disagreements(AGREEING_WORKFLOW), [])

    def test_a_workflow_that_stops_reading_the_manifest_is_reported(self) -> None:
        stripped = AGREEING_WORKFLOW.replace(
            "      - run: python3 scripts/postgres_integration_suites.py"
            " --archive-plan\n",
            "",
        )

        failures = self.disagreements(stripped)

        self.assertEqual(len(failures), 1)
        self.assertIn("postgres_integration_suites.py", failures[0])

    def test_a_suite_without_an_artifact_is_reported(self) -> None:
        failures = self.disagreements(
            AGREEING_WORKFLOW, suite(name="alpha"), suite(name="beta")
        )

        self.assertEqual(len(failures), 1)
        self.assertIn("beta", failures[0])

    def test_an_artifact_without_a_suite_is_reported(self) -> None:
        failures = self.disagreements(
            AGREEING_WORKFLOW + "          name: "
            "postgres-integration-archive-ghost\n"
        )

        self.assertEqual(len(failures), 1)
        self.assertIn("ghost", failures[0])

    def test_an_ignored_cargo_test_run_in_the_workflow_is_reported(self) -> None:
        failures = self.disagreements(
            AGREEING_WORKFLOW
            + "      - run: cargo test -p alpha --tests -- --ignored\n"
        )

        self.assertEqual(len(failures), 1)
        self.assertIn("outside", failures[0])

    def test_a_cargo_test_run_without_ignored_is_allowed(self) -> None:
        self.assertEqual(
            self.disagreements(
                AGREEING_WORKFLOW + "      - run: cargo test --workspace\n"
            ),
            [],
        )

    def test_leaving_ubuntu_is_reported(self) -> None:
        failures = self.disagreements(
            AGREEING_WORKFLOW.replace("ubuntu-latest", "windows-latest")
        )

        self.assertEqual(len(failures), 1)
        self.assertIn("windows-latest", failures[0])

    def test_the_repository_workflow_agrees_with_its_manifest(self) -> None:
        manifest = ROOT / ".github/postgres-integration-suites.toml"

        self.assertEqual(
            workflow_disagreements(
                ROOT, parse_suites(manifest.read_text(encoding="utf-8"))
            ),
            [],
        )


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
            ["filter", "partition", "partitions", "suite"],
        )

    def test_archive_plan_emits_one_row_per_suite(self) -> None:
        result = run_reader("--archive-plan")

        self.assertEqual(result.returncode, 0, result.stderr)
        rows = result.stdout.splitlines()
        self.assertTrue(rows)
        self.assertTrue(all(len(row.split("\t")) == 3 for row in rows))

    def test_check_reports_the_resolved_topology(self) -> None:
        result = run_reader("--check")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("shards", result.stdout)

    def test_a_mode_is_required(self) -> None:
        self.assertNotEqual(run_reader().returncode, 0)


if __name__ == "__main__":
    unittest.main(verbosity=1)

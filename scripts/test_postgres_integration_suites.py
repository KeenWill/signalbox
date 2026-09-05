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


# The upload step comes last so a case can append a step, or extra keys to that
# step, and have them land where a reader would expect. A trailing block scalar
# would swallow anything appended after it.
AGREEING_WORKFLOW = """
jobs:
  postgres-integration-run:
    runs-on: signalbox-docker
    strategy:
      matrix: ${{ fromJSON(needs.postgres-integration-build.outputs.matrix) }}
    steps:
      - env:
          SUITE: ${{ matrix.suite }}
          PARTITION: ${{ matrix.partition }}
          PARTITIONS: ${{ matrix.partitions }}
          FILTER: ${{ matrix.filter }}
        run: >-
          cargo nextest run --archive-file "$RUNNER_TEMP/$SUITE.tar.zst"
          --partition "count:$PARTITION/$PARTITIONS"
          --run-ignored only --no-fail-fast -E "$FILTER"
  postgres-integration-build:
    runs-on: signalbox-docker
    steps:
      - run: python3 scripts/postgres_integration_suites.py --archive-plan
      - run: python3 scripts/postgres_integration_suites.py --matrix
      - uses: actions/upload-artifact@v7
        with:
          name: postgres-integration-archive-alpha
          path: ${{ runner.temp }}/alpha.tar.zst
  postgres-integration:
    if: ${{ always() }}
    runs-on: ubuntu-latest
    steps:
      - env:
          BUILD_RESULT: ${{ needs.postgres-integration-build.result }}
          RUN_RESULT: ${{ needs.postgres-integration-run.result }}
        run: |
          test "$BUILD_RESULT" = success
          test "$RUN_RESULT" = success
"""
# Only the `run:` scalar, so a case can remove or vary the command while the
# step's `matrix.*` bindings stay in place and are judged separately.
ARCHIVE_RUN_STEP = """        run: >-
          cargo nextest run --archive-file "$RUNNER_TEMP/$SUITE.tar.zst"
          --partition "count:$PARTITION/$PARTITIONS"
          --run-ignored only --no-fail-fast -E "$FILTER"
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
        self.assertIn("--archive-plan", failures[0])

    def test_each_required_invocation_is_checked_separately(self) -> None:
        stripped = AGREEING_WORKFLOW.replace(
            "      - run: python3 scripts/postgres_integration_suites.py"
            " --matrix\n",
            "",
        )

        failures = self.disagreements(stripped)

        self.assertEqual(len(failures), 1)
        self.assertIn("--matrix", failures[0])

    def test_the_reader_named_only_in_a_comment_is_not_an_invocation(self) -> None:
        # A filename occurrence is not the workflow reading the manifest. If a
        # comment satisfied this check, the workflow could restate package and
        # feature selection while the docs gate kept treating the manifest as
        # ground truth.
        commented = AGREEING_WORKFLOW.replace(
            "      - run: python3 scripts/postgres_integration_suites.py"
            " --archive-plan\n",
            "      # was: python3 scripts/postgres_integration_suites.py"
            " --archive-plan\n"
            "      - run: cargo nextest archive -p alpha\n",
        )

        failures = self.disagreements(commented)

        self.assertEqual(len(failures), 1)
        self.assertIn("--archive-plan", failures[0])

    def test_a_suite_without_an_artifact_is_reported(self) -> None:
        failures = self.disagreements(
            AGREEING_WORKFLOW, suite(name="alpha"), suite(name="beta")
        )

        self.assertEqual(len(failures), 1)
        self.assertIn("beta", failures[0])

    def test_an_artifact_without_a_suite_is_reported(self) -> None:
        failures = self.disagreements(
            AGREEING_WORKFLOW
            + "  ghost-upload:\n"
            "    runs-on: ubuntu-latest\n"
            "    steps:\n"
            "      - uses: actions/upload-artifact@v7\n"
            "        with:\n"
            "          name: postgres-integration-archive-ghost\n"
            "          path: ${{ runner.temp }}/ghost.tar.zst\n"
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

    def test_file_media_isolation_run_is_allowed(self) -> None:
        self.assertEqual(
            self.disagreements(
                AGREEING_WORKFLOW
                + "      - run: cargo test --no-fail-fast -p "
                "signalbox-file-media-processor-runtime --features test-worker "
                "--test isolation -- --ignored\n"
            ),
            [],
        )

    def test_other_file_media_ignored_target_is_reported(self) -> None:
        failures = self.disagreements(
            AGREEING_WORKFLOW
            + "      - run: cargo test --no-fail-fast -p "
            "signalbox-file-media-processor-runtime --features test-worker "
            "--test other-isolation -- --ignored\n"
        )

        self.assertEqual(len(failures), 1)
        self.assertIn("outside", failures[0])

    def test_a_backslash_continued_ignored_run_is_reported(self) -> None:
        failures = self.disagreements(
            AGREEING_WORKFLOW
            + "      - run: |\n"
            "          cargo test -p alpha --tests \\\n"
            "            -- --ignored\n"
        )

        self.assertEqual(len(failures), 1)
        self.assertIn("outside", failures[0])

    def test_a_multi_command_block_without_ignored_is_allowed(self) -> None:
        # Flattening a `|` block joins its commands into one string; that must
        # not invent an ignored-test run out of neighbouring lines.
        self.assertEqual(
            self.disagreements(
                AGREEING_WORKFLOW
                + "      - run: |\n"
                "          cargo test -p alpha --tests\n"
                "          echo done\n"
            ),
            [],
        )

    def test_a_workflow_that_runs_no_archive_is_reported(self) -> None:
        # Every other assertion is negative, and negatives alone are satisfied
        # by a workflow that runs nothing: delete the run step and the shards
        # pass having only downloaded their archives.
        failures = self.disagreements(
            AGREEING_WORKFLOW.replace(ARCHIVE_RUN_STEP, "")
        )

        self.assertEqual(len(failures), 1)
        self.assertIn("--run-ignored only", failures[0])

    def test_an_archive_run_without_the_ignored_selection_is_reported(self) -> None:
        failures = self.disagreements(
            AGREEING_WORKFLOW.replace(
                ARCHIVE_RUN_STEP,
                '        run: cargo nextest run --archive-file "$RUNNER_TEMP/$SUITE.z"'
                ' --partition "count:$PARTITION/$PARTITIONS" -E "$FILTER"\n',
            )
        )

        self.assertEqual(len(failures), 1)
        self.assertIn("--run-ignored only", failures[0])

    def test_an_archive_run_not_parameterised_by_the_matrix_is_reported(self) -> None:
        # A run naming one fixed archive would have every shard execute the
        # same suite while the aggregate check still passed.
        failures = self.disagreements(
            AGREEING_WORKFLOW.replace(
                '--archive-file "$RUNNER_TEMP/$SUITE.tar.zst"',
                "--archive-file /tmp/persistence.tar.zst",
            )
        )

        self.assertTrue(any("--run-ignored only" in failure for failure in failures))

    def test_a_pinned_partition_numerator_is_reported(self) -> None:
        # Expanding *a* variable is not enough: `count:1/$PARTITIONS` runs
        # partition 1 on every shard, so 2/3 and 3/3 never execute while the
        # aggregate check succeeds.
        failures = self.disagreements(
            AGREEING_WORKFLOW.replace(
                'count:$PARTITION/$PARTITIONS', "count:1/$PARTITIONS"
            )
        )

        self.assertTrue(any("--run-ignored only" in failure for failure in failures))

    def test_a_pinned_partition_denominator_is_reported(self) -> None:
        failures = self.disagreements(
            AGREEING_WORKFLOW.replace(
                'count:$PARTITION/$PARTITIONS', "count:$PARTITION/3"
            )
        )

        self.assertTrue(any("--run-ignored only" in failure for failure in failures))

    def test_a_literal_filterset_is_reported(self) -> None:
        failures = self.disagreements(AGREEING_WORKFLOW.replace('-E "$FILTER"', '-E "all()"'))

        self.assertTrue(any("--run-ignored only" in failure for failure in failures))

    def test_an_option_reading_the_wrong_matrix_variable_is_reported(self) -> None:
        # `$SUITE` in the partition is a variable, and the wrong one.
        failures = self.disagreements(
            AGREEING_WORKFLOW.replace(
                'count:$PARTITION/$PARTITIONS', "count:$SUITE/$PARTITIONS"
            )
        )

        self.assertTrue(any("--run-ignored only" in failure for failure in failures))

    def test_a_run_without_an_archive_is_reported(self) -> None:
        # Two distinct problems, reported separately: no archive-backed run
        # exists, and the run that does exist chooses its own packages.
        failures = self.disagreements(
            AGREEING_WORKFLOW.replace(
                ARCHIVE_RUN_STEP,
                '        run: cargo nextest run --run-ignored only'
                ' --partition "count:$PARTITION/$PARTITIONS" -E "$FILTER"\n',
            )
        )

        self.assertEqual(len(failures), 2)
        self.assertTrue(any("--run-ignored only" in failure for failure in failures))
        self.assertTrue(any("manifest-driven" in failure for failure in failures))

    def test_an_environment_prefix_does_not_hide_the_command(self) -> None:
        failures = self.disagreements(
            AGREEING_WORKFLOW
            + "      - run: RUST_LOG=debug cargo test -p alpha -- --ignored\n"
        )

        self.assertEqual(len(failures), 1)
        self.assertIn("outside", failures[0])

    def test_an_env_wrapper_does_not_hide_the_command(self) -> None:
        failures = self.disagreements(
            AGREEING_WORKFLOW
            + "      - run: env -u FOO BAR=1 cargo test -p alpha -- --ignored\n"
        )

        self.assertEqual(len(failures), 1)
        self.assertIn("outside", failures[0])

    def test_valueless_env_options_do_not_hide_the_command(self) -> None:
        # `-i`/`--ignore-environment` take no value and still run the command.
        failures = self.disagreements(
            AGREEING_WORKFLOW
            + '      - run: env -i PATH=/bin cargo test -p alpha -- --ignored\n'
        )

        self.assertEqual(len(failures), 1)
        self.assertIn("outside", failures[0])

    def test_an_env_terminator_does_not_hide_the_command(self) -> None:
        failures = self.disagreements(
            AGREEING_WORKFLOW
            + "      - run: env -i -- cargo test -p alpha -- --ignored\n"
        )

        self.assertEqual(len(failures), 1)
        self.assertIn("outside", failures[0])

    def test_an_unarchived_nextest_ignored_run_is_reported(self) -> None:
        # The archived run existing is not enough: a rogue one can sit beside
        # it, choosing its own packages.
        failures = self.disagreements(
            AGREEING_WORKFLOW
            + "      - run: cargo nextest run -p rogue --run-ignored only\n"
        )

        self.assertEqual(len(failures), 1)
        self.assertIn("manifest-driven", failures[0])

    def test_a_toolchain_selector_does_not_hide_the_subcommand(self) -> None:
        # `cargo +toolchain …` is rustup's selector, not an option.
        failures = self.disagreements(
            AGREEING_WORKFLOW
            + "      - run: cargo +1.95.0 test -p alpha --tests -- --ignored\n"
        )

        self.assertEqual(len(failures), 1)
        self.assertIn("outside", failures[0])

    def test_the_include_ignored_spelling_is_reported(self) -> None:
        # libtest runs the ignored tests under `--include-ignored` too, so it
        # is as much an unmanifested run as `--ignored` is.
        failures = self.disagreements(
            AGREEING_WORKFLOW
            + "      - run: cargo test -p alpha --tests -- --include-ignored\n"
        )

        self.assertEqual(len(failures), 1)
        self.assertIn("outside", failures[0])

    def test_block_headers_are_recognized_in_either_indicator_order(self) -> None:
        # `|2-` and `|-2` are the same scalar; a header read as command text
        # would join a literal block's lines with spaces and fuse its commands.
        commands = workflow_shell_commands(
            "jobs:\n  a:\n    steps:\n"
            "      - run: |2-\n"
            "          echo first\n"
            "          echo second\n"
        )

        self.assertEqual(
            [command for command, _, _ in commands], ["echo first\necho second"]
        )

    def test_a_global_option_before_the_subcommand_is_reported(self) -> None:
        failures = self.disagreements(
            AGREEING_WORKFLOW
            + "      - run: cargo --locked test -p alpha --tests -- --ignored\n"
        )

        self.assertEqual(len(failures), 1)
        self.assertIn("outside", failures[0])

    def test_a_folded_scalar_ignored_run_is_reported(self) -> None:
        # The shape the previous workflow used, and the one a reader is most
        # likely to reach for: no backslashes, the arguments simply wrapped.
        failures = self.disagreements(
            AGREEING_WORKFLOW
            + "      - run: >-\n"
            "          cargo test -p alpha --tests\n"
            "          -- --ignored\n"
        )

        self.assertEqual(len(failures), 1)
        self.assertIn("outside", failures[0])

    def test_a_matrix_command_scalar_ignored_run_is_reported(self) -> None:
        failures = self.disagreements(
            AGREEING_WORKFLOW
            + "  legacy-matrix:\n"
            "    runs-on: ubuntu-latest\n"
            "    strategy:\n"
            "      matrix:\n"
            "        include:\n"
            "          - suite: alpha\n"
            "            command: >-\n"
            "              cargo test -p alpha --tests\n"
            "              -- --ignored\n"
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

    def test_prose_mentioning_the_command_is_not_read_as_one(self) -> None:
        # An apostrophe in a comment is not an unterminated quote, and a
        # sentence about the command is not a command.
        self.assertEqual(
            self.disagreements(
                AGREEING_WORKFLOW
                + "      # cargo test doesn't run --ignored here anymore\n"
            ),
            [],
        )

    def test_diverging_shard_selections_are_reported(self) -> None:
        dynamic = (
            "runs-on: ${{ github.event_name == 'pull_request' && "
            "(github.event.pull_request.head.repo.full_name != github.repository "
            "|| contains(fromJSON('[\"dependabot[bot]\",\"renovate[bot]\"]'), "
            "github.event.pull_request.user.login)) && 'ubuntu-latest' || "
            "'signalbox-docker' }}"
        )
        failures = self.disagreements(
            AGREEING_WORKFLOW.replace(
                "  postgres-integration-run:\n    runs-on: signalbox-docker\n",
                "  postgres-integration-run:\n    " + dynamic + "\n",
            )
        )

        self.assertEqual(len(failures), 1)
        self.assertIn("share one complete runner selection", failures[0])

    def test_run_job_leaving_signalbox_docker_is_reported(self) -> None:
        failures = self.disagreements(
            AGREEING_WORKFLOW.replace(
                "  postgres-integration-run:\n    runs-on: signalbox-docker\n",
                "  postgres-integration-run:\n    runs-on: windows-latest\n",
            )
        )

        self.assertEqual(len(failures), 1)
        self.assertIn("windows-latest", failures[0])

    def test_build_job_leaving_signalbox_docker_is_reported(self) -> None:
        failures = self.disagreements(
            AGREEING_WORKFLOW.replace(
                "  postgres-integration-build:\n    runs-on: signalbox-docker\n",
                "  postgres-integration-build:\n    runs-on: ubuntu-latest\n",
            )
        )

        self.assertEqual(len(failures), 1)
        self.assertIn("signalbox-docker", failures[0])

    def test_naming_the_reader_without_running_it_is_not_an_invocation(self) -> None:
        # `echo python3 scripts/…py --matrix` contains the reader and the mode
        # and executes neither. Only the command word separates the two.
        echoed = AGREEING_WORKFLOW.replace(
            "      - run: python3 scripts/postgres_integration_suites.py"
            " --matrix\n",
            "      - run: echo python3 scripts/postgres_integration_suites.py"
            " --matrix\n",
        )

        failures = self.disagreements(echoed)

        self.assertEqual(len(failures), 1)
        self.assertIn("--matrix", failures[0])

    def test_the_reader_inside_a_command_substitution_counts(self) -> None:
        # How this workflow reads the shard matrix: the invocation is inside
        # `$( … )` within a quoted argument.
        substituted = AGREEING_WORKFLOW.replace(
            "      - run: python3 scripts/postgres_integration_suites.py"
            " --matrix\n",
            "      - run: printf 'matrix=%s\\n'"
            ' "$(python3 scripts/postgres_integration_suites.py --matrix)"'
            ' >> "$GITHUB_OUTPUT"\n',
        )

        self.assertEqual(self.disagreements(substituted), [])

    def test_the_reader_run_directly_counts(self) -> None:
        direct = AGREEING_WORKFLOW.replace(
            "      - run: python3 scripts/postgres_integration_suites.py"
            " --matrix\n",
            "      - run: scripts/postgres_integration_suites.py --matrix\n",
        )

        self.assertEqual(self.disagreements(direct), [])

    def test_an_inline_comment_is_not_an_invocation(self) -> None:
        # The reader's name trailing a real command as a `#` comment is text
        # the runner never executes.
        commented = AGREEING_WORKFLOW.replace(
            "      - run: python3 scripts/postgres_integration_suites.py"
            " --archive-plan\n",
            "      - run: cargo nextest archive -p alpha"
            " # python3 scripts/postgres_integration_suites.py --archive-plan\n",
        )

        failures = self.disagreements(commented)

        self.assertEqual(len(failures), 1)
        self.assertIn("--archive-plan", failures[0])

    def test_an_inline_comment_does_not_hide_the_rest_of_a_block(self) -> None:
        # Comments end their own line, not the `|` block they sit inside.
        failures = self.disagreements(
            AGREEING_WORKFLOW
            + "      - run: |\n"
            "          echo starting  # a note\n"
            "          cargo test -p alpha --tests -- --ignored\n"
        )

        self.assertEqual(len(failures), 1)
        self.assertIn("outside", failures[0])

    def test_a_quoted_hash_is_not_a_comment(self) -> None:
        self.assertEqual(
            [
                command
                for command, _, _ in workflow_shell_commands(
                    "jobs:\n  a:\n    steps:\n      - run: |\n          echo 'a # b'\n"
                )
            ],
            ["echo 'a # b'"],
        )

    def test_an_artifact_uploading_another_suites_archive_is_reported(self) -> None:
        # The right label over the wrong archive: every shard passes having run
        # a suite the manifest did not declare for that name.
        failures = self.disagreements(
            AGREEING_WORKFLOW.replace(
                "${{ runner.temp }}/alpha.tar.zst",
                "${{ runner.temp }}/persistence.tar.zst",
            )
        )

        self.assertEqual(len(failures), 1)
        self.assertIn("alpha.tar.zst", failures[0])

    def test_a_second_nonconforming_archived_run_is_reported(self) -> None:
        # One conforming run existing is not enough; a leftover archived run
        # dropping the partition would rerun a whole suite on every shard.
        failures = self.disagreements(
            AGREEING_WORKFLOW
            + '      - run: cargo nextest run --archive-file "$RUNNER_TEMP/a.z"'
            " --run-ignored only\n"
        )

        self.assertEqual(len(failures), 1)
        self.assertIn("manifest-driven", failures[0])

    def test_a_reader_invocation_outside_the_build_job_does_not_count(self) -> None:
        # A stale literal matrix in the build job, with the invocation left in
        # an unrelated step, would run one row and skip the rest.
        stripped = AGREEING_WORKFLOW.replace(
            "      - run: python3 scripts/postgres_integration_suites.py --matrix\n",
            "",
        )
        failures = self.disagreements(
            stripped
            + "  decoy:\n"
            "    runs-on: ubuntu-latest\n"
            "    steps:\n"
            "      - run: python3 scripts/postgres_integration_suites.py --matrix\n"
        )

        self.assertEqual(len(failures), 1)
        self.assertIn("--matrix", failures[0])

    def test_a_non_blocking_archived_run_is_reported(self) -> None:
        # `continue-on-error: true` lets every shard fail while the job reports
        # success and the aggregate's assertion passes.
        failures = self.disagreements(
            AGREEING_WORKFLOW.replace(
                "      - env:\n          SUITE:",
                "      - continue-on-error: true\n        env:\n          SUITE:",
            )
        )

        self.assertTrue(any("runs no archive-backed" in failure for failure in failures))

    def test_an_expression_enabled_continue_on_error_is_reported(self) -> None:
        # What `${{ … }}` evaluates to is not decidable here, so a step that
        # might be allowed to fail cannot be credited with enforcing anything.
        failures = self.disagreements(
            AGREEING_WORKFLOW.replace(
                "      - env:\n          SUITE:",
                "      - continue-on-error: ${{ true }}\n        env:\n          SUITE:",
            )
        )

        self.assertTrue(any("runs no archive-backed" in failure for failure in failures))

    def test_an_explicitly_false_continue_on_error_stays_blocking(self) -> None:
        self.assertEqual(
            self.disagreements(
                AGREEING_WORKFLOW.replace(
                    "      - env:\n          SUITE:",
                    "      - continue-on-error: false\n        env:\n          SUITE:",
                )
            ),
            [],
        )

    def test_an_unrelated_non_ubuntu_job_is_allowed(self) -> None:
        # Cross-platform CI elsewhere in this workflow is nobody's business
        # here; only the jobs whose environment reaches the archives matter.
        self.assertEqual(
            self.disagreements(
                AGREEING_WORKFLOW
                + "  cross-platform:\n"
                "    runs-on: macos-latest\n"
                "    steps:\n"
                "      - run: echo hi\n"
            ),
            [],
        )

    def test_an_integration_job_leaving_signalbox_docker_is_reported(self) -> None:
        failures = self.disagreements(
            AGREEING_WORKFLOW.replace(
                "  postgres-integration-run:\n    runs-on: signalbox-docker\n",
                "  postgres-integration-run:\n    runs-on: macos-latest\n",
            )
        )

        self.assertTrue(any("macos-latest" in failure for failure in failures))

    def test_the_command_builtin_does_not_hide_the_command(self) -> None:
        failures = self.disagreements(
            AGREEING_WORKFLOW
            + "      - run: command cargo test -p alpha --tests -- --ignored\n"
        )

        self.assertEqual(len(failures), 1)
        self.assertIn("outside", failures[0])

    def test_command_v_prints_rather_than_runs(self) -> None:
        # `command -v cargo` describes Cargo; it runs nothing.
        self.assertEqual(
            self.disagreements(AGREEING_WORKFLOW + "      - run: command -v cargo\n"),
            [],
        )

    def test_an_aggregate_check_without_always_is_reported(self) -> None:
        # Without `always()` the job is skipped when a dependency fails, and a
        # skipped required check reports success.
        failures = self.disagreements(
            AGREEING_WORKFLOW.replace("    if: ${{ always() }}\n", "")
        )

        self.assertEqual(len(failures), 1)
        self.assertIn("always()", failures[0])

    def test_a_command_under_an_env_key_is_not_executed(self) -> None:
        # A conforming nextest string parked in an environment value would
        # satisfy the archived-run requirement while no step ran anything.
        failures = self.disagreements(
            AGREEING_WORKFLOW.replace(
                ARCHIVE_RUN_STEP,
                "      - env:\n"
                "          command: cargo nextest run --run-ignored only\n"
                "        run: echo no archived run\n",
            )
        )

        self.assertTrue(any("runs no archive-backed" in failure for failure in failures))

    def test_a_result_assertion_in_another_job_does_not_count(self) -> None:
        # The aggregate job carries the required check's name, so the same
        # binding and assertion sitting elsewhere proves nothing about it.
        stripped = AGREEING_WORKFLOW.replace(
            "          RUN_RESULT: ${{ needs.postgres-integration-run.result }}\n",
            "",
        ).replace('          test "$RUN_RESULT" = success\n', "")
        failures = self.disagreements(
            stripped
            + "  decoy:\n"
            "    runs-on: ubuntu-latest\n"
            "    steps:\n"
            "      - env:\n"
            "          RUN_RESULT: ${{ needs.postgres-integration-run.result }}\n"
            "        run: |\n"
            '          test "$RUN_RESULT" = success\n'
        )

        self.assertEqual(len(failures), 1)
        self.assertIn("postgres-integration-run", failures[0])

    def test_an_aggregate_check_that_only_mentions_the_result_is_reported(self) -> None:
        # `echo "$RUN_RESULT was not success"` names the variable and the word
        # and exits zero regardless.
        failures = self.disagreements(
            AGREEING_WORKFLOW.replace(
                '          test "$RUN_RESULT" = success\n',
                '          echo "$RUN_RESULT was not success"\n',
            )
        )

        self.assertEqual(len(failures), 1)
        self.assertIn("postgres-integration-run", failures[0])

    def test_a_matrix_binding_in_another_step_does_not_count(self) -> None:
        # The run step pins the partition while a decoy step keeps the binding.
        failures = self.disagreements(
            AGREEING_WORKFLOW.replace(
                "      - env:\n          SUITE: ${{ matrix.suite }}\n",
                "      - env:\n"
                "          PARTITION: ${{ matrix.partition }}\n"
                "        run: echo decoy\n"
                "      - env:\n"
                "          SUITE: ${{ matrix.suite }}\n",
            ).replace(
                "          PARTITION: ${{ matrix.partition }}\n"
                "          PARTITIONS: ${{ matrix.partitions }}\n",
                '          PARTITION: "1"\n'
                "          PARTITIONS: ${{ matrix.partitions }}\n",
            )
        )

        self.assertTrue(any("manifest-driven" in failure for failure in failures))

    def test_an_aggregate_check_dropping_the_run_result_is_reported(self) -> None:
        # The aggregate job carries the required check's name, so it going
        # green without consulting the shards would let branch protection pass
        # while every declared test failed.
        failures = self.disagreements(
            AGREEING_WORKFLOW.replace('          test "$RUN_RESULT" = success\n', "")
        )

        self.assertEqual(len(failures), 1)
        self.assertIn("postgres-integration-run", failures[0])

    def test_an_aggregate_check_dropping_the_build_result_is_reported(self) -> None:
        failures = self.disagreements(
            AGREEING_WORKFLOW.replace(
                '          test "$BUILD_RESULT" = success\n', ""
            )
        )

        self.assertEqual(len(failures), 1)
        self.assertIn("postgres-integration-build", failures[0])

    def test_an_artifact_named_only_in_a_comment_is_not_published(self) -> None:
        # An upload step deleted while its artifact name survives in a comment
        # would otherwise still count as published, and the docs gate would
        # keep asserting a suite runs whose archive does not exist.
        commented = AGREEING_WORKFLOW.replace(
            "      - uses: actions/upload-artifact@v7\n"
            "        with:\n"
            "          name: postgres-integration-archive-alpha\n"
            "          path: ${{ runner.temp }}/alpha.tar.zst\n",
            "      # dropped: postgres-integration-archive-alpha\n",
        )

        failures = self.disagreements(commented)

        self.assertEqual(len(failures), 1)
        self.assertIn("publishes no", failures[0])

    def test_a_step_name_is_not_an_artifact_name(self) -> None:
        renamed = AGREEING_WORKFLOW.replace(
            "      - uses: actions/upload-artifact@v7\n"
            "        with:\n"
            "          name: postgres-integration-archive-alpha\n"
            "          path: ${{ runner.temp }}/alpha.tar.zst\n",
            "      - name: postgres-integration-archive-alpha\n"
            "        run: echo not an upload\n",
        )

        failures = self.disagreements(renamed)

        self.assertEqual(len(failures), 1)
        self.assertIn("publishes no", failures[0])

    def test_simple_commands_splits_on_operators_and_substitutions(self) -> None:
        executed = simple_commands('a && b | c; "$(d --flag)"')

        self.assertIn(["d", "--flag"], executed)
        self.assertIn(["a"], executed)
        self.assertIn(["b"], executed)
        self.assertIn(["c"], executed)

    def test_invokes_reader_requires_the_command_word(self) -> None:
        reader = "scripts/postgres_integration_suites.py"

        self.assertTrue(invokes_reader(["python3", reader, "--matrix"], "--matrix"))
        self.assertTrue(invokes_reader([reader, "--matrix"], "--matrix"))
        self.assertFalse(
            invokes_reader(["echo", "python3", reader, "--matrix"], "--matrix")
        )
        self.assertFalse(invokes_reader(["python3", reader, "--matrix"], "--archive-plan"))

    def test_a_block_indicator_is_not_command_text(self) -> None:
        commands = workflow_shell_commands(
            "jobs:\n"
            "  a:\n"
            "    steps:\n"
            "      - run: >-\n"
            "          cargo nextest run\n"
            "          --workspace-remap .\n"
            "      - run: |2\n"
            "          echo indented\n"
        )

        self.assertEqual(
            [command for command, _, _ in commands],
            ["cargo nextest run --workspace-remap .", "echo indented"],
        )

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

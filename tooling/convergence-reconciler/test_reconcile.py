#!/usr/bin/env python3
"""Driver scheduling and persistence tests with recorded convergence evidence."""
from __future__ import annotations

import json
import math
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest import mock

from differential import FIXTURES, POLICY, ROOT, read_recording
from reconcile import (Config, GitHubGraphQL, choose_decision, configured_path,
    evaluate_current, load_state, nonnegative_number, positive_number,
    process_pull_request, project_evaluation, save_state)


def recorded_evaluation():
    fixture = FIXTURES / "mutations/check-red.json"
    completed = subprocess.run(
        [str(ROOT / "target/debug/signalbox-converge"), "evaluate", "--fixture", str(fixture), "--policy", str(POLICY)],
        text=True, capture_output=True, check=False,
    )
    if completed.returncode != 1:
        raise AssertionError(completed.stderr)
    return json.loads(completed.stdout)


def driver_pull_request():
    return project_evaluation(recorded_evaluation())



class DecisionTests(unittest.TestCase):
    def test_converged_pull_request_is_merge_ready(self) -> None:
        decision = choose_decision(
            converged=True,
            now=1000,
            last_dispatched_at=None,
            cool_off_seconds=300,
            active_work=None,
            dry_run=False,
            dispatch_configured=True,
        )

        self.assertEqual(decision.name, "merge-ready")
        self.assertEqual(decision.reason, "convergence-predicate-satisfied")

    def test_recent_dispatch_is_in_cool_off(self) -> None:
        decision = choose_decision(
            converged=False,
            now=1000,
            last_dispatched_at=900,
            cool_off_seconds=300,
            active_work=None,
            dry_run=False,
            dispatch_configured=True,
        )

        self.assertEqual(decision.name, "cooling-off")
        self.assertEqual(decision.reason, "dispatch-cool-off:200s-remaining")

    def test_active_work_prevents_dispatch(self) -> None:
        decision = choose_decision(
            converged=False,
            now=1000,
            last_dispatched_at=None,
            cool_off_seconds=300,
            active_work=True,
            dry_run=False,
            dispatch_configured=True,
        )

        self.assertEqual(decision.name, "already-active")
        self.assertEqual(decision.reason, "operator-command-reported-active-work")

    def test_dry_run_reports_the_dispatch_it_would_make(self) -> None:
        decision = choose_decision(
            converged=False,
            now=1000,
            last_dispatched_at=None,
            cool_off_seconds=300,
            active_work=False,
            dry_run=True,
            dispatch_configured=False,
        )

        self.assertEqual(decision.name, "would-dispatch")
        self.assertEqual(decision.reason, "dry-run-and-no-active-work")

    def test_inactive_work_dispatches_outside_cool_off(self) -> None:
        decision = choose_decision(
            converged=False,
            now=1000,
            last_dispatched_at=600,
            cool_off_seconds=300,
            active_work=False,
            dry_run=False,
            dispatch_configured=True,
        )

        self.assertEqual(decision.name, "dispatch")
        self.assertEqual(decision.reason, "no-active-work-and-outside-cool-off")

    def test_missing_dispatch_command_skips_mutation(self) -> None:
        decision = choose_decision(
            converged=False,
            now=1000,
            last_dispatched_at=None,
            cool_off_seconds=300,
            active_work=False,
            dry_run=False,
            dispatch_configured=False,
        )

        self.assertEqual(decision.name, "skipped")
        self.assertEqual(decision.reason, "dispatch-command-not-configured")



class InputValidationTests(unittest.TestCase):
    def test_non_path_configuration_is_rejected_as_value_error(self) -> None:
        with self.assertRaisesRegex(ValueError, "state_file"):
            configured_path([], "state_file")
        with self.assertRaisesRegex(ValueError, "log_file"):
            configured_path({}, "log_file")

    def test_non_string_repository_is_rejected_as_malformed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "state.json"
            path.write_text(
                '{"version":1,"repository":7,"pull_requests":{}}\n',
                encoding="utf-8",
            )
            with self.assertRaisesRegex(ValueError, "malformed state file"):
                load_state(path, "OWNER/REPOSITORY")

    def test_non_object_pull_request_record_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "state.json"
            path.write_text(
                '{"version":1,"repository":"OWNER/REPOSITORY",'
                '"pull_requests":{"17":[]}}\n',
                encoding="utf-8",
            )
            with self.assertRaisesRegex(ValueError, "malformed state file"):
                load_state(path, "OWNER/REPOSITORY")

    def test_non_finite_positive_number_is_rejected(self) -> None:
        with self.assertRaises(ValueError):
            positive_number(math.inf, "interval_seconds")

    def test_non_finite_nonnegative_number_is_rejected(self) -> None:
        with self.assertRaises(ValueError):
            nonnegative_number(math.nan, "cool_off_seconds")

    def test_non_numeric_configuration_is_rejected_as_value_error(self) -> None:
        with self.assertRaisesRegex(ValueError, "interval_seconds"):
            positive_number(None, "interval_seconds")
        with self.assertRaisesRegex(ValueError, "cool_off_seconds"):
            nonnegative_number([], "cool_off_seconds")

    def test_non_object_state_is_rejected_as_malformed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "state.json"
            path.write_text("[]\n", encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "malformed state file"):
                load_state(path, "OWNER/REPOSITORY")



class DispatchFenceTests(unittest.TestCase):
    def test_dispatch_fence_uses_time_immediately_before_dispatch(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            state_file = Path(directory) / "state.json"
            config = Config(
                repository="OWNER/REPOSITORY",
                head_pattern="agent/*",
                interval_seconds=300,
                cool_off_seconds=1800,
                command_timeout_seconds=60,
                state_file=state_file,
                log_file=None,
                active_command=("active",),
                dispatch_command=("dispatch",),
                summary="none",
                dry_run=False,
                once=True,
            )
            state = {
                "version": 1,
                "repository": config.repository,
                "pull_requests": {},
            }
            pull_request = driver_pull_request()
            inactive = subprocess.CompletedProcess(["active"], 1, "", "")
            accepted = subprocess.CompletedProcess(["dispatch"], 0, "", "")
            logger = mock.Mock()
            with mock.patch(
                "reconcile.run_operator_command", side_effect=[inactive, accepted]
            ), mock.patch("reconcile.time.time", return_value=1300):
                process_pull_request(config, logger, state, pull_request, 1000)

        self.assertEqual(
            state["pull_requests"][str(pull_request["number"])]["last_dispatched_at"], 1300
        )
        self.assertEqual(
            state["pull_requests"][str(pull_request["number"])]["head_oid"],
            pull_request["head_oid"],
        )

    def test_state_save_fsyncs_file_and_parent_directory(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            state_file = Path(directory) / "state.json"
            state = {
                "version": 1,
                "repository": "OWNER/REPOSITORY",
                "pull_requests": {},
            }
            with mock.patch("reconcile.os.fsync") as fsync:
                save_state(state_file, state)

        self.assertEqual(fsync.call_count, 2)

    def test_nonzero_dispatch_exit_retains_cool_off_fence(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            state_file = Path(directory) / "state.json"
            config = Config(
                repository="OWNER/REPOSITORY",
                head_pattern="agent/*",
                interval_seconds=300,
                cool_off_seconds=1800,
                command_timeout_seconds=60,
                state_file=state_file,
                log_file=None,
                active_command=("active",),
                dispatch_command=("dispatch",),
                summary="none",
                dry_run=False,
                once=True,
            )
            state = {
                "version": 1,
                "repository": config.repository,
                "pull_requests": {},
            }
            pull_request = driver_pull_request()
            inactive = subprocess.CompletedProcess(["active"], 1, "", "")
            ambiguous = subprocess.CompletedProcess(["dispatch"], 9, "", "")
            logger = mock.Mock()
            with mock.patch(
                "reconcile.run_operator_command", side_effect=[inactive, ambiguous]
            ), mock.patch("reconcile.time.time", return_value=1000):
                result = process_pull_request(config, logger, state, pull_request, 1000)
        self.assertEqual(
            result["reason"], "dispatch-command-exited:9-cool-off-retained"
        )
        self.assertEqual(state["pull_requests"][str(pull_request["number"])]["last_dispatched_at"], 1000)



class CliDriverTests(unittest.TestCase):
    def test_listing_does_not_evaluate_a_matching_branch_from_another_repository(self):
        recording = read_recording(FIXTURES / "mutations/settled.json")
        node = recording["observations"][0][0]["response"]["data"]["repository"]["pullRequest"]
        listing = {"repository": {"pullRequests": {"nodes": [node], "pageInfo": {"hasNextPage": False, "endCursor": None}}}, "tracked": []}
        client = GitHubGraphQL("OTHER/REPOSITORY", 12)
        with mock.patch.object(client, "execute", return_value=listing) as execute:
            candidates, tracked = client.snapshot([], "*")
        self.assertEqual(candidates, [])
        self.assertEqual(tracked, [])
        execute.assert_called_once()

    def test_listing_matches_recorded_repository_identity_without_case_sensitivity(self):
        recording = read_recording(FIXTURES / "mutations/settled.json")
        node = recording["observations"][0][0]["response"]["data"]["repository"]["pullRequest"]
        listing = {"repository": {"pullRequests": {"nodes": [node], "pageInfo": {"hasNextPage": False, "endCursor": None}}}, "tracked": []}
        client = GitHubGraphQL(recording["repository"].swapcase(), 12)
        with mock.patch.object(client, "execute", return_value=listing):
            candidates, tracked = client.snapshot([], "*")
        self.assertEqual(candidates, [node])
        self.assertEqual(tracked, [])

    def test_terminal_recording_is_returned_for_bookkeeping_without_evaluation(self):
        recording = read_recording(FIXTURES / "pr-1582.json.gz")
        node = recording["observations"][0][0]["response"]["data"]["repository"]["pullRequest"]
        self.assertNotEqual(node["state"], "OPEN")
        listing = {"repository": {"pullRequests": {"nodes": [], "pageInfo": {"hasNextPage": False, "endCursor": None}}}, "tracked": [node]}
        client = GitHubGraphQL(recording["repository"], 12)
        with mock.patch.object(client, "execute", return_value=listing) as execute:
            candidates, tracked = client.snapshot([node["id"]], "*")
        self.assertEqual(candidates, [])
        self.assertEqual(tracked, [node])
        execute.assert_called_once()

    def test_cli_receives_previous_state_and_returns_its_evidence(self):
        evaluation = recorded_evaluation()
        previous = read_recording(FIXTURES / "mutations/settled.json")["previous"]
        with tempfile.TemporaryDirectory() as directory:
            config = Config(
                repository=evaluation["pull_request"]["headRepository"]["nameWithOwner"],
                head_pattern="agent/*", interval_seconds=300, cool_off_seconds=1800,
                command_timeout_seconds=60, state_file=Path(directory)/"state.json",
                log_file=None, active_command=("active",), dispatch_command=("dispatch",),
                summary="none", dry_run=False, once=True, convergence_policy=POLICY,
            )
            def completed(command, **kwargs):
                self.assertEqual(command[:2], ["signalbox-converge", "evaluate"])
                self.assertEqual(command[command.index("--pr")+1], str(evaluation["pull_request"]["number"]))
                self.assertEqual(command[command.index("--repo")+1], config.repository)
                self.assertEqual(command[command.index("--policy")+1], str(POLICY))
                state = Path(command[command.index("--state")+1])
                self.assertEqual(json.loads(state.read_text()), previous)
                return subprocess.CompletedProcess(command, 1, json.dumps(evaluation), "")
            with mock.patch("reconcile.subprocess.run", side_effect=completed):
                result = evaluate_current(config, evaluation["pull_request"]["number"], previous)
        self.assertEqual(result["_evaluation"], evaluation)

    def test_graphql_subprocess_timeout_uses_tick_failure_path(self):
        client = GitHubGraphQL("OWNER/REPOSITORY", 12)
        with mock.patch("reconcile.subprocess.run", side_effect=subprocess.TimeoutExpired("gh", 12)):
            with self.assertRaisesRegex(RuntimeError, "timed out"):
                client.execute("query", {})


if __name__ == "__main__":
    unittest.main()

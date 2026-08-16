#!/usr/bin/env python3
"""Unit tests for the standalone convergence reconciler."""

from __future__ import annotations

import dataclasses
import json
import math
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest import mock

from reconcile import (
    Config,
    GitHubGraphQL,
    choose_decision,
    evaluate_convergence,
    load_state,
    nonnegative_number,
    positive_number,
    process_pull_request,
)


FIXTURES = Path(__file__).with_name("fixtures")


def fixture(filename: str, case: str) -> dict[str, object]:
    with (FIXTURES / filename).open(encoding="utf-8") as stream:
        cases = json.load(stream)
    return cases[case]


class ConvergencePredicateTests(unittest.TestCase):
    def test_green_checks_resolved_threads_and_mergeable_base_converge(self) -> None:
        case = fixture("convergence.json", "converged_with_non_gating_failures")
        self.assertEqual(evaluate_convergence(case["pull_request"]), case["expected"])

    def test_unresolved_review_thread_blocks_convergence(self) -> None:
        case = fixture("convergence.json", "unresolved_thread_blocks")
        self.assertEqual(evaluate_convergence(case["pull_request"]), case["expected"])

    def test_pending_gating_check_blocks_convergence(self) -> None:
        case = fixture("convergence.json", "pending_check_blocks")
        self.assertEqual(evaluate_convergence(case["pull_request"]), case["expected"])

    def test_check_snapshot_for_an_older_head_blocks_convergence(self) -> None:
        case = fixture("convergence.json", "stale_check_snapshot_blocks")
        self.assertEqual(evaluate_convergence(case["pull_request"]), case["expected"])

    def test_base_conflict_blocks_convergence(self) -> None:
        case = fixture("convergence.json", "conflict_blocks")
        self.assertEqual(evaluate_convergence(case["pull_request"]), case["expected"])

    def test_unknown_mergeability_blocks_convergence(self) -> None:
        case = fixture("convergence.json", "unknown_mergeability_blocks")
        self.assertEqual(evaluate_convergence(case["pull_request"]), case["expected"])


class DecisionTests(unittest.TestCase):
    def test_converged_pull_request_is_merge_ready(self) -> None:
        case = fixture("decisions.json", "converged")
        self.assertEqual(dataclasses.asdict(choose_decision(**case["input"])), case["expected"])

    def test_recent_dispatch_is_in_cool_off(self) -> None:
        case = fixture("decisions.json", "cooling_off")
        self.assertEqual(dataclasses.asdict(choose_decision(**case["input"])), case["expected"])

    def test_active_work_prevents_dispatch(self) -> None:
        case = fixture("decisions.json", "active_work")
        self.assertEqual(dataclasses.asdict(choose_decision(**case["input"])), case["expected"])

    def test_dry_run_reports_the_dispatch_it_would_make(self) -> None:
        case = fixture("decisions.json", "dry_run")
        self.assertEqual(dataclasses.asdict(choose_decision(**case["input"])), case["expected"])

    def test_inactive_work_dispatches_outside_cool_off(self) -> None:
        case = fixture("decisions.json", "dispatch")
        self.assertEqual(dataclasses.asdict(choose_decision(**case["input"])), case["expected"])

    def test_missing_dispatch_command_skips_mutation(self) -> None:
        case = fixture("decisions.json", "missing_dispatch_command")
        self.assertEqual(dataclasses.asdict(choose_decision(**case["input"])), case["expected"])


class InputValidationTests(unittest.TestCase):
    def test_non_finite_positive_number_is_rejected(self) -> None:
        with self.assertRaises(ValueError):
            positive_number(math.inf, "interval_seconds")

    def test_non_finite_nonnegative_number_is_rejected(self) -> None:
        with self.assertRaises(ValueError):
            nonnegative_number(math.nan, "cool_off_seconds")

    def test_non_object_state_is_rejected_as_malformed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "state.json"
            path.write_text("[]\n", encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "malformed state file"):
                load_state(path, "OWNER/REPOSITORY")


class GitHubGraphQLTests(unittest.TestCase):
    def test_graphql_subprocess_timeout_uses_tick_failure_path(self) -> None:
        client = GitHubGraphQL("OWNER/REPOSITORY", 12)
        timeout = subprocess.TimeoutExpired(["gh"], 12)
        with mock.patch("reconcile.subprocess.run", side_effect=timeout) as run:
            with self.assertRaisesRegex(RuntimeError, "GraphQL request timed out"):
                client.execute("query { viewer { login } }", {})
        self.assertEqual(run.call_args.kwargs["timeout"], 12)

    def test_matching_fork_pull_request_is_not_detailed(self) -> None:
        client = GitHubGraphQL("OWNER/REPOSITORY", 12)
        listing = {
            "repository": {
                "pullRequests": {
                    "nodes": [
                        {
                            "id": "fork-node",
                            "headRefName": "agent/untrusted",
                            "headRepository": {"nameWithOwner": "OTHER/FORK"},
                        }
                    ],
                    "pageInfo": {"hasNextPage": False, "endCursor": None},
                }
            },
            "tracked": [],
        }
        with mock.patch.object(client, "execute", return_value=listing) as execute:
            pull_requests, tracked = client.snapshot([], "agent/*")
        self.assertEqual(pull_requests, [])
        self.assertEqual(tracked, [])
        execute.assert_called_once()

    def test_terminal_detail_result_is_recorded_without_processing(self) -> None:
        client = GitHubGraphQL("OWNER/REPOSITORY", 12)
        listing = {
            "repository": {
                "pullRequests": {
                    "nodes": [
                        {
                            "id": "terminal-node",
                            "headRefName": "agent/raced",
                            "headRepository": {"nameWithOwner": "OWNER/REPOSITORY"},
                        }
                    ],
                    "pageInfo": {"hasNextPage": False, "endCursor": None},
                }
            },
            "tracked": [],
        }
        terminal = {
            "nodes": [
                {
                    "id": "terminal-node",
                    "number": 17,
                    "state": "MERGED",
                    "mergedAt": "2026-08-16T00:00:00Z",
                    "closedAt": None,
                    "headRefName": "agent/raced",
                    "headRefOid": "abc123",
                }
            ]
        }
        with mock.patch.object(client, "execute", side_effect=[listing, terminal]):
            pull_requests, tracked = client.snapshot([], "agent/*")
        self.assertEqual(pull_requests, [])
        self.assertEqual(tracked, terminal["nodes"])


class DispatchFenceTests(unittest.TestCase):
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
            pull_request = {
                "node_id": "node",
                "number": 17,
                "title": "title",
                "url": "https://example.invalid/pull/17",
                "is_draft": False,
                "base_ref": "main",
                "head_ref": "agent/work",
                "head_oid": "head",
                "checked_head_oid": "head",
                "mergeable": "MERGEABLE",
                "review_threads": [{"isResolved": False}],
                "checks": [],
            }
            inactive = subprocess.CompletedProcess(["active"], 1, "", "")
            ambiguous = subprocess.CompletedProcess(["dispatch"], 9, "", "")
            logger = mock.Mock()
            with mock.patch(
                "reconcile.run_operator_command", side_effect=[inactive, ambiguous]
            ):
                result = process_pull_request(config, logger, state, pull_request, 1000)
        self.assertEqual(
            result["reason"], "dispatch-command-exited:9-cool-off-retained"
        )
        self.assertEqual(state["pull_requests"]["17"]["last_dispatched_at"], 1000)


if __name__ == "__main__":
    unittest.main()

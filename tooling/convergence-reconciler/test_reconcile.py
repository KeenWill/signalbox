#!/usr/bin/env python3
"""Unit tests for the standalone convergence reconciler."""

from __future__ import annotations

import dataclasses
import json
from pathlib import Path
import unittest

from reconcile import choose_decision, evaluate_convergence


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


if __name__ == "__main__":
    unittest.main()

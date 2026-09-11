#!/usr/bin/env python3
"""Exercise complete-inventory and independent judge-result admission."""

import unittest
from types import SimpleNamespace
from unittest.mock import patch

from review_judge_eval import run_trials, validate_result


class JudgmentResultTests(unittest.TestCase):
    def test_all_findings_can_be_declined(self):
        result = {"members": [{
            "finding_id": "candidate", "bar_category": "none",
            "decline_class": "hypothetical-hardening", "confidence": 5,
            "reason": "The candidate provides no failing scenario.",
        }]}
        self.assertIs(validate_result(result, ["candidate"]), result)

    def test_repeated_member_cannot_cover_a_missing_finding(self):
        member = {
            "finding_id": "first", "bar_category": "own-behavior-defect",
            "decline_class": None, "confidence": 4,
            "reason": "The changed branch drops the accepted input.",
        }
        with self.assertRaisesRegex(ValueError, "exact finding inventory"):
            validate_result({"members": [member, member]}, ["first", "second"])

    def test_producer_basis_points_are_not_judge_confidence(self):
        result = {"members": [{
            "finding_id": "candidate", "bar_category": "own-behavior-defect",
            "decline_class": None, "confidence": 9500,
            "reason": "The changed branch drops the accepted input.",
        }]}
        with self.assertRaisesRegex(ValueError, "one through five"):
            validate_result(result, ["candidate"])


class TrialAdmissionTests(unittest.TestCase):
    def test_failure_stops_new_sessions_until_the_operator_resolves_it(self):
        cases = [{"id": "active"}, {"id": "later"}]
        with patch("review_judge_eval.Trial") as trial:
            trial.return_value.terminal = False
            trial.return_value.run.side_effect = RuntimeError("transcript unavailable")
            failures = run_trials(SimpleNamespace(workers=1), cases)
        self.assertEqual(trial.call_count, 1)
        self.assertEqual([failure["id"] for failure in failures], ["active", "later"])
        self.assertEqual(failures[1]["error"], "not submitted while a failed trial may remain active")

    def test_completed_bad_output_does_not_prevent_scoring_later_cases(self):
        cases = [{"id": "bad-output"}, {"id": "later"}]
        with patch("review_judge_eval.Trial") as trial:
            trial.return_value.terminal = True
            trial.return_value.run.side_effect = [ValueError("missing result"), {"wall_seconds": 1}]
            failures = run_trials(SimpleNamespace(workers=1), cases)
        self.assertEqual(trial.call_count, 2)
        self.assertEqual([failure["id"] for failure in failures], ["bad-output"])


if __name__ == "__main__":
    unittest.main()

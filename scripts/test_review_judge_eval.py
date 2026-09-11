#!/usr/bin/env python3
"""Exercise complete-inventory and independent judge-result admission."""

import unittest

from review_judge_eval import validate_result


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


if __name__ == "__main__":
    unittest.main()

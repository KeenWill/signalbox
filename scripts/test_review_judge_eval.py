#!/usr/bin/env python3
"""Exercise complete-inventory and independent judge-result admission."""

from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
import subprocess
import tempfile
import unittest
from types import SimpleNamespace
from unittest.mock import patch

from review_judge_eval import git, prepare_checkout, run_trials, validate_result


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


class ScratchCheckoutTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        root = Path(self.temporary.name)
        self.repository = root / "repository"
        self.repository.mkdir()
        self.workspace = root / "workspace"
        git(self.repository, "init", "--initial-branch=main")
        self.head = self.commit("first")

    def commit(self, text):
        (self.repository / "source.txt").write_text(text)
        git(self.repository, "add", "source.txt")
        git(self.repository, "-c", "user.name=Judge fixture", "-c",
            "user.email=judge@example.com", "commit", "-m", text)
        return git(self.repository, "rev-parse", "HEAD")

    def test_concurrent_cases_and_later_runs_reuse_one_checkout(self):
        with ThreadPoolExecutor(max_workers=4) as workers:
            trees = list(workers.map(lambda _: prepare_checkout(
                self.repository, self.workspace, self.head), range(8)))
        later = prepare_checkout(self.repository, self.workspace, self.head)
        self.assertEqual(set(trees), {later})
        self.assertEqual((later / "source.txt").read_text(), "first")
        registrations = git(self.repository, "worktree", "list", "--porcelain")
        self.assertEqual(registrations.count("worktree "), 2)

    def test_different_heads_preserve_their_own_source(self):
        first = prepare_checkout(self.repository, self.workspace, self.head)
        next_head = self.commit("second")
        second = prepare_checkout(self.repository, self.workspace, next_head)
        self.assertNotEqual(first, second)
        self.assertEqual((first / "source.txt").read_text(), "first")
        self.assertEqual((second / "source.txt").read_text(), "second")

    def test_changed_shared_source_is_rejected_without_resetting_it(self):
        tree = prepare_checkout(self.repository, self.workspace, self.head)
        (tree / "source.txt").write_text("changed by a case")
        with self.assertRaises(subprocess.CalledProcessError):
            prepare_checkout(self.repository, self.workspace, self.head)
        self.assertEqual((tree / "source.txt").read_text(), "changed by a case")


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

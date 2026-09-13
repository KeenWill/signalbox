"""Full-context judgment input and sibling evidence boundaries."""

import json
import unittest
from unittest.mock import patch

from review_judge_eval import default_context
from review_judge_full_context import judgment_prompt


class FullContextTests(unittest.TestCase):
    def test_prompt_keeps_default_evidence_and_excludes_subject_threads_and_labels(self):
        finding = {"finding_id": "subject", "source_thread_id": "original", "finding_text": "Candidate"}
        case = {"id": "subject", "head_sha": "head", "base_sha": "base", "pr_title": "Title",
                "pr_scope": "Full scope", "findings": [finding], "context": "Full head and base windows",
                "pr": 42, "ruling": "DECLINE", "review_context": {"pr": 42, "head_sha": "head",
                "findings": [], "threads": [
                    {"thread_id": "original"},
                    {"thread_id": "other", "author": "reviewer", "resolved": True, "path": "file.rs",
                     "line": 17, "text": "Other defect", "ruling": "ACCEPT"}]}}
        resolved = [{**finding, "citation_evidence": "Resolved source"}]
        with patch("review_judge_eval.resolve_findings", return_value=resolved):
            context = default_context(case, "checkout")
        prompt = judgment_prompt(context, case, "sha256:context", ["none"], ["duplicate"])
        data = json.loads(prompt.split("<default_judgment_context>\n")[1].split("\n</default_judgment_context>")[0])
        self.assertEqual(data, {"id": "subject", "head_sha": "head", "base_sha": "base", "pr_title": "Title",
                               "pr_scope": "Full scope", "findings": resolved, "context": "Full head and base windows"})
        siblings = json.loads(prompt.split("<sibling_context>\n")[1].split("\n</sibling_context>")[0])
        self.assertEqual([entry["thread_id"] for entry in siblings["entries"]], ["other"])
        self.assertEqual(siblings["entries"][0]["text"], "Other defect")
        self.assertEqual(siblings["entries"][0]["tool_id"], "sha256:context#other")
        self.assertNotIn('"ruling"', prompt)


if __name__ == "__main__":
    unittest.main()

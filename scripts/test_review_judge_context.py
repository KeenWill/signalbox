"""Exercise subject exclusion, label isolation, and bounded sibling previews."""

import unittest

from review_judge_context import sibling_context


HEAD = "reviewed-head"


def finding(identity, text):
    return {"finding_id": identity, "author": "reviewer", "path": "src/example.rs",
            "line": 12, "text": text, "ruling": "ACCEPT", "rationale": "private label"}


class SiblingContextTests(unittest.TestCase):
    def project(self, snapshot, **overrides):
        return sibling_context(snapshot, **{
            "pr": 42, "head_sha": HEAD, "finding_ids": {"subject"},
            "source_thread_ids": {"subject-thread"}, "full_text_bytes": 16_384,
            **overrides,
        })

    def test_subject_and_its_original_thread_cannot_match_themselves(self):
        own_thread = {"thread_id": "subject-thread", "author": "reviewer",
                      "path": "src/example.rs", "line": 12, "text": "subject text",
                      "resolved": False}
        prior_thread = {**own_thread, "thread_id": "prior-thread", "resolved": True}
        projected = self.project({"pr": 42, "head_sha": HEAD,
                                  "findings": [finding("subject", "subject text"),
                                               finding("peer", "another defect")],
                                  "threads": [own_thread, prior_thread]})
        self.assertEqual([e.get("finding_id", e.get("thread_id"))
                          for e in projected["entries"]], ["peer", "prior-thread"])
        self.assertIs(projected["entries"][1]["resolved"], True)

    def test_only_review_evidence_is_projected_from_labeled_corpus_rows(self):
        projected = self.project({"pr": 42, "head_sha": HEAD,
                                  "findings": [finding("peer", "A concrete defect.")],
                                  "threads": []})
        self.assertEqual(projected["entries"], [{
            "finding_id": "peer", "author": "reviewer", "resolved": None,
            "path": "src/example.rs", "line": 12, "text": "A concrete defect.",
            "text_truncated": False,
        }])

    def test_large_sets_preview_unicode_characters_without_splitting_them(self):
        text = "é" * 301
        projected = self.project({"pr": 42, "head_sha": HEAD, "findings": [finding("peer", text)],
                                  "threads": []}, full_text_bytes=300)
        self.assertEqual(projected["entries"][0]["text"], "é" * 300)
        self.assertTrue(projected["entries"][0]["text_truncated"])

    def test_full_text_budget_applies_to_the_whole_serialized_set(self):
        snapshot = {"pr": 42, "head_sha": HEAD, "findings": [finding("peer", "x" * 301),
                                                    finding("other", "y" * 301)],
                    "threads": []}
        size = self.project(snapshot)["full_text_bytes"]
        at_budget = self.project(snapshot, full_text_bytes=size)
        below_budget = self.project(snapshot, full_text_bytes=size - 1)
        self.assertEqual([len(e["text"]) for e in at_budget["entries"]], [301, 301])
        self.assertEqual([len(e["text"]) for e in below_budget["entries"]], [300, 300])

    def test_missing_snapshot_is_not_an_empty_thread_inventory(self):
        with self.assertRaises(KeyError):
            self.project({"pr": 42, "head_sha": HEAD, "findings": []})

    def test_context_from_another_head_is_rejected(self):
        with self.assertRaisesRegex(ValueError, "review head"):
            self.project({"pr": 42, "head_sha": "different-head", "findings": [], "threads": []})

    def test_same_head_does_not_authorize_another_pull_requests_threads(self):
        with self.assertRaisesRegex(ValueError, "pull request"):
            self.project({"pr": 43, "head_sha": HEAD, "findings": [], "threads": []})


if __name__ == "__main__":
    unittest.main()

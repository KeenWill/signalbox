"""Behavior of retained context projection and committed verdict selection."""

import json
import unittest

from review_judge_agentic import retained_context, context_bytes_and_synopsis, terminal_result, sum_usage


class AgenticContextTests(unittest.TestCase):
    def test_synopsis_omits_labels_and_subject_but_retained_text_is_complete(self):
        text = "\n".join(["a finding with full details"] * 30)
        sibling = {"finding_id": "other", "author": "reviewer", "path": "a.rs",
                   "line": 10, "text": text, "ruling": "ACCEPT"}
        snapshot = {"pr": 1, "head_sha": "abc", "findings": [sibling, {**sibling, "finding_id": "subject"}],
                    "threads": [{"thread_id": "source", "resolved": True}]}
        projected = retained_context(snapshot, pr=1, head_sha="abc", finding_ids={"subject"}, source_thread_ids={"source"})
        encoded, digest, synopsis = context_bytes_and_synopsis(projected)
        self.assertEqual(json.loads(encoded)["findings"][0]["text"], text)
        self.assertEqual(projected["threads"], [])
        self.assertEqual(len(synopsis), 1)
        self.assertEqual(synopsis[0]["finding_id"], digest + "#other")
        self.assertNotIn("ruling", synopsis[0])
        self.assertNotIn(b'"ruling"', encoded)
        self.assertEqual(len(synopsis[0]["synopsis"]), 160)
        self.assertNotIn("\n", synopsis[0]["synopsis"])

    def test_usage_totals_include_every_call_and_keep_cache_axes_separate(self):
        first = {"input_tokens": "10", "output_tokens": "3", "cache_creation_input_tokens": "0", "cache_read_input_tokens": "7"}
        second = {"input_tokens": "20", "output_tokens": "4", "cache_creation_input_tokens": "2", "cache_read_input_tokens": "9"}
        self.assertEqual(sum_usage([{"usage": first}, {"usage": second}]), {
            "input_tokens": 30, "output_tokens": 7, "cache_creation_input_tokens": 2, "cache_read_input_tokens": 16})

    def test_unknown_usage_does_not_become_a_zero_or_partial_total(self):
        totals = sum_usage([{"usage": {"input_tokens": "10", "output_tokens": "2"}},
                            {"usage": {"input_tokens": None, "output_tokens": "3"}}])
        self.assertIsNone(totals["input_tokens"])
        self.assertEqual(totals["output_tokens"], 5)
        self.assertTrue(all(value is None for value in sum_usage([{"usage": None}]).values()))
        self.assertTrue(all(value is None for value in sum_usage([]).values()))

    def test_context_from_a_different_head_is_rejected(self):
        with self.assertRaisesRegex(ValueError, "differs"):
            retained_context({"pr": 1, "head_sha": "other"}, pr=1, head_sha="head",
                             finding_ids=set(), source_thread_ids=set())

    def test_result_comes_from_the_latest_assistant_entry_in_the_judged_turn(self):
        def entry(index, turn):
            return {"type": "transcript_text_entry", "entry_index": str(index), "entry_id": str(index),
                    "source_session_id": "session", "entry": {"type": "assistant", "turn_id": turn}}
        def chunk(index, text):
            return {"type": "transcript_content", "entry_index": str(index), "fragment_index": "0",
                    "final_fragment": True, "content_fragment": text}
        transcript = [entry(1, "judged"), chunk(1, "Working."), entry(3, "judged"),
                      chunk(3, '{"members":[]}'), entry(5, "other"), chunk(5, "Unrelated.")]
        result, witness = terminal_result(transcript, "judged")
        self.assertEqual(result, {"members": []})
        self.assertEqual(witness["entry_id"], "3")
        transcript[3]["final_fragment"] = False
        with self.assertRaisesRegex(ValueError, "incomplete"):
            terminal_result(transcript, "judged")


if __name__ == "__main__":
    unittest.main()

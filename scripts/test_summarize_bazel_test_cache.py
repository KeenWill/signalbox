"""Cache accounting separates this invocation from cached historical work."""

import json
import unittest

from summarize_bazel_test_cache import read_events, summarize


class CacheSummaryTests(unittest.TestCase):
    def test_remote_cached_historical_duration_is_not_executed_time(self):
        report = summarize([
            {"id": {"testResult": {"label": "//:test", "attempt": 1}},
             "testResult": {"executionInfo": {"cachedRemotely": True},
                            "testAttemptDuration": "400s"}},
            {"id": {"testSummary": {"label": "//:test"}},
             "testSummary": {"overallStatus": "PASSED"}},
            {"finished": {}, "lastMessage": True},
        ])
        self.assertIn("| //:test | 0 | 1 | 0 | 0 | 0.000 | PASSED |", report)

    def test_local_cache_hit_does_not_recount_its_historical_remote_hit(self):
        report = summarize([
            {"id": {"testResult": {"label": "//:test", "attempt": 2}},
             "testResult": {"cachedLocally": True,
                            "executionInfo": {"cachedRemotely": True},
                            "testAttemptDurationMillis": "400000"}},
            {"finished": {}, "lastMessage": True},
        ])
        self.assertIn("| //:test | 1 | 0 | 0 | 0 | 0.000 |", report)

    def test_failed_attempt_and_retry_both_count_as_executed_work(self):
        report = summarize([
            {"id": {"testResult": {"label": "//:test", "attempt": 1}},
             "testResult": {"status": "FAILED", "testAttemptDuration": "230s"}},
            {"id": {"testResult": {"label": "//:test", "attempt": 2}},
             "testResult": {"status": "PASSED", "testAttemptDurationMillis": "156000"}},
            {"id": {"testSummary": {"label": "//:test"}},
             "testSummary": {"overallStatus": "FLAKY"}},
            {"finished": {}, "lastMessage": True},
        ])
        self.assertIn("| //:test | 0 | 0 | 2 | 1 | 386.000 | FLAKY |", report)

    def test_missing_duration_is_not_reported_as_zero_execution_seconds(self):
        report = summarize([
            {"id": {"testResult": {"label": "//:test"}}, "testResult": {}},
        ])
        self.assertIn("| //:test | 0 | 0 | 1 | 0 | unavailable |", report)
        self.assertIn("Partial event stream", report)

    def test_truncated_final_record_preserves_preceding_test_results(self):
        lines = [
            '{"id":{"testResult":{"label":"//:test"}},"testResult":{"cachedLocally":true}}\n',
            '{"finished":{}}\n',
            '{"buildToolLogs":',
        ]
        report = summarize(read_events(lines))
        self.assertIn("| //:test | 1 | 0 | 0 | 0 | 0.000 |", report)
        self.assertIn("Partial event stream", report)

    def test_build_finished_without_the_last_message_is_incomplete(self):
        report = summarize([{"finished": {}}])
        self.assertIn("Partial event stream", report)

    def test_logs_after_build_finished_can_close_the_stream(self):
        report = summarize([{"finished": {}}, {"buildToolLogs": {}, "lastMessage": True}])
        self.assertNotIn("Partial event stream", report)

    def test_corrupt_interior_record_is_not_silently_ignored(self):
        with self.assertRaises(json.JSONDecodeError):
            summarize(read_events(['{broken}\n', '{"lastMessage":true}\n']))

    def test_build_without_test_results_does_not_claim_a_cache_hit_rate(self):
        self.assertIn("cache reuse is unmeasured", summarize([{"finished": {}}]))


if __name__ == "__main__":
    unittest.main()

"""Real wire samples plus missing-input and counter-reset measurement checks."""

import base64
import json
from pathlib import Path
import tempfile
import unittest

from scripts.cache_benchmark_summary import (
    counter_deltas, summarize, summarize_bep, summarize_grpc,
)

# Three complete RPCs from the public run's ordinary-job evidence, retaining
# original timestamps and payload sizes. BEP below keeps its runner counts.
# https://github.com/KeenWill/signalbox/actions/runs/34394350559
GRPC = base64.b64decode(
    'qwIKiQEKDgoFYmF6ZWwSBTkuMi4wEhFyZW1vdGVfZG93bmxvYWRlchokZmU4ZGM0YjUtNmUyMS00YjdhLTkzZDUtZjllMWUyNTMy'
    'MzAzIiRhNmI3OWNjOC0zNjU5LTQ4MjUtYjMzOC1kZGU2N2M0YzRkOWEyGHJlcG9zaXRvcnkgQEBydWxlc19ydXN0KxIAGiFnb29n'
    'bGUuYnl0ZXN0cmVhbS5CeXRlU3RyZWFtL1JlYWQiXSpbClEKT2Jsb2JzL2Q4YmNjMWUxMTFlOTgyNzBkYzAzMTcyZTIyZWY3YWY5'
    'YzJmOTRmMzI2OTgwMTYxOGEyYjgzZDIwODkwZjhkZjUvNjYyOTQ4ODEQ9AcY4ajOHyoLCNrihtUGEICfnWIyDAja4obVBhCA7NP5'
    'Aq8GCpUCCg4KBWJhemVsEgU5LjIuMBJANWE0YmEwN2MzMmY1MDdlNDBkNDNlOWVkODBhMmZmM2VhMDAyNTZkMTQ4NjY5ZGEzYWIx'
    'NzdhZjVlZGNhMmJlYhokZmU4ZGM0YjUtNmUyMS00YjdhLTkzZDUtZjllMWUyNTMyMzAzIiRhNmI3OWNjOC0zNjU5LTQ4MjUtYjMz'
    'OC1kZGU2N2M0YzRkOWEqCENvcHlGaWxlMikvL2NyYXRlcy93ZWItY29udHJhY3Q6Z2VuZXJhdGVkX3JvdW5kdHJpcDpAOGU2YmE1'
    'ZTM0YjE4MWUxNzUzYWNhMWZhNmNlMzgyM2Y5ZWJiZTUwNDI1YWU4ZDBjYmFkNTA5ZTcxM2I2ZDkzNhIAGjtidWlsZC5iYXplbC5y'
    'ZW1vdGUuZXhlY3V0aW9uLnYyLkFjdGlvbkNhY2hlL0dldEFjdGlvblJlc3VsdCK7A0K4AwpJEkUKQDVhNGJhMDdjMzJmNTA3ZTQw'
    'ZDQzZTllZDgwYTJmZjNlYTAwMjU2ZDE0ODY2OWRhM2FiMTc3YWY1ZWRjYTJiZWIQkgEwARLqAhKYAQpMYmF6ZWwtb3V0L2s4LWZh'
    'c3RidWlsZC9iaW4vY3JhdGVzL3dlYi1jb250cmFjdC90ZXN0cy9nZW5lcmF0ZWRfcm91bmR0cmlwLm1qcxJGCkAzZjlmNDkxOWIz'
    'YzFkOWQyZjAxNzk5YTRlMzYzODhjODRhOTc3OTlmNzcyMjZjY2Q2NDRkMWY5OTI4MGY3NDRmEMzUBiABMkIKQGUzYjBjNDQyOThm'
    'YzFjMTQ5YWZiZjRjODk5NmZiOTI0MjdhZTQxZTQ2NDliOTM0Y2E0OTU5OTFiNzg1MmI4NTVCQgpAZTNiMGM0NDI5OGZjMWMxNDlh'
    'ZmJmNGM4OTk2ZmI5MjQyN2FlNDFlNDY0OWI5MzRjYTQ5NTk5MWI3ODUyYjg1NUpFCgsxMC40Mi4wLjE1MBoMCPfMhdUGEJiqppgC'
    'IgwI98yF1QYQmNeInQI6DAj3zIXVBhCYqqaYAkIMCPfMhdUGEJjXiJ0CKgsInOOG1QYQwNKPWjILCJzjhtUGEIDM/2a8AgpoCg4K'
    'BWJhemVsEgU5LjIuMBIKYmVzLXVwbG9hZBokZmU4ZGM0YjUtNmUyMS00YjdhLTkzZDUtZjllMWUyNTMyMzAzIiRhNmI3OWNjOC0z'
    'NjU5LTQ4MjUtYjMzOC1kZGU2N2M0YzRkOWESABoiZ29vZ2xlLmJ5dGVzdHJlYW0uQnl0ZVN0cmVhbS9Xcml0ZSKNATKKAQp3dXBs'
    'b2Fkcy8wNjUwZTc0OC00NTZiLTQ2ZmEtOTgxZi1hZjI5ODg1YzVjNjAvYmxvYnMvODJiYTc5OGZlMDlkZjViZTZiMDJjNTZlZjg5'
    'YzAwNmM5ZTk4OTk1NmQ1NzMwNzA4NGYxZDZjNmRmNDkyOTU5MC80NDAQARi4AyIDCLgDKgEAMgK4AyoMCM/jhtUGEMDDnIEBMgwI'
    'z+OG1QYQgOPHhAE='
 )
BEP = [
    {"finished": {"exitCode": {"name": "BUILD_FAILURE", "code": 1}}},
    {"buildMetrics": {"actionSummary": {"runnerCount": [
        {"name": "total", "count": 3927},
        {"name": "remote cache hit", "count": 2420},
        {"name": "internal", "count": 1617},
    ]}}},
]


class SummaryTests(unittest.TestCase):
    def test_captured_rpc_payloads_and_nearest_rank_latencies(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "grpc.bin"
            path.write_bytes(GRPC)
            result = summarize_grpc(path)
        self.assertEqual(result["bytes_down"], 66294881)
        self.assertEqual(result["bytes_up"], 440)
        self.assertAlmostEqual(result["latency"]["p50_ms"], 27, places=3)
        self.assertAlmostEqual(result["latency"]["p95_ms"], 586, places=3)
        self.assertAlmostEqual(result["latency"]["p99_ms"], 586, places=3)
        self.assertEqual(result["blob_histogram_log2"], {"Read": {"25": 1}, "Write": {"8": 1}})

    def test_internal_bookkeeping_is_not_inferred_as_local_execution(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "bep.jsonl"
            path.write_text("\n".join(json.dumps(event) for event in BEP))
            result = summarize_bep(path)
        self.assertEqual(result["actions_total"], 3927)
        self.assertEqual(result["actions_cached"], 2420)
        self.assertEqual(result["actions_executed"], 0)

    def test_truncated_rpc_log_reports_unavailable_instead_of_partial_totals(self):
        with tempfile.TemporaryDirectory() as temp:
            directory = Path(temp)
            (directory / "grpc.bin").write_bytes(GRPC[:-1])
            result = summarize(directory)
        self.assertEqual(result["row"]["bytes_down"], "NA")
        self.assertIn("truncated", result["missing"]["bytes_down"])

    def test_missing_evidence_does_not_become_zero(self):
        with tempfile.TemporaryDirectory() as temp:
            result = summarize(Path(temp))
        self.assertEqual(result["row"]["actions_cached"], "NA")
        self.assertEqual(result["row"]["runner_cpu_s"], "NA")
        self.assertIn("runner_cpu_s", result["missing"])

    def test_counter_delta_does_not_extrapolate_scrape_boundaries(self):
        record = {"response": {"data": {"result": [
            {"metric": {"pod": "runner"}, "values": [[10, "2.5"], [25, "8.25"]]},
        ]}}}
        self.assertEqual(counter_deltas(record), [({"pod": "runner"}, 5.75)])

    def test_counter_reset_is_unavailable(self):
        record = {"response": {"data": {"result": [
            {"metric": {}, "values": [[10, "9"], [25, "1"], [40, "12"]]},
        ]}}}
        with self.assertRaisesRegex(ValueError, "counter reset"):
            counter_deltas(record)


if __name__ == "__main__":
    unittest.main()

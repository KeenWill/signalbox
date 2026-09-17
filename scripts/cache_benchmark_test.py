"""Real wire samples plus missing-input and counter-reset measurement checks."""

import base64
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

from scripts.cache_benchmark_summary import (
    counter_deltas, main, summarize, summarize_bep, summarize_grpc,
)

sys.path.insert(0, str(Path(__file__).resolve().parent))
from cache_benchmark_run import remove_scratch

# Three complete RPCs from the public run's ordinary-job evidence, retaining
# original timestamps and payload sizes. BEP below keeps its runner counts.
# https://github.com/KeenWill/signalbox/actions/runs/35287076591
GRPC = base64.b64decode(
    'rAIKiQEKDgoFYmF6ZWwSBTkuMi4wEhFyZW1vdGVfZG93bmxvYWRlchokMWIxODkzODQtMDM0Yy00NDhlLThhMDgtZjJmZWE5ZTkw'
    'MzUyIiQzZDg0YWQ2ZS0zZmZjLTQ2YTEtODRhZS1lODY2ZjEwZmJmYzYyGHJlcG9zaXRvcnkgQEBydWxlc19ydXN0KxIAGiFnb29n'
    'bGUuYnl0ZXN0cmVhbS5CeXRlU3RyZWFtL1JlYWQiXSpbClEKT2Jsb2JzL2Q4YmNjMWUxMTFlOTgyNzBkYzAzMTcyZTIyZWY3YWY5'
    'YzJmOTRmMzI2OTgwMTYxOGEyYjgzZDIwODkwZjhkZjUvNjYyOTQ4ODEQ9AcY4ajOHyoMCPjvsdUGEID3oO0CMgwI+O+x1QYQwPjl'
    'vgOdBgqNAgoOCgViYXplbBIFOS4yLjASQGE4NTZlZDdiNzM0Mzc0MTA1MWY3MDIyMDFhNTljYjFiMTQ5YzcxMzhmNzg0YjM4NzNh'
    'ZTVkMmE2MDg3NzMzMzYaJDFiMTg5Mzg0LTAzNGMtNDQ4ZS04YTA4LWYyZmVhOWU5MDM1MiIkM2Q4NGFkNmUtM2ZmYy00NmExLTg0'
    'YWUtZTg2NmYxMGZiZmM2KghDb3B5RmlsZTIhLy9jbGllbnRzL3dlYjpjb250cmFjdF9qYXZhc2NyaXB0OkAyMGZlOTA0YzllNTBl'
    'YzFiMmYzMjAyMDU3MzY3MTc0MThjZWZkZDQxYTExNTdlOThmZTY2N2ZhNDYzMjVhZGZhEgAaO2J1aWxkLmJhemVsLnJlbW90ZS5l'
    'eGVjdXRpb24udjIuQWN0aW9uQ2FjaGUvR2V0QWN0aW9uUmVzdWx0Iq8DQqwDCkkSRQpAYTg1NmVkN2I3MzQzNzQxMDUxZjcwMjIw'
    'MWE1OWNiMWIxNDljNzEzOGY3ODRiMzg3M2FlNWQyYTYwODc3MzMzNhCSATABEt4CEpEBCkViYXplbC1vdXQvazgtZmFzdGJ1aWxk'
    'L2Jpbi9jbGllbnRzL3dlYi9zcmMvZ2VuZXJhdGVkL3dlYi1jb250cmFjdC5tanMSRgpAOWVjZGU4NTBlZjZkZDJhOGZmZTU3YWNm'
    'MzQ2ZjNlMjlhNWY1YWMxM2NmOTZlZjk0ZGJiOGQzMTM5ZGI4MzY3NxC1tBQgATJCCkBlM2IwYzQ0Mjk4ZmMxYzE0OWFmYmY0Yzg5'
    'OTZmYjkyNDI3YWU0MWU0NjQ5YjkzNGNhNDk1OTkxYjc4NTJiODU1QkIKQGUzYjBjNDQyOThmYzFjMTQ5YWZiZjRjODk5NmZiOTI0'
    'MjdhZTQxZTQ2NDliOTM0Y2E0OTU5OTFiNzg1MmI4NTVKQAoKMTAuNDIuMi4yNhoLCKfdm9UGELGayD0iCwin3ZvVBhCxrLw/OgsI'
    'p92b1QYQsZrIPUILCKfdm9UGELGsvD8qDAiX8LHVBhDAmc2ZAzIMCJfwsdUGEICnhJsDvQIKaAoOCgViYXplbBIFOS4yLjASCmJl'
    'cy11cGxvYWQaJDFiMTg5Mzg0LTAzNGMtNDQ4ZS04YTA4LWYyZmVhOWU5MDM1MiIkM2Q4NGFkNmUtM2ZmYy00NmExLTg0YWUtZTg2'
    'NmYxMGZiZmM2EgAaImdvb2dsZS5ieXRlc3RyZWFtLkJ5dGVTdHJlYW0vV3JpdGUijgEyiwEKeHVwbG9hZHMvYmE4ODc0YjgtOTRi'
    'MC00MzU5LWE3Y2ItNjg0M2ZlZWE3ZTYzL2Jsb2JzL2I3ZmViYWNlNDYzNjJmYWI4Y2Q5MWUyNDU1OWY4YTQxZjg5NzFmY2U4YmU5'
    'ZmI4YTY5YmJjMDYyM2UwYjY0ZmEvMjAwMxABGNMPIgMI0w8qAQAyAtMPKgwIqfCx1QYQgI3EzgMyDAip8LHVBhDAmvvPAw=='
)
BEP = [
    {"finished": {"exitCode": {"name": "TESTS_FAILED", "code": 3}}},
    {"buildMetrics": {"actionSummary": {"runnerCount": [
        {"name": "total", "count": 4345},
        {"name": "remote cache hit", "count": 2901},
        {"name": "internal", "count": 1674},
        {"name": "local", "count": 27},
        {"name": "processwrapper-sandbox", "count": 5},
    ]}}},
]


class SummaryTests(unittest.TestCase):
    def test_scratch_cleanup_removes_read_only_directories_without_following_symlinks(self):
        with tempfile.TemporaryDirectory() as temp:
            parent = Path(temp)
            outside = parent / "retained"
            outside.write_text("evidence")
            scratch = parent / "scratch"
            readonly = scratch / "repository"
            readonly.mkdir(parents=True)
            (readonly / ".lock").write_text("")
            (scratch / "link").symlink_to(parent, target_is_directory=True)
            readonly.chmod(0o555)
            remove_scratch(scratch)
            self.assertFalse(scratch.exists())
            self.assertEqual(outside.read_text(), "evidence")

    def test_captured_rpc_payloads_and_nearest_rank_latencies(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "grpc.bin"
            path.write_bytes(GRPC)
            result = summarize_grpc(path)
        self.assertEqual(result["bytes_down"], 66294881)
        self.assertEqual(result["bytes_up"], 2003)
        self.assertAlmostEqual(result["latency"]["p50_ms"], 3, places=3)
        self.assertAlmostEqual(result["latency"]["p95_ms"], 171, places=3)
        self.assertAlmostEqual(result["latency"]["p99_ms"], 171, places=3)
        self.assertEqual(result["blob_histogram_log2"], {"Read": {"25": 1}, "Write": {"10": 1}})

    def test_internal_bookkeeping_is_not_inferred_as_local_execution(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "bep.jsonl"
            path.write_text("\n".join(json.dumps(event) for event in BEP))
            result = summarize_bep(path)
        self.assertEqual(result["actions_total"], 4345)
        self.assertEqual(result["actions_cached"], 2901)
        self.assertEqual(result["actions_executed"], 32)

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

    def test_interrupted_run_enrichment_still_writes_na_summary(self):
        with tempfile.TemporaryDirectory() as temp:
            directory = Path(temp)
            (directory / "run.json").write_text(json.dumps({"start": 100}))
            with patch("sys.argv", ["summary", temp, "--prometheus", "http://unused.invalid",
                                    "--cache-namespace", "cache"]), patch("sys.stdout"):
                main()
            result = json.loads((directory / "summary.json").read_text())
            self.assertTrue((directory / "summary.tsv").is_file())
        self.assertEqual(result["row"]["cache_cpu_s"], "NA")
        self.assertIn("end", result["missing"]["cache_cpu_s"])


if __name__ == "__main__":
    unittest.main()

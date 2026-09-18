"""Real wire samples plus missing-input and counter-reset measurement checks."""

import base64
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

from scripts.cache_benchmark_summary import (
    collect_prometheus, counter_deltas, main, summarize, summarize_bep, summarize_grpc,
)

sys.path.insert(0, str(Path(__file__).resolve().parent))
from cache_benchmark_run import load_config, main as run_benchmark, remove_scratch

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
    def test_failed_help_flag_check_or_shutdown_still_finalizes_and_continues(self):
        for failure, expected in (("help", 17), ("flag", 1), ("shutdown", 19)):
            with self.subTest(failure=failure), tempfile.TemporaryDirectory() as temp:
                roots = []

                def run(command, **kwargs):
                    root = command[1]
                    if root not in roots:
                        roots.append(root)
                    first_run = root == roots[0]
                    if "help" in command:
                        if first_run and failure == "help":
                            raise subprocess.CalledProcessError(17, command)
                        kwargs["stdout"].write("missing" if first_run and failure == "flag" else
                                               "build_event_json_file execution_log_compact_file remote_grpc_log profile remote_accept_cached")
                    if "shutdown" in command and first_run and failure == "shutdown":
                        raise subprocess.CalledProcessError(19, command)
                    return subprocess.CompletedProcess(command, 0)

                output = Path(temp) / "evidence"
                config = dict(cache_endpoint="grpc://cache.invalid", mode="warm", runs=2,
                              targets="//:rust_build", label="finalization")
                with patch("cache_benchmark_run.load_config", return_value=config), \
                        patch("cache_benchmark_run.subprocess.run", side_effect=run), \
                        patch("cache_benchmark_run.subprocess.check_output", return_value="test-sha"), \
                        patch.dict(os.environ, RUNNER_TEMP=temp), \
                        patch("sys.argv", ["runner", str(output)]), patch("sys.stdout"):
                    self.assertEqual(run_benchmark(), expected)
                for repetition in (1, 2):
                    evidence = output / f"run-{repetition}"
                    self.assertTrue((evidence / "summary.tsv").is_file())
                    self.assertTrue((evidence / "summary.json").is_file())
                    metadata = json.loads((evidence / "run.json").read_text())
                    self.assertEqual(metadata["harness_exit_code"], expected if repetition == 1 else 0)
                self.assertEqual(list(Path(temp).glob("cache-benchmark-*")), [])
                self.assertEqual(len(roots), 2)

    def test_buildbarn_metrics_select_frontend_requests_and_all_tier_nodes(self):
        def query_result(url, expression, when):
            if expression.startswith("kube_pod_info{namespace=\"cache\""):
                result = [{"metric": {"node": node}} for node in ("worker-1", "worker-2")]
            elif expression.startswith("node_uname_info"):
                result = [{"metric": {"instance": host}} for host in ("10.0.0.1:9100", "10.0.0.2:9100")]
            else:
                result = []
            return {"data": {"result": result}}
        with patch("scripts.cache_benchmark_summary.query", side_effect=query_result):
            result = collect_prometheus({"start": 10, "end": 40, "runner_pod": "runner"},
                                        "http://unused.invalid", "cache", "buildbarn-.*", "buildbarn-l1")
        self.assertIn('pod=~"buildbarn-.*"', result["cache_cpu_s"]["query"])
        self.assertIn('service="buildbarn-l1"', result["ac_cas"]["query"])
        self.assertIn("grpc_server_handled_total", result["ac_cas"]["query"])
        self.assertIn(json.dumps(r"worker\-1|worker\-2"), result["cache_node"]["query"])
        self.assertIn("9100|", result["network_in_bytes"]["query"])

    def test_buildbarn_request_statuses_keep_grpc_meaning(self):
        record = {"response": {"data": {"result": [
            {"metric": {"grpc_service": "build.bazel.remote.execution.v2.ActionCache",
                        "grpc_method": "GetActionResult", "grpc_code": "NotFound"},
             "values": [[10, "2"], [20, "5"]]},
            {"metric": {"grpc_service": "google.bytestream.ByteStream",
                        "grpc_method": "Read", "grpc_code": "OK"},
             "values": [[10, "7"], [20, "11"]]},
        ]}}}
        with tempfile.TemporaryDirectory() as temp:
            directory = Path(temp)
            (directory / "prometheus.json").write_text(json.dumps({"ac_cas": record}))
            result = summarize(directory)
        self.assertEqual(result["row"]["ac_cas"], {"ac/GetActionResult/NotFound": 3, "cas/Read/OK": 4})

    def test_pr_endpoint_overrides_repository_default_and_blank_falls_back(self):
        for selected in ("grpc://selected.invalid:9092", ""):
            with self.subTest(selected=selected), patch.dict("os.environ", {
                "GITHUB_EVENT_NAME": "pull_request", "CACHE_ENDPOINT": "grpc://default.invalid:9092",
            }), patch.object(Path, "read_text", return_value=json.dumps({"cache_endpoint": selected})):
                self.assertEqual(load_config()["cache_endpoint"], selected or "grpc://default.invalid:9092")

    def test_dispatch_uses_its_input_endpoint_without_reading_pr_config(self):
        with patch.dict("os.environ", {
            "GITHUB_EVENT_NAME": "workflow_dispatch", "CACHE_ENDPOINT": "grpc://dispatch.invalid:9092",
            "BENCHMARK_MODE": "warm", "BENCHMARK_RUNS": "1", "BENCHMARK_TARGETS": "//:rust_build",
            "BENCHMARK_LABEL": "dispatch",
        }), patch.object(Path, "read_text", side_effect=AssertionError("PR config read")):
            self.assertEqual(load_config()["cache_endpoint"], "grpc://dispatch.invalid:9092")

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
        self.assertEqual(result["row"]["job_wall_s"], "NA")
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

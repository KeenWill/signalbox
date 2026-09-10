"""Check resource counters, accounting gaps and the measured command's result."""

import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

import measure_docker_resources as resources


def snapshot(identifier, cpu, charged=1024, working=512):
    return {
        "id": identifier,
        "read": "2026-09-10T00:00:00Z",
        "cpu_seconds_total": cpu,
        "charged_memory_bytes": charged,
        "working_memory_bytes": working,
    }


class DockerResourcesTests(unittest.TestCase):
    def sample(self, sampler, rows, at):
        with (
            patch.object(resources, "docker_json", return_value=rows),
            patch.object(resources, "container_stats", side_effect=lambda row: row),
            patch.object(resources.time, "monotonic", return_value=at),
        ):
            return sampler.sample()

    def test_counters_exclude_existing_cpu_and_include_new_containers(self):
        sampler = resources.DockerSampler()
        first = self.sample(sampler, [snapshot("existing", 100)], 10)
        self.assertIsNone(first["cpu_cores"])
        second = self.sample(sampler, [snapshot("existing", 104), snapshot("new", 2)], 12)
        self.assertEqual(second["cpu_cores"], 3)
        self.assertEqual(second["cpu_seconds_observed"], 6)
        self.assertEqual(second["charged_memory_bytes"], 2048)
        self.assertEqual(second["working_memory_bytes"], 1024)
        self.assertEqual(second["containers"], 2)

    def test_disappeared_container_is_not_charged_twice_when_it_reappears(self):
        sampler = resources.DockerSampler()
        self.sample(sampler, [], 0)
        self.sample(sampler, [snapshot("one", 2)], 1)
        self.sample(sampler, [], 2)
        resumed = self.sample(sampler, [snapshot("one", 3)], 3)
        self.assertEqual(resumed["cpu_seconds_observed"], 3)
        self.assertEqual(resumed["cpu_cores"], 1)

    def test_memory_peak_sums_simultaneous_containers(self):
        sampler = resources.DockerSampler()
        first = self.sample(sampler, [snapshot("a", 0, 400, 300)], 0)
        second = self.sample(sampler, [snapshot("b", 0, 500, 400)], 1)
        self.assertEqual(max(first["working_memory_bytes"], second["working_memory_bytes"]), 400)

    def test_empty_daemon_is_measured_zero_after_cpu_baseline(self):
        sampler = resources.DockerSampler()
        self.sample(sampler, [], 0)
        sample = self.sample(sampler, [], 1)
        self.assertEqual(sample["cpu_cores"], 0)
        self.assertEqual(sample["working_memory_bytes"], 0)
        self.assertEqual(sample["containers"], 0)

    def test_one_shot_preserves_charged_memory_and_subtracts_inactive_cache(self):
        raw = {
            "read": "2026-09-10T00:00:00Z",
            "cpu_stats": {"cpu_usage": {"total_usage": 2500000000}},
            "memory_stats": {"usage": 4096, "stats": {"inactive_file": 1024}},
        }
        with patch.object(resources, "docker_json", return_value=raw) as request:
            result = resources.container_stats({"Id": "one"})
        request.assert_called_once_with("/containers/one/stats?stream=false&one-shot=true")
        self.assertEqual(result, snapshot("one", 2.5, 4096, 3072))

    def test_missing_container_statistics_leave_a_gap(self):
        sampler = resources.DockerSampler()
        with (
            patch.object(resources, "docker_json", return_value=[{"Id": "gone"}]),
            patch.object(resources, "container_stats", side_effect=ValueError("HTTP 404")),
            self.assertRaisesRegex(ValueError, "HTTP 404"),
        ):
            sampler.sample()
        self.assertIsNone(sampler.previous_time)

    def test_sampling_failure_preserves_measured_command_exit(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "usage.json"
            sampled = Path(directory) / "sampled"

            def unavailable_docker():
                sampled.touch()
                raise FileNotFoundError("docker socket")

            command = (
                "import pathlib, sys, time\n"
                "sampled = pathlib.Path(sys.argv[1])\n"
                "deadline = time.monotonic() + 10\n"
                "while not sampled.exists() and time.monotonic() < deadline:\n"
                "    time.sleep(0.01)\n"
                "sys.exit(7 if sampled.exists() else 99)\n"
            )
            with patch.object(
                resources.DockerSampler, "sample", side_effect=unavailable_docker
            ) as sample:
                result = resources.run_command(
                    [sys.executable, "-c", command, str(sampled)], output
                )
            sample.assert_called()
            self.assertEqual(result, 7)
            recorded = json.loads(output.read_text())
            self.assertEqual(recorded["command_returncode"], 7)
            self.assertEqual(set(recorded["errors"]), {"docker socket"})
            self.assertIn("unmeasured", resources.report(output))

    def test_report_does_not_call_the_initial_cpu_baseline_a_measurement(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "usage.json"
            sample = self.sample(resources.DockerSampler(), [snapshot("a", 100)], 1)
            output.write_text(json.dumps({"samples": [sample], "errors": []}))
            report = resources.report(output)
            self.assertIn("| CPU cores | unmeasured |", report)
            self.assertIn("| Charged memory, GiB |", report)

    def test_missing_output_is_unmeasured(self):
        with tempfile.TemporaryDirectory() as directory:
            self.assertIn("unmeasured", resources.report(Path(directory) / "absent"))


if __name__ == "__main__":
    unittest.main()

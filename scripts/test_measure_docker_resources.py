"""Check Docker accounting and preservation of the measured command's result."""

import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

import measure_docker_resources as resources


class DockerResourcesTests(unittest.TestCase):
    def test_sample_sums_simultaneous_containers_and_converts_cpu_percent(self):
        rows = [
            {"CPUPerc": "250%", "MemUsage": "1GiB / 128GiB"},
            {"CPUPerc": "50%", "MemUsage": "512MiB / 128GiB"},
        ]
        self.assertEqual(
            resources.parse_stats("\n".join(map(json.dumps, rows))),
            {"cpu_cores": 3, "working_memory_bytes": 1.5 * 1024**3, "containers": 2},
        )

    def test_empty_daemon_is_a_measured_zero(self):
        self.assertEqual(
            resources.parse_stats(""),
            {"cpu_cores": 0, "working_memory_bytes": 0, "containers": 0},
        )

    def test_invalid_resource_units_and_nonfinite_cpu_are_not_zero_usage(self):
        for cpu, memory in [
            ("NaN%", "1GiB"),
            ("-1%", "1GiB"),
            ("10", "1GiB"),
            ("1%", "unknown"),
        ]:
            with self.subTest(cpu=cpu, memory=memory), self.assertRaises(ValueError):
                resources.parse_stats(json.dumps({"CPUPerc": cpu, "MemUsage": memory}))

    def test_decimal_and_binary_memory_units_differ(self):
        self.assertEqual(resources.memory_bytes("1.5 MB"), 1500000)
        self.assertEqual(resources.memory_bytes("1.5MiB"), 1572864)

    def test_missing_or_null_statistics_are_not_measured_zero(self):
        for value in [{}, {"CPUPerc": None, "MemUsage": "1GiB"}, []]:
            with self.subTest(value=value), self.assertRaises(ValueError):
                resources.parse_stats(json.dumps(value))

    def test_report_uses_concurrent_totals_instead_of_summing_individual_peaks(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "usage.json"
            output.write_text(
                json.dumps(
                    {
                        "samples": [
                            {
                                "cpu_cores": 3,
                                "working_memory_bytes": 2 * 1024**3,
                                "containers": 2,
                            },
                            {
                                "cpu_cores": 2,
                                "working_memory_bytes": 3 * 1024**3,
                                "containers": 3,
                            },
                        ],
                        "errors": [],
                    }
                )
            )
            self.assertIn("| Working memory, GiB | 3.00 |", resources.report(output))

    def test_command_failure_survives_missing_docker(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "usage.json"
            with patch.object(
                resources, "sample_docker", side_effect=FileNotFoundError("docker")
            ):
                result = resources.run_command(
                    [sys.executable, "-c", "import sys; sys.exit(7)"], output
                )
            self.assertEqual(result, 7)
            self.assertEqual(json.loads(output.read_text())["command_returncode"], 7)
            self.assertIn("unmeasured", resources.report(output))

    def test_missing_output_is_unmeasured(self):
        with tempfile.TemporaryDirectory() as directory:
            self.assertIn("unmeasured", resources.report(Path(directory) / "absent"))


if __name__ == "__main__":
    unittest.main()

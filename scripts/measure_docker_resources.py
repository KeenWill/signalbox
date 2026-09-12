"""Sample Docker test-container resources while preserving a command's exit status."""

import argparse
from concurrent.futures import ThreadPoolExecutor
import http.client
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import time
from typing import Any

SAMPLE_INTERVAL_SECONDS = 5
DOCKER_TIMEOUT_SECONDS = 5
GIBIBYTE = 1024**3


class DockerConnection(http.client.HTTPConnection):
    """Read the local Docker socket used by hosted and ARC Linux runners."""

    def __init__(self, socket_path: str):
        super().__init__("localhost", timeout=DOCKER_TIMEOUT_SECONDS)
        self.socket_path = socket_path

    def connect(self) -> None:
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.socket_path)


def docker_json(path: str) -> Any:
    endpoint = os.environ.get("DOCKER_HOST", "unix:///var/run/docker.sock")
    if not endpoint.startswith("unix://"):
        raise ValueError("Resource collection requires the runner's Unix Docker socket")
    connection = DockerConnection(endpoint.removeprefix("unix://"))
    try:
        connection.request("GET", path)
        response = connection.getresponse()
        body = response.read()
        if response.status != 200:
            raise ValueError(f"Docker {path}: HTTP {response.status}")
        return json.loads(body)
    finally:
        connection.close()


def container_stats(container: dict[str, Any]) -> dict[str, Any]:
    identifier = container["Id"]
    raw = docker_json(f"/containers/{identifier}/stats?stream=false&one-shot=true")
    memory = raw["memory_stats"]
    charged = memory["usage"]
    inactive = memory["stats"].get("inactive_file", memory["stats"].get("total_inactive_file", 0))
    return {
        "id": identifier,
        "read": raw["read"],
        "cpu_seconds_total": raw["cpu_stats"]["cpu_usage"]["total_usage"] / 1e9,
        "charged_memory_bytes": charged,
        "working_memory_bytes": max(0, charged - inactive),
    }


class DockerSampler:
    def __init__(self) -> None:
        self.previous_cpu: dict[str, float] = {}
        self.previous_time: float | None = None
        self.cpu_seconds_observed = 0.0

    def sample(self) -> dict[str, Any]:
        containers = docker_json("/containers/json")
        # One-shot reads avoid the CLI's wait for two CPU snapshots per invocation.
        # A container disappearing during collection makes this an explicit gap.
        with ThreadPoolExecutor(max_workers=4) as pool:
            rows = list(pool.map(container_stats, containers))
        sampled_at = time.monotonic()
        current = {row["id"]: row["cpu_seconds_total"] for row in rows}
        delta = sum(
            max(0, value - self.previous_cpu.get(identifier, 0))
            for identifier, value in current.items()
        )
        cores = None
        if self.previous_time is not None:
            self.cpu_seconds_observed += delta
            cores = delta / (sampled_at - self.previous_time)
        # Retain counters for disappeared containers so a transient list omission
        # does not charge their complete lifetimes a second time.
        self.previous_cpu.update(current)
        self.previous_time = sampled_at
        return {
            "cpu_cores": cores,
            "cpu_seconds_observed": self.cpu_seconds_observed,
            "working_memory_bytes": sum(row["working_memory_bytes"] for row in rows),
            "charged_memory_bytes": sum(row["charged_memory_bytes"] for row in rows),
            "containers": len(rows),
            "container_stats": rows,
        }


def run_command(command: list[str], output: Path) -> int:
    samples = []
    sampler = DockerSampler()
    errors = []
    started_at_unix_seconds = time.time()
    started = time.monotonic()
    with subprocess.Popen(command) as process:
        while process.poll() is None:
            sample_started = time.monotonic()
            try:
                samples.append({**sampler.sample(), "elapsed_seconds": time.monotonic() - started})
            except (
                OSError,
                http.client.HTTPException,
                subprocess.SubprocessError,
                ValueError,
                KeyError,
                TypeError,
            ) as error:
                errors.append(str(error))
            try:
                process.wait(
                    timeout=max(0, SAMPLE_INTERVAL_SECONDS - (time.monotonic() - sample_started))
                )
            except subprocess.TimeoutExpired:
                continue
        returncode = process.wait()
    result = {
        "started_at_unix_seconds": started_at_unix_seconds,
        "duration_seconds": time.monotonic() - started,
        "command_returncode": returncode,
        "samples": samples,
        "errors": errors,
    }
    try:
        output.write_text(json.dumps(result) + "\n")
    except OSError as error:
        print(f"Docker resource report unavailable: {error}", file=sys.stderr)
    return returncode if returncode >= 0 else 128 - returncode


def report(path: Path) -> str:
    if not path.exists():
        return "Docker test-container resources: unmeasured (no completed collection).\n"
    result = json.loads(path.read_text())
    samples = result["samples"]
    if not samples:
        return f"Docker test-container resources: unmeasured; {len(result['errors'])} sampling errors.\n"
    cpu_samples = [row["cpu_cores"] for row in samples if row["cpu_cores"] is not None]
    cpu_peak = f"{max(cpu_samples):.2f}" if cpu_samples else "unmeasured"
    return (
        "### Docker test-container resources\n\n"
        f"{len(samples)} samples; {len(result['errors'])} sampling errors. "
        "Errors leave gaps in these observations.\n\n"
        "| Observed aggregate peak | Value |\n|---|---:|\n"
        f"| CPU cores | {cpu_peak} |\n"
        f"| Working memory, GiB | {max(row['working_memory_bytes'] for row in samples) / GIBIBYTE:.2f} |\n"
        f"| Charged memory, GiB | {max(row['charged_memory_bytes'] for row in samples) / GIBIBYTE:.2f} |\n"
        f"| Observed CPU seconds | {samples[-1]['cpu_seconds_observed']:.2f} |\n"
        f"| Concurrent Docker containers | {max(row['containers'] for row in samples)} |\n\n"
        "Docker containers only; excludes the runner and Docker daemon. "
        "Samples sum concurrent containers. Working memory excludes inactive file cache on Linux; "
        "charged memory includes it. "
        "CPU is derived from cumulative counters; the first sample establishes a baseline. "
        "Short-lived containers, their final CPU increments, and peaks between five-second samples can be missed. "
        "Do not add this to pod totals unless cgroup placement proves the processes are outside that pod.\n"
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    modes = parser.add_subparsers(dest="mode", required=True)
    run = modes.add_parser("run")
    run.add_argument("--output", type=Path, required=True)
    run.add_argument("command", nargs=argparse.REMAINDER)
    summarize = modes.add_parser("report")
    summarize.add_argument("path", type=Path)
    arguments = parser.parse_args()
    if arguments.mode == "report":
        print(report(arguments.path), end="")
        return 0
    command = arguments.command
    if command[:1] == ["--"]:
        command = command[1:]
    if not command:
        parser.error("run requires a command after --")
    return run_command(command, arguments.output)


if __name__ == "__main__":
    sys.exit(main())

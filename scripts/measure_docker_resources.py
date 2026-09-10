"""Sample Docker test-container resources while preserving a command's exit status."""

import argparse
import json
import math
from pathlib import Path
import re
import subprocess
import sys
import time

SAMPLE_INTERVAL_SECONDS = 5
DOCKER_TIMEOUT_SECONDS = 3
GIBIBYTE = 1024**3
MEMORY_UNITS = {
    "B": 1,
    "kB": 1000,
    "MB": 1000**2,
    "GB": 1000**3,
    "TB": 1000**4,
    "KiB": 1024,
    "MiB": 1024**2,
    "GiB": GIBIBYTE,
    "TiB": 1024**4,
}


def memory_bytes(value: str) -> float:
    match = re.fullmatch(r"([0-9]+(?:\.[0-9]+)?)\s*([A-Za-z]+)", value.strip())
    if match is None or match[2] not in MEMORY_UNITS:
        raise ValueError(f"Unrecognized Docker memory value: {value}")
    return float(match[1]) * MEMORY_UNITS[match[2]]


def parse_stats(output: str) -> dict[str, float | int]:
    cores = memory = 0.0
    containers = 0
    for line in output.splitlines():
        if not line.strip():
            continue
        row = json.loads(line)
        if not isinstance(row, dict) or not all(
            isinstance(row.get(key), str) for key in ("CPUPerc", "MemUsage")
        ):
            raise ValueError("Docker statistics require CPU and memory strings")
        percentage = row["CPUPerc"]
        if not percentage.endswith("%"):
            raise ValueError("Docker CPU value requires a percentage")
        cpu = float(percentage[:-1]) / 100
        if not math.isfinite(cpu) or cpu < 0:
            raise ValueError("Docker CPU value must be finite and nonnegative")
        cores += cpu
        memory += memory_bytes(row["MemUsage"].split("/", 1)[0])
        containers += 1
    return {
        "cpu_cores": cores,
        "working_memory_bytes": memory,
        "containers": containers,
    }


def sample_docker() -> dict[str, float | int]:
    result = subprocess.run(
        ["docker", "stats", "--no-stream", "--format", "{{json .}}"],
        capture_output=True,
        text=True,
        check=True,
        timeout=DOCKER_TIMEOUT_SECONDS,
    )
    return parse_stats(result.stdout)


def run_command(command: list[str], output: Path) -> int:
    samples = []
    errors = []
    started_at_unix_seconds = time.time()
    started = time.monotonic()
    with subprocess.Popen(command) as process:
        while process.poll() is None:
            sample_started = time.monotonic()
            try:
                samples.append(
                    {"elapsed_seconds": sample_started - started, **sample_docker()}
                )
            except (
                OSError,
                subprocess.SubprocessError,
                ValueError,
                KeyError,
                TypeError,
            ) as error:
                errors.append(str(error))
            try:
                process.wait(
                    timeout=max(
                        0, SAMPLE_INTERVAL_SECONDS - (time.monotonic() - sample_started)
                    )
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
        return (
            "Docker test-container resources: unmeasured (no completed collection).\n"
        )
    result = json.loads(path.read_text())
    samples = result["samples"]
    if not samples:
        return f"Docker test-container resources: unmeasured; {len(result['errors'])} sampling errors.\n"
    return (
        "### Docker test-container resources\n\n"
        f"{len(samples)} samples; {len(result['errors'])} sampling errors. "
        "Errors leave gaps in these observations.\n\n"
        "| Observed aggregate peak | Value |\n|---|---:|\n"
        f"| CPU cores | {max(row['cpu_cores'] for row in samples):.2f} |\n"
        f"| Working memory, GiB | {max(row['working_memory_bytes'] for row in samples) / GIBIBYTE:.2f} |\n"
        f"| Concurrent Docker containers | {max(row['containers'] for row in samples)} |\n\n"
        "Docker containers only; excludes the runner and Docker daemon. "
        "Samples sum concurrent containers. Memory excludes inactive file cache on Linux. "
        "Short-lived containers and peaks between five-second samples can be missed. "
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

"""Run the CI target set with isolated local state and retain measurement inputs."""

import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import tempfile
import time

from cache_benchmark_summary import summarize, write_summary


def main():
    output = Path(sys.argv[1]).resolve()
    output.mkdir(parents=True, exist_ok=True)
    endpoint = os.environ["CACHE_ENDPOINT"]
    mode = os.environ["BENCHMARK_MODE"]
    runs = int(os.environ["BENCHMARK_RUNS"])
    targets = shlex.split(os.environ["BENCHMARK_TARGETS"])
    if not endpoint or mode not in ("warm", "cold") or runs < 1 or not targets:
        raise ValueError("endpoint, warm/cold mode, positive runs and targets required")
    overall_status = 0
    for repetition in range(1, runs + 1):
        evidence = output / f"run-{repetition}"
        evidence.mkdir()
        scratch = Path(tempfile.mkdtemp(prefix="cache-benchmark-", dir=os.environ["RUNNER_TEMP"]))
        bazel = ["bazel", f"--output_user_root={scratch / 'user'}", f"--output_base={scratch / 'output'}"]
        flags = [
            "--jobs=4", "--local_resources=cpu=4", "--local_resources=memory=4096",
            "--local_test_jobs=1", f"--repository_cache={scratch / 'repository'}",
            f"--remote_cache={endpoint}", f"--experimental_remote_downloader={endpoint}",
            "--experimental_remote_downloader_local_fallback",
            f"--build_event_json_file={evidence / 'bep.jsonl'}",
            f"--execution_log_compact_file={evidence / 'execution.bin.zst'}",
            f"--experimental_remote_grpc_log={evidence / 'grpc.bin'}",
            f"--profile={evidence / 'profile.json.gz'}",
        ]
        if mode == "cold":
            flags.append("--noremote_accept_cached")
        command = [*bazel, "test", *flags, "--", *targets]
        metadata = {
            "label": os.environ["BENCHMARK_LABEL"], "mode": mode,
            "repetition": repetition, "cache_endpoint": endpoint,
            "runner_pod": os.environ.get("HOSTNAME", "NA"),
            "run_id": os.environ.get("GITHUB_RUN_ID"),
            "sha": os.environ.get("GITHUB_SHA"), "command": command,
            "bazelrc": Path(".bazelrc").read_text(),
        }
        # Run help before the timed workload and retain the actual binary's help.
        with (evidence / "flag-help.txt").open("w") as log:
            subprocess.run([*bazel, "help", "test", "--long"], stdout=log,
                           stderr=subprocess.STDOUT, check=True)
        help_text = (evidence / "flag-help.txt").read_text()
        for flag in ("build_event_json_file", "execution_log_compact_file",
                     "remote_grpc_log", "profile", "remote_accept_cached"):
            if flag not in help_text:
                raise RuntimeError(f"bazel help test --long does not list {flag}")
        metadata["start"] = time.time()
        tick = time.monotonic()
        (evidence / "run.json").write_text(json.dumps(metadata, indent=2) + "\n")
        with (evidence / "console.log").open("w") as log:
            result = subprocess.run(command, stdout=log, stderr=subprocess.STDOUT)
        metadata.update(end=time.time(), wall_s=time.monotonic() - tick, exit_code=result.returncode)
        (evidence / "run.json").write_text(json.dumps(metadata, indent=2) + "\n")
        with (evidence / "bazel-info.txt").open("w") as log:
            info = subprocess.run([*bazel, "info"], stdout=log, stderr=subprocess.STDOUT)
        metadata["bazel_info_exit_code"] = info.returncode
        (evidence / "run.json").write_text(json.dumps(metadata, indent=2) + "\n")
        subprocess.run([*bazel, "shutdown"], check=True)
        write_summary(evidence, summarize(evidence))
        print((evidence / "summary.tsv").read_text(), flush=True)
        shutil.rmtree(scratch)
        overall_status = overall_status or result.returncode or info.returncode
    return overall_status


if __name__ == "__main__":
    sys.exit(main())

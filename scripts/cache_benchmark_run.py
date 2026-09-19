"""Run the CI target set with private output bases and retain measurement inputs."""

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


def remove_scratch(scratch):
    # Bazel repository caches contain read-only directories. Only walk this
    # repetition's owned tree; os.walk does not follow runfile symlinks.
    for directory, _, _ in os.walk(scratch):
        os.chmod(directory, 0o700)
    shutil.rmtree(scratch)


def load_config():
    if os.environ.get("GITHUB_EVENT_NAME") == "pull_request":
        config = json.loads(Path(".github/cache-benchmark.json").read_text())
    else:
        config = {name: os.environ[f"BENCHMARK_{name.upper()}"]
                  for name in ("mode", "runs", "targets", "label")}
        config["extra_flags"] = shlex.split(os.environ.get("BENCHMARK_EXTRA_FLAGS", ""))
        config["reuse_repository_cache"] = os.environ.get("BENCHMARK_REUSE_REPOSITORY_CACHE") == "true"
    config["cache_endpoint"] = config.get("cache_endpoint") or os.environ["CACHE_ENDPOINT"]
    return config


def main():
    output = Path(sys.argv[1]).resolve()
    output.mkdir(parents=True, exist_ok=True)
    config = load_config()
    endpoint = config["cache_endpoint"]
    mode = config["mode"]
    runs = int(config["runs"])
    targets = shlex.split(config["targets"])
    if not endpoint or mode not in ("warm", "cold") or runs < 1 or not targets:
        raise ValueError("endpoint, warm/cold mode, positive runs and targets required")
    overall_status = 0
    for repetition in range(1, runs + 1):
        evidence = output / f"run-{repetition}"
        evidence.mkdir()
        scratch = Path(tempfile.mkdtemp(prefix="cache-benchmark-", dir=os.environ["RUNNER_TEMP"]))
        repository_cache = (Path(os.environ["BAZEL_REPOSITORY_CACHE"])
                            if config.get("reuse_repository_cache") else scratch / "repository")
        bazel = ["bazel", f"--output_user_root={scratch / 'user'}", f"--output_base={scratch / 'output'}"]
        flags = [
            "--jobs=4", "--local_resources=cpu=4", "--local_resources=memory=4096",
            "--local_test_jobs=1", f"--repository_cache={repository_cache}",
            f"--remote_cache={endpoint}", f"--experimental_remote_downloader={endpoint}",
            "--experimental_remote_downloader_local_fallback",
            f"--build_event_json_file={evidence / 'bep.jsonl'}",
            f"--execution_log_compact_file={evidence / 'execution.bin.zst'}",
            f"--experimental_remote_grpc_log={evidence / 'grpc.bin'}",
            f"--profile={evidence / 'profile.json.gz'}",
        ]
        if mode == "cold":
            flags.append("--noremote_accept_cached")
        flags.extend(config.get("extra_flags", []))
        command = [*bazel, "test", *flags, "--", *targets]
        metadata = {
            "label": config["label"], "mode": mode,
            "repetition": repetition, "cache_endpoint": endpoint,
            "runner_pod": os.environ.get("HOSTNAME", "NA"),
            "run_id": os.environ.get("GITHUB_RUN_ID"),
            "command": command,
            "repository_cache": str(repository_cache),
            "reuse_repository_cache": bool(config.get("reuse_repository_cache")),
        }
        status = 0
        try:
            metadata["sha"] = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
            metadata["bazelrc"] = Path(".bazelrc").read_text()
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
            status = result.returncode
            metadata.update(end=time.time(), wall_s=time.monotonic() - tick, exit_code=result.returncode)
            (evidence / "run.json").write_text(json.dumps(metadata, indent=2) + "\n")
            with (evidence / "bazel-info.txt").open("w") as log:
                info = subprocess.run([*bazel, "info"], stdout=log, stderr=subprocess.STDOUT)
            metadata["bazel_info_exit_code"] = info.returncode
            (evidence / "run.json").write_text(json.dumps(metadata, indent=2) + "\n")
            status = status or info.returncode
        except (OSError, RuntimeError, subprocess.SubprocessError) as error:
            metadata["harness_error"] = str(error)
            status = status or getattr(error, "returncode", 1) or 1
        finally:
            try:
                subprocess.run([*bazel, "shutdown"], check=True)
            except (OSError, subprocess.SubprocessError) as error:
                metadata["shutdown_error"] = str(error)
                status = status or getattr(error, "returncode", 1) or 1
            try:
                metadata["harness_exit_code"] = status
                (evidence / "run.json").write_text(json.dumps(metadata, indent=2) + "\n")
                write_summary(evidence, summarize(evidence))
                print((evidence / "summary.tsv").read_text(), flush=True)
            finally:
                remove_scratch(scratch)
        overall_status = overall_status or status
    return overall_status


if __name__ == "__main__":
    sys.exit(main())

"""Execute the first manifest shard with real tests and the canary Docker socket."""

import json, pathlib, subprocess, sys, time, tomllib

root = pathlib.Path.cwd()
suites = tomllib.loads((root / ".github/postgres-integration-suites.toml").read_text())[
    "suite"
]
suite = next(s for s in suites if s["name"] == "persistence")
output = pathlib.Path(sys.argv[1])
output.mkdir(exist_ok=True)
command = [
    "bazel",
    "test",
    "--keep_going",
    "--flaky_test_attempts=2",
    "--jobs=8",
    "--local_resources=cpu=8",
    "--local_resources=memory=12288",
    "--local_test_jobs=1",
    "--test_sharding_strategy=disabled",
    "--test_env=SIGNALBOX_TEST_SHARD_INDEX=0",
    f"--test_env=SIGNALBOX_TEST_TOTAL_SHARDS={suite['shards']}",
    "--test_env=DOCKER_HOST=unix:///var/run/cicapacity.sock",
    "--cache_test_results=no",
    f"--build_event_json_file={output}/events.jsonl",
    "//:postgres_persistence",
]
started = time.time()
result = subprocess.run(command, check=False)
(output / "command.json").write_text(
    json.dumps(
        {
            "command": command,
            "started_at": started,
            "ended_at": time.time(),
            "returncode": result.returncode,
        }
    )
)
sys.exit(result.returncode)

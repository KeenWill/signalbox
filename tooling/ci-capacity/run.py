import concurrent.futures, json, os, pathlib, subprocess, threading, time, uuid

root = pathlib.Path("/bench")
results = root / os.environ.get("BENCH_RESULTS", "results")
results.mkdir(exist_ok=True)
budget = pathlib.Path(
    "/sys/fs/cgroup" + pathlib.Path("/benchmark-cgroup").read_text().strip()
)
command = [
    "/bench/lib/ld-linux-x86-64.so.2",
    "--library-path",
    "/bench/lib",
    "/bench/postgres-tests",
]
listing = subprocess.check_output(command + ["--list"], text=True)
names = sorted(
    line.removesuffix(": test")
    for line in listing.splitlines()
    if line.startswith("session_creation_and_submit::") and line.endswith(": test")
)[:24]
assert len(names) == 24
(root / "selected-tests.json").write_text(json.dumps(names))


def counters():
    def kv(name):
        return {
            k: int(v)
            for k, v in (
                line.split() for line in (budget / name).read_text().splitlines()
            )
        }

    cpu = kv("cpu.stat")
    memory = kv("memory.stat")
    charged = int((budget / "memory.current").read_text())
    return {
        "at": time.monotonic(),
        "cpu_seconds": cpu["usage_usec"] / 1e6,
        "charged": charged,
        "working": max(0, charged - memory["inactive_file"]),
        "throttled_seconds": cpu.get("throttled_usec", 0) / 1e6,
    }


def trial(mode, workers, repetition):
    identity = str(uuid.uuid4())
    tag = f"{mode}-{workers}-{repetition}"
    directory = results / tag
    directory.mkdir()
    before = counters()
    samples = [before]
    stop = threading.Event()

    def monitor():
        while not stop.wait(0.5):
            samples.append(counters())

    thread = threading.Thread(target=monitor)
    thread.start()

    def worker(slot):
        outcomes = []
        for index in range(slot, len(names), workers):
            name = names[index]
            env = os.environ.copy()
            env.pop("NEXTEST_RUN_ID", None)
            if mode == "shared":
                env.update(
                    NEXTEST_RUN_ID=identity,
                    NEXTEST_TEST_THREADS=str(workers),
                    NEXTEST_TEST_GLOBAL_SLOT=str(slot),
                )
            at = time.monotonic()
            with (directory / f"{index}.log").open("w") as log:
                try:
                    r = subprocess.run(
                        command + ["--ignored", "--exact", name, "--test-threads=1"],
                        env=env,
                        stdout=log,
                        stderr=subprocess.STDOUT,
                        timeout=120,
                    )
                    code = r.returncode
                except subprocess.TimeoutExpired:
                    code = 124
            outcomes.append(
                {"test": name, "returncode": code, "seconds": time.monotonic() - at}
            )
        return outcomes

    with concurrent.futures.ThreadPoolExecutor(max_workers=workers) as pool:
        outcomes = [row for group in pool.map(worker, range(workers)) for row in group]
    # This daemon belongs exclusively to this benchmark. Remove remaining fixture
    # containers before charging the completed trial, including shared-server teardown.
    ids = subprocess.check_output(["docker", "ps", "-aq"], text=True).split()
    if ids:
        subprocess.run(
            ["docker", "rm", "-f", "-v", *ids], check=True, stdout=subprocess.DEVNULL
        )
    after = counters()
    samples.append(after)
    stop.set()
    thread.join()
    result = {
        "mode": mode,
        "workers": workers,
        "repetition": repetition,
        "tests": outcomes,
        "wall_seconds": after["at"] - before["at"],
        "cpu_seconds": after["cpu_seconds"] - before["cpu_seconds"],
        "throttled_seconds": after["throttled_seconds"] - before["throttled_seconds"],
        "peak_working_gib": max(s["working"] for s in samples) / 2**30,
        "peak_charged_gib": max(s["charged"] for s in samples) / 2**30,
        "samples": samples,
    }
    (directory / "result.json").write_text(json.dumps(result))
    print(
        json.dumps({k: v for k, v in result.items() if k not in ["tests", "samples"]})
        + f" failures={sum(t['returncode'] != 0 for t in outcomes)}",
        flush=True,
    )
    if any(t["returncode"] for t in outcomes):
        raise RuntimeError(
            "Benchmark test failed; results are not a successful-work throughput comparison"
        )


order = [
    ("dedicated", 2),
    ("shared", 2),
    ("shared", 4),
    ("dedicated", 4),
    ("dedicated", 8),
    ("shared", 8),
]
for repetition in range(int(os.environ.get("BENCH_REPETITIONS", "3"))):
    for mode, workers in order if repetition % 2 == 0 else list(reversed(order)):
        trial(mode, workers, repetition)

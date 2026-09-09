"""Start one labeled PostgreSQL server for the owning test run under a file lock."""

import fcntl
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time


def docker(*args):
    return subprocess.check_output(["docker", *args], text=True).strip()


def main():
    directory, image, config = sys.argv[1:]
    test_pid = os.getppid()
    owner = int(subprocess.check_output(["ps", "-o", "ppid=", "-p", str(test_pid)]))
    run = os.environ["NEXTEST_RUN_ID"]
    directory = Path(directory)
    directory.mkdir(parents=True, exist_ok=True)
    with (directory / f"{run}.lock").open("w") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        label = f"org.signalbox.test-run={run}"
        container = docker("ps", "-q", "--filter", f"label={label}")
        if not container:
            ceiling = int(re.search(r"^disposable_postgres_state_ceiling_bytes = (\d+)$", config, re.M)[1])
            threads = int(os.environ.get("NEXTEST_TEST_THREADS", len(os.sched_getaffinity(0))))
            labels = ["--label", label]
            keep = os.environ.get("TESTCONTAINERS_COMMAND") == "keep"
            if not keep:
                labels += ["--label", "org.signalbox.disposable=test-container"]
            container = docker(
                "run", "-d", *labels,
                "-e", "POSTGRES_USER=signalbox", "-e", "POSTGRES_PASSWORD=signalbox-test-only",
                "-e", "POSTGRES_DB=postgres", "-p", "127.0.0.1::5432",
                "--tmpfs", f"/var/lib/postgresql:rw,size={ceiling * threads}",
                f"postgres:{image}", "-c", "max_connections=512", "-c", "fsync=off",
                "-c", "synchronous_commit=off", "-c", "full_page_writes=off",
                "-c", "shared_buffers=1GB",
            )
            if not keep:
                # The guardian owns only this container and outlives test subprocesses.
                # pidfd remains tied to this exact runner even if its PID is reused.
                owner_fd = os.pidfd_open(owner)
                subprocess.Popen(
                    [sys.executable, "-c",
                     "import select,subprocess,sys; "
                     "select.select([int(sys.argv[1])],[],[]); "
                     "subprocess.run(['docker','rm','-f','-v',sys.argv[2]],check=True)",
                     str(owner_fd), container],
                    pass_fds=(owner_fd,), start_new_session=True,
                    stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                )
                os.close(owner_fd)
            for _ in range(120):
                ready = subprocess.run(
                    ["docker", "exec", container, "pg_isready", "-h", "127.0.0.1", "-U", "signalbox"],
                    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                )
                if ready.returncode == 0:
                    break
                time.sleep(0.25)
            else:
                raise RuntimeError("test PostgreSQL server did not become ready")
        bindings = json.loads(docker("inspect", "--format", "{{json .NetworkSettings.Ports}}", container))
        port = bindings["5432/tcp"][0]["HostPort"]
        print(f"postgres://signalbox:signalbox-test-only@127.0.0.1:{port}/postgres")


if __name__ == "__main__":
    main()

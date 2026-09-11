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
    directory, image, config = sys.argv[1:4]
    if len(sys.argv) == 7:
        run, owner, slots = sys.argv[4:]
        owner, slots = int(owner), int(slots)
        requested_slot = 0  # The libtest process owns and allocates its individual slots.
    else:
        test_pid = os.getppid()
        owner = int(subprocess.check_output(["ps", "-o", "ppid=", "-p", str(test_pid)]))
        run = os.environ["NEXTEST_RUN_ID"]
        try:
            slots = max(1, int(os.environ.get("NEXTEST_TEST_THREADS", len(os.sched_getaffinity(0)))))
        except ValueError:
            slots = len(os.sched_getaffinity(0))
        requested_slot = int(os.environ.get("NEXTEST_TEST_GLOBAL_SLOT", 0))
    directory = Path(directory)
    directory.mkdir(parents=True, exist_ok=True)
    with (directory / f"{run}.lock").open("w") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        label = f"org.signalbox.test-run={run}"
        container = docker("ps", "-q", "--filter", f"label={label}")
        if not container:
            ceiling = int(re.search(r"^disposable_postgres_state_ceiling_bytes = (\d+)$", config, re.M)[1])
            mounts = []
            for slot in range(slots):
                mounts += ["--tmpfs", f"/sbx-tablespaces/{slot}:rw,size={ceiling}"]
            labels = ["--label", label]
            keep = os.environ.get("TESTCONTAINERS_COMMAND") == "keep"
            if not keep:
                labels += ["--label", "org.signalbox.disposable=test-container"]
            container = docker(
                "run", "-d", *labels,
                "-e", "POSTGRES_USER=signalbox", "-e", "POSTGRES_PASSWORD=signalbox-test-only",
                "-e", "POSTGRES_DB=postgres", "-p", "127.0.0.1::5432",
                "--tmpfs", f"/var/lib/postgresql:rw,size={ceiling}",
                *mounts,
                f"postgres:{image}", "-c", "max_connections=512", "-c", "fsync=off",
                "-c", "synchronous_commit=off", "-c", "full_page_writes=off",
                "-c", "shared_buffers=1GB",
                # Leave room for catalogs and WAL bursts within the fixed server mount.
                "-c", f"max_wal_size={ceiling // 4 // (1024 * 1024)}MB",
            )
            cleanup_directory = os.environ.get("SIGNALBOX_TEST_POSTGRES_CONTAINERS")
            if not keep and cleanup_directory:
                # The Bazel wrapper stays alive until it removes its recorded
                # servers; Bazel itself reaps detached guardian processes.
                (Path(cleanup_directory) / run).write_text(container + "\n")
            elif not keep:
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
            docker("exec", container, "sh", "-c", "chown postgres:postgres /sbx-tablespaces/*")
            subprocess.run(
                ["docker", "exec", "-i", container, "psql", "-X", "-U", "signalbox",
                 "-d", "postgres", "-v", "ON_ERROR_STOP=1"],
                input="\n".join(
                    f"CREATE TABLESPACE sbx_slot_{slot} LOCATION '/sbx-tablespaces/{slot}';"
                    for slot in range(slots)
                ),
                text=True, stdout=subprocess.DEVNULL, check=True,
            )
        bindings = json.loads(docker("inspect", "--format", "{{json .NetworkSettings.Ports}}", container))
        port = bindings["5432/tcp"][0]["HostPort"]
        mounts = json.loads(docker("inspect", "--format", "{{json .HostConfig.Tmpfs}}", container))
        slots = sum(path.startswith("/sbx-tablespaces/") for path in mounts)
        # Older nextest versions may omit the resolved thread count. Sharing a
        # bounded slot under oversubscription still preserves each database's cap.
        slot = requested_slot % slots
        print(json.dumps({"url": f"postgres://signalbox:signalbox-test-only@127.0.0.1:{port}/postgres", "slot": slot}))


if __name__ == "__main__":
    main()

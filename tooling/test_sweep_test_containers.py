#!/usr/bin/env python3
"""Regression tests for tooling/sweep-test-containers.sh.

A real Docker daemon is deliberately never contacted here: a fake `docker` on
`PATH` reports a controlled container inventory and records the removals it is
asked to perform, so the sweep's selection logic — the label filter, the age
bound, and the dry-run default — is exercised in isolation and
deterministically. The fake also lets the unreachable-daemon, unresponsive-
daemon, and mid-sweep-removal paths be tested, none of which a real daemon can
be asked to demonstrate on demand.
"""

from __future__ import annotations

import datetime as dt
import json
import os
import re
import stat
import signal
import subprocess
import tempfile
import time
import unittest
from pathlib import Path

REPOSITORY = Path(__file__).resolve().parent.parent
SWEEP = REPOSITORY / "tooling" / "sweep-test-containers.sh"
PERSISTENCE_LIB = REPOSITORY / "crates" / "persistence" / "src" / "lib.rs"

FAKE_DOCKER = '''#!/usr/bin/env python3
import os, sys, time

command = sys.argv[1]

if os.environ.get("FAKE_DOCKER_HANGS_ON") == command:
    if os.environ.get("FAKE_DOCKER_IGNORES_TERM"):
        import signal as signals

        signals.signal(signals.SIGTERM, signals.SIG_IGN)
    started = os.environ.get("FAKE_DOCKER_START_LOG")
    if started:
        with open(started, "a") as log:
            print(command, file=log)
    time.sleep(float(os.environ.get("FAKE_DOCKER_HANG_SECONDS", "3600")))
    survival = os.environ.get("FAKE_DOCKER_SURVIVAL_LOG")
    if survival:
        with open(survival, "a") as log:
            print(command, file=log)

if command == "version":
    sys.exit(1) if os.environ.get("FAKE_DOCKER_DOWN") else print("27.0.0")
elif command == "ps":
    assert "--format" in sys.argv and sys.argv[sys.argv.index("--format") + 1] == "json"
    with open(os.environ["FAKE_DOCKER_PS_ARGUMENTS"], "w") as log:
        log.write("\\n".join(sys.argv[1:]))
    if os.environ.get("FAKE_DOCKER_PS_REFUSED"):
        print(os.environ["FAKE_DOCKER_PS_REFUSED"], file=sys.stderr)
        sys.exit(1)
    print(os.environ["FAKE_DOCKER_PS"], end="")
elif command == "rm":
    assert "--force" in sys.argv and "--volumes" in sys.argv, sys.argv
    if os.environ.get("FAKE_DOCKER_RM_REFUSED"):
        print(os.environ["FAKE_DOCKER_RM_REFUSED"], file=sys.stderr)
        sys.exit(1)
    if os.environ.get("FAKE_DOCKER_RM_SILENTLY_FAILS"):
        sys.exit(1)
    gone = set(filter(None, os.environ.get("FAKE_DOCKER_RM_GONE", "").split(",")))
    removed = [name for name in sys.argv[4:] if name not in gone]
    with open(os.environ["FAKE_DOCKER_RM_LOG"], "a") as log:
        log.write("\\n".join(removed) + "\\n")
    for name in removed:
        print(name)
    for name in sys.argv[4:]:
        if name in gone:
            print(
                f"Error response from daemon: No such container: {name}",
                file=sys.stderr,
            )
    if os.environ.get("FAKE_DOCKER_RM_NOTE"):
        print(os.environ["FAKE_DOCKER_RM_NOTE"], file=sys.stderr)
    if os.environ.get("FAKE_DOCKER_RM_NOTE_IS_FATAL"):
        sys.exit(1)
    sys.exit(1 if gone & set(sys.argv[4:]) else 0)
elif command == "volume":
    assert "dangling=true" in sys.argv, sys.argv
    if os.environ.get("FAKE_DOCKER_VOLUME_REFUSED"):
        print(os.environ["FAKE_DOCKER_VOLUME_REFUSED"], file=sys.stderr)
        sys.exit(1)
    print(os.environ["FAKE_DOCKER_DANGLING"], end="")
else:
    raise SystemExit(f"fake docker: unexpected command: {command}")
'''


def rust_number_constant(name: str) -> int:
    """Read one `pub const … u64` from the persistence crate, outside test bodies.

    The sweep's default age bound and the bound anything holding a marked
    container checks itself against have to be the same number.
    """
    pattern = rf"^pub const {name}: u64 = (\d+);$"
    found = re.search(pattern, PERSISTENCE_LIB.read_text(), re.MULTILINE)
    assert found is not None, f"{PERSISTENCE_LIB} declares no {name}"
    return int(found.group(1))


def rust_string_constant(name: str) -> str:
    """Read one `pub const … &str` from the persistence crate, outside test bodies.

    The sweep script and the harness that marks containers must name the same
    label, and nothing but this reading connects the two spellings.
    """
    pattern = rf'^pub const {name}: &str = "([^"]+)";$'
    found = re.search(pattern, PERSISTENCE_LIB.read_text(), re.MULTILINE)
    assert found is not None, f"{PERSISTENCE_LIB} declares no {name}"
    return found.group(1)


DISPOSABLE_LABEL_KEY = rust_string_constant("DISPOSABLE_TEST_CONTAINER_LABEL_KEY")
DISPOSABLE_LABEL_VALUE = rust_string_constant("DISPOSABLE_TEST_CONTAINER_LABEL_VALUE")
DISPOSABLE_FILTER = f"label={DISPOSABLE_LABEL_KEY}={DISPOSABLE_LABEL_VALUE}"

DISPOSABLE_LIFETIME_HOURS = rust_number_constant(
    "DISPOSABLE_TEST_CONTAINER_LIFETIME_HOURS"
)

MANAGED_BY_FILTER = "label=org.testcontainers.managed-by=testcontainers"

REFUSED_VOLUME_LISTING = "Error response from daemon: volume listing is denied"

REFUSED_CONTAINER_LISTING = "Error response from daemon: listing is denied by policy"

# A SIGKILL leaves no trap to cancel the deadline, so the deadline outlives the
# sweep and must decline to signal: by the time it wakes, the identifiers it
# holds may belong to somebody else. The abandoned call therefore finishes, and
# the next sweep reclaims what it left. Seeing it reaped would mean the deadline
# signalled on behalf of a process that no longer existed.
DECLINED_TO_SIGNAL = "the orphaned deadline signalled a group it could not still own"

# Well above any run this suite asks for and well below the CI job's own
# timeout: a sweep that stopped bounding its daemon calls would otherwise hang
# the whole job for twenty minutes instead of failing here in a minute.
SWEEP_TIMEOUT_SECONDS = 60

# Distinctive enough that no other process on the machine is sleeping for it,
# so a leaked deadline from this sweep is the only thing the count can find.
LEAK_PROBE_DEADLINE_SECONDS = 883

REFUSED_REMOVAL = "Error response from daemon: removal is denied by policy"

HARMLESS_REMOVAL_NOTE = "WARNING: --volumes is deprecated in favour of -v"

MARKED_START = ".with_labels(disposable_test_container_labels())"

# A chain longer than this is not a container start; the walk backwards stops
# rather than reaching into whatever precedes an unrecognized statement.
CHAIN_LINE_LIMIT = 40

# A scan that silently matched nothing would otherwise satisfy the marking
# test with no evidence at all.
CONTAINER_START_SITES = 37


def container_start_sites() -> tuple[list[str], list[str]]:
    """Locate every testcontainers start in the tree, and those carrying no mark.

    A start site that omits the disposable mark strands containers the sweep
    cannot see, which is the leak this whole tool exists to bound, so the check
    is mechanical rather than a convention anyone has to remember.

    The scan arms on `.start()` in any file that reaches for `testcontainers`,
    not on any one image type: `AsyncRunner::start` is the only way that library
    creates a container. Keying on `Postgres::default()` would miss a suite that
    later builds its container through `GenericImage`, a shared helper, or
    anything else, and an unmarked start would then pass unnoticed.
    """
    tracked = subprocess.run(
        ["git", "ls-files", "-z", "--", "*.rs"],
        cwd=REPOSITORY,
        capture_output=True,
        text=True,
        check=True,
    ).stdout.split("\0")
    sites = []
    unmarked = []
    for name in filter(None, tracked):
        text = (REPOSITORY / name).read_text()
        if "testcontainers" not in text:
            continue
        lines = text.splitlines()
        # Every `.start()` that is not the beginning of a longer chain. The
        # runner's `start` returns a future the caller may await anywhere —
        # adjacently, or after binding it — so requiring `.await` next would
        # miss a start that was simply stored first. What it must not match is
        # the synchronous domain accessors of the same name in these suites,
        # and those are always read straight through: `activated.start()` is
        # followed by `.lineage()` or `.frontier()`, never left standing.
        # A synchronous runner reads its container straight through —
        # `image.start().unwrap()` — so in a file that imports one every
        # `.start()` counts, and the exclusion below would drop exactly those.
        # No file imports one today; the alternative is a start form this scan
        # cannot see.
        method = (
            r"\.start\(\)"
            if "SyncRunner" in text
            else r"\.start\(\)(?!\s*\.\s*(?!await\b)\w)"
        )
        # Whatever the runner trait is called at this use site. An import may
        # rename it, and `<Image as AsyncRunner>::start` qualifies it, so the
        # spellings are collected from the file rather than assumed.
        runners = {"(?:Async|Sync)Runner"} | {
            re.escape(alias)
            for alias in re.findall(r"\b(?:Async|Sync)Runner\s+as\s+(\w+)", text)
        }
        universal = r"\b(?:" + "|".join(sorted(runners)) + r")\s*>?\s*::\s*start\s*\("
        for found in re.finditer(f"{method}|{universal}", text):
            number = text.count("\n", 0, found.start()) + 1
            sites.append(f"{name}:{number}")
            head = number
            while (
                head > 1
                and number - head < CHAIN_LINE_LIMIT
                and lines[head - 2].strip()
                and not lines[head - 2].lstrip().startswith("let ")
            ):
                head -= 1
            if MARKED_START not in "\n".join(lines[head - 2 : number]):
                unmarked.append(f"{name}:{number}")
    return sites, unmarked


def sleeping_deadlines(seconds: int) -> int:
    """Count deadline processes still sleeping for `seconds`, outside test bodies.

    A deadline is a shell whose `sleep` is a child of it, so one signalled by
    identifier rather than by group leaves that `sleep` running until it expires.
    Nothing inside the sweep can observe that, which is why this looks at the
    process table.
    """
    found = subprocess.run(
        ["pgrep", "-f", f"sleep {seconds}"], capture_output=True, text=True
    )
    assert found.returncode in (0, 1), f"pgrep is unavailable: {found.stderr}"
    return len([line for line in found.stdout.split() if line])


def write_fake_docker(bin_dir: Path) -> None:
    """Install a fake docker driven by environment fixtures, outside test bodies."""
    fake = bin_dir / "docker"
    fake.write_text(FAKE_DOCKER)
    fake.chmod(fake.stat().st_mode | stat.S_IEXEC | stat.S_IXGRP | stat.S_IXOTH)


def aged(container_id: str, hours_old: float, status: str, image: str) -> tuple[str, str]:
    """State one container's identity and its creation stamp, outside test bodies."""
    created = dt.datetime.now(dt.timezone.utc) - dt.timedelta(hours=hours_old)
    stamp = created.strftime("%Y-%m-%dT%H:%M:%S") + ".123456789Z"
    return container_id, f"{stamp} {status} {image}"


class SweepRun:
    """One completed sweep and everything the fake daemon recorded about it."""

    def __init__(
        self,
        completed: subprocess.CompletedProcess[str],
        removed: list[str],
        listing_arguments: list[str],
        survived: list[str],
    ) -> None:
        self.completed = completed
        self.removed = removed
        self.listing_arguments = listing_arguments
        self.survived = survived

    @property
    def status(self) -> int:
        return self.completed.returncode

    @property
    def stdout(self) -> str:
        return self.completed.stdout

    @property
    def stderr(self) -> str:
        return self.completed.stderr

    @property
    def listing_filters(self) -> list[str]:
        """The label filters the sweep asked the daemon to list by."""
        pairs = zip(self.listing_arguments, self.listing_arguments[1:])
        return [value for flag, value in pairs if flag == "--filter"]


def restore_cancellation_signal_dispositions() -> None:
    """Give the sweep default dispositions for the signals these tests deliver.

    Daemonized CI runner services commonly start their children with SIGHUP and
    SIGQUIT ignored, and an ignored-at-entry signal cannot be trapped by the
    sweep's shell, so the cancellation would never be delivered: the sweep would
    outlive the hang and exit 0 instead of reporting the signal.
    """
    for number in (signal.SIGHUP, signal.SIGINT, signal.SIGQUIT, signal.SIGTERM):
        signal.signal(number, signal.SIG_DFL)


def cancel_a_hung_sweep(
    arguments: list[str],
    environment: dict[str, str],
    start_log: Path,
    signal_number: int,
) -> subprocess.CompletedProcess[str]:
    """Signal a sweep that is waiting on a hung daemon call, outside test bodies.

    The signal is sent only once the fake has recorded that the call it hangs on
    has begun, so the cancellation always lands while the sweep is blocked
    rather than racing its startup.
    """
    process = subprocess.Popen(
        [str(SWEEP), *arguments],
        env=environment,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        preexec_fn=restore_cancellation_signal_dispositions,
    )
    began = time.monotonic()
    while not start_log.exists() and time.monotonic() - began < SWEEP_TIMEOUT_SECONDS:
        time.sleep(0.02)
    assert start_log.exists(), "the sweep never reached the call it should hang on"
    process.send_signal(signal_number)
    stdout, stderr = process.communicate(timeout=SWEEP_TIMEOUT_SECONDS)
    return subprocess.CompletedProcess(
        process.args, process.returncode, stdout=stdout, stderr=stderr
    )


def run_sweep(
    inventory: list[tuple[str, str]],
    *,
    arguments: list[str],
    daemon_reachable: bool = True,
    hangs_on: str | None = None,
    hang_seconds: float | None = None,
    ignores_term: bool = False,
    cancel_with: int | None = None,
    listing_refused: str | None = None,
    volume_listing_refused: str | None = None,
    dangling_volumes: int = 0,
    gone_before_removal: tuple[str, ...] = (),
    removal_refused: str | None = None,
    removal_fails_silently: bool = False,
    removal_note: str | None = None,
    removal_note_is_fatal: bool = False,
) -> SweepRun:
    """Invoke the real sweep against one fake inventory, outside test bodies.

    Docker's JSON listing is the controlled inventory consumed by the sweep.
    """
    with tempfile.TemporaryDirectory() as scratch:
        scratch_path = Path(scratch)
        write_fake_docker(scratch_path)
        removal_log = scratch_path / "removed.txt"
        listing_log = scratch_path / "listing-arguments.txt"
        environment = dict(os.environ)
        environment["PATH"] = f"{scratch_path}:{environment['PATH']}"
        listing = []
        for container_id, rest in inventory:
            stamp, status, image = rest.split(" ", 2)
            created = dt.datetime.fromisoformat(stamp.replace("Z", "+00:00"))
            listing.append(
                json.dumps(
                    {
                        "ID": container_id,
                        "CreatedAt": created.strftime("%Y-%m-%d %H:%M:%S %z UTC"),
                        "State": status,
                        "Image": image,
                    }
                )
            )
        environment["FAKE_DOCKER_PS"] = "".join(f"{row}\n" for row in listing)
        environment["FAKE_DOCKER_PS_ARGUMENTS"] = str(listing_log)
        environment["FAKE_DOCKER_RM_LOG"] = str(removal_log)
        environment["FAKE_DOCKER_DANGLING"] = "".join(
            f"volume{n}\n" for n in range(dangling_volumes)
        )
        environment.pop("FAKE_DOCKER_DOWN", None)
        environment.pop("FAKE_DOCKER_HANGS_ON", None)
        environment.pop("FAKE_DOCKER_VOLUME_REFUSED", None)
        environment.pop("FAKE_DOCKER_PS_REFUSED", None)
        environment.pop("FAKE_DOCKER_RM_REFUSED", None)
        environment.pop("FAKE_DOCKER_RM_SILENTLY_FAILS", None)
        environment["FAKE_DOCKER_RM_GONE"] = ",".join(gone_before_removal)
        if removal_refused is not None:
            environment["FAKE_DOCKER_RM_REFUSED"] = removal_refused
        environment.pop("FAKE_DOCKER_RM_NOTE", None)
        environment.pop("FAKE_DOCKER_RM_NOTE_IS_FATAL", None)
        if removal_fails_silently:
            environment["FAKE_DOCKER_RM_SILENTLY_FAILS"] = "1"
        if removal_note is not None:
            environment["FAKE_DOCKER_RM_NOTE"] = removal_note
        if removal_note_is_fatal:
            environment["FAKE_DOCKER_RM_NOTE_IS_FATAL"] = "1"
        if not daemon_reachable:
            environment["FAKE_DOCKER_DOWN"] = "1"
        survival_log = scratch_path / "survived.txt"
        start_log = scratch_path / "started.txt"
        environment["FAKE_DOCKER_SURVIVAL_LOG"] = str(survival_log)
        environment["FAKE_DOCKER_START_LOG"] = str(start_log)
        environment.pop("FAKE_DOCKER_HANG_SECONDS", None)
        environment.pop("FAKE_DOCKER_IGNORES_TERM", None)
        if ignores_term:
            environment["FAKE_DOCKER_IGNORES_TERM"] = "1"
        if hangs_on is not None:
            environment["FAKE_DOCKER_HANGS_ON"] = hangs_on
        if hang_seconds is not None:
            environment["FAKE_DOCKER_HANG_SECONDS"] = str(hang_seconds)
        if listing_refused is not None:
            environment["FAKE_DOCKER_PS_REFUSED"] = listing_refused
        if volume_listing_refused is not None:
            environment["FAKE_DOCKER_VOLUME_REFUSED"] = volume_listing_refused
        if cancel_with is not None:
            completed = cancel_a_hung_sweep(arguments, environment, start_log, cancel_with)
            time.sleep((hang_seconds or 0) + 1)
            removed = removal_log.read_text().split() if removal_log.exists() else []
            listing = listing_log.read_text().split("\n") if listing_log.exists() else []
            survived = survival_log.read_text().split() if survival_log.exists() else []
            return SweepRun(completed, removed, listing, survived)
        try:
            completed = subprocess.run(
                [str(SWEEP), *arguments],
                env=environment,
                capture_output=True,
                text=True,
                timeout=SWEEP_TIMEOUT_SECONDS,
            )
        except subprocess.TimeoutExpired as expired:
            raise AssertionError(
                f"the sweep did not return within {SWEEP_TIMEOUT_SECONDS}s, so its "
                f"daemon calls are no longer bounded: {arguments}"
            ) from expired
        if hang_seconds is not None:
            # Outlast the hang the fake was given, so a daemon call the sweep
            # abandoned rather than killed has time to record that it survived.
            time.sleep(hang_seconds + 1)
        removed = removal_log.read_text().split() if removal_log.exists() else []
        listing = listing_log.read_text().split("\n") if listing_log.exists() else []
        survived = survival_log.read_text().split() if survival_log.exists() else []
        return SweepRun(completed, removed, listing, survived)


class SweepTestContainersTest(unittest.TestCase):
    def test_the_listing_asks_for_this_repository_s_disposable_label_alone(self) -> None:
        run = run_sweep(
            [aged("old111", 72, "running", "postgres:18.4-alpine3.23")],
            arguments=[],
        )

        self.assertEqual(run.status, 0, run.stderr)
        self.assertEqual(run.listing_filters, [DISPOSABLE_FILTER])

    def test_the_listing_does_not_ask_for_the_global_testcontainers_label(self) -> None:
        run = run_sweep(
            [aged("old111", 72, "running", "postgres:18.4-alpine3.23")],
            arguments=[],
        )

        self.assertEqual(run.status, 0, run.stderr)
        self.assertNotIn(MANAGED_BY_FILTER, run.listing_filters)

    def test_the_script_and_the_harness_spell_the_same_label(self) -> None:
        script = SWEEP.read_text()

        self.assertIn(f"'{DISPOSABLE_LABEL_KEY}={DISPOSABLE_LABEL_VALUE}'", script)

    def test_the_script_and_the_harness_agree_on_the_age_bound(self) -> None:
        script = SWEEP.read_text()

        self.assertIn(f"\nreadonly DISPOSABLE_LIFETIME_HOURS={DISPOSABLE_LIFETIME_HOURS}\n", script)

    def test_every_test_container_start_marks_the_container_disposable(self) -> None:
        _, unmarked = container_start_sites()

        self.assertEqual(unmarked, [], f"unswept-able container starts: {unmarked}")

    def test_the_start_site_scan_finds_the_starts_this_repository_has(self) -> None:
        sites, _ = container_start_sites()

        self.assertEqual(len(sites), CONTAINER_START_SITES, sites)

    def test_dry_run_reports_aged_containers_and_removes_nothing(self) -> None:
        run = run_sweep(
            [
                aged("old111", 72, "running", "postgres:18.4-alpine3.23"),
                aged("new222", 0.05, "running", "postgres:18.4-alpine3.23"),
            ],
            arguments=[],
        )

        self.assertEqual(run.status, 0, run.stderr)
        self.assertIn("would remove 1 container(s)", run.stdout)
        self.assertIn("old111", run.stdout)
        self.assertNotIn("new222", run.stdout)
        self.assertEqual(run.removed, [])

    def test_apply_removes_only_containers_past_the_age_bound(self) -> None:
        run = run_sweep(
            [
                aged("old111", 72, "running", "postgres:18.4-alpine3.23"),
                aged("old222", 3, "exited", "postgres:18.4-alpine3.23"),
                aged("new333", 0.05, "running", "postgres:18.4-alpine3.23"),
            ],
            arguments=["--apply"],
        )

        self.assertEqual(run.status, 0, run.stderr)
        self.assertEqual(run.removed, ["old111", "old222"])
        self.assertIn("removed 2 container(s)", run.stdout)

    def test_age_bound_is_configurable(self) -> None:
        run = run_sweep(
            [
                aged("old111", 72, "running", "postgres:18.4-alpine3.23"),
                aged("mid222", 3, "running", "postgres:18.4-alpine3.23"),
            ],
            arguments=["--older-than-hours", "48", "--apply"],
        )

        self.assertEqual(run.status, 0, run.stderr)
        self.assertEqual(run.removed, ["old111"])

    def test_a_container_serving_a_live_test_is_never_swept(self) -> None:
        run = run_sweep(
            [aged("live111", 0.25, "running", "postgres:18.4-alpine3.23")],
            arguments=["--apply"],
        )

        self.assertEqual(run.status, 0, run.stderr)
        self.assertEqual(run.removed, [])
        self.assertIn("none older than 2h", run.stdout)

    def test_volumes_belonging_to_no_container_are_reported_not_removed(self) -> None:
        run = run_sweep(
            [aged("old111", 72, "running", "postgres:18.4-alpine3.23")],
            arguments=["--apply"],
            dangling_volumes=700,
        )

        self.assertEqual(run.status, 0, run.stderr)
        self.assertEqual(run.removed, ["old111"])
        self.assertIn("700 dangling volume(s) remain", run.stdout)
        self.assertIn("docker volume prune", run.stdout)

    def test_no_dangling_volumes_reports_no_prune_advice(self) -> None:
        run = run_sweep(
            [aged("old111", 72, "running", "postgres:18.4-alpine3.23")],
            arguments=["--apply"],
            dangling_volumes=0,
        )

        self.assertEqual(run.status, 0, run.stderr)
        self.assertEqual(run.removed, ["old111"])
        self.assertNotIn("docker volume prune", run.stdout)

    def test_dangling_volumes_are_reported_when_no_container_is_present(self) -> None:
        run = run_sweep([], arguments=["--apply"], dangling_volumes=511)

        self.assertEqual(run.status, 0, run.stderr)
        self.assertIn("no disposable test containers present", run.stdout)
        self.assertIn("511 dangling volume(s) remain", run.stdout)

    def test_dangling_volumes_are_reported_when_nothing_is_old_enough(self) -> None:
        run = run_sweep(
            [aged("live111", 0.25, "running", "postgres:18.4-alpine3.23")],
            arguments=["--apply"],
            dangling_volumes=42,
        )

        self.assertEqual(run.status, 0, run.stderr)
        self.assertIn("none older than 2h", run.stdout)
        self.assertIn("42 dangling volume(s) remain", run.stdout)

    def test_dangling_volumes_are_reported_by_a_dry_run(self) -> None:
        run = run_sweep(
            [aged("old111", 72, "running", "postgres:18.4-alpine3.23")],
            arguments=[],
            dangling_volumes=9,
        )

        self.assertEqual(run.status, 0, run.stderr)
        self.assertEqual(run.removed, [])
        self.assertIn("9 dangling volume(s) remain", run.stdout)

    def test_a_container_removed_before_the_sweep_reaches_it_does_not_abort_removal(
        self,
    ) -> None:
        run = run_sweep(
            [
                aged("old111", 72, "running", "postgres:18.4-alpine3.23"),
                aged("old222", 72, "running", "postgres:18.4-alpine3.23"),
            ],
            arguments=["--apply"],
            gone_before_removal=("old111",),
        )

        self.assertEqual(run.status, 0, run.stderr)
        self.assertEqual(run.removed, ["old222"])
        self.assertIn("removed 1 container(s); 1 had already gone", run.stdout)

    def test_a_refused_removal_fails_instead_of_reporting_a_clean_sweep(self) -> None:
        run = run_sweep(
            [aged("old111", 72, "running", "postgres:18.4-alpine3.23")],
            arguments=["--apply"],
            removal_refused=REFUSED_REMOVAL,
        )

        self.assertEqual(run.status, 1)
        self.assertEqual(run.removed, [])
        self.assertIn("other than a container disappearing", run.stderr)
        self.assertIn(REFUSED_REMOVAL, run.stderr)

    def test_a_removal_failure_carrying_an_unrelated_stderr_line_is_not_read_as_a_race(
        self,
    ) -> None:
        run = run_sweep(
            [aged("old111", 72, "running", "postgres:18.4-alpine3.23")],
            arguments=["--apply"],
            removal_note=REFUSED_REMOVAL,
            removal_note_is_fatal=True,
        )

        self.assertEqual(run.status, 1)
        self.assertIn("other than a container disappearing", run.stderr)
        self.assertIn(REFUSED_REMOVAL, run.stderr)
        self.assertNotIn("had already gone", run.stdout)

    def test_a_successful_removal_that_warns_on_stderr_still_reports_a_clean_sweep(
        self,
    ) -> None:
        run = run_sweep(
            [aged("old111", 72, "running", "postgres:18.4-alpine3.23")],
            arguments=["--apply"],
            removal_note=HARMLESS_REMOVAL_NOTE,
        )

        self.assertEqual(run.status, 0, run.stderr)
        self.assertEqual(run.removed, ["old111"])
        self.assertIn("removed 1 container(s)", run.stdout)

    def test_a_silent_removal_failure_fails_instead_of_reporting_a_clean_sweep(
        self,
    ) -> None:
        run = run_sweep(
            [aged("old111", 72, "running", "postgres:18.4-alpine3.23")],
            arguments=["--apply"],
            removal_fails_silently=True,
        )

        self.assertEqual(run.status, 1)
        self.assertEqual(run.removed, [])
        self.assertIn("other than a container disappearing", run.stderr)
        self.assertNotIn("had already gone", run.stdout)

    def test_an_empty_inventory_exits_clean(self) -> None:
        run = run_sweep([], arguments=["--apply"])

        self.assertEqual(run.status, 0, run.stderr)
        self.assertEqual(run.removed, [])
        self.assertIn("no disposable test containers present", run.stdout)

    def test_a_refused_listing_fails_instead_of_reading_as_a_clean_box(self) -> None:
        run = run_sweep(
            [aged("old111", 72, "running", "postgres:18.4-alpine3.23")],
            arguments=["--apply"],
            listing_refused=REFUSED_CONTAINER_LISTING,
        )

        self.assertEqual(run.status, 1)
        self.assertEqual(run.removed, [])
        self.assertIn("could not list containers", run.stderr)
        self.assertIn(REFUSED_CONTAINER_LISTING, run.stderr)
        self.assertNotIn("no disposable test containers present", run.stdout)

    def test_an_unreachable_daemon_fails_instead_of_reporting_nothing_to_do(self) -> None:
        run = run_sweep(
            [aged("old111", 72, "running", "postgres:18.4-alpine3.23")],
            arguments=["--apply"],
            daemon_reachable=False,
        )

        self.assertEqual(run.status, 1)
        self.assertEqual(run.removed, [])
        self.assertIn("cannot reach the Docker daemon", run.stderr)

    def test_a_daemon_that_never_answers_the_probe_fails_instead_of_blocking(self) -> None:
        run = run_sweep(
            [aged("old111", 72, "running", "postgres:18.4-alpine3.23")],
            arguments=["--apply", "--deadline-seconds", "1"],
            hangs_on="version",
        )

        self.assertEqual(run.status, 1)
        self.assertEqual(run.removed, [])
        self.assertIn("did not answer the version request within 1s", run.stderr)

    def test_a_daemon_that_stops_answering_after_the_probe_fails_instead_of_blocking(
        self,
    ) -> None:
        run = run_sweep(
            [aged("old111", 72, "running", "postgres:18.4-alpine3.23")],
            arguments=["--apply", "--deadline-seconds", "1"],
            hangs_on="ps",
        )

        self.assertEqual(run.status, 1)
        self.assertEqual(run.removed, [])
        self.assertIn("did not answer the container listing within 1s", run.stderr)

    def test_a_daemon_that_hangs_on_removal_fails_instead_of_blocking(self) -> None:
        run = run_sweep(
            [aged("old111", 72, "running", "postgres:18.4-alpine3.23")],
            arguments=["--apply", "--deadline-seconds", "1"],
            hangs_on="rm",
        )

        self.assertEqual(run.status, 1)
        self.assertIn("did not answer the removal request within 1s", run.stderr)

    def test_a_finished_sweep_leaves_no_deadline_process_behind(self) -> None:
        before = sleeping_deadlines(LEAK_PROBE_DEADLINE_SECONDS)
        run = run_sweep(
            [aged("old111", 72, "running", "postgres:18.4-alpine3.23")],
            arguments=["--apply", "--deadline-seconds", str(LEAK_PROBE_DEADLINE_SECONDS)],
        )
        time.sleep(1)

        self.assertEqual(run.status, 0, run.stderr)
        self.assertEqual(run.removed, ["old111"])
        self.assertEqual(before, 0, "a previous run leaked a deadline")
        self.assertEqual(sleeping_deadlines(LEAK_PROBE_DEADLINE_SECONDS), 0)

    def test_a_daemon_call_that_refuses_to_stop_is_ended_anyway(self) -> None:
        run = run_sweep(
            [aged("old111", 72, "running", "postgres:18.4-alpine3.23")],
            arguments=["--apply", "--deadline-seconds", "1"],
            hangs_on="ps",
            ignores_term=True,
        )

        self.assertEqual(run.status, 1)
        self.assertEqual(run.removed, [])
        self.assertIn("did not answer the container listing within 1s", run.stderr)

    def test_a_daemon_call_that_overran_its_deadline_is_not_left_running(self) -> None:
        run = run_sweep(
            [aged("old111", 72, "running", "postgres:18.4-alpine3.23")],
            arguments=["--apply", "--deadline-seconds", "1"],
            hangs_on="ps",
            hang_seconds=4,
        )

        self.assertEqual(run.status, 1)
        self.assertEqual(run.survived, [], "the abandoned call kept talking to the daemon")

    def test_a_hung_sweep_cancelled_with_sigterm_reaps_its_daemon_call(self) -> None:
        run = run_sweep(
            [aged("old111", 72, "running", "postgres:18.4-alpine3.23")],
            arguments=["--apply"],
            hangs_on="ps",
            hang_seconds=4,
            cancel_with=signal.SIGTERM,
        )

        self.assertEqual(run.status, 143)
        self.assertEqual(run.survived, [], "the cancelled call kept talking to the daemon")

    def test_a_hung_sweep_cancelled_with_sigquit_reaps_its_daemon_call(self) -> None:
        run = run_sweep(
            [aged("old111", 72, "running", "postgres:18.4-alpine3.23")],
            arguments=["--apply"],
            hangs_on="ps",
            hang_seconds=4,
            cancel_with=signal.SIGQUIT,
        )

        self.assertEqual(run.status, 131)
        self.assertEqual(run.survived, [], "the cancelled call kept talking to the daemon")

    def test_a_hung_sweep_cancelled_with_sigint_reaps_its_daemon_call(self) -> None:
        run = run_sweep(
            [aged("old111", 72, "running", "postgres:18.4-alpine3.23")],
            arguments=["--apply"],
            hangs_on="ps",
            hang_seconds=4,
            cancel_with=signal.SIGINT,
        )

        self.assertEqual(run.status, 130)
        self.assertEqual(run.survived, [], "the cancelled call kept talking to the daemon")

    def test_a_hung_sweep_cancelled_with_sighup_reaps_its_daemon_call(self) -> None:
        run = run_sweep(
            [aged("old111", 72, "running", "postgres:18.4-alpine3.23")],
            arguments=["--apply"],
            hangs_on="ps",
            hang_seconds=4,
            cancel_with=signal.SIGHUP,
        )

        self.assertEqual(run.status, 129)
        self.assertEqual(run.survived, [], "the cancelled call kept talking to the daemon")

    def test_a_sigkilled_sweep_leaves_its_deadline_signalling_nobody(self) -> None:
        run = run_sweep(
            [aged("old111", 72, "running", "postgres:18.4-alpine3.23")],
            arguments=["--apply", "--deadline-seconds", "2"],
            hangs_on="ps",
            hang_seconds=5,
            cancel_with=signal.SIGKILL,
        )

        self.assertEqual(run.status, -signal.SIGKILL)
        self.assertEqual(run.survived, ["ps"], DECLINED_TO_SIGNAL)

    def test_a_daemon_that_hangs_counting_volumes_still_reports_the_removals(
        self,
    ) -> None:
        run = run_sweep(
            [aged("old111", 72, "running", "postgres:18.4-alpine3.23")],
            arguments=["--apply", "--deadline-seconds", "1"],
            hangs_on="volume",
            hang_seconds=10,
            ignores_term=True,
        )

        self.assertEqual(run.status, 0, run.stderr)
        self.assertEqual(run.removed, ["old111"])
        self.assertIn("removed 1 container(s)", run.stdout)
        self.assertIn("could not count dangling volumes", run.stderr)
        self.assertEqual(run.survived, [], "the abandoned volume count kept running")

    def test_a_refused_volume_listing_does_not_undo_a_completed_sweep(self) -> None:
        run = run_sweep(
            [aged("old111", 72, "running", "postgres:18.4-alpine3.23")],
            arguments=["--apply"],
            volume_listing_refused=REFUSED_VOLUME_LISTING,
        )

        self.assertEqual(run.status, 0, run.stderr)
        self.assertEqual(run.removed, ["old111"])
        self.assertIn("removed 1 container(s)", run.stdout)
        self.assertIn("could not count dangling volumes", run.stderr)

    def test_a_threshold_below_the_disposable_lifetime_is_refused(self) -> None:
        run = run_sweep(
            [aged("live111", 0.25, "running", "postgres:18.4-alpine3.23")],
            arguments=["--older-than-hours", str(DISPOSABLE_LIFETIME_HOURS - 1), "--apply"],
        )

        self.assertEqual(run.status, 2)
        self.assertEqual(run.removed, [])
        self.assertIn("must be at least", run.stderr)

    def test_a_zero_threshold_that_would_take_every_live_container_is_refused(self) -> None:
        run = run_sweep(
            [aged("live111", 0.25, "running", "postgres:18.4-alpine3.23")],
            arguments=["--older-than-hours", "0", "--apply"],
        )

        self.assertEqual(run.status, 2)
        self.assertEqual(run.removed, [])
        self.assertIn("must be at least", run.stderr)

    def test_a_deadline_that_is_not_a_positive_number_is_refused(self) -> None:
        run = run_sweep([], arguments=["--deadline-seconds", "0"])

        self.assertEqual(run.status, 2)
        self.assertIn("must be a positive whole number of seconds", run.stderr)


if __name__ == "__main__":
    unittest.main()

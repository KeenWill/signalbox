#!/usr/bin/env python3
"""Read the PostgreSQL integration suite manifest, for CI and for the docs gate.

`.github/postgres-integration-suites.toml` is the single authority for what the
`postgres-integration` check compiles and executes. This module is the only
reader of it, and it serves two consumers that must never disagree:

* `.github/workflows/rust.yml` calls this file as a program. `--archive-plan`
  emits the tab-separated rows its build job archives from, and `--matrix`
  emits the JSON its run job expands into a shard matrix. The workflow
  therefore restates no package, no feature, no filter, and no shard count.
* `scripts/check_docs_consistency.py` imports it. The manifest's rows tell that
  checker which `#[ignore]`d tests authoritative CI executes, which is what
  `docs/invariants.md` indexes as enforcement.

The checker previously recovered that set by regex-parsing the workflow's
folded `command: >-` shell scalars. Moving CI to `cargo nextest` made those
scalars vanish, and the parser reported zero ignored-test runs without
complaint — an index regeneration would then have silently dropped every
PostgreSQL invariant while asserting CI still enforced it. A manifest both
sides read removes that failure mode, and `check_docs_consistency.py` gates the
agreement so the manifest itself cannot drift from either side.

Run directly with `--matrix`, `--archive-plan`, or `--check` (validate the
manifest alone and print a summary). Exits nonzero with a stable message on a
malformed manifest.
"""

from __future__ import annotations

import argparse
import json
import re
import shlex
import sys
import tomllib
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MANIFEST = Path(".github/postgres-integration-suites.toml")
WORKFLOW = Path(".github/workflows/rust.yml")
EMITTER = "scripts/postgres_integration_suites.py"
ARCHIVE_ARTIFACT = re.compile(
    r"postgres-integration-archive-(?P<suite>[A-Za-z0-9][A-Za-z0-9-]*)"
)
SUITE_NAME = re.compile(r"^[a-z][a-z0-9-]*$")
RUNS_ON = re.compile(r"^[ ]*runs-on:[ ]*(?P<target>[^ #\n]+)", re.MULTILINE)


class ManifestError(Exception):
    """The manifest is absent, unparseable, or violates its own schema."""


@dataclass(frozen=True)
class Suite:
    """One archived-and-sharded PostgreSQL integration suite."""

    name: str
    package: str
    features: tuple[str, ...]
    shards: int
    skip: tuple[str, ...]

    def filterset(self) -> str:
        """Render this suite's nextest filterset expression.

        `not test(<substring>)` reproduces libtest's `--skip <substring>`:
        nextest's `test()` predicate matches a substring of the test path by
        default, which is exactly what libtest matched. With nothing skipped
        the expression is `all()`, so the run job always passes a `-E` and
        needs no conditional.
        """
        if not self.skip:
            return "all()"
        return " and ".join(f"not test({skipped})" for skipped in self.skip)


def manifest_line(text: str, name: str) -> int:
    """Return the manifest line declaring one suite, for diagnostics."""
    for number, line in enumerate(text.splitlines(), start=1):
        if line.strip() == f'name = "{name}"':
            return number
    return 1


def parse_suites(text: str) -> tuple[Suite, ...]:
    """Validate one manifest document and return its suites in file order."""
    try:
        document = tomllib.loads(text)
    except tomllib.TOMLDecodeError as error:
        raise ManifestError(f"{MANIFEST} is not valid TOML: {error}") from error
    declared = document.get("suite")
    if not isinstance(declared, list) or not declared:
        raise ManifestError(f"{MANIFEST} declares no `[[suite]]` entries")
    unexpected = sorted(key for key in document if key != "suite")
    if unexpected:
        listing = ", ".join(unexpected)
        raise ManifestError(f"{MANIFEST} has unknown top-level keys: {listing}")

    suites: list[Suite] = []
    seen: set[str] = set()
    for index, entry in enumerate(declared, start=1):
        where = f"{MANIFEST} suite {index}"
        if not isinstance(entry, dict):
            raise ManifestError(f"{where} is not a table")
        extra = sorted(
            key
            for key in entry
            if key not in {"name", "package", "features", "shards", "skip"}
        )
        if extra:
            raise ManifestError(f"{where} has unknown keys: {', '.join(extra)}")
        name = entry.get("name")
        if not isinstance(name, str) or SUITE_NAME.match(name) is None:
            raise ManifestError(
                f"{where} needs a lowercase `name` matching {SUITE_NAME.pattern}"
            )
        if name in seen:
            raise ManifestError(f"{MANIFEST} declares suite `{name}` twice")
        seen.add(name)
        package = entry.get("package")
        if not isinstance(package, str) or not package:
            raise ManifestError(f"{where} (`{name}`) needs a `package` string")
        features = entry.get("features", [])
        if not isinstance(features, list) or not all(
            isinstance(feature, str) and feature for feature in features
        ):
            raise ManifestError(
                f"{where} (`{name}`) needs `features` as a list of strings"
            )
        shards = entry.get("shards")
        if not isinstance(shards, int) or isinstance(shards, bool) or shards < 1:
            raise ManifestError(
                f"{where} (`{name}`) needs `shards` as an integer of at least 1"
            )
        skip = entry.get("skip", [])
        if not isinstance(skip, list) or not all(
            isinstance(skipped, str) and skipped.strip() for skipped in skip
        ):
            raise ManifestError(
                f"{where} (`{name}`) needs `skip` as a list of non-empty strings"
            )
        # A filterset is assembled by string concatenation, so a skip term
        # carrying filterset punctuation would silently change the expression's
        # meaning rather than exclude a test. Only a plain test-name substring
        # is admissible.
        for skipped in skip:
            if re.fullmatch(r"[A-Za-z0-9_:-]+", skipped) is None:
                raise ManifestError(
                    f"{where} (`{name}`) skip term `{skipped}` is not a plain "
                    "test-name substring"
                )
        suites.append(
            Suite(
                name=name,
                package=package,
                features=tuple(features),
                shards=shards,
                skip=tuple(skip),
            )
        )
    return tuple(suites)


def load_suites(root: Path) -> tuple[Suite, ...]:
    """Read and validate the manifest beneath one repository root."""
    manifest = root / MANIFEST
    try:
        text = manifest.read_text(encoding="utf-8")
    except OSError as error:
        raise ManifestError(f"cannot read {MANIFEST}: {error}") from error
    return parse_suites(text)


def run_matrix(suites: tuple[Suite, ...]) -> dict[str, list[dict[str, object]]]:
    """Expand the suites into the workflow's `strategy.matrix` object.

    One entry per shard, so a suite declaring one shard costs exactly one
    runner and stays in the same machinery as a sharded one.
    """
    include: list[dict[str, object]] = []
    for suite in suites:
        for partition in range(1, suite.shards + 1):
            include.append(
                {
                    "suite": suite.name,
                    "partition": partition,
                    "partitions": suite.shards,
                    "filter": suite.filterset(),
                }
            )
    return {"include": include}


def archive_plan(suites: tuple[Suite, ...]) -> str:
    """Render one tab-separated `name<TAB>package<TAB>features` row per suite.

    Tabs, not JSON: the build job reads these with a plain `while IFS=$'\\t'
    read` loop and needs no parser on the runner. Features are comma-joined
    because that is the spelling `cargo --features` already accepts, and an
    empty field means the suite adds none.
    """
    rows = [
        "\t".join((suite.name, suite.package, ",".join(suite.features)))
        for suite in suites
    ]
    return "".join(f"{row}\n" for row in rows)


def workflow_disagreements(root: Path, suites: tuple[Suite, ...]) -> list[str]:
    """Report every way the Rust workflow disagrees with the manifest.

    The workflow is checked for agreement, never parsed for meaning: this reads
    a fixed artifact-name pattern and two literal substrings, and deliberately
    does not reconstruct Cargo invocations out of YAML. What keeps the two
    sides equal is that the workflow derives its matrix and its archive plan
    from this module at run time; these assertions prove it still does.
    """
    text = (root / WORKFLOW).read_text(encoding="utf-8")
    failures: list[str] = []

    if EMITTER not in text:
        failures.append(
            f"{WORKFLOW} does not invoke {EMITTER}, so its PostgreSQL "
            f"integration jobs no longer derive from {MANIFEST}"
        )

    named = {match.group("suite") for match in ARCHIVE_ARTIFACT.finditer(text)}
    expected = {suite.name for suite in suites}
    for missing in sorted(expected - named):
        failures.append(
            f"{MANIFEST} declares suite `{missing}` but {WORKFLOW} publishes no "
            f"postgres-integration-archive-{missing} artifact"
        )
    for extra in sorted(named - expected):
        failures.append(
            f"{WORKFLOW} publishes a postgres-integration-archive-{extra} "
            f"artifact for a suite {MANIFEST} does not declare"
        )

    # An ignored-test run spelled directly in the workflow is a run the
    # manifest does not describe, which is precisely the drift this gate
    # exists to prevent. `--ignored` reaches libtest only after a `--`.
    #
    # Prose that merely mentions the command — a comment saying what a job
    # does not do — is not a command, and an apostrophe in it is not an
    # unterminated quote. Anything shlex cannot read as a command line is
    # therefore skipped rather than raised.
    for command in re.finditer(r"cargo\s+test\b[^\n]*", text):
        try:
            arguments = shlex.split(command.group(0), comments=True)
        except ValueError:
            continue
        if "--" in arguments and "--ignored" in arguments:
            failures.append(
                f"{WORKFLOW} runs ignored tests through `cargo test` outside "
                f"{MANIFEST}: {command.group(0).strip()}"
            )

    targets = {match.group("target") for match in RUNS_ON.finditer(text)}
    if targets and targets != {"ubuntu-latest"}:
        listing = ", ".join(sorted(targets))
        failures.append(f"Rust CI target changed from ubuntu-latest: {listing}")

    return failures


def documented_ignored_commands(text: str) -> list[tuple[int, list[str]]]:
    """Return documented `cargo test` commands that run ignored tests.

    Backslash continuations are folded into their opening line, so a command
    wrapped across a fenced block reads as one command and is reported at the
    line it starts on rather than the line it happens to end on.
    """
    logical: list[tuple[int, str]] = []
    for number, line in enumerate(text.splitlines(), start=1):
        if logical and logical[-1][1].endswith("\\"):
            start, previous = logical[-1]
            logical[-1] = (start, f"{previous[:-1]} {line.strip()}")
            continue
        logical.append((number, line))

    found: list[tuple[int, list[str]]] = []
    for number, line in logical:
        for match in re.finditer(r"cargo\s+test\b[^`]*", line):
            try:
                arguments = shlex.split(match.group(0))
            except ValueError:
                continue
            if "--" not in arguments or "--ignored" not in arguments:
                continue
            found.append((number, arguments))
    return found


def documentation_disagreements(
    label: str, text: str, suites: tuple[Suite, ...]
) -> list[tuple[int, str]]:
    """Report documented ignored-test commands the manifest does not describe.

    Documentation that tells a reader how to run a suite locally states the
    same package and features CI archives. When the manifest moves and the
    prose does not, the prose is wrong in the one way a reader cannot detect:
    it still runs, and it silently runs a different set of tests.
    """
    known = {(suite.package, frozenset(suite.features)) for suite in suites}
    packages = {suite.package for suite in suites}
    failures: list[tuple[int, str]] = []
    for line, arguments in documented_ignored_commands(text):
        separator = arguments.index("--")
        cargo_arguments = arguments[2:separator]
        package = None
        features: set[str] = set()
        index = 0
        while index < len(cargo_arguments):
            argument = cargo_arguments[index]
            if argument in ("-p", "--package") and index + 1 < len(cargo_arguments):
                package = cargo_arguments[index + 1]
                index += 2
                continue
            if argument in ("--features", "-F") and index + 1 < len(cargo_arguments):
                features.update(
                    part
                    for part in re.split(r"[ ,]+", cargo_arguments[index + 1])
                    if part
                )
                index += 2
                continue
            index += 1
        if package is None or package not in packages:
            continue
        if (package, frozenset(features)) in known:
            continue
        expected = sorted(
            ",".join(suite.features) or "(none)"
            for suite in suites
            if suite.package == package
        )
        failures.append(
            (
                line,
                f"{label} documents `cargo test -p {package}` with features "
                f"{','.join(sorted(features)) or '(none)'} for ignored tests, "
                f"but {MANIFEST} archives that package with "
                f"{' or '.join(expected)}",
            )
        )
    return failures


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument(
        "--matrix",
        action="store_true",
        help="emit the run job's strategy.matrix object as compact JSON",
    )
    mode.add_argument(
        "--archive-plan",
        action="store_true",
        help="emit one `name<TAB>package<TAB>features` row per suite",
    )
    mode.add_argument(
        "--check",
        action="store_true",
        help="validate the manifest and print the resolved shard topology",
    )
    arguments = parser.parse_args()

    try:
        suites = load_suites(ROOT)
    except ManifestError as error:
        print(f"suite manifest FAILED: {error}", file=sys.stderr)
        return 1

    if arguments.matrix:
        print(json.dumps(run_matrix(suites), separators=(",", ":"), sort_keys=True))
        return 0
    if arguments.archive_plan:
        sys.stdout.write(archive_plan(suites))
        return 0

    shards = sum(suite.shards for suite in suites)
    for suite in suites:
        features = ",".join(suite.features) or "(none)"
        print(
            f"{suite.name}: -p {suite.package} --features {features} "
            f"across {suite.shards} shard(s), filter {suite.filterset()}"
        )
    print(f"{len(suites)} suites over {shards} shards")
    return 0


if __name__ == "__main__":
    sys.exit(main())

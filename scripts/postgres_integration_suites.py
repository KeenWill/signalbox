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
UPLOAD_ACTION = re.compile(
    r"^(?P<indent>[ ]*)(?:-[ ]+)?uses:[ ]*actions/upload-artifact"
)
ARCHIVE_ARTIFACT = re.compile(
    r"^[ ]*name:[ ]*"
    r"(?P<suite>postgres-integration-archive-[A-Za-z0-9][A-Za-z0-9-]*)[ ]*$"
)
SUITE_NAME = re.compile(r"^[a-z][a-z0-9-]*$")
RUNS_ON = re.compile(r"^[ ]*runs-on:[ ]*(?P<target>[^ #\n]+)", re.MULTILINE)
SHELL_SCALAR = re.compile(r"^(?P<indent>[ ]*)(?:-[ ]+)?(?:run|command):(?P<inline>.*)$")
# A block header carries its chomping and indentation indicators in either
# order: `|2-` and `|-2` are the same scalar.
BLOCK_INDICATOR = re.compile(r"[|>](?:[+-]\d*|\d+[+-]?)?")
REQUIRED_MODES = ("--archive-plan", "--matrix")
INTERPRETERS = ("python3", "python")
COMMAND_SEPARATOR = re.compile(r"&&|\|\||[;|&\n]")
ATTACHED_SHORT_OPTIONS = ("-p", "-F", "-j")
CARGO_GLOBAL_VALUE_OPTIONS = ("--color", "--config", "--explain", "-Z", "-C")
CARGO_TEST_COMMANDS = ("test", "t")
SUBSTITUTION = re.compile(r"\$\((?P<body>[^()]*)\)")
# Cargo feature names, one per manifest entry. Cargo would read a comma or a
# space inside one entry as a separator and enable two features; the docs
# comparison splits documented commands the same way, so an entry carrying its
# own separator compares unequal to the identical documented command.
FEATURE_NAME = re.compile(r"[A-Za-z0-9_][A-Za-z0-9_+.-]*")


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
            isinstance(feature, str) for feature in features
        ):
            raise ManifestError(
                f"{where} (`{name}`) needs `features` as a list of strings"
            )
        for feature in features:
            if FEATURE_NAME.fullmatch(feature) is None:
                raise ManifestError(
                    f"{where} (`{name}`) feature `{feature}` is not one Cargo "
                    "feature name; list each feature as its own entry"
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


def strip_shell_comment(line: str) -> str:
    """Drop a trailing `#` comment from one shell line, honouring quotes.

    A `#` only opens a comment at the start of a word, and never inside a
    quoted string — `echo 'a # b'` prints the hash. Without this, a comment is
    indistinguishable from the command it follows, and every containment check
    below can be satisfied by text the runner never executes.
    """
    quote: str | None = None
    previous = " "
    for position, character in enumerate(line):
        if quote is not None:
            if character == quote:
                quote = None
        elif character in ("'", '"'):
            quote = character
        elif character == "#" and previous.isspace():
            return line[:position].rstrip()
        previous = character
    return line.rstrip()


def uploaded_artifacts(text: str) -> set[str]:
    """Return the artifact names published by `actions/upload-artifact` steps.

    Read from the upload steps themselves, not from the file's text: an
    artifact name surviving in a comment after its upload step was deleted
    would otherwise still count as published, and the docs gate would keep
    asserting that a suite whose archive no longer exists is executed.
    """
    lines = text.splitlines()
    names: set[str] = set()
    for index, line in enumerate(lines):
        match = UPLOAD_ACTION.match(line)
        if match is None:
            continue
        indentation = len(match.group("indent"))
        for following in lines[index + 1 :]:
            if not following.strip():
                continue
            depth = len(following) - len(following.lstrip(" "))
            if depth < indentation or (
                depth == indentation and following.lstrip().startswith("- ")
            ):
                break
            named = ARCHIVE_ARTIFACT.match(following)
            if named is not None:
                names.add(named.group("suite"))
    return names


def workflow_shell_commands(text: str) -> list[str]:
    """Return each `run:`/`command:` scalar in a workflow, flattened to one line.

    A shell command in a workflow can be spelled four ways — inline, a literal
    `|` block, a folded `>-` block, or backslash continuations — and a check
    that reads only physical lines sees a different command in each. So every
    scalar is collected whole: the opening key's own text plus every following
    line indented past it, joined with spaces, comment lines and continuation
    backslashes dropped.

    This is containment, not interpretation. It answers "does this text appear
    inside a command" without reconstructing what the command selects — the
    inference the suite manifest exists to make unnecessary.
    """
    lines = text.splitlines()
    commands: list[str] = []
    index = 0
    while index < len(lines):
        match = SHELL_SCALAR.match(lines[index])
        if match is None:
            index += 1
            continue
        indentation = len(match.group("indent"))
        # `|`, `>-`, `|+2` and friends open a block; they are not command text.
        # Which one decides how the block's lines rejoin: a literal `|` block
        # keeps its newlines, and a newline separates commands, while a folded
        # `>` block and an inline scalar wrap one command across lines.
        inline = match.group("inline").strip()
        opens_block = BLOCK_INDICATOR.fullmatch(inline) is not None
        separator = "\n" if opens_block and inline.startswith("|") else " "
        body = ["" if opens_block else inline]
        index += 1
        while index < len(lines):
            line = lines[index]
            if line.strip() and len(line) - len(line.lstrip(" ")) <= indentation:
                break
            if line.strip():
                body.append(line.strip())
            index += 1
        # Comments are stripped per physical line, before the lines rejoin: a
        # comment ends its own line, not the rest of the block. A line ending
        # in a backslash continues regardless of the block style.
        joined = ""
        pending = " "
        for part in body:
            stripped = strip_shell_comment(part)
            continues = stripped.endswith("\\")
            stripped = stripped.removesuffix("\\").strip()
            if not stripped:
                continue
            joined = stripped if not joined else f"{joined}{pending}{stripped}"
            pending = " " if continues else separator
        if joined:
            commands.append(joined)
    return commands


def simple_commands(command: str) -> list[list[str]]:
    """Split one flattened shell command into the argument lists it executes.

    Shell operators separate commands, and `$( … )` bodies are commands too —
    this workflow reads the shard matrix through one. Each piece is tokenized;
    a piece that does not tokenize is dropped rather than raised, since prose
    reaches here as readily as a command does.
    """
    segments = [command]
    segments.extend(match.group("body") for match in SUBSTITUTION.finditer(command))
    executed: list[list[str]] = []
    for segment in segments:
        for piece in COMMAND_SEPARATOR.split(segment):
            try:
                tokens = shlex.split(piece, comments=True)
            except ValueError:
                continue
            if tokens:
                executed.append(tokens)
    return executed


def invokes_reader(tokens: list[str], mode: str) -> bool:
    """Return whether one argument list actually runs the reader in `mode`.

    The reader has to be the command word — directly, or as the argument of a
    Python interpreter. `echo python3 scripts/…py --matrix` names the reader
    and every flag, and runs nothing; only the leading word separates the two.
    """
    if tokens[0] == EMITTER:
        arguments = tokens[1:]
    elif tokens[0] in INTERPRETERS and len(tokens) > 1 and tokens[1] == EMITTER:
        arguments = tokens[2:]
    else:
        return False
    return mode in arguments


def workflow_disagreements(root: Path, suites: tuple[Suite, ...]) -> list[str]:
    """Report every way the Rust workflow disagrees with the manifest.

    The workflow is checked for agreement, never parsed for meaning: this reads
    the upload steps' artifact names and the commands the runner executes, and
    deliberately does not reconstruct Cargo invocations out of YAML. What keeps
    the two sides equal is that the workflow derives its matrix and its archive
    plan from this module at run time; these assertions prove it still does.

    The boundary, chosen rather than overlooked: this detects drift, not
    sabotage. Shell control flow is not modelled, so `false && python3 …
    --matrix` reads as an invocation. An author working around the gate has
    unbounded options anyway — writing a literal matrix to `$GITHUB_OUTPUT`,
    or invoking the reader and discarding it — and none of them is decidable
    from the file. Modelling `&&` would buy one evasion at the price of making
    this a partial shell interpreter, which is the coupling the manifest was
    introduced to remove. What is caught is every way the derivation is
    honestly lost: the invocation replaced, commented out, or merely named.
    """
    text = (root / WORKFLOW).read_text(encoding="utf-8")
    commands = workflow_shell_commands(text)
    failures: list[str] = []

    # Both modes, each as a command the shell actually executes — naming the
    # reader is not running it. The modes feed different jobs: `--archive-plan`
    # the build, `--matrix` the shards, so each is asserted on its own.
    executed = [tokens for command in commands for tokens in simple_commands(command)]
    for mode in REQUIRED_MODES:
        if not any(invokes_reader(tokens, mode) for tokens in executed):
            failures.append(
                f"{WORKFLOW} executes no `{EMITTER} {mode}` command, so its "
                f"PostgreSQL integration jobs no longer derive from {MANIFEST}"
            )

    named = {
        name.removeprefix("postgres-integration-archive-")
        for name in uploaded_artifacts(text)
    }
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
    # exists to prevent. Read from the same executed commands as above, so the
    # YAML wrapping, the shell operators, and Cargo's global options before the
    # subcommand are all already resolved.
    for tokens in executed:
        arguments = cargo_test_arguments(tokens)
        if arguments is not None and runs_ignored_tests(arguments):
            failures.append(
                f"{WORKFLOW} runs ignored tests through `cargo test` outside "
                f"{MANIFEST}: {' '.join(tokens)}"
            )

    if not any(runs_archived_ignored_tests(tokens) for tokens in executed):
        failures.append(
            f"{WORKFLOW} runs no archive-backed `cargo nextest run` with "
            f"`--run-ignored only`, so the suites {MANIFEST} declares are "
            "never executed"
        )

    targets = {match.group("target") for match in RUNS_ON.finditer(text)}
    if targets and targets != {"ubuntu-latest"}:
        listing = ", ".join(sorted(targets))
        failures.append(f"Rust CI target changed from ubuntu-latest: {listing}")

    return failures


def normalized_cargo_arguments(arguments: list[str]) -> list[str]:
    """Split Cargo's attached option spellings into option and value.

    Cargo accepts `--package=spec`, `-p=spec`, and bare `-pspec` alike, and
    documentation uses all of them. Normalizing here lets every reader below
    assume the separated form; without it an attached option reads as no option
    at all, which is silence rather than disagreement.
    """
    normalized: list[str] = []
    for argument in arguments:
        if argument.startswith("--") and "=" in argument:
            option, _, value = argument.partition("=")
            normalized.extend((option, value))
            continue
        attached = next(
            (
                option
                for option in ATTACHED_SHORT_OPTIONS
                if argument.startswith(option) and len(argument) > len(option)
            ),
            None,
        )
        if attached is not None:
            normalized.extend((attached, argument[len(attached) :].removeprefix("=")))
            continue
        normalized.append(argument)
    return normalized


def cargo_subcommand_arguments(
    tokens: list[str], names: tuple[str, ...]
) -> list[str] | None:
    """Return one Cargo subcommand's arguments, or `None` for another command.

    Cargo takes global options before the subcommand — `cargo --locked test` is
    as valid as `cargo test --locked` — so the subcommand is located rather
    than assumed adjacent.
    """
    if not tokens or tokens[0].rsplit("/", 1)[-1] != "cargo":
        return None
    index = 1
    # `cargo +toolchain …` is rustup's selector, not an option; Cargo's own
    # usage line spells it `cargo [+toolchain] [OPTIONS] [COMMAND]`.
    if index < len(tokens) and tokens[index].startswith("+"):
        index += 1
    while index < len(tokens) and tokens[index].startswith("-"):
        if tokens[index] in CARGO_GLOBAL_VALUE_OPTIONS:
            index += 1
        index += 1
    if index >= len(tokens) or tokens[index] not in names:
        return None
    return tokens[index + 1 :]


def cargo_test_arguments(tokens: list[str]) -> list[str] | None:
    """Return one `cargo test` invocation's normalized arguments.

    `t` is Cargo's own alias for `test` and selects the same tests, so a
    command spelled with it makes the same claim about what CI runs.
    """
    arguments = cargo_subcommand_arguments(tokens, CARGO_TEST_COMMANDS)
    return None if arguments is None else normalized_cargo_arguments(arguments)


def runs_archived_ignored_tests(tokens: list[str]) -> bool:
    """Return whether one command runs a nextest archive's ignored tests.

    The positive half of the workflow's contract. Every other assertion here is
    negative — nothing else may run ignored tests, no suite may lack an
    artifact — and negatives alone are satisfied by a workflow that runs
    nothing at all: delete the run step and the shards pass having merely
    downloaded their archives, while `docs/invariants.md` goes on claiming
    those tests are enforced.
    """
    arguments = cargo_subcommand_arguments(tokens, ("nextest",))
    if not arguments or arguments[0] != "run":
        return False
    arguments = normalized_cargo_arguments(arguments[1:])
    if "--archive-file" not in arguments or "--run-ignored" not in arguments:
        return False
    selection = arguments.index("--run-ignored") + 1
    return selection < len(arguments) and arguments[selection] == "only"


def runs_ignored_tests(arguments: list[str]) -> bool:
    """Return whether one `cargo test` argument list selects libtest's ignored tests.

    Two spellings run them: `--ignored` runs only those, `--include-ignored`
    runs them alongside the rest. Both execute tests the manifest is supposed
    to be the sole description of, so both count.
    """
    if "--" not in arguments:
        return False
    harness = arguments[arguments.index("--") + 1 :]
    return "--ignored" in harness or "--include-ignored" in harness


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
        for match in re.finditer(r"cargo\b[^`]*", line):
            try:
                tokens = shlex.split(match.group(0))
            except ValueError:
                continue
            arguments = cargo_test_arguments(tokens)
            if arguments is None or not runs_ignored_tests(arguments):
                continue
            found.append((number, arguments))
    return found


def manifest_path_package(root: Path, relative: str) -> str | None:
    """Return the package name one `--manifest-path` selects, if it names one."""
    try:
        declared = tomllib.loads((root / relative).read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError):
        return None
    package = declared.get("package")
    name = package.get("name") if isinstance(package, dict) else None
    return name if isinstance(name, str) else None


def documentation_disagreements(
    label: str, text: str, suites: tuple[Suite, ...], root: Path = ROOT
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
        cargo_arguments = arguments[: arguments.index("--")]
        package = None
        features: set[str] = set()
        index = 0
        while index < len(cargo_arguments):
            argument = cargo_arguments[index]
            if argument in ("-p", "--package") and index + 1 < len(cargo_arguments):
                package = cargo_arguments[index + 1]
                index += 2
                continue
            # A manifest path selects its own package as surely as `-p` does.
            # `-p` wins if both appear, matching Cargo.
            if argument == "--manifest-path" and index + 1 < len(cargo_arguments):
                selected = manifest_path_package(root, cargo_arguments[index + 1])
                package = package or selected
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
        # `--all-features` and `--no-default-features` name a feature set by
        # reference to the package's own table rather than by listing it, so
        # what they select cannot be compared against the manifest without
        # resolving that table — and a reader cannot see which suite they mean
        # either. Documentation of a manifested suite states the manifest's
        # features explicitly, so these are reported rather than guessed at.
        indirect = sorted(
            flag
            for flag in ("--all-features", "--no-default-features")
            if flag in cargo_arguments
        )
        if indirect:
            failures.append(
                (
                    line,
                    f"{label} documents `cargo test -p {package}` for ignored "
                    f"tests with {' and '.join(indirect)}; state the features "
                    f"{MANIFEST} archives that suite with instead",
                )
            )
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

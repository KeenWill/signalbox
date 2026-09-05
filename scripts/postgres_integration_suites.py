#!/usr/bin/env python3
"""Read the PostgreSQL integration suite manifest, for CI and for the docs gate.

`.github/postgres-integration-suites.toml` is the single authority for what the
`postgres-integration` check compiles and executes. This module is the only
reader of it, and it serves two consumers that must never disagree:

* `.github/workflows/rust.yml` calls this file as a program. `--archive-plan`
  emits the tab-separated rows its build job archives from, and `--matrix`
  emits the JSON its run job expands into a shard matrix. The workflow
  therefore restates no package, no feature, no filter, and no shard count.
* `scripts/check_docs_consistency.py` imports it to check agreement among the
  suite manifest, workflow, documentation, and workspace packages.

A manifest both sides read turns ordinary drift into a check failure, and
`check_docs_consistency.py` gates the agreement so the manifest itself cannot
drift from either side.

Run directly with `--matrix`, `--archive-plan`, or `--check` (validate the
manifest alone and print a summary). Exits nonzero with a stable message on a
malformed manifest.

## Scope of the workflow agreement checks

`workflow_disagreements` reads `.github/workflows/rust.yml` to confirm the
workflow still derives its jobs from the manifest. **Its coverage of workflow,
shell, and Cargo spellings is best-effort by design, and completeness is not a
goal.** This is a deliberate limit, settled by owner ruling, not an oversight
or a backlog.

The reason is that the space of spellings is unbounded. A command can be
wrapped by any launcher, a value can arrive through any expression, and YAML
offers several ways to write everything; each spelling this reader does not
know is one more it could be taught, without end. Chasing that to completion
would make this module a shell and YAML interpreter — which is precisely the
coupling the manifest was introduced to remove, since the checker it replaced
failed exactly by trying to infer CI's behaviour from CI's text.

What that buys, and what it costs:

- **This detects drift, not sabotage.** It is built to catch a workflow that
  stops honouring the manifest through ordinary editing — a step restructured,
  an invocation replaced, an assertion dropped. It is not a barrier against an
  author working around it, who has unbounded options anyway: writing a literal
  matrix to `$GITHUB_OUTPUT`, invoking the reader and discarding its output, or
  spelling a command in a form written after this was.
- **Where a spelling is ambiguous, it fails closed.** A `continue-on-error`
  whose value is an expression is read as non-blocking; a documented command
  naming features by reference rather than by listing them is reported rather
  than guessed at. A false positive is a conversation; a false negative is a
  green check over tests that never ran.
- **A new spelling appearing in this repository is a fix worth making.** A new
  spelling that merely *could* exist is not. The distinction is whether the
  workflow or the documentation actually acquired it.

The manifest's own agreement with the workspace and with the documented
commands is checked independently of the workflow syntax scan.
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
RUNS_ON = re.compile(r"^[ ]*runs-on:[ ]*(?P<target>[^#\n]+?)[ ]*$", re.MULTILINE)
# Untrusted (fork or Dependabot) pull requests route to a hosted runner; the
# self-hosted arm of that expression is the target this manifest pins.
DYNAMIC_RUNS_ON = re.compile(
    r"^\$\{\{ github\.event_name == 'pull_request' && "
    r"\(github\.event\.pull_request\.head\.repo\.full_name != github\.repository "
    r"\|\| contains\(fromJSON\('\[\"dependabot\[bot\]\",\"renovate\[bot\]\"\]'\), github\.event\.pull_request\.user\.login\)\) && 'ubuntu-latest' \|\| "
    r"'(?P<pool>[^']+)' \}\}$"
)


def _resolved_runs_on(value: str) -> str:
    match = DYNAMIC_RUNS_ON.match(value)
    return match.group("pool") if match else value
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
ENVIRONMENT_ASSIGNMENT = re.compile(r"[A-Za-z_][A-Za-z0-9_]*=.*", re.DOTALL)
ENV_VALUE_OPTIONS = ("-u", "--unset", "-C", "--chdir", "-S", "--split-string")
# Cargo package specs may carry a version or a source URL; only the name is
# comparable against the manifest.
PACKAGE_SPEC = re.compile(r"(?:.*#)?(?P<name>[^@#/]+?)(?:@[^@]*)?$")
MATRIX_BINDING = re.compile(r"\$\{\{[ ]*matrix\.(?P<field>[A-Za-z_][A-Za-z0-9_]*)[ ]*\}\}")
MATRIX_FIELDS = ("suite", "partition", "partitions", "filter")
MATRIX_ENV_BINDING = re.compile(
    r"(?P<variable>[A-Za-z_][A-Za-z0-9_]*):[ ]*"
    r"\$\{\{[ ]*matrix\.(?P<field>[A-Za-z_][A-Za-z0-9_]*)[ ]*\}\}"
)
# Which matrix field each archived-run option must resolve from. A `$` alone is
# not enough: `--partition "count:1/$PARTITIONS"` expands a variable and still
# pins every shard to partition 1.
ARCHIVED_RUN_OPTIONS = {
    "--archive-file": ("suite",),
    "--partition": ("partition", "partitions"),
    "-E": ("filter",),
}
WORKSPACE_SELECTORS = ("--workspace", "--all")
AGGREGATE_JOB = "postgres-integration"
RUN_JOB = "postgres-integration-run"
BUILD_JOB = "postgres-integration-build"
BUILD_RUNNER = "signalbox-docker"
RUN_RUNNER = "signalbox-docker"
# A step or job that may fail without failing anything above it. The archived
# run carrying this would let every shard fail while the matrix job reports
# success and the aggregate's assertion passes.
CONTINUE_ON_ERROR = re.compile(
    r"^[ ]*(?:-[ ]+)?continue-on-error:[ ]*(?P<value>.*?)[ ]*$"
)
# Bash's `command [-pVv] name [args]` runs `name`; only `-v`/`-V` print instead.
COMMAND_BUILTIN_OPTIONS = ("-p",)
ENV_KEY = re.compile(r"^(?P<indent>[ ]*)(?P<dash>-[ ]+)?env:[ ]*$")
ALWAYS_CONDITION = re.compile(r"^[ ]*if:.*\balways\(\)", re.MULTILINE)
# A path value may contain spaces inside a `${{ … }}` expression, so it runs
# to the end of the line rather than to the first space.
ARTIFACT_PATH = re.compile(r"^[ ]*path:[ ]*(?P<path>.+?)[ ]*$")
NEEDS_RESULT = re.compile(
    r"(?P<variable>[A-Za-z_][A-Za-z0-9_]*):[ ]*"
    r"\$\{\{[ ]*needs\.postgres-integration-(?P<job>build|run)\.result[ ]*\}\}"
)
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
    include_binaries: tuple[str, ...]
    exclude_binaries: tuple[str, ...]

    def filterset(self) -> str:
        """Render this suite's nextest filterset expression.

        Binary predicates partition same-package test targets before
        `not test(<substring>)` reproduces libtest's `--skip <substring>`:
        nextest's `test()` predicate matches a substring of the test path by
        default, which is exactly what libtest matched. With nothing skipped
        the expression is `all()`, so the run job always passes a `-E` and
        needs no conditional.
        """
        terms: list[str] = []
        if self.include_binaries:
            included = " or ".join(
                f"binary({binary})" for binary in self.include_binaries
            )
            terms.append(f"({included})")
        terms.extend(
            f"not binary({binary})" for binary in self.exclude_binaries
        )
        terms.extend(f"not test({skipped})" for skipped in self.skip)
        return " and ".join(terms) if terms else "all()"


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
            if key
            not in {
                "name",
                "package",
                "features",
                "shards",
                "skip",
                "include_binaries",
                "exclude_binaries",
            }
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
        include_binaries = entry.get("include_binaries", [])
        exclude_binaries = entry.get("exclude_binaries", [])
        for field, binaries in (
            ("include_binaries", include_binaries),
            ("exclude_binaries", exclude_binaries),
        ):
            if not isinstance(binaries, list) or not all(
                isinstance(binary, str)
                and re.fullmatch(r"[A-Za-z0-9_-]+", binary) is not None
                for binary in binaries
            ):
                raise ManifestError(
                    f"{where} (`{name}`) needs `{field}` as a list of plain "
                    "Cargo test-target names"
                )
            if len(set(binaries)) != len(binaries):
                raise ManifestError(
                    f"{where} (`{name}`) declares a `{field}` target twice"
                )
        overlap = sorted(set(include_binaries) & set(exclude_binaries))
        if overlap:
            raise ManifestError(
                f"{where} (`{name}`) both includes and excludes "
                f"{', '.join(overlap)}"
            )
        suites.append(
            Suite(
                name=name,
                package=package,
                features=tuple(features),
                shards=shards,
                skip=tuple(skip),
                include_binaries=tuple(include_binaries),
                exclude_binaries=tuple(exclude_binaries),
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


def uploaded_artifacts(text: str) -> list[tuple[str, str | None]]:
    """Return each `actions/upload-artifact` step's artifact name and path.

    Read from the upload steps themselves, not from the file's text: an
    artifact name surviving in a comment after its upload step was deleted
    would otherwise still count as published, and the docs gate would keep
    asserting that a suite whose archive no longer exists is executed.
    """
    lines = text.splitlines()
    uploads: list[tuple[str, str | None]] = []
    for index, line in enumerate(lines):
        match = UPLOAD_ACTION.match(line)
        if match is None:
            continue
        indentation = len(match.group("indent"))
        name: str | None = None
        path: str | None = None
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
                name = named.group("suite")
            located = ARTIFACT_PATH.match(following)
            if located is not None:
                path = located.group("path")
        if name is not None:
            uploads.append((name, path))
    return uploads


def job_lines(text: str, name: str) -> list[str]:
    """Return the lines of one workflow job's block, or none if it is absent.

    The aggregate job carries the required check's name, so what it asserts has
    to be read from that job and nowhere else: the same binding and assertion
    sitting in an unrelated job says nothing about whether branch protection
    consults the shards.
    """
    lines = text.splitlines()
    opening = re.compile(rf"^(?P<indent>[ ]+){re.escape(name)}:[ ]*$")
    for index, line in enumerate(lines):
        match = opening.match(line)
        if match is None:
            continue
        indent = len(match.group("indent"))
        for end in range(index + 1, len(lines)):
            following = lines[end]
            if following.strip() and (
                len(following) - len(following.lstrip(" ")) <= indent
            ):
                return lines[index:end]
        return lines[index:]
    return []


def step_span(lines: list[str], run_index: int, indent: int) -> tuple[int, int]:
    """Return the line range of the step enclosing one `run:` scalar.

    The step is the list item containing the command, bounded by the nearest
    `- ` at the command key's own indentation on either side.
    """
    start = 0
    for index in range(run_index, -1, -1):
        line = lines[index]
        depth = len(line) - len(line.lstrip(" "))
        if line.strip() and depth <= indent and line.lstrip().startswith("- "):
            start = index
            break
    end = len(lines)
    for index in range(run_index + 1, len(lines)):
        line = lines[index]
        if not line.strip():
            continue
        depth = len(line) - len(line.lstrip(" "))
        if depth < indent or (depth == indent and line.lstrip().startswith("- ")):
            end = index
            break
    return start, end


def step_is_blocking(lines: list[str], start: int, end: int) -> bool:
    """Return whether a step's failure still fails its job.

    `continue-on-error` on the archived run would let every shard fail while
    the matrix job reported success and the aggregate's assertion passed — the
    required check green with nothing enforced. The archived run has to be
    unconditionally blocking, so only a literal `false` counts as blocking.
    """
    for line in lines[start:end]:
        match = CONTINUE_ON_ERROR.match(line)
        if match is None:
            continue
        # Only a literal `false` keeps the step blocking. `true`, an expression
        # (`${{ … }}`), and anything else are all read as non-blocking, because
        # what an expression evaluates to is not decidable here and a step that
        # might be allowed to fail cannot be credited with enforcing anything.
        if match.group("value").strip("\"'") != "false":
            return False
    return True


def step_matrix_variables(lines: list[str], run_index: int, indent: int) -> dict[str, str]:
    """Return the matrix bindings in scope for one `run:` scalar's own step.

    Collected from the step the command belongs to, never from the file: a run
    step pinning `PARTITION: "1"` while some other step still binds
    `PARTITION: ${{ matrix.partition }}` would otherwise read as parameterised
    while every shard ran the same partition.

    The step is the list item enclosing the command — bounded by the nearest
    `- ` at the command key's own indentation on either side.
    """
    start, end = step_span(lines, run_index, indent)
    return {
        match.group("field"): match.group("variable")
        for line in lines[start:end]
        for match in MATRIX_ENV_BINDING.finditer(line)
    }


def workflow_shell_commands(
    text: str,
) -> list[tuple[str, dict[str, str], bool]]:
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
    # A `run:`/`command:` key nested under `env:` is an environment value, not
    # a command the runner executes. Left in, a conforming nextest string
    # parked in one would satisfy the archived-run requirement while no step
    # ran anything.
    inert = set()
    for index, line in enumerate(lines):
        opening = ENV_KEY.match(line)
        if opening is None:
            continue
        # `- env:` puts the key two columns right of the list item, and its
        # sibling `run:` sits at the key's indent — not inside the mapping.
        indent = len(opening.group("indent")) + len(opening.group("dash") or "")
        for following in range(index + 1, len(lines)):
            entry = lines[following]
            if entry.strip() and len(entry) - len(entry.lstrip(" ")) <= indent:
                break
            inert.add(following)

    commands: list[tuple[str, dict[str, str], bool]] = []
    index = 0
    while index < len(lines):
        match = SHELL_SCALAR.match(lines[index])
        if match is None or index in inert:
            index += 1
            continue
        indentation = len(match.group("indent"))
        variables = step_matrix_variables(lines, index, indentation)
        blocking = step_is_blocking(lines, *step_span(lines, index, indentation))
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
            commands.append((joined, variables, blocking))
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
    # Each executed command travels with the matrix bindings of its own step,
    # so an archived run is judged against the variables it can actually see.
    executed = [
        (tokens, variables)
        for command, variables, _ in commands
        for tokens in simple_commands(command)
    ]

    # Scoped to the job whose steps actually consume the reader's output: the
    # same invocation sitting in an unrelated step proves nothing about whether
    # the archive plan and the shard matrix still come from the manifest.
    build_job = "\n".join(job_lines(text, BUILD_JOB))
    build_commands = [
        tokens
        for command, _, _ in workflow_shell_commands(build_job)
        for tokens in simple_commands(command)
    ]
    for mode in REQUIRED_MODES:
        if not any(invokes_reader(tokens, mode) for tokens in build_commands):
            failures.append(
                f"{WORKFLOW} job `{BUILD_JOB}` executes no `{EMITTER} {mode}` "
                f"command, so its PostgreSQL integration jobs no longer derive "
                f"from {MANIFEST}"
            )

    uploads = uploaded_artifacts(text)
    named = {
        name.removeprefix("postgres-integration-archive-") for name, _ in uploads
    }
    # An upload keeping its name while pointing at another suite's archive
    # would publish the wrong tests under the right label, and every shard
    # would pass having run the wrong suite.
    for name, path in uploads:
        suite = name.removeprefix("postgres-integration-archive-")
        basename = None if path is None else path.rsplit("/", 1)[-1]
        if basename != f"{suite}.tar.zst":
            failures.append(
                f"{WORKFLOW} uploads `{name}` from `{path}`, which is not that "
                f"suite's `{suite}.tar.zst` archive"
            )
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
    for tokens, variables in executed:
        arguments = cargo_test_arguments(tokens)
        if (
            arguments is not None
            and runs_ignored_tests(arguments)
            and not runs_file_media_isolation_tests(arguments)
        ):
            failures.append(
                f"{WORKFLOW} runs ignored tests through `cargo test` outside "
                f"{MANIFEST}: {' '.join(tokens)}"
            )
        # A nextest run selecting ignored tests without reading an archive is
        # not the manifest-driven run: it chooses its own packages. Requiring
        # only that one archive-backed run exists would let a rogue one sit
        # beside it.
        if nonconforming_ignored_nextest(tokens, variables):
            failures.append(
                f"{WORKFLOW} runs ignored tests through a `cargo nextest run` "
                f"that is not the manifest-driven archived run, outside "
                f"{MANIFEST}: {' '.join(tokens)}"
            )

    shard_job = "\n".join(job_lines(text, RUN_JOB))
    shard_commands = [
        (tokens, variables, blocking)
        for command, variables, blocking in workflow_shell_commands(shard_job)
        for tokens in simple_commands(command)
    ]
    # Blocking, because a step allowed to fail enforces nothing.
    if not any(
        runs_archived_ignored_tests(tokens, variables) and blocking
        for tokens, variables, blocking in shard_commands
    ):
        failures.append(
            f"{WORKFLOW} job `{RUN_JOB}` runs no archive-backed `cargo nextest "
            f"run` with `--run-ignored only`, so the suites {MANIFEST} declares "
            "are never executed"
        )

    # The aggregate job carries the required check's name, so it going green
    # without consulting the shards would let branch protection pass while
    # every manifest-declared test failed or never ran. Read from that job's
    # own block: the same binding and assertion elsewhere proves nothing.
    aggregate = "\n".join(job_lines(text, AGGREGATE_JOB))
    # Without `always()` the aggregate job is skipped when a dependency fails,
    # and a skipped required check reports success — branch protection green
    # with the build or every shard failed.
    if aggregate and ALWAYS_CONDITION.search(aggregate) is None:
        failures.append(
            f"{WORKFLOW} job `{AGGREGATE_JOB}` has no `if: always()`, so it is "
            "skipped when a dependency fails instead of failing the check"
        )
    asserted = {
        match.group("job"): match.group("variable")
        for match in NEEDS_RESULT.finditer(aggregate)
    }
    aggregate_commands = [
        tokens
        for command, _, _ in workflow_shell_commands(aggregate)
        for tokens in simple_commands(command)
    ]
    for job in ("build", "run"):
        variable = asserted.get(job)
        if variable is None or not any(
            asserts_success(tokens, variable) for tokens in aggregate_commands
        ):
            failures.append(
                f"{WORKFLOW} does not assert "
                f"`needs.postgres-integration-{job}.result` is success, so the "
                "aggregate check can pass without it"
            )

    # A weaker, independent statement than the per-step check below: the
    # workflow must bind every field the generated matrix supplies, whether or
    # not any one step reads it.
    bound = {match.group("field") for match in MATRIX_BINDING.finditer(text)}
    for field in MATRIX_FIELDS:
        if field not in bound:
            failures.append(
                f"{WORKFLOW} binds no `matrix.{field}`, so its shards no longer "
                f"take that value from the matrix {MANIFEST} generates"
            )

    # The build and run shards must share the dedicated Docker fleet's image and
    # absolute work-path shape: nextest archives retain paths from compilation,
    # and every shard needs an isolated Docker daemon for PostgreSQL. Check each
    # job independently so either half cannot drift to another environment.
    expected_targets = {
        BUILD_JOB: BUILD_RUNNER,
        RUN_JOB: RUN_RUNNER,
    }
    raw_selections = {}
    shards_resolve_clean = True
    for job, expected_target in expected_targets.items():
        job_text = "\n".join(job_lines(text, job))
        raw = {match.group("target") for match in RUNS_ON.finditer(job_text)}
        raw_selections[job] = raw
        targets = {_resolved_runs_on(value) for value in raw}
        if targets != {expected_target}:
            shards_resolve_clean = False
            listing = ", ".join(sorted(targets)) or "none"
            failures.append(
                f"{WORKFLOW} job `{job}` must run on `{expected_target}`, "
                f"found: {listing}"
            )
    # The arms must agree in full, not merely resolve to the same fleet: a
    # divergent hosted arm would build the archive in one environment and run
    # it in another on routed (fork or bot) pull requests. Reported only when
    # both shards resolve clean, so single-shard drift keeps one diagnostic.
    if shards_resolve_clean and raw_selections[BUILD_JOB] != raw_selections[RUN_JOB]:
        failures.append(
            f"{WORKFLOW} jobs `{BUILD_JOB}` and `{RUN_JOB}` must share one "
            "complete runner selection, found: "
            + " vs ".join(
                ", ".join(sorted(raw_selections[job])) or "none"
                for job in (BUILD_JOB, RUN_JOB)
            )
        )

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


def launched_command(tokens: list[str]) -> list[str]:
    """Strip environment prefixes and `env` wrappers from one command.

    `RUST_LOG=debug cargo test …`, `env RUST_LOG=debug cargo test …`, and
    `command cargo test …` all run Cargo; only the command word differs. Left
    unstripped they read as some other program entirely, which is silence
    rather than disagreement.

    `command -v cargo` is not one of these: `-v` and `-V` make the builtin
    print a description instead of running anything, so the prefix is only
    stripped when it still launches its argument.
    """
    index = 0
    while index < len(tokens):
        word = tokens[index]
        if ENVIRONMENT_ASSIGNMENT.fullmatch(word):
            index += 1
            continue
        if word == "command":
            following = index + 1
            while following < len(tokens) and tokens[following] in (
                *COMMAND_BUILTIN_OPTIONS,
                "--",
            ):
                following += 1
            if following < len(tokens) and not tokens[following].startswith("-"):
                index = following
                continue
            break
        if word.rsplit("/", 1)[-1] == "env":
            index += 1
            while index < len(tokens):
                argument = tokens[index]
                if argument == "--":
                    index += 1
                    break
                if argument in ENV_VALUE_OPTIONS:
                    index += 2
                    continue
                # `-i`, `--ignore-environment`, `-0` and friends take no value
                # and still run the trailing command.
                if argument.startswith("-") and argument != "-":
                    index += 1
                    continue
                if ENVIRONMENT_ASSIGNMENT.fullmatch(argument):
                    index += 1
                    continue
                break
            continue
        break
    return tokens[index:]


def cargo_subcommand_arguments(
    tokens: list[str], names: tuple[str, ...]
) -> list[str] | None:
    """Return one Cargo subcommand's arguments, or `None` for another command.

    Cargo takes global options before the subcommand — `cargo --locked test` is
    as valid as `cargo test --locked` — so the subcommand is located rather
    than assumed adjacent.
    """
    tokens = launched_command(tokens)
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


def nonconforming_ignored_nextest(
    tokens: list[str], variables: dict[str, str]
) -> bool:
    """Return whether one nextest run selects ignored tests some other way.

    Every ignored-test run has to be the manifest-driven one, not merely one of
    them: a second archived run that drops the partition would rerun a whole
    suite on every shard, and one naming its own packages would run tests the
    manifest never declared. Both would sit beside a conforming run unreported
    if only the existence of a conforming run were required.
    """
    arguments = cargo_subcommand_arguments(tokens, ("nextest",))
    if not arguments or arguments[0] != "run":
        return False
    if "--run-ignored" not in normalized_cargo_arguments(arguments[1:]):
        return False
    return not runs_archived_ignored_tests(tokens, variables)


def runs_archived_ignored_tests(
    tokens: list[str], variables: dict[str, str]
) -> bool:
    """Return whether one command runs a nextest archive's ignored tests.

    The positive half of the workflow's contract. Every other assertion here is
    negative — nothing else may run ignored tests, no suite may lack an
    artifact — and negatives alone are satisfied by a workflow that runs
    nothing at all: delete the run step and the shards pass having merely
    downloaded their archives while those tests do not execute.
    """
    arguments = cargo_subcommand_arguments(tokens, ("nextest",))
    if not arguments or arguments[0] != "run":
        return False
    arguments = normalized_cargo_arguments(arguments[1:])
    if "--archive-file" not in arguments or "--run-ignored" not in arguments:
        return False
    selection = arguments.index("--run-ignored") + 1
    if selection >= len(arguments) or arguments[selection] != "only":
        return False
    # Parameterised by the matrix, and by the right field of it. A run naming
    # one fixed archive, dropping the filterset, or pinning a partition
    # numerator would execute a different set of tests on every shard than the
    # manifest describes while still being an archive-backed ignored run.
    for option, fields in ARCHIVED_RUN_OPTIONS.items():
        value = option_value(arguments, option)
        if value is None:
            return False
        for field in fields:
            variable = variables.get(field)
            if variable is None or not references_variable(value, variable):
                return False
    return True


def asserts_success(tokens: list[str], variable: str) -> bool:
    """Return whether one command fails unless `variable` equals `success`.

    A real comparison, not a mention: `echo "$RUN_RESULT was not success"`
    names the variable and the word and exits zero regardless, which would let
    the aggregate job pass while every shard failed.
    """
    if not tokens or tokens[0] not in ("test", "["):
        return False
    words = [word for word in tokens if word != "]"]
    return any(
        references_variable(words[index], variable)
        and words[index + 1] in ("=", "==")
        and words[index + 2] == "success"
        for index in range(len(words) - 2)
    )


def references_variable(value: str, variable: str) -> bool:
    """Return whether one shell word expands the named variable."""
    return re.search(
        rf"\${{?{re.escape(variable)}}}?(?![A-Za-z0-9_])", value
    ) is not None


def option_value(arguments: list[str], option: str) -> str | None:
    """Return the value following one option, or `None` if it is absent."""
    if option not in arguments:
        return None
    index = arguments.index(option) + 1
    return arguments[index] if index < len(arguments) else None


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


def runs_file_media_isolation_tests(arguments: list[str]) -> bool:
    """Recognize the ignored isolation suite enforced outside the PostgreSQL manifest.

    This exception is intentionally exact: changing the package, feature, test
    target, or harness selection remains an unmanifested ignored-test run. Both
    this module's workflow gate and `check_docs_consistency.py` use this single
    predicate so ignored-test credit cannot disagree with workflow admission.
    """
    return arguments == [
        "--no-fail-fast",
        "-p",
        "signalbox-file-media-processor-runtime",
        "--features",
        "test-worker",
        "--test",
        "isolation",
        "--",
        "--ignored",
    ]


def documented_ignored_commands(
    text: str,
) -> list[tuple[int, list[str], str | None]]:
    """Return documented `cargo test` commands that run ignored tests.

    Each is reported with the directory a preceding `cd` in the same chain put
    it in, because `cd crates/persistence && cargo test …` selects that package
    as surely as `-p` does.

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

    found: list[tuple[int, list[str], str | None]] = []
    for number, line in logical:
        # Backticks bound an inline code span; a fenced block has none and is
        # one segment. A chain is walked in order so a `cd` reaches the command
        # that follows it — `cargo fmt && cargo test …` is two commands, and
        # `cd pkg && cargo test …` is a command with a working directory.
        for segment in line.split("`"):
            directory: str | None = None
            for tokens in simple_commands(segment):
                if tokens[0] == "cd" and len(tokens) > 1:
                    directory = tokens[1]
                    continue
                arguments = cargo_test_arguments(tokens)
                if arguments is None or not runs_ignored_tests(arguments):
                    continue
                found.append((number, arguments, directory))
    return found


def package_spec_name(spec: str) -> str | None:
    """Return the package name one Cargo `-p <SPEC>` selects.

    A spec may carry a version (`name@1.0.0`) or a source URL
    (`path+file:///…#name`). Comparing the whole spec against the manifest's
    package names makes a qualified selection read as an unknown package, and
    an unknown package is skipped rather than compared.
    """
    matched = PACKAGE_SPEC.fullmatch(spec.strip())
    return matched.group("name") if matched else None


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
    for line, arguments, directory in documented_ignored_commands(text):
        cargo_arguments = arguments[: arguments.index("--")]
        # Cargo accepts `-p` repeatedly and runs every package named. Keeping
        # only the last one let an unmanifested package trailing a manifested
        # one hide the suite that actually needed checking.
        selected: list[str] = []
        by_manifest_path: list[str] = []
        declared: set[str] = set()
        excluded: set[str] = set()
        index = 0
        while index < len(cargo_arguments):
            argument = cargo_arguments[index]
            if argument in ("-p", "--package") and index + 1 < len(cargo_arguments):
                name = package_spec_name(cargo_arguments[index + 1])
                if name:
                    selected.append(name)
                index += 2
                continue
            # A manifest path selects its own package as surely as `-p` does,
            # and `-p` wins if both appear, matching Cargo.
            if argument == "--manifest-path" and index + 1 < len(cargo_arguments):
                name = manifest_path_package(root, cargo_arguments[index + 1])
                if name:
                    by_manifest_path.append(name)
                index += 2
                continue
            if argument == "--exclude" and index + 1 < len(cargo_arguments):
                name = package_spec_name(cargo_arguments[index + 1])
                if name:
                    excluded.add(name)
                index += 2
                continue
            if argument in ("--features", "-F") and index + 1 < len(cargo_arguments):
                declared.update(
                    part
                    for part in re.split(r"[ ,]+", cargo_arguments[index + 1])
                    if part
                )
                index += 2
                continue
            index += 1
        # `--workspace` (and its `--all` alias) selects every workspace member,
        # so it selects every manifested suite's package too — and it carries
        # none of their features, which is exactly the drift worth reporting.
        workspace = [
            suite.package
            for suite in suites
            if any(flag in cargo_arguments for flag in WORKSPACE_SELECTORS)
            and suite.package not in excluded
        ]
        # A `cd` into a workspace member selects that member.
        entered = (
            manifest_path_package(root, f"{directory}/Cargo.toml")
            if directory
            else None
        )
        local = [entered] if entered else []
        for name in selected or by_manifest_path or workspace or local:
            report_documented_selection(
                failures, label, line, name, declared, cargo_arguments,
                packages, known, suites,
            )
    return failures


def report_documented_selection(
    failures: list[tuple[int, str]],
    label: str,
    line: int,
    package: str,
    declared: set[str],
    cargo_arguments: list[str],
    packages: set[str],
    known: set[tuple[str, frozenset[str]]],
    suites: tuple[Suite, ...],
) -> None:
    """Compare one selected package's documented features with the manifest."""
    if package not in packages:
        return
    # `--features pkg/feature` enables `feature` on `pkg`; for the package
    # under comparison that is the same thing its bare name means, so the
    # matching prefix is dropped. A different package's qualified feature
    # enables something on a dependency and stays distinct.
    features = {
        feature.split("/", 1)[1]
        if feature.startswith(f"{package}/")
        else feature
        for feature in declared
    }
    # `--all-features` and `--no-default-features` name a feature set by
    # reference to the package's own table rather than by listing it, so what
    # they select cannot be compared against the manifest without resolving
    # that table — and a reader cannot see which suite they mean either.
    # Documentation of a manifested suite states the manifest's features
    # explicitly, so these are reported rather than guessed at.
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
        return
    if (package, frozenset(features)) in known:
        return
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

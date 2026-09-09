#!/usr/bin/env python3
"""Read the PostgreSQL suite manifest for Bazel CI and the docs gate.

The workflow matrix, Bazel suite targets, and shard invocations consume the
manifest directly. Workflow checks inspect structured job dependencies and
routing; shell commands are not inferred.
"""

from __future__ import annotations

import argparse
import json
import re
import shlex
import subprocess
import sys
import tomllib
from dataclasses import dataclass
from pathlib import Path


ROOT = Path(__file__).resolve().parent.parent
MANIFEST = Path(".github/postgres-integration-suites.toml")
WORKFLOW = Path(".github/workflows/bazel.yml")
RUST_WORKFLOW = Path(".github/workflows/rust.yml")
SUITE_NAME = re.compile(r"^[a-z][a-z0-9-]*$")
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
COMMAND_SEPARATOR = re.compile(r"&&|\|\||[;|&\n]")
ATTACHED_SHORT_OPTIONS = ("-p", "-F", "-j")
CARGO_GLOBAL_VALUE_OPTIONS = ("--color", "--config", "--explain", "-Z", "-C")
CARGO_TEST_COMMANDS = ("test", "t")
ENVIRONMENT_ASSIGNMENT = re.compile(r"[A-Za-z_][A-Za-z0-9_]*=.*", re.DOTALL)
ENV_VALUE_OPTIONS = ("-u", "--unset", "-C", "--chdir", "-S", "--split-string")
# Cargo package specs may carry a version or a source URL; only the name is
# comparable against the manifest.
PACKAGE_SPEC = re.compile(r"(?:.*#)?(?P<name>[^@#/]+?)(?:@[^@]*)?$")
WORKSPACE_SELECTORS = ("--workspace", "--all")
# Bash's `command [-pVv] name [args]` runs `name`; only `-v`/`-V` print instead.
COMMAND_BUILTIN_OPTIONS = ("-p",)
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
    """One PostgreSQL integration suite."""

    name: str
    package: str
    features: tuple[str, ...]
    shards: int
    skip: tuple[str, ...]
    include_binaries: tuple[str, ...]
    exclude_binaries: tuple[str, ...]


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
    """Give every manifest shard its own worker and native test partition."""
    return {"include": [
        {"suite": suite.name, "shard_index": index}
        for suite in suites for index in range(suite.shards)
    ]}


def workflow_document(text: str) -> dict[str, object]:
    """Decode one GitHub Actions workflow with the maintained YAML parser."""
    import yaml

    try:
        document = yaml.safe_load(text)
    except yaml.YAMLError as error:
        raise ManifestError(f"{WORKFLOW} is not valid YAML: {error}") from error
    if not isinstance(document, dict):
        raise ManifestError(f"{WORKFLOW} is not a YAML mapping")
    return document


def simple_commands(command: str) -> list[list[str]]:
    """Tokenize documented command examples for comparison with the manifest.

    Shell operators separate examples, and `$( … )` bodies can contain them.
    A piece that does not tokenize is dropped because prose also reaches this
    documentation-only reader.
    """
    command = re.sub(r"\\\r?\n[ \t]*", " ", command)
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


def workflow_disagreements(root: Path, suites: tuple[Suite, ...]) -> list[str]:
    """Check structured workflow bindings without interpreting shell commands."""
    jobs = workflow_document((root / WORKFLOW).read_text(encoding="utf-8")).get("jobs", {})
    failures = []
    run = jobs.get("bazel-postgres", {})
    if run.get("strategy", {}).get("matrix") != "${{ fromJSON(needs.postgres-matrix.outputs.matrix) }}":
        failures.append(f"{WORKFLOW} bazel-postgres does not use the manifest matrix")
    if run.get("continue-on-error", False) is not False:
        failures.append(f"{WORKFLOW} bazel-postgres must be blocking")
    if _resolved_runs_on(run.get("runs-on", "")) != "signalbox-integration-tests":
        failures.append(f"{WORKFLOW} bazel-postgres must run on signalbox-integration-tests")
    gate = "github.event_name == 'workflow_dispatch' || inputs.postgres"
    for name in ("postgres-matrix", "bazel-postgres"):
        if jobs.get(name, {}).get("if") != gate:
            failures.append(f"{WORKFLOW} {name} does not use the PostgreSQL scope gate")
    if run.get("needs") != "postgres-matrix":
        failures.append(f"{WORKFLOW} bazel-postgres does not depend on postgres-matrix")
    rust_jobs = workflow_document((root / RUST_WORKFLOW).read_text(encoding="utf-8")).get("jobs", {})
    if rust_jobs.get("bazel", {}).get("with", {}).get("postgres") != "${{ needs.rust-change-scope.outputs.postgres == 'true' }}":
        failures.append(f"{RUST_WORKFLOW} does not pass the PostgreSQL change scope")
    if rust_jobs.get("bazel", {}).get("uses") != "./.github/workflows/bazel.yml":
        failures.append(f"{RUST_WORKFLOW} does not call the Bazel workflow")
    aggregate = rust_jobs.get("validate", {})
    if aggregate.get("if") not in ("${{ always() }}", "always()"):
        failures.append(f"{RUST_WORKFLOW} validate has no always() condition")
    if "bazel" not in aggregate.get("needs", []):
        failures.append(f"{RUST_WORKFLOW} validate does not depend on bazel")
    return failures


def run_suite(suites: tuple[Suite, ...], name: str, shard_index: int) -> int:
    """Execute one manifest partition as an argument vector, without a shell."""
    suite = next((suite for suite in suites if suite.name == name), None)
    if suite is None:
        raise ManifestError(f"unknown suite `{name}`")
    if not 0 <= shard_index < suite.shards:
        raise ManifestError(f"suite `{name}` has no shard {shard_index}")
    command = [
        "bazel", "test", "--keep_going", "--flaky_test_attempts=2", "--jobs=4",
        "--local_resources=cpu=4", "--local_resources=memory=4096",
        "--local_test_jobs=1", "--test_sharding_strategy=disabled",
        f"--test_env=SIGNALBOX_TEST_SHARD_INDEX={shard_index}",
        f"--test_env=SIGNALBOX_TEST_TOTAL_SHARDS={suite.shards}",
        "--test_env=DOCKER_HOST=unix:///var/run/docker.sock",
    ]
    command.append("//:postgres_" + suite.name.replace("-", "_"))
    return subprocess.run(command, check=False).returncode


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

    Documentation of this exact non-PostgreSQL suite is outside the manifest.
    Its package, feature, target, and harness selection must all match.
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
    same package and features CI compiles. When the manifest moves and the
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
                f"{MANIFEST} compiles that suite with instead",
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
            f"but {MANIFEST} compiles that package with "
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
        "--check",
        action="store_true",
        help="validate the manifest and print the resolved shard topology",
    )
    mode.add_argument("--run-suite", metavar="NAME", help="run one manifest suite partition")
    parser.add_argument("--shard-index", type=int, help="zero-based suite partition")
    arguments = parser.parse_args()
    if (arguments.run_suite is not None) != (arguments.shard_index is not None):
        parser.error("--run-suite and --shard-index are required together")

    try:
        suites = load_suites(ROOT)
        if arguments.run_suite is not None:
            return run_suite(suites, arguments.run_suite, arguments.shard_index)
    except ManifestError as error:
        print(f"suite manifest FAILED: {error}", file=sys.stderr)
        return 1

    if arguments.matrix:
        print(json.dumps(run_matrix(suites), separators=(",", ":"), sort_keys=True))
        return 0
    shards = sum(suite.shards for suite in suites)
    for suite in suites:
        features = ",".join(suite.features) or "(none)"
        print(
            f"{suite.name}: -p {suite.package} --features {features} "
            f"across {suite.shards} shard(s), skip {list(suite.skip)}, "
            f"include binaries {list(suite.include_binaries)}, "
            f"exclude binaries {list(suite.exclude_binaries)}"
        )
    print(f"{len(suites)} suites over {shards} shards")
    return 0


if __name__ == "__main__":
    sys.exit(main())

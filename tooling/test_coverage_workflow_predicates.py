#!/usr/bin/env python3
"""Agreement tests for the jq programs embedded in the coverage workflows.

The coverage pipeline is split across two languages that must agree about the
same documents. A Python summarizer reads an llvm-cov or xccov export and
renders Markdown; shell in `.github/workflows/coverage.yml` and
`.github/workflows/swift.yml` wraps that Markdown in a marker, hands it to jq,
and hands the result to the GitHub API. Nothing ever executed those jq programs
under test: they were checked by eye, and four separate loader/predicate
divergences reached review that way. All four are recorded below as named
fixtures, alongside a fifth this module found while being written.

What happens here, in order:

1. The jq programs are extracted from the workflow files as they are on disk.
   No copy of a program lives in this file. A test asserting against a
   transcribed copy would keep passing after the workflow changed underneath
   it, which is the failure this module exists to prevent.
2. Each extracted program runs under the real jq binary against fixture
   documents.
3. The outcome is cross-checked against the Python side of the same pipeline:
   the real summarizer renders the report, the workflow's own shell shape
   builds the comment, and the workflow's own jq program must then accept what
   that produced.

Extraction fails loudly by construction, in both directions. Every registered
program that is meant to be present must be found, and every program found must
be registered. A workflow refactor that moves a program out of reach fails this
module by name rather than silently testing an empty set, and a new predicate
added without fixtures fails it too — a harness that passes vacuously is worse
than no harness, because it also reports that the ground is covered.

No coverage run, no network call, and no GitHub API request happens here. Every
document is synthetic and built in this file.

What this reader models, and what it refuses
-------------------------------------------

Reading a filter out of a workflow means modelling a little of the shell, and a
partial shell model can always be shown wrong on some input it never meets. So
this one refuses rather than guesses: `unreadable_invocations()` fails the suite
on any invocation whose filter it cannot read, which makes an unmodelled
construct cost a loud failure instead of a silent wrong pass.

`RECORDED_INVOCATION_FORMS` records the forms these two workflows actually use —
every filter a single-quoted literal, reached either directly or through
`gh api --jq` — and `test_the_workflows_reach_jq_only_in_the_recorded_forms`
pins that inventory. A workflow reaching jq a new way fails it, so the reader is
never quietly asked to parse a form nobody checked it against.

Those two together bound what this reader has to model, which is why it does not
grow to cover shell it never meets. How review findings against this module are
triaged is a process rule and is not restated here; it lives with the pull
request that introduced this file, which owns it.
"""

from __future__ import annotations

import importlib.util
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import unittest
from dataclasses import dataclass
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent

sys.path.insert(0, str(REPO_ROOT / "tooling"))

from summarize_coverage import EXPORT_TYPE, read_file_summaries, render  # noqa: E402

COVERAGE_WORKFLOW = REPO_ROOT / ".github" / "workflows" / "coverage.yml"
SWIFT_WORKFLOW = REPO_ROOT / ".github" / "workflows" / "swift.yml"

# The native summarizer's filename carries a hyphen, so it is not importable by
# name. It is loaded by path rather than copied, for the same reason the jq
# programs are extracted rather than transcribed: this module must fail when the
# real thing changes.
NATIVE_SUMMARIZER_PATH = REPO_ROOT / "clients" / "native" / "scripts" / "summarize-coverage.py"

# The two sticky-comment markers. Both workflows comment on the same pull
# request, and each rewrites its own comment in place on every push. One marker
# selecting the other's comment would have each workflow destroy the other's
# report, so the workflows state that the two never collide; that claim is
# asserted here rather than trusted.
RUST_MARKER = "<!-- signalbox-rust-coverage -->"
NATIVE_MARKER = "<!-- signalbox-native-coverage -->"

# Comment identities. Only distinctness matters: the selector tests need to tell
# one comment from another and nothing depends on the values.
MARKER_COMMENT_ID = 11
SECOND_MARKER_COMMENT_ID = 12
FOREIGN_COMMENT_ID = 13
# Comments from people that precede the sticky one on a long-lived pull request.
EARLIER_COMMENT_ID = 14
SECOND_EARLIER_COMMENT_ID = 15

# The checkout root the fixture paths are reported relative to. Deliberately not
# this checkout: the summarizers take the root as an argument, and a synthetic
# one keeps every rendered row identical on every machine.
FIXTURE_REPO_ROOT = Path("/repo")

RUST_REPORT_TITLE = "Rust coverage (report only)"
NATIVE_REPORT_TITLE = "Native client coverage (report only)"

# How many uncovered files each renderer is asked for. Any value renders; these
# are the ones the workflows pass, so the fixtures exercise the real shape.
RUST_TOP_UNCOVERED = 25
NATIVE_TOP_UNCOVERED = 30

JQ_BINARY = shutil.which("jq")

# Whether this run is CI. A missing jq is a developer's machine and skips; the
# same absence on a runner is a broken image or PATH, and skipping there would
# leave every predicate unexecuted behind a green check — which is the shape of
# silent pass this module exists to refuse. GitHub sets both of these.
RUNNING_IN_CI = os.environ.get("GITHUB_ACTIONS") == "true" or os.environ.get("CI") == "true"

# Every failure a malformed document can raise out of either summarizer. The
# summarizers themselves catch this same set when they read a baseline, so a
# document is "read" here exactly when it survives all of them.
LOADER_REFUSALS = (ValueError, KeyError, TypeError, AttributeError, OverflowError)


def load_native_summarizer():
    """Load the native summarizer from its hyphenated path, outside test bodies."""
    specification = importlib.util.spec_from_file_location(
        "native_summarize_coverage", NATIVE_SUMMARIZER_PATH
    )
    if specification is None or specification.loader is None:
        raise RuntimeError(f"cannot load the native summarizer at {NATIVE_SUMMARIZER_PATH}")
    module = importlib.util.module_from_spec(specification)
    # Registered before execution, not after: a dataclass declared in the
    # loaded module resolves its own annotations through `sys.modules`, and a
    # module absent from there fails to define one at all.
    sys.modules[specification.name] = module
    specification.loader.exec_module(module)
    return module


NATIVE_SUMMARIZER = load_native_summarizer()


# --------------------------------------------------------------------------
# Extraction
# --------------------------------------------------------------------------

# A workflow `run:` value is either a block scalar whose body is every following
# line indented past the key, or one inline command. Both are read: a refactor
# that collapses a block into a single line must not drop a program out of the
# tested set.
RUN_BLOCK_SCALAR = re.compile(r"^(?P<indent>[ ]*)run:[ ]*[|>][-+]?[ ]*$")
RUN_INLINE = re.compile(r"^(?P<indent>[ ]*)run:[ ]+(?P<body>\S.*)$")
JOB_HEADING = re.compile(r"^[ ]{2}(?P<name>[A-Za-z_][A-Za-z0-9_-]*):[ ]*$")
STEP_NAME = re.compile(r"^[ ]*-?[ ]*name:[ ]+(?P<name>.+?)[ ]*$")

# jq is reached two ways in these workflows: the binary, and `gh api`'s `--jq`
# flag. The binary takes its filter as the first operand that is neither an
# option nor an option's value, so finding it means knowing which options
# consume following words. Taking "the next single-quoted word" instead reads
# `--arg marker '<!-- x -->' '.filter'` as the filter `<!-- x -->`.
JQ_TOKEN = re.compile(r"(?:--jq|jq)(?![-\w])")
# A jq token only starts a command where a command can start. Without this a
# filter named `filter.jq` reads as an invocation, because the dot before it is
# not a word character.
COMMAND_POSITION_PREFIXES = " \t\n(;|&"
JQ_OPTIONS_TAKING_A_NAME_AND_A_VALUE = ("--arg", "--argjson", "--slurpfile", "--rawfile")
JQ_OPTIONS_TAKING_ONE_VALUE = ("-f", "--from-file", "--indent", "-L")
JQ_OPTIONS_READING_THE_FILTER_FROM_A_FILE = ("-f", "--from-file")
# The two-word options whose second word names a file rather than a value. Only
# these are redirected at a fixture when an invocation is replayed.
JQ_OPTIONS_TAKING_A_FILE = ("--rawfile", "--slurpfile")

# Terminators that end a shell word, and the subset that ends a whole command.
WORD_TERMINATORS = " \t;|&\n()<>"
LITERAL_WORD_KINDS = ("single-quoted", "double-quoted", "bare")

# The only characters a backslash escapes inside double quotes. Before anything
# else the shell keeps the backslash as well, which matters for a filter
# carrying a regex: a scan that always dropped it would read `test(\"\d\")` as
# `test("d")` and verify a program jq is never given.
DOUBLE_QUOTE_ESCAPABLE = ("$", "`", '"', "\\")


@dataclass(frozen=True)
class ScannedInvocation:
    """One jq invocation found in a script, either read or refused.

    A refusal carries its reason. The alternative to recording one is dropping
    the invocation, which would leave a predicate the workflow really runs
    absent from the inventory while the exhaustiveness check still passed — the
    exact vacuous pass this module exists to rule out.
    """

    program: str | None
    reason: str
    excerpt: str
    through_gh_flag: bool
    arguments: tuple[tuple[str, str], ...] = ()
    filter_kind: str = ""

    @property
    def readable(self) -> bool:
        return self.program is not None


def read_shell_word(script: str, index: int) -> tuple[str, str, int]:
    """Read one shell word, returning its text, how it was written, and the next index.

    Runs outside test bodies. The "how" is what decides whether a filter can be
    tested: a single- or double-quoted literal is the program itself, while a
    word carrying a shell expansion only names something this module cannot see.
    """
    length = len(script)
    while index < length:
        if script[index] in " \t":
            index += 1
            continue
        if script[index] == "\\" and script[index + 1 : index + 2] == "\n":
            index += 2
            continue
        break

    if index >= length or script[index] in WORD_TERMINATORS:
        return "", "end-of-command", index

    parts: list[str] = []
    kinds: set[str] = set()
    while index < length and script[index] not in WORD_TERMINATORS:
        character = script[index]

        if character == "\\":
            if script[index + 1 : index + 2] == "\n":
                index += 2
                continue
            parts.append(script[index + 1 : index + 2])
            kinds.add("bare")
            index += 2
            continue

        if character == "'":
            closing = script.find("'", index + 1)
            if closing < 0:
                return "", "unterminated-quote", length
            parts.append(script[index + 1 : closing])
            kinds.add("single")
            index = closing + 1
            continue

        if character == '"':
            cursor = index + 1
            buffer: list[str] = []
            while cursor < length and script[cursor] != '"':
                if script[cursor] == "\\":
                    escaped = script[cursor + 1 : cursor + 2]
                    # Inside double quotes a backslash escapes only these; before
                    # anything else the shell keeps both characters. Dropping it
                    # unconditionally would hand jq a different program than the
                    # workflow does — `test(\"\\d\")` would arrive as `test("d")`,
                    # and the harness would verify a filter jq never sees.
                    if escaped == "\n":
                        pass
                    elif escaped in DOUBLE_QUOTE_ESCAPABLE:
                        buffer.append(escaped)
                    else:
                        buffer.append("\\" + escaped)
                    cursor += 2
                    continue
                if script[cursor] == "$":
                    kinds.add("expansion")
                buffer.append(script[cursor])
                cursor += 1
            if cursor >= length:
                return "", "unterminated-quote", length
            parts.append("".join(buffer))
            kinds.add("double")
            index = cursor + 1
            continue

        if character == "$":
            kinds.add("expansion")
            if script.startswith("$(", index):
                depth = 0
                cursor = index
                while cursor < length:
                    if script[cursor] == "(":
                        depth += 1
                    elif script[cursor] == ")":
                        depth -= 1
                        if depth == 0:
                            cursor += 1
                            break
                    cursor += 1
                parts.append(script[index:cursor])
                index = cursor
                continue

        parts.append(character)
        kinds.add("bare")
        index += 1

    text = "".join(parts)
    if "expansion" in kinds:
        return text, "shell-expansion", index
    if kinds == {"single"}:
        return text, "single-quoted", index
    if kinds == {"double"}:
        return text, "double-quoted", index
    return text, "bare", index


def read_jq_invocation(script: str, index: int, *, through_gh_flag: bool) -> ScannedInvocation:
    """Read the filter belonging to one jq invocation, outside test bodies.

    `gh api --jq` takes its filter as the very next word. The jq binary takes
    the first operand that is not an option or an option's value, so the option
    table above is walked rather than guessed at.
    """
    excerpt = " ".join(script[index : index + 70].split())
    if through_gh_flag:
        text, kind, _ = read_shell_word(script, index)
        if kind in LITERAL_WORD_KINDS:
            return ScannedInvocation(
                program=text,
                reason="",
                excerpt=excerpt,
                through_gh_flag=through_gh_flag,
                filter_kind=kind,
            )
        return ScannedInvocation(
            program=None,
            reason=f"the --jq filter is written as {kind}",
            excerpt=excerpt,
            through_gh_flag=through_gh_flag,
        )

    filter_comes_from_a_file = False
    # The options the workflow passes, kept so a fixture can replay the
    # invocation instead of choosing its own. `-n` is behaviour, not decoration:
    # without it jq reads stdin, and a payload built with `--rawfile` alone
    # produces nothing while still exiting zero.
    arguments: list[tuple[str, str]] = []
    cursor = index
    while True:
        text, kind, following = read_shell_word(script, cursor)

        if kind == "end-of-command":
            return ScannedInvocation(
                program=None,
                reason="the invocation ends before naming a filter",
                excerpt=excerpt,
                through_gh_flag=through_gh_flag,
            )
        if kind == "unterminated-quote":
            return ScannedInvocation(
                program=None,
                reason="an unterminated quote",
                excerpt=excerpt,
                through_gh_flag=through_gh_flag,
            )

        if kind == "bare" and text.startswith("-") and text != "-":
            if text in JQ_OPTIONS_READING_THE_FILTER_FROM_A_FILE:
                filter_comes_from_a_file = True
            arguments.append((text, kind))
            consumed = 0
            if text in JQ_OPTIONS_TAKING_A_NAME_AND_A_VALUE:
                consumed = 2
            elif text in JQ_OPTIONS_TAKING_ONE_VALUE:
                consumed = 1
            for _ in range(consumed):
                value, value_kind, following = read_shell_word(script, following)
                arguments.append((value, value_kind))
            cursor = following
            continue

        if filter_comes_from_a_file:
            return ScannedInvocation(
                program=None,
                reason="the filter is read from a file",
                excerpt=excerpt,
                through_gh_flag=through_gh_flag,
            )
        if kind in LITERAL_WORD_KINDS:
            return ScannedInvocation(
                program=text,
                reason="",
                excerpt=excerpt,
                through_gh_flag=through_gh_flag,
                arguments=tuple(arguments),
                filter_kind=kind,
            )
        return ScannedInvocation(
            program=None,
            reason=f"the filter is written as {kind}",
            excerpt=excerpt,
            through_gh_flag=through_gh_flag,
        )


@dataclass(frozen=True)
class RunBlock:
    """One `run:` script, with the job and step that own it."""

    workflow: str
    job: str
    step: str
    script: str


@dataclass(frozen=True)
class ExtractedProgram:
    """One jq program, with the run block it came out of."""

    workflow: str
    job: str
    step: str
    program: str
    through_gh_flag: bool
    arguments: tuple[tuple[str, str], ...]
    filter_kind: str

    def describe(self) -> str:
        excerpt = " ".join(self.program.split())[:80]
        return f"{self.workflow}/{self.job}/{self.step!r}: {excerpt!r}"


def read_run_blocks(path: Path) -> list[RunBlock]:
    """Extract every `run:` script from a workflow, outside test bodies.

    This reads text rather than parsing YAML: the repository's checkers run on a
    stock interpreter with no third-party packages, and a `run:` block is
    recoverable from indentation alone. The narrowness is deliberate, and the
    inventory tests below are what guard it — if this reader stops finding the
    programs, those tests say so by name instead of passing over nothing.
    """
    lines = path.read_text(encoding="utf-8").splitlines()
    blocks: list[RunBlock] = []
    job = "(no job)"
    step = "(unnamed step)"
    index = 0
    while index < len(lines):
        line = lines[index]

        heading = JOB_HEADING.match(line)
        if heading:
            job = heading.group("name")

        named = STEP_NAME.match(line)
        if named and line.lstrip().startswith(("- name:", "name:")):
            step = named.group("name")

        scalar = RUN_BLOCK_SCALAR.match(line)
        if scalar:
            indent = len(scalar.group("indent"))
            body: list[str] = []
            index += 1
            while index < len(lines):
                following = lines[index]
                if not following.strip():
                    body.append("")
                    index += 1
                    continue
                if len(following) - len(following.lstrip()) <= indent:
                    break
                body.append(following)
                index += 1
            filled = [entry for entry in body if entry.strip()]
            margin = min((len(e) - len(e.lstrip()) for e in filled), default=0)
            blocks.append(
                RunBlock(
                    workflow=path.name,
                    job=job,
                    step=step,
                    script="\n".join(e[margin:] if e.strip() else "" for e in body),
                )
            )
            continue

        inline = RUN_INLINE.match(line)
        if inline:
            blocks.append(
                RunBlock(workflow=path.name, job=job, step=step, script=inline.group("body"))
            )

        index += 1
    return blocks


def scan_jq_invocations(script: str) -> list[ScannedInvocation]:
    """Find every jq invocation in one shell script, outside test bodies.

    The scan tracks shell quoting rather than pattern-matching the text, and it
    has to. These scripts carry comments that mention jq, and the programs
    themselves carry comments that mention jq; a scan that ignored quoting would
    take the word inside a program's own comment for a fresh invocation and
    extract the surrounding shell as if it were a filter.

    Every invocation is returned, including the ones whose filter this module
    cannot read. Dropping those would be the worse failure: the workflow would
    still run the predicate, while the exhaustiveness check reported that every
    predicate in the file was accounted for.
    """
    invocations: list[ScannedInvocation] = []
    index = 0
    length = len(script)
    # Command substitution reopens shell quoting inside a double-quoted word,
    # which is how every `gh api` call in these workflows reaches its filter:
    # `existing="$(gh api ... --jq '...')"`. A flat quoted/unquoted flag reads
    # the opening double quote and then treats the whole command as text.
    contexts = ["shell"]
    while index < length:
        character = script[index]

        if character == "\\":
            index += 2
            continue

        if character == "$" and script.startswith("$(", index):
            contexts.append("shell")
            index += 2
            continue

        if character == ")" and len(contexts) > 1 and contexts[-1] == "shell":
            contexts.pop()
            index += 1
            continue

        if contexts[-1] == "double-quoted":
            if character == '"':
                contexts.pop()
            index += 1
            continue

        if character == '"':
            contexts.append("double-quoted")
            index += 1
            continue

        if character == "'":
            closing = script.find("'", index + 1)
            if closing < 0:
                break
            index = closing + 1
            continue

        if character == "#" and (index == 0 or script[index - 1] in " \t\n"):
            newline = script.find("\n", index)
            index = length if newline < 0 else newline
            continue

        in_command_position = index == 0 or script[index - 1] in COMMAND_POSITION_PREFIXES
        token = JQ_TOKEN.match(script, index)
        if token and in_command_position:
            invocations.append(
                read_jq_invocation(
                    script, token.end(), through_gh_flag=token.group().startswith("--")
                )
            )
            index = token.end()
            continue

        index += 1
    return invocations


def jq_programs_in(script: str) -> list[str]:
    """Every readable jq filter in one shell script, outside test bodies."""
    return [
        invocation.program
        for invocation in scan_jq_invocations(script)
        if invocation.program is not None
    ]


def extract_jq_programs(blocks: list[RunBlock]) -> list[ExtractedProgram]:
    """Extract every readable jq program from the given run blocks, outside test bodies."""
    return [
        ExtractedProgram(
            workflow=block.workflow,
            job=block.job,
            step=block.step,
            program=invocation.program,
            through_gh_flag=invocation.through_gh_flag,
            arguments=invocation.arguments,
            filter_kind=invocation.filter_kind,
        )
        for block in blocks
        for invocation in scan_jq_invocations(block.script)
        if invocation.program is not None
    ]


# The HTML comment a workflow stamps on the report it writes, and the one its
# selector later looks for. Both are read out of the workflow rather than
# trusted to match a constant here: a producer whose marker changed while its
# selector did not would keep passing a round trip built from a local copy,
# while real runs stopped finding the old comment and posted a second one
# beside it on every push.
EMITTED_MARKER = re.compile(r"<!--[^>]*-->")
SELECTED_MARKER = re.compile(r'startswith\("(?P<marker>[^"]*)"\)')

# The file each workflow writes its sticky-comment body into, and how many
# steps write one. Two, in both workflows, and the pair is the point: one step
# writes the comment for a run that produced a report, and a second writes the
# comment that retires those numbers when a run produced none. Both must carry
# the marker, and a workflow-wide check cannot tell that one of them stopped.
COMMENT_FILE = "comment.md"
EXPECTED_COMMENT_PRODUCERS = 2


def shell_emitted_markers(script: str) -> list[str]:
    """Markers the shell itself writes in one script, outside test bodies.

    Every jq filter is blanked out first. A selector carries the marker it
    searches for inside its own program text, so a scan that read the whole
    script would find the consumer's copy and call it a producer's — which
    makes the producer/consumer comparison compare a string with itself.

    Occurrences are returned rather than a set, because each one belongs to a
    different path through the step and each has to be right on its own.
    """
    shell_only = script
    for program in jq_programs_in(script):
        shell_only = shell_only.replace(program, " " * len(program))
    return EMITTED_MARKER.findall(shell_only)


def comment_producing_blocks(workflow: Path) -> list[RunBlock]:
    """Every run block that writes a sticky-comment body, outside test bodies."""
    return [block for block in read_run_blocks(workflow) if COMMENT_FILE in block.script]


def emitted_markers(workflow: Path) -> set[str]:
    """Every distinct marker the workflow's shell writes, outside test bodies."""
    return {
        marker
        for block in read_run_blocks(workflow)
        for marker in shell_emitted_markers(block.script)
    }


def producers_disagreeing_with_selector(workflow: Path, identifier: str) -> list[str]:
    """Comment producers that do not emit the marker their selector looks for.

    Runs outside test bodies, and checks each producer separately rather than
    collapsing the workflow into one set of markers. A workflow writes its
    comment down more than one path — one for a run that measured something and
    one for a run that measured nothing — and a set is satisfied as long as any
    single path still emits the marker. The path that stopped emitting it would
    post a comment its own selector can never find again, so the next run
    writes a second one instead of rewriting the first.
    """
    marker = selected_marker(identifier)
    problems: list[str] = []
    for block in comment_producing_blocks(workflow):
        emitted = shell_emitted_markers(block.script)
        if not emitted:
            problems.append(
                f"{block.workflow}/{block.job}/{block.step!r} writes {COMMENT_FILE} but emits "
                f"no marker, so the comment it writes cannot be found again"
            )
        problems.extend(
            f"{block.workflow}/{block.job}/{block.step!r} emits {found!r} but the selector "
            f"looks for {marker!r}"
            for found in emitted
            if found != marker
        )
    return problems


def emitted_marker(workflow: Path) -> str:
    """The single marker a workflow stamps on its report, outside test bodies.

    Raises when a workflow emits none or several. One workflow owns one sticky
    comment, and a second marker would mean a second comment nobody rewrites.
    """
    markers = emitted_markers(workflow)
    if len(markers) != 1:
        raise AssertionError(
            f"expected exactly one sticky-comment marker emitted by {workflow.name}, "
            f"found {sorted(markers)}"
        )
    return markers.pop()


def selected_marker(identifier: str) -> str:
    """The marker one extracted selector searches for, outside test bodies."""
    program = one_program_for(identifier)
    found = SELECTED_MARKER.search(program)
    if found is None:
        raise AssertionError(
            f"the selector {identifier!r} no longer tests a marker with startswith: {program!r}"
        )
    return found.group("marker")


def refusal_reasons(script: str) -> list[str]:
    """Why the scanner refused each unreadable invocation in a script, outside test bodies."""
    return [
        invocation.reason
        for invocation in scan_jq_invocations(script)
        if not invocation.readable
    ]


# Every way the two workflows reach jq, on the axis a shell-parsing finding
# targets: how the filter is written, and what runs it. Both workflows write
# every filter as a single-quoted literal, which is the form with no escape
# processing at all — the shell hands jq exactly the bytes between the quotes.
# This is the mechanical gate the module docstring describes: while this is the
# whole inventory, a finding about parsing some other construct is about code
# these workflows do not contain.
RECORDED_INVOCATION_FORMS = {
    ("gh api --jq", "single-quoted"),
    ("jq binary", "single-quoted"),
}


def invocation_forms() -> set[tuple[str, str]]:
    """Every way the workflows reach jq, outside test bodies."""
    return {
        ("gh api --jq" if program.through_gh_flag else "jq binary", program.filter_kind)
        for program in all_extracted_programs()
    }


GH_TOKEN = re.compile(r"gh(?![-\w])")
# The flag that makes `gh api` follow Link headers. A sticky comment older than
# the first page is invisible without it, and the workflow then posts a second
# comment beside the one it meant to rewrite.
PAGINATION_FLAG = "--paginate"
REQUEST_BODY_FLAG = "--input"
REQUEST_METHOD_FLAG = "-X"
# The two branches a publishing step takes: rewrite the existing comment, or
# create the first one. Both must send the payload; only one runs per run.
BODY_CARRYING_METHODS = ("PATCH", "POST")

# The runner's temporary directory, which a workflow expression and a shell
# variable spell differently while naming the same place. The producer reaches
# it one way and the artifact step names it the other, so both are folded onto
# one token before they are compared.
RUNNER_TEMP = "<runner-temp>"
RUNNER_TEMP_SPELLINGS = ("${{ runner.temp }}", "${RUNNER_TEMP}", "$RUNNER_TEMP")
ARTIFACT_PATH = re.compile(r"^\s+path:\s*(?P<path>\S.*)$", re.MULTILINE)
WORKFLOW_ENVIRONMENT_BINDING = re.compile(
    r"^\s+(?P<name>[A-Z][A-Z0-9_]*):\s+(?P<value>\S.*)$", re.MULTILINE
)
UNRESOLVED_NAME = re.compile(r"<(?P<name>[A-Za-z_][A-Za-z0-9_]*)>")
SHELL_ASSIGNMENT = re.compile(
    r'^\s*(?P<name>[A-Za-z_][A-Za-z0-9_]*)="(?P<value>[^"]*)"\s*$', re.MULTILINE
)
# The shell reduction that keeps only the first id the selector emitted.
FIRST_LINE_REDUCTION = re.compile(r"%%\$'\\n'\*")


def gh_api_commands(script: str) -> list[list[str]]:
    """Every `gh api` command in a script, as words, outside test bodies."""
    found: list[list[str]] = []
    index, length = 0, len(script)
    while index < length:
        character = script[index]
        if character == "\\":
            index += 2
            continue
        if character == "'":
            closing = script.find("'", index + 1)
            if closing < 0:
                break
            index = closing + 1
            continue
        if character == "#" and (index == 0 or script[index - 1] in " \t\n"):
            newline = script.find("\n", index)
            index = length if newline < 0 else newline
            continue
        in_command_position = index == 0 or script[index - 1] in COMMAND_POSITION_PREFIXES
        token = GH_TOKEN.match(script, index)
        if token and in_command_position:
            words, following = read_command_words(script, token.end())
            if words and words[0] == "api":
                found.append(words)
            index = following + 1 if following > index else index + 1
            continue
        index += 1
    return found


def gh_command_carrying(identifier: str) -> list[str]:
    """The `gh api` command that runs one extracted filter, outside test bodies."""
    program = one_program_for(identifier)
    workflow = SELECTOR_WORKFLOWS[identifier]
    for block in read_run_blocks(workflow):
        for words in gh_api_commands(block.script):
            if program in words:
                return words
    raise AssertionError(
        f"no gh api command in {workflow.name} runs the {identifier!r} filter; the "
        "surrounding call moved and the flags it carries are no longer under test"
    )


def unpaginated_selectors() -> list[str]:
    """Selectors whose gh api call would read only the first page, outside test bodies."""
    return [
        identifier
        for identifier in SELECTOR_WORKFLOWS
        if PAGINATION_FLAG not in gh_command_carrying(identifier)
    ]


def payload_handoff_problems() -> list[str]:
    """Body-carrying requests that do not read the payload the workflow wrote.

    Runs outside test bodies. The workflow writes the payload by redirecting jq
    into a file and then hands that file to `gh api --input`. Nothing connects
    the two but the name, so a redirection that lost its target, or an
    `--input` naming something else, leaves the request with no body while jq
    still exits zero and a well-formed payload still exists here.

    Every request is checked, not the set of them. There are two — the PATCH
    that rewrites an existing comment and the POST that creates the first one —
    and asking only whether some request uses the payload passes a workflow
    whose POST branch does not. That branch runs on every pull request that has
    no coverage comment yet, which is every pull request exactly once.
    """
    problems: list[str] = []
    for identifier, workflow in (
        (RUST_COMMENT_PAYLOAD, COVERAGE_WORKFLOW),
        (NATIVE_COMMENT_PAYLOAD, SWIFT_WORKFLOW),
    ):
        program = one_program_for(identifier)
        for block in read_run_blocks(workflow):
            if program not in block.script:
                continue
            written = redirect_target_after(block.script, program)
            if written is None:
                problems.append(
                    f"{workflow.name}: the payload command does not redirect its output to a "
                    "file, so nothing writes the body the API request reads"
                )
                continue
            requests = [
                words
                for words in gh_api_commands(block.script)
                if REQUEST_METHOD_FLAG in words
                and words.index(REQUEST_METHOD_FLAG) + 1 < len(words)
                and words[words.index(REQUEST_METHOD_FLAG) + 1] in BODY_CARRYING_METHODS
            ]
            methods = sorted(
                words[words.index(REQUEST_METHOD_FLAG) + 1] for words in requests
            )
            if methods != sorted(BODY_CARRYING_METHODS):
                problems.append(
                    f"{workflow.name}: expected one request per branch "
                    f"({', '.join(sorted(BODY_CARRYING_METHODS))}), found {methods}. Counting "
                    "them is not enough: two rewrites and no create leaves a pull request "
                    "unable to get its first coverage comment at all"
                )
            problems.extend(
                f"{workflow.name}: the {words[words.index(REQUEST_METHOD_FLAG) + 1]} request "
                f"does not send {written!r}, so that branch posts no body"
                for words in requests
                if REQUEST_BODY_FLAG not in words
                or words.index(REQUEST_BODY_FLAG) + 1 >= len(words)
                or words[words.index(REQUEST_BODY_FLAG) + 1] != written
            )
    return problems


def unpinned_selector_consumers() -> list[str]:
    """Publishing steps that do not reduce the selector output to one line.

    Runs outside test bodies. The selectors emit one id per matching comment,
    and the shell keeps only the first before building the PATCH endpoint. That
    reduction is the reason emitting several ids is safe, and nothing in the
    filter states it, so a step that started consuming the whole value would
    send a multiline comment id and update nothing while every filter check
    stayed green.
    """
    return [
        f"{workflow.name}: no step reduces the selector output to its first line, so a "
        "duplicate marker comment would make the request carry several ids at once"
        for workflow in SELECTOR_WORKFLOWS.values()
        if not any(
            FIRST_LINE_REDUCTION.search(block.script) for block in read_run_blocks(workflow)
        )
    ]


def report_producer_path_problems() -> list[str]:
    """Report producers writing outside the directory their workflow uploads.

    Runs outside test bodies. The publishing job never sees the producer's disk;
    it downloads an artifact, and that artifact is a directory named in the
    upload step. A producer that moves — even moving its report and its comment
    together, so the two still sit beside each other — leaves that directory,
    and the publishing job then finds no comment and reports that the run
    measured nothing.
    """
    problems: list[str] = []
    for workflow in (COVERAGE_WORKFLOW, SWIFT_WORKFLOW):
        marker = emitted_marker(workflow)
        uploaded = artifact_directories(workflow)
        for block in read_run_blocks(workflow):
            group = marker_emitting_group(block.script, marker)
            if group is None or REPORT_FILE not in group:
                continue
            written = marker_group_output(block.script, marker)
            report = report_path_in(group)
            if written is None or report is None:
                problems.append(
                    f"{workflow.name}/{block.job}: could not read where the comment or the "
                    "report is written, so nothing checks they reach the publishing job"
                )
                continue
            comment_directory = resolved_directory(workflow, block.script, written)
            report_directory = resolved_directory(workflow, block.script, report)
            if comment_directory != report_directory:
                problems.append(
                    f"{workflow.name}/{block.job}: the comment is written to {written!r} but "
                    f"the report it wraps is at {report!r}; they must travel together"
                )
            if comment_directory not in uploaded:
                problems.append(
                    f"{workflow.name}/{block.job}: the comment lands in {comment_directory!r}, "
                    f"which no artifact step carries ({sorted(uploaded)}), so the publishing "
                    "job would download no comment and post the no-report body"
                )
    return problems


def artifact_directories(workflow: Path) -> set[str]:
    """Every directory the workflow uploads or downloads, outside test bodies."""
    return {
        normalised_workflow_path(found.group("path").strip())
        for found in ARTIFACT_PATH.finditer(workflow.read_text(encoding="utf-8"))
    }


def normalised_workflow_path(text: str) -> str:
    """One path with the temporary directory spelled one way, outside test bodies.

    A workflow expression and a shell variable name the same directory, and the
    producer reaches it through the second while the artifact step names it with
    the first. Comparing them as written would report a mismatch that is not one.
    """
    for spelling in RUNNER_TEMP_SPELLINGS:
        text = text.replace(spelling, RUNNER_TEMP)
    return text.rstrip("/")


def directory_bindings(workflow: Path, script: str) -> dict[str, str]:
    """Every name the workflow binds to a path, outside test bodies.

    Both the step environment and the block's own assignments, because a
    producer reaches its directory through either: one workflow sets an
    environment variable and assigns it to a shell name, the other assigns the
    temporary directory straight through.
    """
    bindings: dict[str, str] = {}
    for found in WORKFLOW_ENVIRONMENT_BINDING.finditer(workflow.read_text(encoding="utf-8")):
        bindings[found.group("name")] = found.group("value").strip().strip('"')
    for found in SHELL_ASSIGNMENT.finditer(script):
        bindings[found.group("name")] = found.group("value")
    return bindings


def resolved_directory(workflow: Path, script: str, path: str) -> str:
    """The directory a written file lands in, with names resolved, outside test bodies."""
    bindings = directory_bindings(workflow, script)
    resolved = path
    # Two spellings reach here: a raw `$name`, and the `<name>` a reader that
    # already substituted leaves behind for a name it had no value for. Both
    # stand for the same binding, so both are resolved.
    for _ in range(len(bindings) + 1):
        expanded = SHELL_VARIABLE.sub(
            lambda found: bindings.get(found.group("name"), found.group(0)), resolved
        )
        expanded = UNRESOLVED_NAME.sub(
            lambda found: bindings.get(found.group("name"), found.group(0)), expanded
        )
        if expanded == resolved:
            break
        resolved = expanded
    return normalised_workflow_path(resolved).rsplit("/", 1)[0]


def report_path_in(group: str) -> str | None:
    """The report file a producer group splices in, outside test bodies."""
    for emission_words in shell_command_words(group):
        if emission_words and emission_words[0] == "cat":
            for word in emission_words[1:]:
                if REPORT_FILE in word:
                    return word
    return None


def shell_command_words(group: str) -> list[list[str]]:
    """Every command in a brace group, as words, outside test bodies."""
    commands: list[list[str]] = []
    index, length = 0, len(group)
    while index < length:
        while index < length and group[index] in " \t\n;":
            index += 1
        if index >= length:
            break
        if group[index] == "#":
            newline = group.find("\n", index)
            index = length if newline < 0 else newline
            continue
        words, following = read_command_words(group, index)
        if words:
            commands.append(words)
        index = following + 1 if following > index else index + 1
    return commands


def marker_group_output(script: str, marker: str) -> str | None:
    """The file a producer's brace group is redirected into, outside test bodies."""
    lines = script.splitlines()
    start = None
    for index, line in enumerate(lines):
        stripped = line.strip()
        if stripped == "{":
            start = index
        elif stripped.startswith("}") and start is not None:
            if marker in "\n".join(lines[start + 1 : index]):
                if ">" not in stripped:
                    return None
                target, kind, _ = read_shell_word(stripped, stripped.index(">") + 1)
                return None if kind == "end-of-command" else substitute_shell_variables(target)
            start = None
    return None


def redirect_target_after(script: str, program: str) -> str | None:
    """The file a command redirects into, read after its filter, outside test bodies."""
    position = script.find(program)
    if position < 0:
        return None
    tail = script[position + len(program) :]
    line = tail.split("\n", 1)[0]
    if ">" not in line:
        return None
    target, kind, _ = read_shell_word(line, line.index(">") + 1)
    if kind == "end-of-command":
        return None
    # Normalised the same way the command words are, so the file written and the
    # file read are compared as the same name rather than as two spellings of it.
    return substitute_shell_variables(target)


# The scope a job needs to write a pull-request comment. Both publishing jobs
# continue on error, so losing it fails no workflow: the comment simply stops
# updating, or is never created, and nothing says so.
COMMENT_WRITE_SCOPE = "pull-requests"
COMMENT_WRITE_LEVEL = "write"


def job_permissions(workflow: Path) -> dict[str, dict[str, str]]:
    """Each job's declared permissions, outside test bodies."""
    declared: dict[str, dict[str, str]] = {}
    job: str | None = None
    reading = False
    for line in workflow.read_text(encoding="utf-8").splitlines():
        heading = JOB_HEADING.match(line)
        if heading:
            job = heading.group("name")
            declared.setdefault(job, {})
            reading = False
            continue
        if job is None:
            continue
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        if stripped == "permissions:":
            reading = True
            continue
        if reading:
            if not line.startswith("      "):
                reading = False
                continue
            scope, _, level = stripped.partition(":")
            declared[job][scope.strip()] = level.strip()
    return declared


def jobs_missing_comment_write_permission() -> list[str]:
    """Publishing jobs that could not write the comment they build.

    Runs outside test bodies. The filter and the request can both be perfect and
    the comment still never appear, because the job was not granted the scope
    that lets it write one. Both publishing jobs continue on error, so the
    failure is silent: the sticky comment goes stale, or a first one is never
    created, and the workflow still reports success.
    """
    problems: list[str] = []
    for identifier, workflow in (
        (RUST_COMMENT_PAYLOAD, COVERAGE_WORKFLOW),
        (NATIVE_COMMENT_PAYLOAD, SWIFT_WORKFLOW),
    ):
        program = one_program_for(identifier)
        declared = job_permissions(workflow)
        for block in read_run_blocks(workflow):
            if program not in block.script:
                continue
            granted = declared.get(block.job, {})
            if granted.get(COMMENT_WRITE_SCOPE) != COMMENT_WRITE_LEVEL:
                problems.append(
                    f"{workflow.name}/{block.job} builds a comment payload but declares "
                    f"{COMMENT_WRITE_SCOPE}: {granted.get(COMMENT_WRITE_SCOPE)!r} rather than "
                    f"{COMMENT_WRITE_LEVEL!r}; both requests would be refused and the job "
                    "continues on error, so nothing would report it"
                )
    return problems


def unreadable_invocations() -> list[str]:
    """Every jq invocation in either workflow whose filter cannot be read.

    Runs outside test bodies. This is the escape hatch the exhaustiveness check
    would otherwise have: an invocation whose filter is built at run time, read
    from a file, or hidden behind an option this table does not know is a
    predicate that CI executes and this module never sees.
    """
    return [
        f"{block.workflow}/{block.job}/{block.step!r}: {invocation.reason} "
        f"({invocation.excerpt!r})"
        for block in read_run_blocks(COVERAGE_WORKFLOW) + read_run_blocks(SWIFT_WORKFLOW)
        for invocation in scan_jq_invocations(block.script)
        if not invocation.readable
    ]


def all_extracted_programs() -> list[ExtractedProgram]:
    """Every jq program in both coverage workflows, outside test bodies."""
    return extract_jq_programs(
        read_run_blocks(COVERAGE_WORKFLOW) + read_run_blocks(SWIFT_WORKFLOW)
    )


def comment_payload_blocks() -> list[RunBlock]:
    """The coverage workflow's run blocks that build a comment payload, outside test bodies."""
    return [block for block in read_run_blocks(COVERAGE_WORKFLOW) if "--rawfile" in block.script]


# --------------------------------------------------------------------------
# The registry of known programs
# --------------------------------------------------------------------------

# Every jq program in either workflow must match one specification here, and the
# check runs both ways. A specification marked PRESENT that finds no program
# means extraction broke or the program was deleted. A program matching no
# specification means a predicate was added without fixtures, which is exactly
# the state that produced every divergence recorded in this module.
#
# PENDING is for a program this repository expects but does not carry yet, and
# it is a waiting room rather than a resting place. Two rules keep it honest,
# because a state that merely records an expectation would quietly become a way
# to claim coverage without having any:
#
#   Arrival promotes. A pending specification whose program has landed fails
#   this module until it is set PRESENT. Left pending it would be permanently
#   optional — nothing requires a pending program to match — so a later rewrite
#   or deletion would retire its tests in silence instead of failing.
#
#   Registration is not coverage. A specification matches on token substrings,
#   which says a program of roughly the right shape arrived, not that it does
#   the right thing. Only `exercised` specifications, the ones with fixtures
#   that actually run, satisfy the exhaustiveness check; a program that lands
#   covered by nothing else fails as though it were unregistered.
#
# The coverage-delta work on #484 owns both workflow files and adds seven
# programs between them. All seven are registered here by role so the failure
# that greets them names each one and what it needs. The two document
# predicates arrive with the whole fixture table below already written and need
# only promotion; the five baseline-selection filters need fixtures of their
# own, which belong with the work that introduces them.
PRESENT = "present"
PENDING = "pending"


@dataclass(frozen=True)
class PredicateSpec:
    """One jq program this module knows how to account for.

    Matching is by role, not by exact text. A specification pinning a program
    verbatim would fail on every reformatting, which teaches a reader to relax
    it; matching on the structure a program reaches into survives an edit and
    still fails when the program stops being that program.
    """

    identifier: str
    workflow: str
    presence: str
    exercised: bool
    role: str
    tokens: tuple[str, ...]

    def matches(self, extracted: ExtractedProgram) -> bool:
        if extracted.workflow != self.workflow:
            return False
        return all(token in extracted.program for token in self.tokens)


RUST_DOCUMENT_PREDICATE = "rust-coverage-document-predicate"
NATIVE_DOCUMENT_PREDICATE = "native-coverage-document-predicate"
RUST_COMMENT_PAYLOAD = "rust-comment-payload"
NATIVE_COMMENT_PAYLOAD = "native-comment-payload"
RUST_STICKY_SELECTOR = "rust-sticky-selector"
NATIVE_STICKY_SELECTOR = "native-sticky-selector"

PREDICATE_SPECS: tuple[PredicateSpec, ...] = (
    PredicateSpec(
        identifier=RUST_COMMENT_PAYLOAD,
        exercised=True,
        workflow=COVERAGE_WORKFLOW.name,
        presence=PRESENT,
        role="wraps the rendered report as the GitHub comment payload",
        tokens=("body:", "$body"),
    ),
    PredicateSpec(
        identifier=RUST_STICKY_SELECTOR,
        exercised=True,
        workflow=COVERAGE_WORKFLOW.name,
        presence=PRESENT,
        role="finds this workflow's own sticky comment by its marker",
        tokens=("startswith", RUST_MARKER),
    ),
    PredicateSpec(
        identifier=NATIVE_COMMENT_PAYLOAD,
        exercised=True,
        workflow=SWIFT_WORKFLOW.name,
        presence=PRESENT,
        role="wraps the rendered native report as the GitHub comment payload",
        tokens=("body:", "$body"),
    ),
    PredicateSpec(
        identifier=NATIVE_STICKY_SELECTOR,
        exercised=True,
        workflow=SWIFT_WORKFLOW.name,
        presence=PRESENT,
        role="finds this workflow's own sticky comment by its marker",
        tokens=("startswith", NATIVE_MARKER),
    ),
    PredicateSpec(
        identifier="rust-baseline-candidate-selection",
        exercised=False,
        workflow=COVERAGE_WORKFLOW.name,
        presence=PENDING,
        role="orders the main-branch runs a baseline may be taken from",
        tokens=("workflow_runs", "@tsv"),
    ),
    PredicateSpec(
        identifier="rust-baseline-artifact-lookup",
        exercised=False,
        workflow=COVERAGE_WORKFLOW.name,
        presence=PENDING,
        role="finds the coverage artifact uploaded by one candidate run",
        tokens=(".artifacts[]",),
    ),
    PredicateSpec(
        identifier=RUST_DOCUMENT_PREDICATE,
        exercised=True,
        workflow=COVERAGE_WORKFLOW.name,
        presence=PENDING,
        role="decides whether a baseline llvm-cov export is one the summarizer can read",
        tokens=("llvm.coverage.json.export",),
    ),
    PredicateSpec(
        identifier="native-baseline-candidate-selection",
        exercised=False,
        workflow=SWIFT_WORKFLOW.name,
        presence=PENDING,
        role="orders the main-branch runs a baseline may be taken from",
        tokens=("workflow_runs", "@tsv"),
    ),
    PredicateSpec(
        identifier="native-baseline-artifact-lookup",
        exercised=False,
        workflow=SWIFT_WORKFLOW.name,
        presence=PENDING,
        role="finds the coverage artifact uploaded by one candidate run",
        tokens=(".artifacts[]",),
    ),
    PredicateSpec(
        identifier="native-test-step-conclusion",
        exercised=False,
        workflow=SWIFT_WORKFLOW.name,
        presence=PENDING,
        role="judges whether a candidate run finished the tests its measurement rests on",
        tokens=(".jobs[]", "run_attempt"),
    ),
    PredicateSpec(
        identifier=NATIVE_DOCUMENT_PREDICATE,
        exercised=True,
        workflow=SWIFT_WORKFLOW.name,
        presence=PENDING,
        role="decides whether a baseline xccov report is one the summarizer can read",
        tokens=(".targets",),
    ),
)


def programs_for(identifier: str) -> list[ExtractedProgram]:
    """Every extracted program matching one specification, outside test bodies."""
    spec = next(entry for entry in PREDICATE_SPECS if entry.identifier == identifier)
    return [program for program in all_extracted_programs() if spec.matches(program)]


def one_extracted_for(identifier: str) -> ExtractedProgram:
    """The single extracted program matching one specification, outside test bodies.

    Carries how the workflow reaches it, not only its text: a filter reached
    through `gh api --jq` is run by the GitHub CLI, and one passed to the binary
    is run by jq, and the two do not print the same things.
    """
    matches = programs_for(identifier)
    if len(matches) != 1:
        raise AssertionError(
            f"predicate not found / extraction pattern broke: expected exactly one jq "
            f"program matching {identifier!r}, found {len(matches)}. Extracted overall: "
            f"{[program.describe() for program in all_extracted_programs()]}"
        )
    return matches[0]


def one_program_for(identifier: str) -> str:
    """The single extracted program matching one specification, outside test bodies.

    Raises rather than returning a default. A caller reaching for a program that
    is not there must fail the test that called it, and say which program and
    what was found instead.
    """
    matches = programs_for(identifier)
    if len(matches) != 1:
        raise AssertionError(
            f"predicate not found / extraction pattern broke: expected exactly one jq "
            f"program matching {identifier!r}, found {len(matches)}. Extracted overall: "
            f"{[program.describe() for program in all_extracted_programs()]}"
        )
    return matches[0].program


# Which fixture set drives each specification. `exercised` is checked against
# this rather than trusted: a specification counts as covering a program only
# when cases behind it appear here, so setting the flag without writing fixtures
# fails by name instead of making the harness green over nothing. That failure
# mode is the one this module exists to refuse, and it applies to the module's
# own bookkeeping as much as to the workflows it reads.
def fixture_cases_by_specification() -> dict[str, list]:
    """The fixture cases driving each specification, outside test bodies.

    These are the same collections the tests below iterate, so a specification
    named here with cases behind it is one the suite actually drives. What this
    cannot tell on its own is whether a name has been pointed at an unrelated
    collection; it converts a bare assertion into one that fails when nothing
    backs it, which is the property being claimed and not more.
    """
    return {
        RUST_COMMENT_PAYLOAD: list(producer_bodies(COVERAGE_WORKFLOW, "").values()),
        NATIVE_COMMENT_PAYLOAD: list(producer_bodies(SWIFT_WORKFLOW, "").values()),
        RUST_STICKY_SELECTOR: list(selector_cases(RUST_STICKY_SELECTOR)),
        NATIVE_STICKY_SELECTOR: list(selector_cases(NATIVE_STICKY_SELECTOR)),
        RUST_DOCUMENT_PREDICATE: list(RUST_DOCUMENTS),
        NATIVE_DOCUMENT_PREDICATE: list(NATIVE_DOCUMENTS),
    }


def misdeclared_exercised_specs(
    specifications: tuple[PredicateSpec, ...] | None = None,
    cases: dict[str, list] | None = None,
) -> list[str]:
    """Specifications whose `exercised` flag disagrees with their fixtures.

    Runs outside test bodies, and checks both directions. A flag set without
    fixtures makes `unexercised_programs()` accept a program nothing drives —
    the silent pass this module is built to prevent, turned inward. A flag left
    unset while fixtures exist is the opposite mistake: the exhaustiveness check
    keeps demanding work that is already done.

    The registry and its fixture map are arguments so both branches can be
    driven from controlled inputs. Called against the real ones they report
    nothing, which is the answer that proves least: a helper only ever run over
    a correct registry would keep passing if either branch were deleted.
    """
    if specifications is None:
        specifications = PREDICATE_SPECS
    if cases is None:
        cases = fixture_cases_by_specification()
    problems: list[str] = []
    for spec in specifications:
        backing = cases.get(spec.identifier, [])
        if spec.exercised and not backing:
            problems.append(
                f"{spec.identifier} declares exercised=True but no fixture set drives it. "
                "Give it cases in fixture_cases_by_specification(), or set exercised=False: "
                "the exhaustiveness check counts this specification as covering a program, "
                "and a flag on its own covers nothing"
            )
        if not spec.exercised and backing:
            problems.append(
                f"{spec.identifier} has {len(backing)} fixture cases but declares "
                "exercised=False, so the exhaustiveness check still demands work already done"
            )
    return problems


def unexercised_programs() -> list[str]:
    """Every extracted program no fixture-backed specification covers, outside test bodies.

    A specification alone does not make a program tested. Matching on token
    substrings says only that somebody expected a program of roughly this
    shape; a filter that carries the right tokens and still selects the wrong
    runs would satisfy a registry that counted mere registration as coverage.
    So only a specification whose fixtures actually run counts here, and a
    program that lands with none fails as if it were unregistered.
    """
    return [
        program.describe()
        for program in all_extracted_programs()
        if not any(spec.matches(program) for spec in PREDICATE_SPECS if spec.exercised)
    ]


def misdeclared_presence() -> list[str]:
    """Specifications whose declared presence disagrees with the workflows.

    Runs outside test bodies, and closes the pending state in both directions.
    A pending specification whose program has landed must be promoted, or it
    stays permanently optional and a later deletion would silently retire its
    tests rather than fail. A present one whose program is gone means the
    program was deleted or extraction broke.
    """
    extracted = all_extracted_programs()
    misdeclared: list[str] = []
    for spec in PREDICATE_SPECS:
        found = len([program for program in extracted if spec.matches(program)])
        if spec.presence == PENDING and found:
            misdeclared.append(
                f"{spec.identifier} is declared {PENDING} but {found} matching program(s) "
                f"have landed in {spec.workflow}. Set presence={PRESENT!r} so a later "
                "deletion fails this module, and give it fixtures if it has none"
            )
        if spec.presence == PRESENT and found != 1:
            misdeclared.append(
                f"{spec.identifier} is declared {PRESENT} but matched {found} programs in "
                f"{spec.workflow}: extraction broke, or the program was removed"
            )
    return misdeclared


# --------------------------------------------------------------------------
# Fixtures
# --------------------------------------------------------------------------

HEALTHY_FILE = "/repo/crates/domain/src/session.rs"
SECOND_FILE = "/repo/crates/domain/src/turn.rs"
PRODUCT_TARGET = "Signalbox.app"
NATIVE_SOURCE_PATH = "/repo/clients/native/Sources/Signalbox/LiveScreen.swift"

# A healthy baseline: one partly covered file, nothing degenerate about it.
# Every rejection fixture is this document with one thing changed, so a failure
# names the change rather than the whole document.
HEALTHY_LINES = 100
HEALTHY_COVERED = 40

# The boundary the impossible counters sit one step past. `covered == count` is
# an ordinary fully covered file and must stay acceptable to both sides.
BOUNDARY_LINES = 7

# Counter magnitudes. The predicates bound a counter at 2^53-1, the largest
# integer below the point where a double stops representing every integer, so
# that is the largest they accept and one more is the smallest they refuse.
# 2^53+1 is a separate number and a separate fact: it is the first integer a
# double genuinely cannot hold, which is what the arithmetic below demonstrates.
LARGEST_COUNTER_A_PREDICATE_ACCEPTS = 2**53 - 1
SMALLEST_COUNTER_A_PREDICATE_REFUSES = 2**53
FIRST_INTEGER_A_DOUBLE_CANNOT_HOLD = 2**53 + 1
FOUR_HUNDRED_DIGITS = int("9" * 400)
BEYOND_DOUBLE_RANGE = "1e999"

# A counter claiming more covered lines than exist.
IMPOSSIBLE_COUNT = 1
IMPOSSIBLE_COVERED = 2

# A target name that is not a string.
NON_STRING_TARGET_NAME = 42

# Every real xccov report carries the test bundles Xcode measured alongside the
# product targets. The summarizer accepts them and excludes them from the
# product total by design, so a predicate that refused a document containing one
# would discard the ordinary baselines the summarizer reads.
TEST_BUNDLE_TARGET = "SignalboxTests.xctest"


def coverage_file(filename: str, *, lines: int, covered: int) -> dict:
    """One export file entry whose single knob is its line coverage.

    Function and region counters are derived from that same knob rather than
    taken as free integers, matching the summarizer's own regression tests: no
    test here depends on them varying independently, and deriving them keeps a
    reader from reconciling three unrelated numbers to understand one row.
    """
    return {
        "filename": filename,
        "summary": {
            "lines": {"count": lines, "covered": covered},
            "functions": {"count": lines, "covered": covered},
            "regions": {"count": lines, "covered": covered},
        },
    }


def export(*files: dict) -> dict:
    """One llvm-cov export document wrapping the given file entries."""
    return {"type": EXPORT_TYPE, "version": "2.0.1", "data": [{"files": list(files)}]}


def line_counter_export(count) -> dict:
    """One export whose single file carries the given line count, nothing covered.

    Written without the shared file helper because the counter under test is the
    whole point: deriving function and region counters from it would put three
    copies of an extreme value into one document and blur which one a predicate
    rejected.
    """
    return {
        "type": EXPORT_TYPE,
        "version": "2.0.1",
        "data": [
            {
                "files": [
                    {
                        "filename": HEALTHY_FILE,
                        "summary": {"lines": {"count": count, "covered": 0}},
                    }
                ]
            }
        ],
    }


def healthy_export() -> dict:
    """The export every rejection fixture is a one-change edit of."""
    return export(coverage_file(HEALTHY_FILE, lines=HEALTHY_LINES, covered=HEALTHY_COVERED))


def empty_first_element_export() -> dict:
    """An export whose first data element carries no `files` key at all.

    cargo-llvm-cov merges everything it instruments into one element, but the
    format permits more and an element may be empty. The summarizer reads
    `element.get("files", [])` and passes over it.
    """
    return {
        "type": EXPORT_TYPE,
        "version": "2.0.1",
        "data": [
            {},
            {"files": [coverage_file(HEALTHY_FILE, lines=HEALTHY_LINES, covered=HEALTHY_COVERED)]},
        ],
    }


def native_source(path: str, *, covered: int, executable: int) -> dict:
    """One source file inside an xccov target."""
    return {"path": path, "coveredLines": covered, "executableLines": executable}


def native_target(name, *, covered: int, executable: int, files: tuple[dict, ...] = ()) -> dict:
    """One xccov target whose knobs are its covered and executable line counts."""
    return {
        "name": name,
        "coveredLines": covered,
        "executableLines": executable,
        "lineCoverage": 0.0 if executable == 0 else covered / executable,
        "files": list(files),
    }


def native_report(*targets: dict) -> dict:
    """One xccov report document wrapping the given targets."""
    return {"targets": list(targets)}


def rust_renders(document: dict) -> bool:
    """Whether the Rust summarizer produces a report from a document, outside test bodies.

    This is the operational reading of "the loader reads it": the pipeline's
    Python half either turns the document into a report or refuses it. The jq
    programs stand in front of exactly that, so it is the thing they must agree
    with.
    """
    try:
        render(document, FIXTURE_REPO_ROOT, RUST_TOP_UNCOVERED, RUST_REPORT_TITLE)
    except LOADER_REFUSALS:
        return False
    return True


def native_renders(document: dict) -> bool:
    """Whether the native summarizer produces a report from a document, outside test bodies."""
    try:
        NATIVE_SUMMARIZER.render(
            document, FIXTURE_REPO_ROOT, NATIVE_TOP_UNCOVERED, NATIVE_REPORT_TITLE
        )
    except LOADER_REFUSALS:
        return False
    return True


def rust_comment_body() -> str:
    """Rebuild `comment.md` the way the coverage workflow's own shell does.

    The marker comes out of the workflow rather than from the constant above,
    so a producer that changed it without changing its selector fails the round
    trip instead of passing one built from a local copy of the old value.
    """
    report = render(healthy_export(), FIXTURE_REPO_ROOT, RUST_TOP_UNCOVERED, RUST_REPORT_TITLE)
    return f"{emitted_marker(COVERAGE_WORKFLOW)}\n\n{report}"


def native_comment_body() -> str:
    """Rebuild `comment.md` the way the swift workflow's own shell does."""
    report = NATIVE_SUMMARIZER.render(
        native_report(native_target(PRODUCT_TARGET, covered=40, executable=100)),
        FIXTURE_REPO_ROOT,
        NATIVE_TOP_UNCOVERED,
        NATIVE_REPORT_TITLE,
    )
    return f"{emitted_marker(SWIFT_WORKFLOW)}\n\n{report}"


# The values the workflows interpolate into the comments they write. Only these
# are invented here; every word around them is read out of the workflow, because
# the wording is producer-specific and inventing a plausible stand-in tests
# nothing. One workflow says it uploads the HTML report and LCOV, the other the
# xccov reports, so a payload filter that rewrote only producer-specific text
# would round-trip a shared invented footer perfectly.
SYNTHETIC_SHELL_VALUES = {
    "MEASURED_SHA": "0123456789abcdef0123456789abcdef01234567",
    "HEAD_SHA": "fedcba9876543210fedcba9876543210fedcba98",
    "GITHUB_RUN_ID": "1234567890",
    "GITHUB_SERVER_URL": "https://github.example",
    "GITHUB_REPOSITORY": "owner/repository",
}

SHELL_VARIABLE = re.compile(r"\$\{?(?P<name>\w+)\}?")
PRINTF_TOKEN = re.compile(r"printf(?![-\w])")
CAT_TOKEN = re.compile(r"cat(?![-\w])")
REPORT_FILE = "report.md"

REPORT_BODY = "report"
NO_REPORT_BODY = "no-report"
UNREPRODUCIBLE = "unreproducible"

# Words that appear at the head of a command inside a producer group without
# writing any of the comment. Anything else that does is an emitter this
# composer does not know, and is refused rather than skipped.
NON_EMITTING_SHELL_WORDS = frozenset({"if", "then", "else", "elif", "fi", "[", "test", ":", "true"})


@dataclass(frozen=True)
class Emission:
    """One thing a producer writes into the comment: literal text, or the report."""

    kind: str
    text: str


def substitute_shell_variables(text: str) -> str:
    """Replace shell variables with synthetic values, outside test bodies."""
    return SHELL_VARIABLE.sub(
        lambda found: SYNTHETIC_SHELL_VALUES.get(
            found.group("name"), f"<{found.group('name')}>"
        ),
        text,
    )


def read_command_words(script: str, index: int) -> tuple[list[str], int]:
    """Read one command's words, with variables substituted, outside test bodies."""
    words: list[str] = []
    while True:
        text, kind, following = read_shell_word(script, index)
        if kind == "end-of-command":
            return words, index
        if kind == "unterminated-quote":
            return words, following
        words.append(substitute_shell_variables(text) if kind == "shell-expansion" else text)
        index = following


def render_printf(words: list[str]) -> str:
    """Render one printf call, outside test bodies.

    Only `%s` is interpreted, which is all these workflows use, and `\\n` and
    `\\t` are decoded because a single-quoted format reaches printf with them
    still literal.
    """
    if not words:
        return ""
    template = words[0].replace("\\n", "\n").replace("\\t", "\t")
    arguments = list(words[1:])
    return re.sub("%s", lambda _: arguments.pop(0) if arguments else "", template)


def marker_emitting_group(script: str, marker: str) -> str | None:
    """The brace group that writes the comment body, outside test bodies.

    A producing step does more than write the comment — the coverage one also
    builds a preamble and echoes the report into the job summary — so the body
    is the brace group carrying the marker, not the whole step. Taking the step
    would splice the report in twice.
    """
    lines = script.splitlines()
    start = None
    for index, line in enumerate(lines):
        stripped = line.strip()
        if stripped == "{":
            start = index
        elif stripped.startswith("}") and start is not None:
            group = "\n".join(lines[start + 1 : index])
            if marker in group:
                return group
            start = None
    return None


def shell_emissions(group: str) -> list[Emission]:
    """Everything a producer's brace group writes, in order, outside test bodies.

    Every command is read, not only the ones this composer knows. A command it
    cannot reproduce is recorded as such rather than stepped over: skipping it
    would compose a body shorter than the one the workflow writes, and every
    round trip built on that body would then pass while the payload corrupted
    text no fixture contained. Silently testing less is the failure this module
    exists to refuse, so an unknown emitter is surfaced and fails a test.
    """
    emissions: list[Emission] = []
    index, length = 0, len(group)
    while index < length:
        while index < length and group[index] in " \t\n;":
            index += 1
        if index >= length:
            break
        if group[index] == "#":
            newline = group.find("\n", index)
            index = length if newline < 0 else newline
            continue

        words, following = read_command_words(group, index)
        if not words:
            index += 1
            continue

        command = words[0]
        if command == "printf":
            emissions.append(Emission(kind="text", text=render_printf(words[1:])))
        elif command == "cat" and any(REPORT_FILE in word for word in words[1:]):
            emissions.append(Emission(kind=REPORT_BODY, text=""))
        elif command not in NON_EMITTING_SHELL_WORDS:
            emissions.append(Emission(kind=UNREPRODUCIBLE, text=" ".join(words)))

        index = following + 1 if following > index else index + 1
    return emissions


def unreproducible_producer_commands() -> list[str]:
    """Commands in a producer this composer cannot reproduce, outside test bodies.

    A producer that emitted part of its comment some other way — an `echo`
    where a `printf` used to be — would leave the composed body short while the
    producer count and the report classification stayed the same, so every
    whole-comment round trip would keep passing over text no fixture holds.
    """
    found: list[str] = []
    for workflow in (COVERAGE_WORKFLOW, SWIFT_WORKFLOW):
        marker = emitted_marker(workflow)
        for block in comment_producing_blocks(workflow):
            group = marker_emitting_group(block.script, marker)
            if group is None:
                continue
            found.extend(
                f"{workflow.name}/{block.job}: {emission.text!r} writes part of the comment "
                "in a form this module cannot reproduce, so the composed body would be short"
                for emission in shell_emissions(group)
                if emission.kind == UNREPRODUCIBLE
            )
    return found


def producer_bodies(workflow: Path, report: str) -> dict[str, str]:
    """The comment each producer writes, composed from the workflow's own text.

    Runs outside test bodies. The literal words come from the workflow rather
    than from this file: they are producer-specific, they belong to files this
    branch does not edit, and a stand-in written here would be a phrase neither
    workflow ever prints — which is exactly what a payload filter rewriting
    producer text would slip past.

    The conditional provenance clause is composed as though the head SHA is
    set, which is the pull-request case these comments are written for.
    """
    marker = emitted_marker(workflow)
    bodies: dict[str, str] = {}
    for block in comment_producing_blocks(workflow):
        group = marker_emitting_group(block.script, marker)
        if group is None:
            continue
        emissions = shell_emissions(group)
        if not emissions:
            continue
        composed = "".join(
            report if emission.kind == REPORT_BODY else emission.text for emission in emissions
        )
        carries_report = any(emission.kind == REPORT_BODY for emission in emissions)
        bodies[REPORT_BODY if carries_report else NO_REPORT_BODY] = composed
    return bodies


def produced_body(workflow: Path, which: str, report: str = "") -> str:
    """One composed producer body, outside test bodies.

    Raises rather than defaulting: a body this module cannot compose is a
    producer it is no longer reading, and every round trip built on it would
    quietly test a shorter string than the workflow writes.
    """
    bodies = producer_bodies(workflow, report)
    if which not in bodies:
        raise AssertionError(
            f"could not compose the {which!r} comment body from {workflow.name}: "
            f"found {sorted(bodies)}. The producer's shape changed, and every round trip "
            "built on it would otherwise test text the workflow never writes."
        )
    return bodies[which]


def provenance_text(workflow: Path) -> str:
    """What a producer writes around the report, marker and report removed.

    Runs outside test bodies. Composing with an empty report and dropping the
    marker leaves exactly the producer-specific prose, which is the part a
    stand-in written in this file would have replaced with a phrase neither
    workflow prints.
    """
    return produced_body(workflow, REPORT_BODY, "").replace(emitted_marker(workflow), "")


def rust_full_comment_body() -> str:
    """The whole comment the coverage workflow writes, in the workflow's own words."""
    return produced_body(
        COVERAGE_WORKFLOW,
        REPORT_BODY,
        render(healthy_export(), FIXTURE_REPO_ROOT, RUST_TOP_UNCOVERED, RUST_REPORT_TITLE),
    )


def native_full_comment_body() -> str:
    """The whole comment the swift workflow writes, in the workflow's own words."""
    return produced_body(
        SWIFT_WORKFLOW,
        REPORT_BODY,
        NATIVE_SUMMARIZER.render(
            native_report(
                native_target(
                    PRODUCT_TARGET,
                    covered=40,
                    executable=100,
                    # An uncovered source file, so the rendered report carries a
                    # file-table row and a `.swift` path. A payload filter that
                    # touched only paths would round-trip a file-free report
                    # untouched and corrupt every real comment.
                    files=(native_source(NATIVE_SOURCE_PATH, covered=10, executable=60),),
                )
            ),
            FIXTURE_REPO_ROOT,
            NATIVE_TOP_UNCOVERED,
            NATIVE_REPORT_TITLE,
        ),
    )


def no_report_comment_body(workflow: Path) -> str:
    """The comment a publish step writes when its run measured nothing.

    Composed from the workflow, and carrying that workflow's marker, because
    replacing the previous run's numbers under one marker is the whole point of
    this body: one that lost the marker would post beside the stale comment
    rather than over it.
    """
    return produced_body(workflow, NO_REPORT_BODY)


def comments_page(*comments: dict) -> str:
    """One page of the GitHub issue-comments API, as `gh api` returns it."""
    return json.dumps(list(comments))


# --------------------------------------------------------------------------
# The recorded documents
# --------------------------------------------------------------------------

# Whether a divergence is acceptable is a judgement, so each one is written
# down with its reason rather than inferred from a mismatch. An empty reason
# means the two halves must agree; a stated one means they deliberately differ,
# and the statement is what a reviewer checks.
AGREEMENT_REQUIRED = ""


@dataclass(frozen=True)
class RecordedDocument:
    """One document both halves of the pipeline have an opinion about."""

    name: str
    document: dict
    loader_reads: bool
    predicate_accepts: bool
    divergence: str

    @property
    def diverges(self) -> bool:
        return self.loader_reads != self.predicate_accepts


# The predicate column records what the coverage-delta predicates on #484 do
# with each document, measured against that branch rather than guessed. Until
# those predicates land, the column is what this module expects of the first
# one to arrive; a mismatch then names the document rather than the whole
# workflow.
STRICTER_BY_DESIGN = (
    "the predicate gates a stored baseline artifact and may discard a usable one; "
    "it deliberately refuses documents the render path still accepts. Owned by #484"
)

RUST_DOCUMENTS: tuple[RecordedDocument, ...] = (
    RecordedDocument(
        name="healthy export",
        document=healthy_export(),
        loader_reads=True,
        predicate_accepts=True,
        divergence=AGREEMENT_REQUIRED,
    ),
    RecordedDocument(
        name="empty first data element",
        document=empty_first_element_export(),
        loader_reads=True,
        predicate_accepts=True,
        divergence=AGREEMENT_REQUIRED,
    ),
    RecordedDocument(
        name="counter one past the predicate bound",
        document=line_counter_export(SMALLEST_COUNTER_A_PREDICATE_REFUSES),
        loader_reads=True,
        predicate_accepts=False,
        divergence=(
            "Python integers stay exact where jq's arithmetic does not, so the "
            f"predicate stops at {LARGEST_COUNTER_A_PREDICATE_ACCEPTS} rather than compare numbers it "
            f"cannot hold. {STRICTER_BY_DESIGN}"
        ),
    ),
    RecordedDocument(
        name="counter at the predicate bound",
        document=line_counter_export(LARGEST_COUNTER_A_PREDICATE_ACCEPTS),
        loader_reads=True,
        predicate_accepts=True,
        divergence=AGREEMENT_REQUIRED,
    ),
    RecordedDocument(
        name="four hundred digit counter",
        document=line_counter_export(FOUR_HUNDRED_DIGITS),
        loader_reads=False,
        predicate_accepts=False,
        divergence=AGREEMENT_REQUIRED,
    ),
    RecordedDocument(
        name="counter beyond the double range",
        document=json.loads(
            '{"type": "%s", "data": [{"files": [{"filename": "%s", "summary": '
            '{"lines": {"count": %s, "covered": 0}}}]}]}'
            % (EXPORT_TYPE, HEALTHY_FILE, BEYOND_DOUBLE_RANGE)
        ),
        loader_reads=False,
        predicate_accepts=False,
        divergence=AGREEMENT_REQUIRED,
    ),
    RecordedDocument(
        name="impossible counters",
        document=export(
            coverage_file(HEALTHY_FILE, lines=IMPOSSIBLE_COUNT, covered=IMPOSSIBLE_COVERED)
        ),
        loader_reads=True,
        predicate_accepts=False,
        divergence=(
            "the summarizer's baseline path refuses impossible counters, but its render "
            f"path still turns them into a percentage above 100. {STRICTER_BY_DESIGN}"
        ),
    ),
    RecordedDocument(
        name="fully covered boundary",
        document=export(
            coverage_file(HEALTHY_FILE, lines=BOUNDARY_LINES, covered=BOUNDARY_LINES)
        ),
        loader_reads=True,
        predicate_accepts=True,
        divergence=AGREEMENT_REQUIRED,
    ),
    RecordedDocument(
        name="one impossible file inside a sane total",
        document=export(
            coverage_file(HEALTHY_FILE, lines=IMPOSSIBLE_COUNT, covered=IMPOSSIBLE_COVERED),
            coverage_file(SECOND_FILE, lines=HEALTHY_LINES, covered=0),
        ),
        loader_reads=True,
        predicate_accepts=False,
        divergence=(
            "the predicate applies its impossibility guard per file while the summarizer "
            "applies it to the total, so a document whose totals are sane is refused for "
            f"one bad file. Found by this module, not by review. {STRICTER_BY_DESIGN}"
        ),
    ),
    RecordedDocument(
        name="negative line total",
        document=export(coverage_file(HEALTHY_FILE, lines=-HEALTHY_LINES, covered=0)),
        loader_reads=True,
        predicate_accepts=False,
        divergence=(
            "the summarizer refuses only a zero line total, so a negative one reaches the "
            f"report while the predicate refuses it. {STRICTER_BY_DESIGN}"
        ),
    ),
)

NATIVE_DOCUMENTS: tuple[RecordedDocument, ...] = (
    RecordedDocument(
        name="healthy report",
        document=native_report(native_target(PRODUCT_TARGET, covered=40, executable=100)),
        loader_reads=True,
        predicate_accepts=True,
        divergence=AGREEMENT_REQUIRED,
    ),
    RecordedDocument(
        name="non-string target name",
        document=native_report(
            native_target(NON_STRING_TARGET_NAME, covered=40, executable=100)
        ),
        loader_reads=True,
        predicate_accepts=True,
        divergence=AGREEMENT_REQUIRED,
    ),
    RecordedDocument(
        name="counter one past the predicate bound",
        document=native_report(
            native_target(PRODUCT_TARGET, covered=0, executable=SMALLEST_COUNTER_A_PREDICATE_REFUSES)
        ),
        loader_reads=True,
        predicate_accepts=False,
        divergence=(
            "the same bound the Rust predicate carries, for the same reason: jq cannot "
            f"compare integers this large. {STRICTER_BY_DESIGN}"
        ),
    ),
    RecordedDocument(
        name="counter at the predicate bound",
        document=native_report(
            native_target(PRODUCT_TARGET, covered=0, executable=LARGEST_COUNTER_A_PREDICATE_ACCEPTS)
        ),
        loader_reads=True,
        predicate_accepts=True,
        divergence=AGREEMENT_REQUIRED,
    ),
    RecordedDocument(
        name="impossible counters",
        document=native_report(
            native_target(PRODUCT_TARGET, covered=IMPOSSIBLE_COVERED, executable=IMPOSSIBLE_COUNT)
        ),
        loader_reads=True,
        predicate_accepts=False,
        divergence=(
            "the summarizer's baseline path refuses more covered lines than exist, but its "
            f"render path still reports them. {STRICTER_BY_DESIGN}"
        ),
    ),
    RecordedDocument(
        name="fully covered boundary",
        document=native_report(
            native_target(PRODUCT_TARGET, covered=BOUNDARY_LINES, executable=BOUNDARY_LINES)
        ),
        loader_reads=True,
        predicate_accepts=True,
        divergence=AGREEMENT_REQUIRED,
    ),
    RecordedDocument(
        name="a target carrying source files",
        document=native_report(
            native_target(
                PRODUCT_TARGET,
                covered=40,
                executable=100,
                files=(native_source(NATIVE_SOURCE_PATH, covered=10, executable=60),),
            )
        ),
        loader_reads=True,
        predicate_accepts=True,
        divergence=AGREEMENT_REQUIRED,
    ),
    RecordedDocument(
        name="a product target beside a test bundle",
        document=native_report(
            native_target(PRODUCT_TARGET, covered=40, executable=100),
            native_target(TEST_BUNDLE_TARGET, covered=90, executable=100),
        ),
        loader_reads=True,
        predicate_accepts=True,
        divergence=AGREEMENT_REQUIRED,
    ),
    RecordedDocument(
        name="one impossible target inside a sane total",
        document=native_report(
            native_target("A.app", covered=IMPOSSIBLE_COVERED, executable=IMPOSSIBLE_COUNT),
            native_target("B.app", covered=0, executable=HEALTHY_LINES),
        ),
        loader_reads=True,
        predicate_accepts=False,
        divergence=(
            "the predicate guards each target while the summarizer sums first, so a report "
            "whose totals are sane is refused for one bad target. Found by this module, not "
            f"by review. {STRICTER_BY_DESIGN}"
        ),
    ),
)


def rust_loader_disagreements() -> list[str]:
    """Recorded Rust documents whose loader column is wrong, outside test bodies."""
    return [
        f"{recorded.name}: recorded loader_reads={recorded.loader_reads}, "
        f"observed {rust_renders(recorded.document)}"
        for recorded in RUST_DOCUMENTS
        if rust_renders(recorded.document) != recorded.loader_reads
    ]


def native_loader_disagreements() -> list[str]:
    """Recorded native documents whose loader column is wrong, outside test bodies."""
    return [
        f"{recorded.name}: recorded loader_reads={recorded.loader_reads}, "
        f"observed {native_renders(recorded.document)}"
        for recorded in NATIVE_DOCUMENTS
        if native_renders(recorded.document) != recorded.loader_reads
    ]


def undeclared_divergences(documents: tuple[RecordedDocument, ...]) -> list[str]:
    """Recorded documents that diverge without stating why, outside test bodies."""
    return [
        recorded.name
        for recorded in documents
        if recorded.diverges and not recorded.divergence.strip()
    ]


def pointless_divergence_notes(documents: tuple[RecordedDocument, ...]) -> list[str]:
    """Recorded documents stating a divergence they do not have, outside test bodies."""
    return [
        recorded.name
        for recorded in documents
        if not recorded.diverges and recorded.divergence.strip()
    ]


# --------------------------------------------------------------------------
# Running jq
# --------------------------------------------------------------------------

# jq's documented exit statuses under `-e`, which is how the workflows run a
# predicate. Zero means it produced a value; 1 means the last output was false
# or null; 4 means it produced no output. Those three are decisions. Every other
# status is jq falling over rather than deciding — 5 for a runtime error, and
# whatever a filter chooses for itself through `halt_error`, which is not 5.
JQ_PRODUCED_A_VALUE = 0
JQ_LAST_OUTPUT_WAS_FALSE = 1
JQ_PRODUCED_NO_OUTPUT = 4
JQ_DECISION_STATUSES = (JQ_PRODUCED_A_VALUE, JQ_LAST_OUTPUT_WAS_FALSE, JQ_PRODUCED_NO_OUTPUT)

# One jq program over one synthetic document is sub-second work. A program that
# runs past this has not decided about its input, it has hung, and the harness
# names it rather than letting the whole job run out its own limit and report a
# timeout that points at nothing.
JQ_TIMEOUT_SECONDS = 30

# jq 1.7 stopped converting untouched number literals to doubles on the way
# through. The recorded counter divergences rest on that: under jq 1.6 an
# untouched 2^53+1 comes back already rounded, and the test that distinguishes
# reading a counter from computing with one would pass for the wrong reason.
MINIMUM_JQ_VERSION = (1, 7)
JQ_VERSION = re.compile(r"jq-(?P<major>\d+)\.(?P<minor>\d+)")


@dataclass(frozen=True)
class JqResult:
    """What one jq invocation produced."""

    status: int
    stdout: str
    stderr: str

    @property
    def accepted(self) -> bool:
        return self.status == 0 and self.stdout.strip() != ""

    @property
    def errored(self) -> bool:
        """Whether jq fell over rather than deciding about its input.

        Any status outside the documented three is a failure, not a rejection.
        A predicate reaching `halt_error(7)` exits 7, and reading that as an
        ordinary rejection would hide a filter that aborts the publishing step
        for exactly the documents it is supposed to refuse quietly.
        """
        return self.status not in JQ_DECISION_STATUSES


def run_jq(program: str, document: str, *arguments: str) -> JqResult:
    """Run one extracted jq program over a document, outside test bodies."""
    completed = subprocess.run(
        [JQ_BINARY, *arguments, program],
        input=document,
        capture_output=True,
        text=True,
        check=False,
        timeout=JQ_TIMEOUT_SECONDS,
    )
    return JqResult(
        status=completed.returncode, stdout=completed.stdout, stderr=completed.stderr
    )


def run_extracted(identifier: str, document: str) -> JqResult:
    """Run one extracted filter the way its own workflow runs it, outside test bodies.

    A filter reached through `gh api --jq` is executed by the GitHub CLI's own
    jq implementation, not by the jq binary, and the CLI prints a string result
    unquoted where the binary quotes it unless asked not to. Running such a
    filter here with `-r` matches that, so a filter whose output is a string is
    compared against what the workflow would actually see.

    What this cannot reproduce is the implementation itself: the CLI exposes its
    formatter only as a modifier on a live API request, so reaching it would
    mean a network call, which this module does not make. The remaining
    exposure is bounded rather than assumed — see the test asserting these
    filters emit only integers, which is a shape both implementations agree on.
    """
    extracted = one_extracted_for(identifier)
    raw_output = ("-r",) if extracted.through_gh_flag else ()
    return run_jq(extracted.program, document, *raw_output)


# Which workflow each selector belongs to. Every selector check below is driven
# from this rather than written per selector: the two are separate programs that
# drift apart, and "covered for one, not the other" has been the shape of three
# separate defects in this file. Driving both from one table means a case cannot
# exist for one selector and be missing for the other.
SELECTOR_WORKFLOWS = {
    RUST_STICKY_SELECTOR: COVERAGE_WORKFLOW,
    NATIVE_STICKY_SELECTOR: SWIFT_WORKFLOW,
}


@dataclass(frozen=True)
class SelectorCase:
    """One comment page a selector must handle, and the ids it must emit."""

    name: str
    page: str
    expected: tuple[str, ...]


def selector_cases(identifier: str) -> list[SelectorCase]:
    """Every page one selector can meet on a pull request, outside test bodies.

    The expected output is exact, not a shape. A selector that broadened to
    match an ordinary comment would emit that comment's id, and the workflow
    assigns the first id it reads to `first` and PATCHes it — rewriting what a
    person wrote. Checking only that the output looks like an id would accept
    exactly that.
    """
    own_workflow = SELECTOR_WORKFLOWS[identifier]
    sibling_workflow = (
        SWIFT_WORKFLOW if own_workflow == COVERAGE_WORKFLOW else COVERAGE_WORKFLOW
    )
    own_body = (
        rust_comment_body() if own_workflow == COVERAGE_WORKFLOW else native_comment_body()
    )
    sibling_body = (
        native_comment_body() if own_workflow == COVERAGE_WORKFLOW else rust_comment_body()
    )
    marker = emitted_marker(own_workflow)
    selected = str(MARKER_COMMENT_ID)
    return [
        SelectorCase(
            name="the comment its own workflow wrote",
            page=comments_page({"id": MARKER_COMMENT_ID, "body": own_body}),
            expected=(selected,),
        ),
        SelectorCase(
            name="its own comment behind unrelated ones",
            page=comments_page(
                {"id": EARLIER_COMMENT_ID, "body": "an earlier comment from a person"},
                {"id": SECOND_EARLIER_COMMENT_ID, "body": "another one, also unrelated"},
                {"id": MARKER_COMMENT_ID, "body": own_body},
            ),
            expected=(selected,),
        ),
        SelectorCase(
            name="duplicates behind unrelated comments",
            page=comments_page(
                {"id": EARLIER_COMMENT_ID, "body": "an earlier comment from a person"},
                {"id": MARKER_COMMENT_ID, "body": own_body},
                {"id": SECOND_MARKER_COMMENT_ID, "body": own_body},
            ),
            expected=(selected, str(SECOND_MARKER_COMMENT_ID)),
        ),
        SelectorCase(
            name="its own no-report body",
            page=comments_page(
                {"id": MARKER_COMMENT_ID, "body": no_report_comment_body(own_workflow)}
            ),
            expected=(selected,),
        ),
        SelectorCase(
            name="the sibling workflow's comment",
            page=comments_page({"id": FOREIGN_COMMENT_ID, "body": sibling_body}),
            expected=(),
        ),
        SelectorCase(
            name="the sibling workflow's no-report body",
            page=comments_page(
                {"id": FOREIGN_COMMENT_ID, "body": no_report_comment_body(sibling_workflow)}
            ),
            expected=(),
        ),
        SelectorCase(
            name="a pull request with no comments yet",
            page=comments_page(),
            expected=(),
        ),
        SelectorCase(
            name="an ordinary comment from a person",
            page=comments_page({"id": FOREIGN_COMMENT_ID, "body": "an ordinary review comment"}),
            expected=(),
        ),
        SelectorCase(
            name="an ordinary comment quoting the marker",
            page=comments_page(
                {"id": FOREIGN_COMMENT_ID, "body": f"Should the {marker} marker move?"}
            ),
            expected=(),
        ),
        SelectorCase(
            name="duplicate marker comments, oldest first",
            page=comments_page(
                {"id": MARKER_COMMENT_ID, "body": own_body},
                {"id": SECOND_MARKER_COMMENT_ID, "body": own_body},
            ),
            expected=(selected, str(SECOND_MARKER_COMMENT_ID)),
        ),
    ]


def selector_failures(identifier: str) -> list[str]:
    """Everything one selector gets wrong across the pages it can meet.

    Runs outside test bodies and checks three things per page, in this order
    because each would mask the next:

    The exit status. The workflow reads the filter through
    `existing="$(gh api ... --jq ...)"` in a step run under `bash -e`, so a
    failure aborts the step and leaves the coverage comment at whatever the last
    successful run wrote. A failing filter also prints nothing, so judging by
    output alone reads an aborted run as a clean no-match.

    The exact ids emitted, in order. The workflow keeps only the first line, so
    ordering across duplicates decides which comment gets rewritten, and an
    unexpected id on a page that should match nothing is a comment belonging to
    a person that the workflow would overwrite.

    That every id is a plain integer. The GitHub CLI runs these filters on its
    own jq implementation rather than the binary here; the two agree about
    numbers and differ about how they print strings, so a selector that only
    ever emits ids behaves the same under both.
    """
    problems: list[str] = []
    for case in selector_cases(identifier):
        result = run_extracted(identifier, case.page)
        if result.status != 0:
            problems.append(
                f"{identifier} on {case.name!r} exited {result.status}, which aborts the "
                f"publishing step: {result.stderr.strip()}"
            )
            continue
        # Lines, not whitespace-separated tokens. The publishing shell keeps
        # only what precedes the first newline, so two ids on one line is one
        # value it would use whole as a comment id, updating neither comment.
        emitted = tuple(line for line in result.stdout.splitlines() if line.strip())
        if emitted != case.expected:
            problems.append(
                f"{identifier} on {case.name!r} emitted {emitted} but must emit "
                f"{case.expected}"
            )
        problems.extend(
            f"{identifier} on {case.name!r} emitted {line!r}, which is not a comment id, "
            "so the two jq implementations need not print it alike"
            for line in emitted
            if not line.lstrip("-").isdigit()
        )
    return problems


def selectors_not_stopped_by_a_bodyless_comment() -> list[str]:
    """Selectors that do not fail on a comment carrying no body, outside test bodies.

    This records a divergence rather than a requirement, and it is checked for
    both selectors because both carry the same clause. `startswith` raises on a
    null body, so one such comment ends the whole selection and the run posts
    nothing. Recorded, not fixed: the workflow files belong elsewhere.
    """
    page = comments_page(
        {"id": FOREIGN_COMMENT_ID},
        {"id": MARKER_COMMENT_ID, "body": rust_comment_body()},
    )
    return [
        identifier
        for identifier in SELECTOR_WORKFLOWS
        if not run_extracted(identifier, page).errored
    ]


def replayed_arguments(
    extracted: ExtractedProgram, body_path: Path | None = None
) -> tuple[str, ...]:
    """The workflow's own jq arguments, with only its body file redirected.

    Runs outside test bodies. Every option the workflow passes is replayed
    rather than chosen here — `-n` is behaviour, not decoration, since without
    it jq reads stdin and a `--rawfile` invocation writes no payload while still
    exiting zero.

    Only the file operand of a file-reading option is pointed at the fixture.
    An option carrying a value rather than a filename keeps its own identity,
    with shell variables filled in synthetically: rewriting every expansion to
    the body path would hand `--arg sha "$HEAD_SHA"` the comment file, and a
    filter that corrupted that value would still round-trip the body unchanged.
    """
    replayed: list[str] = []
    arguments = list(extracted.arguments)
    index = 0
    while index < len(arguments):
        text, kind = arguments[index]
        if text in JQ_OPTIONS_TAKING_A_NAME_AND_A_VALUE and index + 2 < len(arguments):
            name = arguments[index + 1][0]
            value_text, value_kind = arguments[index + 2]
            replayed.extend([text, name])
            if text in JQ_OPTIONS_TAKING_A_FILE and body_path is not None:
                replayed.append(str(body_path))
            elif value_kind == "shell-expansion":
                replayed.append(substitute_shell_variables(value_text))
            else:
                replayed.append(value_text)
            index += 3
            continue
        if text in JQ_OPTIONS_TAKING_ONE_VALUE and index + 1 < len(arguments):
            value_text, value_kind = arguments[index + 1]
            replayed.append(text)
            replayed.append(
                substitute_shell_variables(value_text)
                if value_kind == "shell-expansion"
                else value_text
            )
            index += 2
            continue
        replayed.append(substitute_shell_variables(text) if kind == "shell-expansion" else text)
        index += 1
    return tuple(replayed)


def payload_round_trip_failures() -> list[str]:
    """Producer bodies the payload programs do not return unchanged.

    Runs outside test bodies, and iterates whatever the workflows define rather
    than a list kept by hand here. The named round trips below each document
    one body, but a body variant added to a workflow would not appear in any of
    them until somebody wrote one; this one covers every variant a workflow
    composes, so coverage follows the workflow instead of trailing it.
    """
    failures: list[str] = []
    for workflow, identifier, report in (
        (
            COVERAGE_WORKFLOW,
            RUST_COMMENT_PAYLOAD,
            render(healthy_export(), FIXTURE_REPO_ROOT, RUST_TOP_UNCOVERED, RUST_REPORT_TITLE),
        ),
        (
            SWIFT_WORKFLOW,
            NATIVE_COMMENT_PAYLOAD,
            NATIVE_SUMMARIZER.render(
                native_report(native_target(PRODUCT_TARGET, covered=40, executable=100)),
                FIXTURE_REPO_ROOT,
                NATIVE_TOP_UNCOVERED,
                NATIVE_REPORT_TITLE,
            ),
        ),
    ):
        for variant, body in sorted(producer_bodies(workflow, report).items()):
            extracted = one_extracted_for(identifier)
            with tempfile.TemporaryDirectory() as workspace:
                body_path = Path(workspace) / COMMENT_FILE
                body_path.write_text(body, encoding="utf-8")
                result = run_jq(
                    extracted.program, "", *replayed_arguments(extracted, body_path)
                )
            if result.status != 0:
                failures.append(f"{workflow.name}/{variant}: jq failed: {result.stderr.strip()}")
                continue
            if not result.stdout.strip():
                failures.append(
                    f"{workflow.name}/{variant}: the invocation wrote no payload while exiting "
                    "zero, which is what dropping -n from a --rawfile invocation does"
                )
                continue
            returned = json.loads(result.stdout)
            if list(returned) != ["body"] or returned["body"] != body:
                failures.append(
                    f"{workflow.name}/{variant}: the payload did not return the body unchanged"
                )
    return failures


def predicate_disagreements(
    identifier: str, documents: tuple[RecordedDocument, ...]
) -> list[str]:
    """Compare a landed document predicate against every recorded document.

    Runs outside test bodies, and reports nothing at all while the predicate has
    not landed — the loader half of the same table is asserted separately, so an
    absent predicate leaves that half testing the documents rather than leaving
    the module testing nothing.
    """
    landed = programs_for(identifier)
    disagreements: list[str] = []
    for program in landed:
        for recorded in documents:
            result = run_jq(
                program.program,
                json.dumps(recorded.document),
                *replayed_arguments(program),
            )
            if result.errored:
                disagreements.append(
                    f"{recorded.name}: the predicate errored instead of deciding "
                    f"({result.stderr.strip()})"
                )
                continue
            if result.accepted != recorded.predicate_accepts:
                disagreements.append(
                    f"{recorded.name}: recorded predicate_accepts="
                    f"{recorded.predicate_accepts}, observed {result.accepted}"
                )
    return disagreements


def predicate_errors_on_readable_documents(
    identifier: str, documents: tuple[RecordedDocument, ...]
) -> list[str]:
    """Landed-predicate runtime errors on documents the summarizer reads.

    Runs outside test bodies. A predicate may refuse a document the summarizer
    reads — several deliberately do — but it may never fall over on one. An
    error is not a decision, and it takes the whole step down rather than
    discarding one candidate.
    """
    landed = programs_for(identifier)
    errors: list[str] = []
    for program in landed:
        for recorded in documents:
            if not recorded.loader_reads:
                continue
            result = run_jq(
                program.program,
                json.dumps(recorded.document),
                *replayed_arguments(program),
            )
            if result.errored:
                errors.append(f"{recorded.name}: {result.stderr.strip()}")
    return errors


class JqBackedTestCase(unittest.TestCase):
    """Base for tests that execute the real jq binary.

    jq ships in the GitHub-hosted Ubuntu runner image, and both coverage
    workflows already invoke it with no install step of their own, so CI runs
    every test below. The development shell does not provide it, so an absent jq
    means a developer machine rather than CI, and those tests skip with a message
    naming what went untested. A jq that is present but cannot run is failed
    rather than skipped: a broken toolchain reporting itself as a pass is the
    shape of failure this whole module exists to rule out.
    """

    def setUp(self) -> None:
        if JQ_BINARY is None and RUNNING_IN_CI:
            self.fail(
                "jq is not on PATH in CI, so every predicate below would have gone "
                "unexecuted while this step still exited zero. The runner image is "
                "expected to provide jq — both coverage workflows invoke it with no "
                "install step of their own — so its absence here is a broken runner or a "
                "broken PATH, and it is failed rather than skipped."
            )
        if JQ_BINARY is None:
            raise unittest.SkipTest(
                "jq is not on PATH, so the workflow predicate tests did not run. CI has jq "
                "and fails without it; this skip is a local-machine gap, not a pass."
            )
        probe = subprocess.run(
            [JQ_BINARY, "--version"],
            capture_output=True,
            text=True,
            check=False,
            timeout=JQ_TIMEOUT_SECONDS,
        )
        if probe.returncode != 0:
            self.fail(
                f"jq is present at {JQ_BINARY} but cannot run: status {probe.returncode}, "
                f"stderr {probe.stderr.strip()!r}. Refusing to skip, because a broken jq "
                "would let every predicate below pass without executing."
            )

        reported = probe.stdout.strip() or probe.stderr.strip()
        version = JQ_VERSION.match(reported)
        if version is None:
            self.fail(
                f"jq at {JQ_BINARY} reported a version this module cannot parse: "
                f"{reported!r}. The recorded counter divergences depend on which jq is "
                "running, so an unknown one is failed rather than assumed recent."
            )
        if (int(version.group("major")), int(version.group("minor"))) < MINIMUM_JQ_VERSION:
            self.fail(
                f"jq at {JQ_BINARY} reports {reported!r}, older than "
                f"{MINIMUM_JQ_VERSION[0]}.{MINIMUM_JQ_VERSION[1]}. Before 1.7 an untouched "
                "number literal was converted to a double on the way through, so the "
                "counter divergences recorded here would be measured against different "
                "behaviour and would pass or fail for the wrong reason."
            )


# --------------------------------------------------------------------------
# Extraction
# --------------------------------------------------------------------------


class WorkflowReadingTests(unittest.TestCase):
    """The reader finds the programs, and says so loudly when it stops.

    These need no jq. They answer whether this module is testing anything at
    all, which is the question a vacuous harness answers wrongly and silently.
    """

    def test_the_coverage_workflow_is_on_disk(self) -> None:
        """Every assertion in this module reads this file, so a rename that
        moved it would otherwise turn the whole module into a pass over
        nothing."""
        self.assertTrue(COVERAGE_WORKFLOW.is_file(), f"missing {COVERAGE_WORKFLOW}")

    def test_the_swift_workflow_is_on_disk(self) -> None:
        """The native half of the same pipeline, and the same failure if it
        moves."""
        self.assertTrue(SWIFT_WORKFLOW.is_file(), f"missing {SWIFT_WORKFLOW}")

    def test_run_blocks_are_recovered_from_the_coverage_workflow(self) -> None:
        """The block-scalar reader is the narrowest part of this module. A
        workflow reformatted into a shape it cannot read returns an empty list,
        and every extraction below then finds nothing while passing."""
        self.assertNotEqual(
            read_run_blocks(COVERAGE_WORKFLOW),
            [],
            "extraction pattern broke: no run: blocks recovered from coverage.yml",
        )

    def test_run_blocks_are_recovered_from_the_swift_workflow(self) -> None:
        """The same guard for the workflow the native predicates live in."""
        self.assertNotEqual(
            read_run_blocks(SWIFT_WORKFLOW),
            [],
            "extraction pattern broke: no run: blocks recovered from swift.yml",
        )

    def test_a_run_block_keeps_the_shell_around_its_predicate(self) -> None:
        """A predicate is tested against what the surrounding shell actually
        hands it, so the reader returns whole scripts rather than the jq lines
        alone. A reader that returned only the filter would hide the redirection
        and quoting that decide what the filter ever sees."""
        payload_blocks = comment_payload_blocks()

        self.assertEqual(len(payload_blocks), 1)
        self.assertIn("RUNNER_TEMP", payload_blocks[0].script)
        self.assertIn("gh api", payload_blocks[0].script)

    def test_a_shell_comment_naming_jq_yields_no_program(self) -> None:
        """Both workflows carry comments that discuss jq, and the delta work
        adds programs whose own comments do too. A scan that matched the word
        anywhere would take a comment for an invocation and extract the next
        quoted shell word as a filter, testing something that is not a
        predicate."""
        script = "# the pipeline's status is jq's, not the shell's\necho 'not a program'"

        self.assertEqual(jq_programs_in(script), [])

    def test_a_program_mentioning_jq_in_its_own_comment_is_read_once(self) -> None:
        """The delta work's predicates carry jq comments inside the filter
        itself. The word inside a program must not open a second extraction
        that swallows the shell following the program's closing quote."""
        script = "jq -e '\n  # jq loads this as infinity\n  .count < 10\n' file.json"

        self.assertEqual(jq_programs_in(script), ["\n  # jq loads this as infinity\n  .count < 10\n"])

    def test_a_program_split_across_continuation_lines_is_read_whole(self) -> None:
        """`gh api` reaches jq through a flag several backslash-continued lines
        after the command starts, so a scan that stopped at a line end would
        never reach the program."""
        script = 'gh api --paginate \\\n  "repos/$X/comments" \\\n  --jq \'.[] | .id\''

        self.assertEqual(jq_programs_in(script), [".[] | .id"])

    def test_an_option_value_is_not_mistaken_for_the_filter(self) -> None:
        """jq takes its filter as the first operand that is not an option or an
        option's value. A scan that took the next quoted word instead would read
        this invocation's `--arg` value as the filter and test a string that is
        not a predicate at all, while reporting the real one as absent."""
        script = "jq --arg marker '<!-- x -->' '.[] | select(.body)'"

        self.assertEqual(jq_programs_in(script), [".[] | select(.body)"])

    def test_a_double_quoted_filter_is_read(self) -> None:
        """A filter written in double quotes is still a literal the harness can
        run, so it is read rather than refused."""
        self.assertEqual(jq_programs_in('jq ".targets[] | .name" report.json'), [".targets[] | .name"])

    def test_a_filter_built_by_the_shell_is_refused(self) -> None:
        """A filter that only exists at run time cannot be tested here. It is
        recorded as unreadable rather than dropped: dropping it would leave the
        exhaustiveness check reporting that every predicate in the file was
        accounted for while this one ran untested."""
        refusals = refusal_reasons('jq "$PROGRAM" report.json')

        self.assertEqual(refusals, ["the filter is written as shell-expansion"])

    def test_a_filter_read_from_a_file_is_refused(self) -> None:
        """`-f` moves the filter into a file, so the operand that follows is
        input rather than a program. Reading that operand as the filter would
        put a filename under test."""
        refusals = refusal_reasons("jq -f filter.jq report.json")

        self.assertEqual(refusals, ["the filter is read from a file"])

    def test_a_filename_ending_in_jq_is_not_an_invocation(self) -> None:
        """The scanner looks for a command, not for the letters. A file called
        `filter.jq` names no invocation, and treating it as one would refuse a
        script that is perfectly readable."""
        self.assertEqual(scan_jq_invocations("cat filter.jq"), [])

    def test_the_rust_comment_payload_program_is_found(self) -> None:
        """The loud failure this module is built around, for one program: a
        predicate renamed, rewritten, or moved out of a `run:` block stops
        matching, and this names it rather than leaving a program the workflow
        still executes untested."""
        self.assertEqual(len(programs_for(RUST_COMMENT_PAYLOAD)), 1)

    def test_the_rust_sticky_selector_program_is_found(self) -> None:
        """The same guard for the selector that decides which comment a run
        rewrites."""
        self.assertEqual(len(programs_for(RUST_STICKY_SELECTOR)), 1)

    def test_the_native_comment_payload_program_is_found(self) -> None:
        """The same guard on the native side, which is a separate copy of the
        same shell and can drift from it."""
        self.assertEqual(len(programs_for(NATIVE_COMMENT_PAYLOAD)), 1)

    def test_the_native_sticky_selector_program_is_found(self) -> None:
        """The same guard for the native selector."""
        self.assertEqual(len(programs_for(NATIVE_STICKY_SELECTOR)), 1)

    def test_declared_presence_matches_the_workflows(self) -> None:
        """The registry's claim about what is on disk, checked against disk in
        both directions.

        A specification declared present whose program is gone means extraction
        broke or the program was deleted. One declared pending whose program has
        arrived must be promoted: left pending it is permanently optional, since
        nothing requires it to match, and a later rewrite would quietly retire
        every test that depends on it instead of failing. Promotion is a
        one-word edit, and this failure is what demands it."""
        self.assertEqual(misdeclared_presence(), [])

    def test_a_specification_claiming_fixtures_it_lacks_is_named(self) -> None:
        """The branch that closes the reported hole, driven from a controlled
        registry rather than from the repository's own, which is correct and so
        exercises neither branch."""
        claimed = PredicateSpec(
            identifier="claims-fixtures-it-lacks",
            workflow=COVERAGE_WORKFLOW.name,
            presence=PENDING,
            exercised=True,
            role="a specification whose flag is set and whose fixtures are not written",
            tokens=("irrelevant",),
        )

        problems = misdeclared_exercised_specs((claimed,), {})

        self.assertEqual(len(problems), 1)
        self.assertIn("claims-fixtures-it-lacks", problems[0])
        self.assertIn("exercised=True", problems[0])

    def test_a_specification_hiding_fixtures_it_has_is_named(self) -> None:
        """The other branch. Fixtures written against a specification still
        declaring itself uncovered leave the exhaustiveness check demanding
        work that is already done."""
        hidden = PredicateSpec(
            identifier="hides-fixtures-it-has",
            workflow=COVERAGE_WORKFLOW.name,
            presence=PENDING,
            exercised=False,
            role="a specification with cases behind it and its flag unset",
            tokens=("irrelevant",),
        )

        problems = misdeclared_exercised_specs((hidden,), {"hides-fixtures-it-has": ["a case"]})

        self.assertEqual(len(problems), 1)
        self.assertIn("hides-fixtures-it-has", problems[0])
        self.assertIn("exercised=False", problems[0])

    def test_every_exercised_specification_is_backed_by_fixtures(self) -> None:
        """`exercised` decides whether a landed program counts as covered, so
        an unbacked flag makes this module pass over a predicate nothing drives
        — the silent pass it exists to refuse, turned on its own bookkeeping.

        Checked both ways: a flag set without fixtures fails, and so does a
        fixture set whose specification still declares itself uncovered."""
        self.assertEqual(misdeclared_exercised_specs(), [])

    def test_every_extracted_program_is_exercised(self) -> None:
        """The direction that closes the defect class. A jq program that no
        fixture-backed specification covers fails this test, so a predicate
        cannot reach main untested by the route every divergence recorded in
        this module took.

        Registration alone does not count. A specification matches on token
        substrings, which says a program of roughly the right shape arrived, not
        that it does the right thing; only fixtures decide that."""
        self.assertEqual(
            unexercised_programs(),
            [],
            "a jq program in a coverage workflow that no fixture-backed specification "
            "covers: give it a PredicateSpec with fixtures and exercised=True. A "
            "predicate with no fixtures is how every divergence recorded here happened.",
        )

    def test_the_workflows_reach_jq_only_in_the_recorded_forms(self) -> None:
        """The gate on how far this reader has to model the shell.

        Every filter in both workflows is a single-quoted literal, which is the
        form the shell hands through untouched. That is what makes a finding
        about parsing some other construct a finding about code these workflows
        do not contain — and it is checked here rather than asserted, so the
        judgement rests on the files instead of on somebody's memory of them.

        A workflow that reaches jq a new way fails this, which is the point:
        the standing decline on shell-parsing findings lapses exactly when a
        new form arrives, and only for that form."""
        self.assertEqual(invocation_forms(), RECORDED_INVOCATION_FORMS)

    def test_the_selectors_are_run_over_every_page(self) -> None:
        """A sticky comment older than the first API page is invisible without
        `--paginate`, and the workflow then posts a second comment beside the
        one it meant to rewrite. Nothing in the filter itself says so, and a
        fixture is always one preassembled array, so the flag is asserted on
        the `gh api` call that carries the filter rather than inferred."""
        self.assertEqual(unpaginated_selectors(), [])

    def test_the_payload_is_written_where_the_request_reads_it(self) -> None:
        """The workflow redirects jq into a file and then hands that file to
        `gh api --input`. Only the name connects them, so a lost redirection or
        an `--input` naming something else leaves the request with no body
        while jq still exits zero and a well-formed payload still exists
        here."""
        self.assertEqual(payload_handoff_problems(), [])

    def test_both_publishing_branches_send_the_payload(self) -> None:
        """A run either rewrites the existing comment or creates the first one,
        and only one branch executes per run. Asking whether some request sends
        the payload passes a workflow whose POST branch does not — and that
        branch runs on every pull request exactly once, the first time it gets
        a coverage comment at all."""
        self.assertEqual(payload_handoff_problems(), [])

    def test_the_selector_output_is_reduced_to_one_line(self) -> None:
        """The selectors emit one id per matching comment, and emitting several
        is only safe because the shell keeps the first before building the
        PATCH endpoint. Nothing in the filter says so, so a step that started
        consuming the whole value would send several ids as one identifier and
        update nothing, while every filter check stayed green."""
        self.assertEqual(unpinned_selector_consumers(), [])

    def test_the_comment_is_written_beside_the_report_it_wraps(self) -> None:
        """The publishing job reads the comment out of an uploaded directory.
        A producer that kept the `comment.md` name while redirecting elsewhere
        is still a producer by every other check here, but the file leaves the
        uploaded directory and the publishing job reports that the run measured
        nothing."""
        self.assertEqual(report_producer_path_problems(), [])

    def test_the_publishing_jobs_can_write_the_comment(self) -> None:
        """The filter and the request can both be right and the comment still
        never appear, because the job was never granted the scope that lets it
        write one. Both publishing jobs continue on error, so that failure is
        silent — the sticky comment goes stale, or a first one is never
        created, and the workflow still reports success."""
        self.assertEqual(jobs_missing_comment_write_permission(), [])

    def test_every_jq_invocation_in_the_workflows_is_readable(self) -> None:
        """The exhaustiveness check is only worth as much as the scan beneath
        it. An invocation whose filter is built at run time, read from a file,
        or hidden behind an option the scanner does not know would be absent
        from the inventory entirely, so every other check here would pass while
        the workflow ran a predicate this module never saw."""
        self.assertEqual(
            unreadable_invocations(),
            [],
            "a jq invocation whose filter this module cannot read. Teach the scanner "
            "that form, or write the filter as a literal, but do not leave it "
            "unreadable: an unreadable predicate is an untested one.",
        )

    def test_the_coverage_workflow_selects_the_marker_it_emits(self) -> None:
        """Producer and consumer read out of the same file and compared. The
        shell stamps a marker on the report it writes and the selector looks
        for one later; nothing in the workflow makes those the same string, and
        a change to either alone leaves runs unable to find the comment they
        wrote, posting a fresh one on every push instead of rewriting it."""
        self.assertEqual(emitted_marker(COVERAGE_WORKFLOW), selected_marker(RUST_STICKY_SELECTOR))

    def test_the_swift_workflow_selects_the_marker_it_emits(self) -> None:
        """The same pairing in the native workflow, which carries its own copy
        of both halves."""
        self.assertEqual(emitted_marker(SWIFT_WORKFLOW), selected_marker(NATIVE_STICKY_SELECTOR))

    def test_both_coverage_comment_producers_are_found(self) -> None:
        """Each workflow writes its comment down two paths — one for a run that
        measured something, one that retires those numbers when a run measured
        nothing. Finding fewer means a path was removed or the reader stopped
        seeing it, and the per-producer check below would then pass by
        examining less than the workflow does."""
        self.assertEqual(
            len(comment_producing_blocks(COVERAGE_WORKFLOW)), EXPECTED_COMMENT_PRODUCERS
        )
        self.assertEqual(
            len(comment_producing_blocks(SWIFT_WORKFLOW)), EXPECTED_COMMENT_PRODUCERS
        )

    def test_every_producer_command_can_be_reproduced(self) -> None:
        """A producer that writes part of its comment some other way — an
        `echo` where a `printf` used to be — would leave the composed body
        short, while the producer count and the report classification stayed
        the same. Every whole-comment round trip would then keep passing over
        text no fixture holds, which is testing less rather than failing."""
        self.assertEqual(unreproducible_producer_commands(), [])

    def test_every_coverage_comment_producer_emits_the_selected_marker(self) -> None:
        """Each producer checked on its own, not folded into one set.

        A set over the whole workflow is satisfied while any single path still
        emits the marker, so the path that stopped emitting it stays invisible
        — and that path posts a comment its own selector can never find,
        leaving the next run to write a second one beside it rather than
        rewriting the first."""
        self.assertEqual(
            producers_disagreeing_with_selector(COVERAGE_WORKFLOW, RUST_STICKY_SELECTOR), []
        )

    def test_every_native_comment_producer_emits_the_selected_marker(self) -> None:
        """The same per-producer check in the native workflow, which carries
        its own copy of both paths."""
        self.assertEqual(
            producers_disagreeing_with_selector(SWIFT_WORKFLOW, NATIVE_STICKY_SELECTOR), []
        )

    def test_each_workflow_contributes_its_own_provenance(self) -> None:
        """The prose each producer wraps around the report is its own, and is
        composed from the workflow rather than written here.

        One workflow names the artifacts it uploads one way and the other names
        different artifacts, so a single invented footer shared by both fixtures
        would be a sentence neither workflow prints — and a payload filter that
        rewrote only producer-specific text would round-trip it untouched.
        Asserting the two differ pins that the real text is in use, without
        pinning wording that belongs to files this branch does not edit."""
        self.assertNotEqual(
            provenance_text(COVERAGE_WORKFLOW), provenance_text(SWIFT_WORKFLOW)
        )
        self.assertNotEqual(
            no_report_comment_body(COVERAGE_WORKFLOW), no_report_comment_body(SWIFT_WORKFLOW)
        )

    def test_the_recorded_markers_are_the_ones_the_workflows_use(self) -> None:
        """The constants in this module are a convenience for reading the tests
        below, never the source of truth. Checked against the workflows so they
        cannot quietly describe a marker the pipeline stopped using."""
        self.assertEqual(emitted_marker(COVERAGE_WORKFLOW), RUST_MARKER)
        self.assertEqual(emitted_marker(SWIFT_WORKFLOW), NATIVE_MARKER)

    def test_the_two_sticky_markers_are_distinct(self) -> None:
        """Both workflows comment on the same pull request. Were one marker a
        prefix of the other, each workflow's selector would find the other's
        comment and the two would overwrite each other on every push."""
        self.assertNotEqual(RUST_MARKER, NATIVE_MARKER)
        self.assertFalse(RUST_MARKER.startswith(NATIVE_MARKER))
        self.assertFalse(NATIVE_MARKER.startswith(RUST_MARKER))


# --------------------------------------------------------------------------
# The comment payload
# --------------------------------------------------------------------------


class CommentPayloadTests(JqBackedTestCase):
    """The payload program against the real renderers.

    The report this wraps is what a reviewer reads. A payload that truncated it,
    re-escaped it, or dropped its trailing newline would post a subtly wrong
    comment while every step still exited zero.
    """

    def payload_body(self, identifier: str, body: str) -> str:
        """Run one extracted payload program over a body file, as CI does."""
        extracted = one_extracted_for(identifier)
        with tempfile.TemporaryDirectory() as workspace:
            body_path = Path(workspace) / COMMENT_FILE
            body_path.write_text(body, encoding="utf-8")
            payload_path = Path(workspace) / "payload.json"
            result = run_jq(extracted.program, "", *replayed_arguments(extracted, body_path))
            # Replays the workflow's `> "$payload"`: the request body is a file
            # the next command reads, not jq's stdout.
            payload_path.write_text(result.stdout, encoding="utf-8")
            written = payload_path.read_text(encoding="utf-8")

        self.assertEqual(result.status, 0, f"jq failed: {result.stderr}")
        self.assertTrue(
            result.stdout.strip(),
            "the workflow's own invocation wrote no payload while exiting zero, which is "
            "what dropping -n from a --rawfile invocation does",
        )
        payload = json.loads(written)
        self.assertEqual(list(payload), ["body"], "the API takes exactly one field")
        return payload["body"]

    def test_the_rust_payload_round_trips_the_rendered_report(self) -> None:
        """The comment body must be the rendered report character for character.
        This runs the real summarizer rather than a stored sample, so a renderer
        change introducing a character jq treats differently is caught here
        instead of by a reader of a posted comment."""
        body = rust_comment_body()

        self.assertEqual(self.payload_body(RUST_COMMENT_PAYLOAD, body), body)

    def test_the_rust_payload_survives_the_punctuation_the_renderer_emits(self) -> None:
        """The report is full of backticks, pipes, quotes, and backslashes in
        file paths. Each is a character a payload built by string interpolation
        would have had to escape, which is why the workflow reads the body as a
        raw file instead."""
        report = render(
            export(coverage_file('/repo/crates/domain/src/a b"c\\d.rs', lines=10, covered=1)),
            FIXTURE_REPO_ROOT,
            RUST_TOP_UNCOVERED,
            RUST_REPORT_TITLE,
            "Doctests are not measured; `--doctests` needs a nightly toolchain.",
        )
        body = f"{RUST_MARKER}\n\n{report}"

        self.assertEqual(self.payload_body(RUST_COMMENT_PAYLOAD, body), body)

    def test_the_rust_payload_round_trips_the_whole_produced_comment(self) -> None:
        """The report is only the middle of what the workflow posts. The step
        appends provenance after it — a SHA in backticks, a Markdown link, a
        URL — and none of those characters come from the renderer, so a payload
        that preserved the report and mangled the provenance would pass a
        report-only round trip while posting a broken comment."""
        body = rust_full_comment_body()

        self.assertEqual(self.payload_body(RUST_COMMENT_PAYLOAD, body), body)

    def test_the_rust_payload_round_trips_the_no_report_comment(self) -> None:
        """The other body the workflow writes, and the one no earlier test
        passed through the payload program at all. A run that measured nothing
        posts this instead of a report, and it has to survive the same
        machinery."""
        body = no_report_comment_body(COVERAGE_WORKFLOW)

        self.assertEqual(self.payload_body(RUST_COMMENT_PAYLOAD, body), body)

    def test_the_native_payload_round_trips_the_no_report_comment(self) -> None:
        """The native workflow writes its own no-report body through its own
        copy of the payload program."""
        body = no_report_comment_body(SWIFT_WORKFLOW)

        self.assertEqual(self.payload_body(NATIVE_COMMENT_PAYLOAD, body), body)

    def test_the_native_payload_round_trips_its_own_renderer(self) -> None:
        """The native workflow runs its own copy of the payload program over a
        differently shaped report, so it is checked against its own renderer
        rather than assumed to behave like the Rust one."""
        body = native_comment_body()

        self.assertEqual(self.payload_body(NATIVE_COMMENT_PAYLOAD, body), body)

    def test_every_producer_body_survives_its_payload_program(self) -> None:
        """Every body the workflows compose, not only the ones named below.

        The round trips beside this each document one body, which is what makes
        them readable, but each also has to be written by hand, so a body that
        changed shape would keep passing a test built for the old one. This is
        driven by what the workflows compose instead.

        Completeness rests on the producer count being pinned separately: this
        covers the report and no-report bodies a workflow classifies into, and
        a workflow growing a third producer fails that count rather than
        slipping past here."""
        self.assertEqual(payload_round_trip_failures(), [])

    def test_the_native_payload_round_trips_the_whole_produced_comment(self) -> None:
        """The native counterpart of the whole-comment round trip, and the only
        native test that carries provenance at all.

        Neither other native payload test reaches this text: the renderer test
        stops where the report ends, and the no-report body has no footer to
        corrupt. A native payload filter that preserved the report and mangled
        only the workflow-appended provenance would pass both of them, which is
        the same gap the Rust side had. The two workflows carry separate copies
        of this program and can drift apart, so covering one says nothing about
        the other."""
        body = native_full_comment_body()

        self.assertEqual(self.payload_body(NATIVE_COMMENT_PAYLOAD, body), body)


# --------------------------------------------------------------------------
# The sticky-comment selector
# --------------------------------------------------------------------------


class StickySelectorTests(JqBackedTestCase):
    """Each selector against every comment page it can meet.

    Both halves of this pair come out of the workflow: the shell builds
    `comment.md` by prepending a marker to the rendered report, and the selector
    later finds that comment among every comment on the pull request. The
    invariant is agreement — what a workflow writes, its own selector must find,
    and everything else on the page it must leave alone.

    The cases are held in one table driven over both selectors rather than
    written out per selector. They are independent programs that drift apart,
    and a case covered for one and missing for the other has been the shape of
    three separate defects here; a table cannot be asymmetric.
    """

    def test_the_rust_selector_handles_every_page_it_can_meet(self) -> None:
        """Status, exact ids in order, and id-shaped output, over every page:
        its own comment and no-report body, the sibling workflow's, an ordinary
        comment, one quoting the marker, and duplicates."""
        self.assertEqual(selector_failures(RUST_STICKY_SELECTOR), [])

    def test_the_native_selector_handles_every_page_it_can_meet(self) -> None:
        """The same table against the native selector, which is a separate
        program and drifts on its own."""
        self.assertEqual(selector_failures(NATIVE_STICKY_SELECTOR), [])

    def test_a_comment_carrying_no_body_stops_both_selectors(self) -> None:
        """An expected divergence, recorded rather than fixed: the workflow
        files belong to the coverage-delta work and this branch changes neither.

        `startswith` raises on a non-string, so one comment with no body ends
        the whole selection and the run posts nothing. The job continues on
        error, so the visible symptom is a coverage comment that silently stops
        updating. Checked for both selectors, because both carry the clause."""
        self.assertEqual(selectors_not_stopped_by_a_bodyless_comment(), [])


class RecordedDocumentTests(unittest.TestCase):
    """What the summarizers do with each recorded document.

    This half needs no jq and runs everywhere. It is what keeps the module from
    being vacuous while the coverage-delta predicates are still being written:
    the documents that produced four review-caught divergences are pinned
    against the Python side of the pipeline today, so the predicates have
    something to be checked against the moment they land.
    """

    def test_the_healthy_export_renders(self) -> None:
        """The baseline every rejection fixture is a one-change edit of. A
        harness whose accept case did not accept would make every rejection
        below meaningless."""
        summaries = read_file_summaries(healthy_export())

        self.assertEqual(len(summaries), 1)
        self.assertEqual(summaries[0][1].lines.count, HEALTHY_LINES)
        self.assertEqual(summaries[0][1].lines.covered, HEALTHY_COVERED)

    def test_an_empty_first_data_element_is_read(self) -> None:
        """A recorded divergence. The summarizer iterates every data element
        and defaults a missing `files` to empty, so it reads the valid element
        and reports one file. A predicate indexing the first element instead
        refuses the whole document, and the run falls back to an older baseline
        or none for a document the summarizer would have rendered."""
        summaries = read_file_summaries(empty_first_element_export())

        self.assertEqual(len(summaries), 1)
        self.assertEqual(summaries[0][0], HEALTHY_FILE)

    def test_a_non_string_target_name_is_a_product_target(self) -> None:
        """A recorded divergence. The native summarizer converts the name
        before testing its suffix, so a target named with a number is a product
        target to it. A bare suffix test in jq raises on that same input and
        fails the whole program, which is no report rather than a wrong row."""
        target = native_target(NON_STRING_TARGET_NAME, covered=40, executable=100)

        self.assertFalse(NATIVE_SUMMARIZER.is_test_bundle(target))

    def test_a_counter_past_exact_double_range_stays_exact(self) -> None:
        """A recorded divergence, the Python half. Python integers are exact at
        any width, so a counter one past what a double represents is read back
        unchanged and whatever the report says about it is faithful."""
        summaries = read_file_summaries(line_counter_export(SMALLEST_COUNTER_A_PREDICATE_REFUSES))

        self.assertEqual(summaries[0][1].lines.count, SMALLEST_COUNTER_A_PREDICATE_REFUSES)

    def test_a_four_hundred_digit_counter_is_refused_when_rendered(self) -> None:
        """The far end of the same divergence. The count survives reading, and
        the percentage it implies overflows a float, so the summarizer refuses
        the document rather than inventing a number. Loud refusal is the wanted
        behaviour, pinned so that relaxing it is a deliberate act."""
        document = line_counter_export(FOUR_HUNDRED_DIGITS)

        self.assertFalse(rust_renders(document))

    def test_a_counter_beyond_the_double_range_is_refused_when_read(self) -> None:
        """A JSON literal past the double range parses to infinity, and the
        conversion to an integer raises rather than inventing a number. The
        refusal happens one step earlier than the four-hundred-digit case, which
        is why both are recorded."""
        document = json.loads(
            '{"type": "%s", "data": [{"files": [{"filename": "%s", "summary": '
            '{"lines": {"count": %s, "covered": 0}}}]}]}'
            % (EXPORT_TYPE, HEALTHY_FILE, BEYOND_DOUBLE_RANGE)
        )

        with self.assertRaises(OverflowError):
            read_file_summaries(document)

    def test_impossible_counters_render_a_percentage_above_one_hundred(self) -> None:
        """A recorded divergence. A file reporting more covered lines than lines
        is refused by nothing on the render path: the summarizer reads it and
        prints a percentage no reader can act on. Pinned rather than fixed —
        the summarizers belong to #484, and this branch changes neither."""
        document = export(
            coverage_file(HEALTHY_FILE, lines=IMPOSSIBLE_COUNT, covered=IMPOSSIBLE_COVERED)
        )

        report = render(document, FIXTURE_REPO_ROOT, RUST_TOP_UNCOVERED, RUST_REPORT_TITLE)

        self.assertIn(f"| Lines | {IMPOSSIBLE_COVERED} | {IMPOSSIBLE_COUNT} | 200.00% |", report)

    def test_the_fully_covered_boundary_renders_as_complete(self) -> None:
        """The boundary the impossible case sits one step past, and an ordinary
        fully covered file. A predicate refusing this while refusing more
        covered than exist would throw away real reports, so the boundary is
        asserted beside the failure it neighbours."""
        document = export(
            coverage_file(HEALTHY_FILE, lines=BOUNDARY_LINES, covered=BOUNDARY_LINES)
        )

        report = render(document, FIXTURE_REPO_ROOT, RUST_TOP_UNCOVERED, RUST_REPORT_TITLE)

        self.assertIn(f"| Lines | {BOUNDARY_LINES} | {BOUNDARY_LINES} | 100.00% |", report)

    def test_every_recorded_rust_document_matches_the_summarizer(self) -> None:
        """The whole table is checked against the Rust summarizer, so a
        recorded expectation cannot quietly go stale while the named cases above
        keep passing."""
        self.assertEqual(rust_loader_disagreements(), [])

    def test_every_recorded_native_document_matches_the_summarizer(self) -> None:
        """The same over the native table."""
        self.assertEqual(native_loader_disagreements(), [])

    def test_every_rust_divergence_states_why(self) -> None:
        """A divergence is a judgement, so it is written down and reviewed. An
        entry whose two columns disagree with no reason attached is an
        unreviewed decision wearing the clothes of a recorded one."""
        self.assertEqual(undeclared_divergences(RUST_DOCUMENTS), [])

    def test_every_native_divergence_states_why(self) -> None:
        """The same over the native table."""
        self.assertEqual(undeclared_divergences(NATIVE_DOCUMENTS), [])

    def test_no_rust_document_claims_a_divergence_it_does_not_have(self) -> None:
        """The other direction: a reason left behind after the two sides were
        brought into agreement would describe a conflict that no longer exists,
        which is worse than no note at all."""
        self.assertEqual(pointless_divergence_notes(RUST_DOCUMENTS), [])

    def test_no_native_document_claims_a_divergence_it_does_not_have(self) -> None:
        """The same over the native table."""
        self.assertEqual(pointless_divergence_notes(NATIVE_DOCUMENTS), [])


# --------------------------------------------------------------------------
# The recorded documents, predicate half
# --------------------------------------------------------------------------


class LandedPredicateTests(JqBackedTestCase):
    """Every recorded document against whichever document predicate has landed.

    These are the assertions that close the defect class. They report nothing
    while the coverage-delta predicates are still on #484, and pick them up with
    no edit here the moment that work merges, because the specifications match
    on the shape a coverage-document predicate reaches into rather than on its
    text.
    """

    def test_no_landed_rust_predicate_errors_on_a_document_the_summarizer_reads(self) -> None:
        """The invariant that holds whatever a predicate's accept policy is. A
        predicate may refuse a document the summarizer reads — several
        deliberately do — but it may never fall over on one: an error is not a
        decision, and it takes the step down instead of discarding a candidate.
        This is the exact shape of two recorded divergences."""
        self.assertEqual(
            predicate_errors_on_readable_documents(RUST_DOCUMENT_PREDICATE, RUST_DOCUMENTS), []
        )

    def test_no_landed_native_predicate_errors_on_a_document_the_summarizer_reads(self) -> None:
        """The same invariant on the native side, where the non-string target
        name made it fail once already."""
        self.assertEqual(
            predicate_errors_on_readable_documents(NATIVE_DOCUMENT_PREDICATE, NATIVE_DOCUMENTS),
            [],
        )

    def test_a_landed_rust_predicate_decides_each_document_as_recorded(self) -> None:
        """Accept and reject, checked per document against the recorded column.
        A mismatch names the one document that moved, which is what turns a
        predicate rewrite from a review exercise into a test failure."""
        self.assertEqual(predicate_disagreements(RUST_DOCUMENT_PREDICATE, RUST_DOCUMENTS), [])

    def test_a_landed_native_predicate_decides_each_document_as_recorded(self) -> None:
        """The same over the native table."""
        self.assertEqual(
            predicate_disagreements(NATIVE_DOCUMENT_PREDICATE, NATIVE_DOCUMENTS), []
        )


class JqNumericContractTests(JqBackedTestCase):
    """The jq behaviour the recorded counter divergences rest on.

    Pinned against the real binary rather than remembered, so a jq upgrade that
    changed any of it surfaces here — as a failure in a module about jq — rather
    than as a coverage workflow that quietly starts or stops rejecting baselines.
    """

    def test_an_untouched_counter_survives_jq_intact(self) -> None:
        """jq hands a literal it never computed with back unchanged, which is
        why a predicate that only reads a counter looks correct next to a
        summarizer that reads it exactly."""
        result = run_jq(".count", json.dumps({"count": FIRST_INTEGER_A_DOUBLE_CANNOT_HOLD}))

        self.assertEqual(result.stdout.strip(), str(FIRST_INTEGER_A_DOUBLE_CANNOT_HOLD))

    def test_one_arithmetic_step_collapses_the_same_counter(self) -> None:
        """The same counter through a single addition comes back a double, off
        by one. A predicate that compares or sums counters is therefore not
        reading the numbers the summarizer renders, and no amount of reading the
        program shows that."""
        result = run_jq(".count + 0", json.dumps({"count": FIRST_INTEGER_A_DOUBLE_CANNOT_HOLD}))

        self.assertEqual(result.stdout.strip(), str(FIRST_INTEGER_A_DOUBLE_CANNOT_HOLD - 1))

    def test_only_the_documented_statuses_read_as_a_decision(self) -> None:
        """Pinned against the real binary, because the whole accept/reject
        reading rests on it. Under `-e` jq exits 0 with a value, 1 when the
        last output was false or null, and 4 when there was none; those are
        decisions. A runtime error is 5, and a filter can choose its own status
        through `halt_error`, which is why anything outside the three counts as
        falling over rather than as a quiet rejection."""
        document = json.dumps({"a": 1})

        self.assertFalse(run_jq("select(.a)", document, "-e").errored)
        self.assertFalse(run_jq("select(.b)", document, "-e").errored)
        self.assertFalse(run_jq("false", document, "-e").errored)
        self.assertTrue(run_jq('error("boom")', document, "-e").errored)
        self.assertTrue(run_jq("halt_error(7)", document, "-e").errored)

    def test_a_suffix_test_refuses_a_non_string_name(self) -> None:
        """The mechanism behind the recorded non-string target name: jq raises
        where the summarizer converts. The two are not the same test, and the
        difference costs a whole report rather than one row."""
        result = run_jq(
            f'.targets[] | .name | endswith("{NATIVE_SUMMARIZER.TEST_BUNDLE_SUFFIX}")',
            json.dumps(
                native_report(native_target(NON_STRING_TARGET_NAME, covered=5, executable=10))
            ),
        )

        self.assertTrue(result.errored)
        self.assertIn("endswith", result.stderr)


if __name__ == "__main__":
    unittest.main()

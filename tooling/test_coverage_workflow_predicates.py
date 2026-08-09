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
"""

from __future__ import annotations

import importlib.util
import json
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
# flag. Both take the program as the next single-quoted word.
JQ_TOKEN = re.compile(r"(?:--jq|jq)(?![-\w])")
WORD_CHARACTER = re.compile(r"[-\w]")


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


def jq_programs_in(script: str) -> list[str]:
    """Extract every jq program from one shell script, outside test bodies.

    The scan tracks shell quoting rather than pattern-matching the text, and it
    has to. These scripts carry comments that mention jq, and the programs
    themselves carry comments that mention jq; a scan that ignored quoting would
    take the word inside a program's own comment for a fresh invocation and
    extract the surrounding shell as if it were a filter. A single-quoted shell
    word cannot contain a single quote, so a program's extent is unambiguous
    once the scan knows it is looking at one.
    """
    programs: list[str] = []
    awaiting_program = False
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
            if awaiting_program:
                programs.append(script[index + 1 : closing])
                awaiting_program = False
            index = closing + 1
            continue

        if character == "#" and (index == 0 or script[index - 1] in " \t\n"):
            newline = script.find("\n", index)
            index = length if newline < 0 else newline
            continue

        # An unescaped newline or semicolon ends the command, so a jq
        # invocation that never reached a program does not capture the next
        # unrelated quoted word in the script.
        if character in ";\n":
            awaiting_program = False
            index += 1
            continue

        preceded_by_word = index > 0 and WORD_CHARACTER.match(script[index - 1]) is not None
        token = JQ_TOKEN.match(script, index)
        if token and not preceded_by_word:
            awaiting_program = True
            index = token.end()
            continue

        index += 1
    return programs


def extract_jq_programs(blocks: list[RunBlock]) -> list[ExtractedProgram]:
    """Extract every jq program from the given run blocks, outside test bodies."""
    return [
        ExtractedProgram(
            workflow=block.workflow, job=block.job, step=block.step, program=program
        )
        for block in blocks
        for program in jq_programs_in(block.script)
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
# PENDING is for a program this repository expects but does not carry yet. The
# coverage-delta work on #484 owns both workflow files and adds seven programs
# between them; each is registered here in advance, by role, so that landing
# them neither fails this module nor leaves them untested. The document
# predicates among them pick up the whole fixture table below on arrival.
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
        workflow=COVERAGE_WORKFLOW.name,
        presence=PRESENT,
        role="wraps the rendered report as the GitHub comment payload",
        tokens=("body:", "$body"),
    ),
    PredicateSpec(
        identifier=RUST_STICKY_SELECTOR,
        workflow=COVERAGE_WORKFLOW.name,
        presence=PRESENT,
        role="finds this workflow's own sticky comment by its marker",
        tokens=("startswith", RUST_MARKER),
    ),
    PredicateSpec(
        identifier=NATIVE_COMMENT_PAYLOAD,
        workflow=SWIFT_WORKFLOW.name,
        presence=PRESENT,
        role="wraps the rendered native report as the GitHub comment payload",
        tokens=("body:", "$body"),
    ),
    PredicateSpec(
        identifier=NATIVE_STICKY_SELECTOR,
        workflow=SWIFT_WORKFLOW.name,
        presence=PRESENT,
        role="finds this workflow's own sticky comment by its marker",
        tokens=("startswith", NATIVE_MARKER),
    ),
    PredicateSpec(
        identifier="rust-baseline-candidate-selection",
        workflow=COVERAGE_WORKFLOW.name,
        presence=PENDING,
        role="orders the main-branch runs a baseline may be taken from",
        tokens=("workflow_runs", "@tsv"),
    ),
    PredicateSpec(
        identifier="rust-baseline-artifact-lookup",
        workflow=COVERAGE_WORKFLOW.name,
        presence=PENDING,
        role="finds the coverage artifact uploaded by one candidate run",
        tokens=(".artifacts[]",),
    ),
    PredicateSpec(
        identifier=RUST_DOCUMENT_PREDICATE,
        workflow=COVERAGE_WORKFLOW.name,
        presence=PENDING,
        role="decides whether a baseline llvm-cov export is one the summarizer can read",
        tokens=("llvm.coverage.json.export",),
    ),
    PredicateSpec(
        identifier="native-baseline-candidate-selection",
        workflow=SWIFT_WORKFLOW.name,
        presence=PENDING,
        role="orders the main-branch runs a baseline may be taken from",
        tokens=("workflow_runs", "@tsv"),
    ),
    PredicateSpec(
        identifier="native-baseline-artifact-lookup",
        workflow=SWIFT_WORKFLOW.name,
        presence=PENDING,
        role="finds the coverage artifact uploaded by one candidate run",
        tokens=(".artifacts[]",),
    ),
    PredicateSpec(
        identifier="native-test-step-conclusion",
        workflow=SWIFT_WORKFLOW.name,
        presence=PENDING,
        role="judges whether a candidate run finished the tests its measurement rests on",
        tokens=(".jobs[]", "run_attempt"),
    ),
    PredicateSpec(
        identifier=NATIVE_DOCUMENT_PREDICATE,
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


def missing_present_specs() -> list[str]:
    """Every PRESENT specification matching no program, outside test bodies."""
    extracted = all_extracted_programs()
    return [
        f"{spec.identifier} ({spec.role}) matched "
        f"{len([p for p in extracted if spec.matches(p)])} programs in {spec.workflow}"
        for spec in PREDICATE_SPECS
        if spec.presence == PRESENT
        and len([program for program in extracted if spec.matches(program)]) != 1
    ]


def unregistered_programs() -> list[str]:
    """Every extracted program matching no specification, outside test bodies."""
    return [
        program.describe()
        for program in all_extracted_programs()
        if not any(spec.matches(program) for spec in PREDICATE_SPECS)
    ]


# --------------------------------------------------------------------------
# Fixtures
# --------------------------------------------------------------------------

HEALTHY_FILE = "/repo/crates/domain/src/session.rs"
SECOND_FILE = "/repo/crates/domain/src/turn.rs"
PRODUCT_TARGET = "Signalbox.app"

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


def native_target(name, *, covered: int, executable: int) -> dict:
    """One xccov target whose knobs are its covered and executable line counts."""
    return {
        "name": name,
        "coveredLines": covered,
        "executableLines": executable,
        "lineCoverage": 0.0 if executable == 0 else covered / executable,
        "files": [],
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
    """Rebuild `comment.md` the way the coverage workflow's own shell does."""
    report = render(healthy_export(), FIXTURE_REPO_ROOT, RUST_TOP_UNCOVERED, RUST_REPORT_TITLE)
    return f"{RUST_MARKER}\n\n{report}"


def native_comment_body() -> str:
    """Rebuild `comment.md` the way the swift workflow's own shell does."""
    report = NATIVE_SUMMARIZER.render(
        native_report(native_target(PRODUCT_TARGET, covered=40, executable=100)),
        FIXTURE_REPO_ROOT,
        NATIVE_TOP_UNCOVERED,
        NATIVE_REPORT_TITLE,
    )
    return f"{NATIVE_MARKER}\n\n{report}"


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

# jq's exit status for a runtime error, as distinct from a filter that ran and
# selected nothing. The difference is the whole of one recorded divergence: a
# program that errors has not decided about its input, it has fallen over.
JQ_RUNTIME_ERROR = 5


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
        return self.status == JQ_RUNTIME_ERROR


def run_jq(program: str, document: str, *arguments: str) -> JqResult:
    """Run one extracted jq program over a document, outside test bodies."""
    completed = subprocess.run(
        [JQ_BINARY, *arguments, program],
        input=document,
        capture_output=True,
        text=True,
        check=False,
    )
    return JqResult(
        status=completed.returncode, stdout=completed.stdout, stderr=completed.stderr
    )


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
            result = run_jq(program.program, json.dumps(recorded.document), "-e")
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
            result = run_jq(program.program, json.dumps(recorded.document), "-e")
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
        if JQ_BINARY is None:
            raise unittest.SkipTest(
                "jq is not on PATH, so the workflow predicate tests did not run. CI has jq "
                "and runs them; this skip is a local-machine gap, not a pass."
            )
        probe = subprocess.run(
            [JQ_BINARY, "--version"], capture_output=True, text=True, check=False
        )
        if probe.returncode != 0:
            self.fail(
                f"jq is present at {JQ_BINARY} but cannot run: status {probe.returncode}, "
                f"stderr {probe.stderr.strip()!r}. Refusing to skip, because a broken jq "
                "would let every predicate below pass without executing."
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

    def test_no_expected_program_is_missing(self) -> None:
        """Stated once over the whole registry, so a specification added later
        is guarded without anybody remembering to add a test for it."""
        self.assertEqual(missing_present_specs(), [])

    def test_every_extracted_program_is_registered(self) -> None:
        """The direction that closes the defect class. A jq program added to
        either workflow with no specification here fails this test, so a
        predicate cannot reach main untested by the route every divergence
        recorded in this module took."""
        self.assertEqual(
            unregistered_programs(),
            [],
            "unregistered jq program in a coverage workflow: add a PredicateSpec with "
            "fixtures covering it. A predicate with no fixtures is how every divergence "
            "recorded in this module happened.",
        )

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
        with tempfile.TemporaryDirectory() as workspace:
            body_path = Path(workspace) / "comment.md"
            body_path.write_text(body, encoding="utf-8")
            result = run_jq(
                one_program_for(identifier), "", "-n", "--rawfile", "body", str(body_path)
            )

        self.assertEqual(result.status, 0, f"jq failed: {result.stderr}")
        payload = json.loads(result.stdout)
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

    def test_the_native_payload_round_trips_its_own_renderer(self) -> None:
        """The native workflow runs its own copy of the payload program over a
        differently shaped report, so it is checked against its own renderer
        rather than assumed to behave like the Rust one."""
        body = native_comment_body()

        self.assertEqual(self.payload_body(NATIVE_COMMENT_PAYLOAD, body), body)


# --------------------------------------------------------------------------
# The sticky-comment selector
# --------------------------------------------------------------------------


class StickySelectorTests(JqBackedTestCase):
    """The selector must find exactly the comment its own workflow wrote.

    Both halves of this pair come out of the same workflow: the shell builds
    `comment.md` by prepending a marker to the rendered report, and the selector
    later finds that comment among every comment on the pull request. The
    invariant is agreement — what a workflow writes, its own selector must find,
    and the sibling workflow's must not.
    """

    def test_the_rust_selector_finds_the_comment_the_coverage_workflow_wrote(self) -> None:
        """The end-to-end agreement this module exists for: the real summarizer
        renders, the workflow's own marker shell wraps, and the workflow's own
        extracted selector then finds it by id."""
        page = comments_page({"id": MARKER_COMMENT_ID, "body": rust_comment_body()})

        result = run_jq(one_program_for(RUST_STICKY_SELECTOR), page)

        self.assertEqual(result.stdout.strip(), str(MARKER_COMMENT_ID))

    def test_the_native_selector_finds_the_comment_the_swift_workflow_wrote(self) -> None:
        """The same round trip through the native renderer and the native
        marker, which are a separate copy and can drift."""
        page = comments_page({"id": MARKER_COMMENT_ID, "body": native_comment_body()})

        result = run_jq(one_program_for(NATIVE_STICKY_SELECTOR), page)

        self.assertEqual(result.stdout.strip(), str(MARKER_COMMENT_ID))

    def test_the_rust_selector_ignores_the_native_comment(self) -> None:
        """The workflows state that their two sticky comments never collide.
        Were that wrong, each would rewrite the other's comment on every push
        and a pull request would end up with one report where it should carry
        two."""
        page = comments_page({"id": FOREIGN_COMMENT_ID, "body": native_comment_body()})

        result = run_jq(one_program_for(RUST_STICKY_SELECTOR), page)

        self.assertEqual(result.stdout.strip(), "")

    def test_the_native_selector_ignores_the_rust_comment(self) -> None:
        """The other direction of the same claim, which is not implied by the
        first: the markers could have been prefixes of each other one way."""
        page = comments_page({"id": FOREIGN_COMMENT_ID, "body": rust_comment_body()})

        result = run_jq(one_program_for(NATIVE_STICKY_SELECTOR), page)

        self.assertEqual(result.stdout.strip(), "")

    def test_a_comment_merely_quoting_the_marker_is_not_selected(self) -> None:
        """The selector anchors at the start of a body, and it must: a review
        comment quoting the marker is an ordinary comment from a person, and
        rewriting it in place would destroy what they wrote."""
        quoting = f"I think the {RUST_MARKER} marker should move."
        page = comments_page({"id": FOREIGN_COMMENT_ID, "body": quoting})

        result = run_jq(one_program_for(RUST_STICKY_SELECTOR), page)

        self.assertEqual(result.stdout.strip(), "")

    def test_the_oldest_marker_comment_is_reported_first(self) -> None:
        """The shell keeps only the first line of the selector's output. Should
        a duplicate marker comment ever exist, the workflow must keep rewriting
        the same one rather than alternate between them."""
        body = rust_comment_body()
        page = comments_page(
            {"id": MARKER_COMMENT_ID, "body": body},
            {"id": SECOND_MARKER_COMMENT_ID, "body": body},
        )

        result = run_jq(one_program_for(RUST_STICKY_SELECTOR), page)

        self.assertEqual(
            result.stdout.split(), [str(MARKER_COMMENT_ID), str(SECOND_MARKER_COMMENT_ID)]
        )

    def test_a_comment_carrying_no_body_stops_the_selector(self) -> None:
        """An expected divergence, recorded rather than fixed: the workflow
        files belong to #484 and this branch changes neither.

        `startswith` raises on a non-string, so one comment with no body ends
        the whole selection and the run posts nothing — the same shape as the
        recorded non-string target name, in the same costly direction. The job
        continues on error, so the visible symptom is a coverage comment that
        silently stops updating."""
        page = comments_page(
            {"id": FOREIGN_COMMENT_ID},
            {"id": MARKER_COMMENT_ID, "body": rust_comment_body()},
        )

        result = run_jq(one_program_for(RUST_STICKY_SELECTOR), page)

        self.assertTrue(result.errored, "expected jq to refuse a comment with no body")
        self.assertIn("startswith", result.stderr)
        self.assertEqual(result.stdout.strip(), "", "the marker comment went unfound")


# --------------------------------------------------------------------------
# The recorded documents, loader half
# --------------------------------------------------------------------------


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

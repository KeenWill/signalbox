#!/usr/bin/env python3
"""Inventory numeric bound constants and enforce their declarations.

The inventory is every integer, float, ``Duration``, or ``NonZero*`` Rust
constant under ``crates/*/src`` and ``apps/*/src`` whose name contains one of
the boundary tokens in ``BOUNDARY_TOKENS``. Ordinary Rust spellings of those
types count: a qualified path such as ``std::time::Duration`` and the signed
``NonZeroI*`` family are inventoried alongside their bare and unsigned
spellings, so no accepted spelling carries an undeclared bound past the gate. A
trait's associated constant counts even with no initializer, because the trait
is where that bound's contract is stated and its implementors may sit outside
the blocking scope entirely. A local ``type`` alias for a numeric type counts as
that type, so renaming ``usize`` cannot carry a bound past the gate.

Test-only modules are inventoried but do not gate because their constants
describe fixtures, not runtime safety. A module written ``#[cfg(test)] mod
tests;`` is test-only across the separate file it names and the whole module
tree beneath it, because a module reached only through a test module is itself
reached only in test builds. Finding that tree means walking from each crate
root, since where a file's own ``mod name;`` declarations resolve depends on how
that file was reached: beneath its own stem for an ordinary child, beside it for
one named by ``#[path]``, and under the enclosing inline module's name when the
declaration sits inside a ``mod name { ... }`` block. Those three rules were
checked against rustc rather than inferred. An item gated in place — a
``#[cfg(test)]`` constant, function, or ``impl`` — is test-only on the same
footing as a module.

The whole attribute run is read, so an intervening attribute and a compound
``cfg(all(test, not(windows)))`` both still count; ``cfg(any(test, ...))`` and
``cfg(not(test))`` do not, because those modules also compile without ``test``
and their constants really are production.

The blocking scope is deliberately smaller than the inventory: production
constants in the daemon, terminal client, application orchestration,
provider-neutral model runtime, persistence, process wire contracts, and
code-host tools. ``ENFORCED_ROOTS`` is the exact scope. Other workspace bounds
remain visible in the success count but are deferred rather than silently
claimed as compliant.

The two roots added by the required-bounds commission contained pre-existing
candidates outside its authoritative 118-row classification. Their exact
path/name pairs remain outside the blocking set; the baseline is exact so any
new candidate in those roots still fails closed instead of silently expanding
that historical omission.

An in-scope declaration must be immediately preceded by one of:

    // numeric-bound: guard - prevents one wire frame from exhausting memory
    // numeric-bound: not-a-bound - fixed decimal representation maximum

``docs/style.md`` defines both kinds and owns the semantic question of which one
a given constant deserves. The kind and one-line rationale are mechanically
required here; review decides whether they are true. Deployment policy is not a
legal constant declaration: ``ceiling``, ``tunable``, and ``interval`` markers
are therefore rejected.

A mechanically derived bound may use the narrow escape

    // numeric-bound: derived guard from MAX_SOURCE_CHARACTERS

only when its initializer references the named bound by its bare name and that
name resolves to a direct declaration of the same kind. This keeps self-evident
byte/unit translations from repeating rationale while preventing an unexplained
independent cap from hiding behind the escape. A path-qualified reference such
as ``other_crate::MAX_BASE`` does not count: it names an item this scan cannot
see, whose classification may differ from the same-named local one, so the
escape is unavailable and the constant declares its own kind.

The source name resolves in the Rust scope that declares it: the innermost
brace-delimited block containing the derived constant — a module, a function
body, an ``impl``, any block at all — then outward to the file. A declaration in
a sibling module or another function is never in scope, so a derivation whose
initializer really reads an imported constant cannot be validated against an
unrelated same-named constant elsewhere in the file, and a nearer declaration
shadows a farther one as Rust resolves it. What this lexical scan cannot follow,
it refuses: a name reachable only through a ``use`` from outside the file leaves
the escape unproven and the declaration is rejected.

Every other boundary-named constant the initializer reads must resolve, in the
same scope, to a guard. A value assembled from a guard and a representation fact
does not inherit one rationale, and one assembled from a contributor this scan
cannot see is unproven rather than proven; either way the escape is unavailable
and the constant declares its own kind.

Because discovery is deliberately lexical, a fixed representation fact whose
name contains a boundary token may declare ``not-a-bound`` with a one-line
explanation. That escape is for facts such as a numeric type's exact maximum or
UTF-8's continuation width, not for a runtime cap; review owns that semantic
distinction and the marker keeps each use visible in the inventory.

Run from the repository root. ``--root`` exists only for checker self-tests.
"""

from __future__ import annotations

import argparse
import os
import re
import sys
from dataclasses import dataclass
from functools import lru_cache
from pathlib import Path

BOUNDARY_TOKENS = frozenset(
    {
        "BASELINE",
        "BOUND",
        "BUDGET",
        "CAP",
        "CAPACITY",
        "CEILING",
        "DEADLINE",
        "LIMIT",
        "MAX",
        "MAXIMUM",
        "MIN",
        "MINIMUM",
        "THRESHOLD",
        "TIMEOUT",
        "TTL",
    }
)
ENFORCED_ROOTS = (
    "apps/client/src",
    "apps/signalboxd/src",
    "crates/application/src",
    "crates/model-provider-runtime/src",
    "crates/model-runtime/src",
    "crates/persistence/src",
    "crates/process-protocol/src",
    "crates/tools-code-host/src",
)
# These exact candidates predate the commissioned 118-row classification and
# were not assigned a ruled tier. Keeping the baseline exact means a new
# boundary-named constant in either newly enforced root still fails closed,
# while the gate does not invent a semantic classification for omitted work.
PREEXISTING_UNCLASSIFIED_BOUNDS = frozenset(
    {
        ("apps/signalboxd/src/bin/approval-judge-eval.rs", "MAX_PAID_CALLS"),
        ("apps/signalboxd/src/blob_storage_configuration.rs", "MAX_S3_LOCATION_BYTES"),
        ("apps/signalboxd/src/configuration.rs", "MAX_REPOSITORY_WATCH_RULES"),
        ("apps/signalboxd/src/configuration.rs", "MAX_REPOSITORY_WATCH_ACTIONS"),
        ("apps/signalboxd/src/configuration.rs", "MAX_COMPACTION_PROMPT_UTF8_BYTES"),
        ("apps/signalboxd/src/configuration.rs", "DEFAULT_CONVERSATION_IMPORT_MAX_SOURCE_BYTES"),
        ("apps/signalboxd/src/configuration.rs", "MAX_WATCHED_REPOSITORIES"),
        ("apps/signalboxd/src/configuration.rs", "MAX_SIGNAL_REVIEWERS"),
        ("apps/signalboxd/src/credential_pools.rs", "MAX_HEADROOM_RESERVE_PERCENT"),
        ("apps/signalboxd/src/credential_pools.rs", "MAX_CREDENTIAL_DELIVERY_PATH_UTF8_BYTES"),
        ("apps/signalboxd/src/credential_pools.rs", "MAX_CREDENTIAL_CATALOG_NAME_UTF8_BYTES"),
        ("apps/signalboxd/src/credential_pools.rs", "MAX_CREDENTIAL_POOL_MEMBERS"),
        ("apps/signalboxd/src/credential_pools.rs", "MAX_CREDENTIAL_HOME_CONCURRENT_INVOCATIONS"),
        ("apps/signalboxd/src/daemon_tools.rs", "MAX_RETAINED_SESSION_WORKSPACES"),
        ("apps/signalboxd/src/lib.rs", "MAX_QUOTED_CONTEXT_BYTES"),
        ("apps/signalboxd/src/process_runtime/mod.rs", "PROCESS_UPDATE_CAPACITY"),
        ("apps/signalboxd/src/process_runtime/mod.rs", "MAX_ACTIVE_CONNECTIONS"),
        ("apps/signalboxd/src/process_runtime/mod.rs", "MAX_BUFFERED_INBOUND_FRAMES"),
        ("apps/signalboxd/src/process_runtime/mod.rs", "MAX_CONCURRENT_IMPORTS"),
        ("apps/signalboxd/src/process_runtime/mod.rs", "MAX_IMPORT_ADMISSION_WAITERS"),
        ("apps/signalboxd/src/process_runtime/mod.rs", "MAX_CONCURRENT_REVIEW_COMMANDS"),
        ("apps/signalboxd/src/process_runtime/mod.rs", "MAX_CONCURRENT_BLOB_READS"),
        ("apps/signalboxd/src/process_runtime/mod.rs", "BULK_INGEST_IDLE_TIMEOUT"),
        ("apps/signalboxd/src/process_runtime/mod.rs", "BULK_INGEST_SESSION_TIMEOUT"),
        ("apps/signalboxd/src/process_runtime/mod.rs", "BLOB_READ_TIMEOUT"),
        ("apps/signalboxd/src/process_runtime/mod.rs", "MAX_SUBMITTED_INPUT_BYTES"),
        ("apps/signalboxd/src/repo_watch_runtime.rs", "DEFAULT_REQUEST_TIMEOUT"),
        ("apps/signalboxd/src/repo_watch_runtime.rs", "MAX_RESULT_PAGES"),
        ("apps/signalboxd/src/repo_watch_runtime.rs", "MAX_RESPONSE_BYTES"),
        ("apps/signalboxd/src/repo_watch_runtime.rs", "MAX_CREDENTIAL_BYTES"),
        ("apps/signalboxd/src/repo_watch_runtime.rs", "MAX_ENTITY_TAG_BYTES"),
        ("apps/signalboxd/src/repo_watch_runtime.rs", "MAX_REQUESTS_PER_POLL"),
        ("apps/signalboxd/src/repo_watch_runtime.rs", "MAX_CACHED_RESOURCES"),
        ("apps/signalboxd/src/repo_watch_runtime.rs", "MAX_CONCURRENT_PULL_REQUEST_FETCHES"),
        ("apps/signalboxd/src/repo_watch_runtime.rs", "MAX_CONSECUTIVE_SKIPPED_PULL_REQUEST_POLLS"),
        ("apps/signalboxd/src/repo_watch_runtime.rs", "MAX_CHECK_SUITES_PER_COMMIT_CHECK_RUN_SEARCH"),
        ("apps/signalboxd/src/repo_watch_runtime.rs", "MAX_POLL_WIRE_BYTES"),
        ("apps/signalboxd/src/repo_watch_runtime.rs", "MAX_CACHED_WIRE_BYTES"),
        ("apps/signalboxd/src/repo_watch_runtime.rs", "WEBHOOK_DRAIN_RETRY_MAX_DELAY"),
        ("apps/signalboxd/src/repo_watch_runtime.rs", "WEBHOOK_DRAIN_RETRY_MAX_DOUBLINGS"),
        ("apps/signalboxd/src/repo_watch_runtime.rs", "WEBHOOK_DRAIN_STALL_THRESHOLD"),
        ("apps/signalboxd/src/repo_watch_runtime.rs", "WEBHOOK_DRAIN_ATTEMPT_TIMEOUT"),
        ("apps/signalboxd/src/repo_watch_runtime.rs", "WEBHOOK_ATTEMPT_TIMEOUT"),
        ("apps/signalboxd/src/repo_watch_runtime.rs", "WEBHOOK_CANCELLED_FETCH_DRAIN_TIMEOUT"),
        ("apps/signalboxd/src/repo_watch_runtime.rs", "WEBHOOK_DRAIN_MONITOR_QUERY_TIMEOUT"),
        ("apps/signalboxd/src/repo_watch_runtime.rs", "WEBHOOK_DRAIN_PAGE_LIMIT"),
        ("apps/signalboxd/src/repo_watch_runtime.rs", "MAX_WEBHOOK_TERMINAL_ATTEMPTS"),
        ("apps/signalboxd/src/repo_watch_webhook_runtime.rs", "MAX_EVENT_NAME_BYTES"),
        ("apps/signalboxd/src/repo_watch_webhook_runtime.rs", "MAX_ACTION_NAME_BYTES"),
        ("apps/signalboxd/src/repo_watch_webhook_runtime.rs", "MAX_WEBHOOK_SECRET_BYTES"),
        ("apps/signalboxd/src/repo_watch_webhook_runtime.rs", "MAX_WEBHOOK_BODY_BYTES"),
        ("apps/signalboxd/src/repo_watch_webhook_runtime.rs", "MAX_WEBHOOK_IN_FLIGHT"),
        ("apps/signalboxd/src/repo_watch_webhook_runtime.rs", "MAX_WEBHOOK_IN_FLIGHT_BYTES"),
        ("apps/signalboxd/src/repo_watch_webhook_runtime.rs", "MAX_WEBHOOK_DELIVERIES_PER_MINUTE"),
        ("apps/signalboxd/src/repo_watch_webhook_runtime.rs", "MAX_WEBHOOK_CONNECTIONS"),
        ("apps/signalboxd/src/repo_watch_webhook_runtime.rs", "MAX_WEBHOOK_HEADER_BYTES"),
        ("apps/signalboxd/src/repo_watch_webhook_runtime.rs", "MAX_WEBHOOK_HEADER_COUNT"),
        ("apps/signalboxd/src/repo_watch_webhook_runtime.rs", "MAX_WEBHOOK_HEAD_BUFFER_BYTES"),
        ("apps/signalboxd/src/repo_watch_webhook_runtime.rs", "WEBHOOK_CONNECTION_READ_TIMEOUT"),
        ("apps/signalboxd/src/repo_watch_webhook_runtime.rs", "WEBHOOK_BODY_READ_TIMEOUT"),
        ("apps/signalboxd/src/repo_watch_webhook_runtime.rs", "WEBHOOK_BODY_BUDGET_GRANULE_BYTES"),
        ("apps/signalboxd/src/repo_watch_webhook_runtime.rs", "WEBHOOK_BODY_BUDGET_GRANULES"),
        ("apps/signalboxd/src/runner_protocol_runtime.rs", "HANDSHAKE_TIMEOUT"),
        ("apps/signalboxd/src/runner_protocol_runtime.rs", "CONNECTION_DRAIN_TIMEOUT"),
        ("apps/signalboxd/src/runner_protocol_runtime.rs", "MAXIMUM_CONCURRENT_CONNECTIONS"),
        ("apps/signalboxd/src/single_hub.rs", "GUARD_CHECK_TIMEOUT"),
        ("apps/signalboxd/src/telemetry.rs", "OTLP_MAX_QUEUED_SPANS"),
        ("apps/signalboxd/src/telemetry.rs", "OTLP_MAX_EXPORT_BATCH"),
        ("apps/signalboxd/src/telemetry.rs", "OTLP_EXPORT_TIMEOUT"),
        ("apps/signalboxd/src/telemetry.rs", "OTLP_SHUTDOWN_TIMEOUT"),
        ("apps/signalboxd/src/telemetry.rs", "MAX_ENDPOINT_BYTES"),
        ("apps/signalboxd/src/telemetry.rs", "MAX_HEADER_FILE_BYTES"),
        ("apps/signalboxd/src/telemetry.rs", "MAX_HEADER_COUNT"),
        ("apps/signalboxd/src/telemetry.rs", "MAX_HEADER_NAME_BYTES"),
        ("apps/signalboxd/src/telemetry.rs", "MAX_HEADER_VALUE_BYTES"),
        ("apps/signalboxd/src/telemetry.rs", "MAX_SCRAPE_REQUEST_BYTES"),
        ("apps/signalboxd/src/telemetry.rs", "MAX_SCRAPE_CONNECTIONS"),
        ("apps/signalboxd/src/telemetry.rs", "SCRAPE_CONNECTION_TIMEOUT"),
        ("crates/persistence/src/conversation_import_codec.rs", "MAX_CONTAINER_DEPTH"),
        ("crates/persistence/src/hub_fence.rs", "FENCED_POOL_MAX_CONNECTIONS"),
        ("crates/persistence/src/model_execution.rs", "MAX_AVAILABILITY_BACKOFF"),
        ("crates/persistence/src/model_execution.rs", "MAX_EXPONENTIAL_BACKOFF"),
        ("crates/persistence/src/repo_watch.rs", "MAX_EVENT_PAGE_SIZE"),
        ("crates/persistence/src/repo_watch_dispatch_obligation.rs", "DISPATCH_RETRY_BACKOFF_CAP"),
        ("crates/persistence/src/repo_watch_dispatch_obligation.rs", "DISPATCH_RETRY_MAX_DOUBLINGS"),
        ("crates/persistence/src/repo_watch_dispatch_obligation.rs", "MAX_PARK_RELEASE_ACTOR_CHARS"),
        ("crates/persistence/src/repo_watch_webhook.rs", "MAX_PENDING_PAGE_SIZE"),
        ("crates/persistence/src/repo_watch_webhook.rs", "MAX_PENDING_PAGE_BYTES"),
        ("crates/persistence/src/repo_watch_webhook.rs", "MAX_WEBHOOK_NAME_BYTES"),
        ("crates/persistence/src/repo_watch_webhook.rs", "MAX_OUTCOME_CODE_BYTES"),
        ("crates/persistence/src/runner_protocol.rs", "PAGE_LIMIT"),
    }
)
# The optional leading qualifier makes `std::time::Duration` and bare
# `Duration` one declaration to this scan, and the signed `NonZeroI*` family is
# listed beside the unsigned one: a spelling this pattern misses is an
# undeclared bound the gate silently accepts.
INTEGER_TYPE = r"[ui](?:8|16|32|64|128|size)"
TYPE_QUALIFIER = r"(?:[A-Za-z_][A-Za-z0-9_]*\s*::\s*)*"
NUMERIC_TYPE_NAMES = (
    rf"{INTEGER_TYPE}|f(?:32|64)|Duration|"
    r"NonZero(?:U8|U16|U32|U64|U128|Usize|I8|I16|I32|I64|I128|Isize)|"
    rf"NonZero\s*<\s*{TYPE_QUALIFIER}{INTEGER_TYPE}\s*>"
)
ALIAS_DECLARATION = re.compile(
    r"(?m)^[ \t]*(?:pub(?:\([^)]*\))?\s+)?type\s+"
    r"(?P<name>[A-Za-z_][A-Za-z0-9_]*)\s*=\s*(?P<target>[^;]+);"
)


@lru_cache(maxsize=None)
def numeric_type_pattern(aliases: frozenset[str]) -> str:
    """Build the type expression a numeric constant declaration must match."""
    names = "|".join(sorted(re.escape(alias) for alias in aliases))
    inner = f"{NUMERIC_TYPE_NAMES}|{names}" if aliases else NUMERIC_TYPE_NAMES
    return rf"{TYPE_QUALIFIER}(?:{inner})"


@lru_cache(maxsize=None)
def constant_pattern(aliases: frozenset[str]) -> re.Pattern[str]:
    return re.compile(
        rf"(?m)^(?P<indent>[ \t]*)(?:pub(?:\([^)]*\))?\s+)?const\s+"
        rf"(?P<name>[A-Z][A-Z0-9_]*)\s*:\s*"
        rf"(?P<type>{numeric_type_pattern(aliases)})\s*(?P<form>[=;])"
    )


def numeric_aliases(code: str) -> frozenset[str]:
    """Report the file's `type Name = <numeric>;` aliases, following chains.

    A local alias is a silent bypass otherwise: `type ByteCount = usize;` makes
    a bound's declared type unrecognizable to a closed pattern. Chains settle by
    repetition. Two modules may declare one alias name differently, so every
    target is kept and any numeric one admits the name — the direction that
    inventories a bound rather than losing it. An alias imported from another
    crate stays unresolved, which is a stated limit rather than a claim.
    """
    declared: dict[str, set[str]] = {}
    for match in ALIAS_DECLARATION.finditer(code):
        declared.setdefault(match.group("name"), set()).add(match.group("target").strip())
    aliases: frozenset[str] = frozenset()
    while True:
        recognized = re.compile(rf"^{numeric_type_pattern(aliases)}$")
        found = frozenset(
            name
            for name, targets in declared.items()
            if any(recognized.match(target) for target in targets)
        )
        if found <= aliases:
            return aliases
        aliases |= found
USE_STATEMENT = re.compile(r"(?m)^[ \t]*(?:pub(?:\([^)]*\))?\s+)?use\s[^;]*;")
# Only a single-segment `self::`/`super::` import re-binds a name this scan
# already resolves through enclosing scope. `use super::sibling::MAX;` and
# `use super::MAX as OTHER;` both name something else and are recorded.
LOCAL_USE = re.compile(
    r"^[ \t]*(?:pub(?:\([^)]*\))?\s+)?use\s+(?:self|super)\s*::\s*"
    r"[A-Za-z_][A-Za-z0-9_]*\s*;"
)
# Unlike a reference in an initializer, an imported name is looked for through
# its path, so this deliberately matches the qualified spelling too.
IMPORTED_NAME = re.compile(r"\b(?P<name>[A-Z][A-Z0-9_]*)\b")
DIRECT_DECLARATION = re.compile(
    r"^\s*// numeric-bound: (?P<kind>guard|not-a-bound) - "
    r"(?P<rationale>\S.*)$"
)
DERIVED_DECLARATION = re.compile(
    r"^\s*// numeric-bound: derived (?P<kind>guard) from "
    r"(?P<source>[A-Z][A-Z0-9_]*)\s*$"
)
DECLARATION_SITE = re.compile(r"\bconst\s+$")
REFERENCED_BOUND = re.compile(
    r"(?<![\w:])(?P<qualifier>(?:[A-Za-z_][A-Za-z0-9_]*\s*::\s*)*)(?P<name>[A-Z][A-Z0-9_]*)\b"
)
INLINE_MODULE = re.compile(r"\bmod\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)\s*\{")
DECLARED_MODULE = re.compile(
    r"(?P<attributes>(?:#\s*\[[^\]]*\]\s*)*)"
    r"(?:pub(?:\([^)]*\))?\s+)?\bmod\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)\s*(?P<form>[{;])",
    re.MULTILINE,
)
TEST_GATED_ITEM = re.compile(
    r"(?P<attributes>(?:#\s*\[[^\]]*\]\s*)+)(?:[^;{}]*?)\{", re.MULTILINE
)
CFG_ATTRIBUTE = re.compile(r"#\s*\[\s*cfg\s*\((?P<predicate>[^\]]*)\)\s*\]")
TEST_ATTRIBUTE = re.compile(r"#\s*\[\s*test\s*\]")
# An attribute run ending exactly where the item begins. Matched against the
# blanked source so it spans rustfmt-wrapped attributes without a line walk.
TRAILING_ATTRIBUTES = re.compile(r"(?:#\s*\[[^\]]*\]\s*)*$")
PATH_ATTRIBUTE = re.compile(r"#\s*\[\s*path\s*=\s*\"(?P<path>[^\"]+)\"\s*\]")
CRATE_ROOTS = frozenset({"lib.rs", "main.rs"})


@dataclass(frozen=True)
class Bound:
    path: Path
    name: str
    line: int
    offset: int
    reads_at: int
    scope: tuple[int, int] | None
    initializer: str
    annotation: str
    test_only: bool


@dataclass(frozen=True)
class Import:
    """One name a `use` declaration binds, and the block it binds it in."""

    path: Path
    name: str
    scope: tuple[int, int] | None


def blank_non_code(text: str) -> str:
    """Blank Rust comments and strings while preserving offsets and newlines."""
    code = list(text)
    index = 0
    length = len(text)
    while index < length:
        if text[index : index + 2] == "//":
            end = text.find("\n", index)
            end = length if end == -1 else end
            for position in range(index, end):
                code[position] = " "
            index = end
            continue
        if text[index : index + 2] == "/*":
            depth = 1
            cursor = index + 2
            while cursor < length and depth:
                if text[cursor : cursor + 2] == "/*":
                    depth += 1
                    cursor += 2
                elif text[cursor : cursor + 2] == "*/":
                    depth -= 1
                    cursor += 2
                else:
                    cursor += 1
            for position in range(index, cursor):
                if code[position] != "\n":
                    code[position] = " "
            index = cursor
            continue
        if text[index] == "r" and text[index + 1 : index + 2] in ('"', "#"):
            cursor = index + 1
            hashes = 0
            while cursor < length and text[cursor] == "#":
                hashes += 1
                cursor += 1
            if cursor < length and text[cursor] == '"':
                terminator = '"' + "#" * hashes
                end = text.find(terminator, cursor + 1)
                end = length if end == -1 else end + len(terminator)
                for position in range(index, end):
                    if code[position] != "\n":
                        code[position] = " "
                index = end
                continue
        if text[index] == '"':
            cursor = index + 1
            while cursor < length:
                if text[cursor] == "\\":
                    cursor += 2
                    continue
                if text[cursor] == '"':
                    cursor += 1
                    break
                cursor += 1
            for position in range(index, cursor):
                if code[position] != "\n":
                    code[position] = " "
            index = cursor
            continue
        if text[index] == "'" and text[index + 2 : index + 3] == "'":
            code[index + 1] = " "
            index += 3
            continue
        index += 1
    return "".join(code)


def matching_brace(code: str, opening: int) -> int:
    depth = 0
    for position in range(opening, len(code)):
        if code[position] == "{":
            depth += 1
        elif code[position] == "}":
            depth -= 1
            if depth == 0:
                return position
    return len(code)


def split_configurations(predicate: str) -> list[str]:
    """Split one `cfg` predicate list on its top-level commas."""
    members = []
    depth = 0
    start = 0
    for position, character in enumerate(predicate):
        if character == "(":
            depth += 1
        elif character == ")":
            depth -= 1
        elif character == "," and depth == 0:
            members.append(predicate[start:position])
            start = position + 1
    members.append(predicate[start:])
    return members


def configuration_requires_test(predicate: str) -> bool:
    """Report whether a `cfg` predicate holds only when `test` is set.

    Bare `test` does, and so does an `all(...)` with a member that does, which
    covers `all(test, not(windows))`. `any(...)` does not, because the module
    also builds when another member holds, and `not(...)` never does. The
    predicate is read structurally rather than evaluated, so an unrecognised
    form counts as buildable without `test` and leaves its constants gating —
    the safe direction for a blocking check.
    """
    predicate = predicate.strip()
    if predicate == "test":
        return True
    if predicate.startswith("all(") and predicate.endswith(")"):
        return any(
            configuration_requires_test(member)
            for member in split_configurations(predicate[len("all(") : -1])
        )
    return False


def requires_test(attributes: str) -> bool:
    """Report whether an attribute run compiles only in a test build.

    A bare `#[test]` counts alongside `cfg(test)`: rustc excludes the function
    it marks from an ordinary build just as firmly.
    """
    if TEST_ATTRIBUTE.search(attributes) is not None:
        return True
    return any(
        configuration_requires_test(match.group("predicate"))
        for match in CFG_ATTRIBUTE.finditer(attributes)
    )


def test_ranges(code: str) -> list[tuple[int, int]]:
    """Report the span of every braced item that compiles only under test.

    A module is the common case, but `#[cfg(test)] fn fixture() { ... }` and a
    gated `impl` exclude their contents from a production build just as firmly,
    so the pattern is the attribute run and whatever item head follows it.
    """
    ranges = []
    for match in TEST_GATED_ITEM.finditer(code):
        if not requires_test(match.group("attributes")):
            continue
        ranges.append((match.start(), matching_brace(code, match.end() - 1)))
    return ranges


def block_ranges(code: str) -> list[tuple[int, int]]:
    """Report the span of every brace-delimited block in one file.

    A Rust `const` is visible in the block that declares it and in the blocks
    nested inside it, so every brace pair is a scope boundary — a module, a
    function body, an `impl`, a bare block. Unbalanced braces cannot occur in
    code that compiles, and a stray close is dropped rather than trusted.
    """
    ranges = []
    opened: list[int] = []
    for position, character in enumerate(code):
        if character == "{":
            opened.append(position)
        elif character == "}" and opened:
            ranges.append((opened.pop(), position))
    return ranges


def innermost_scope(offset: int, ranges: list[tuple[int, int]]) -> tuple[int, int] | None:
    """Report the narrowest block containing ``offset``, or the file."""
    enclosing = [span for span in ranges if span[0] <= offset <= span[1]]
    return max(enclosing, default=None)


def inline_module_ranges(code: str) -> list[tuple[int, int, str]]:
    """Report the span and name of every inline `mod name { ... }` block."""
    ranges = []
    for match in INLINE_MODULE.finditer(code):
        ranges.append(
            (match.start(), matching_brace(code, match.end() - 1), match.group("name"))
        )
    return ranges


def enclosing_module_names(offset: int, ranges: list[tuple[int, int, str]]) -> list[str]:
    """Name the inline modules containing ``offset``, outermost first."""
    return [name for start, end, name in sorted(ranges) if start <= offset <= end]


def declared_modules(
    path: Path, text: str, code: str, directory: Path
) -> list[tuple[Path, Path, bool]]:
    """Report `(file, its own child directory, test-gated)` per `mod name;`.

    ``directory`` is where this file's top-level declarations resolve. An
    inline `mod name { ... }` moves the declarations inside it into
    ``directory/name``; a `#[path]` names its file relative to the declaring
    file's own directory and then resolves that file's children beside it,
    rather than beneath its stem. A declaration standing inside a `#[cfg(test)]`
    inline module is gated by that module even with no attribute of its own.
    Attributes are matched against ``code`` so a
    commented-out declaration cannot claim a file, then re-read from ``text``
    because blanking has emptied the ``#[path]`` string literal.
    """
    inline = inline_module_ranges(code)
    enclosing_test_modules = test_ranges(code)
    found = []
    for match in DECLARED_MODULE.finditer(code):
        if match.group("form") != ";":
            continue
        gated = requires_test(match.group("attributes")) or in_ranges(
            match.start(), enclosing_test_modules
        )
        attributes = text[match.start("attributes") : match.end("attributes")]
        explicit = PATH_ATTRIBUTE.search(attributes)
        segments = enclosing_module_names(match.start(), inline)
        if explicit is not None:
            # A `#[path]` outside any inline module is relative to the declaring
            # file's own directory; inside one it picks up the enclosing module
            # names, which for a non-mod-rs file already include its stem.
            attributed = directory.joinpath(*segments) if segments else path.parent
            file = Path(os.path.normpath(attributed / explicit.group("path")))
            found.append((file, file.parent, gated))
            continue
        base = directory.joinpath(*segments)
        name = match.group("name")
        found.append((base / f"{name}.rs", base / name, gated))
        found.append((base / name / "mod.rs", base / name, gated))
    return found


def is_crate_root(path: Path) -> bool:
    """Report whether Cargo compiles ``path`` as a target root.

    That is `<package>/src/lib.rs` and `src/main.rs`, plus the automatic binary
    roots `src/bin/tool.rs` and `src/bin/tool/main.rs`. Matching the basename
    alone would seed a `tests/main.rs` deep inside a test tree as its own
    ungated root and pull that whole subtree back out of test-only.
    """
    parent = path.parent
    if path.name in CRATE_ROOTS and parent.name == "src":
        return True
    if path.suffix == ".rs" and parent.name == "bin" and parent.parent.name == "src":
        return True
    return (
        path.name == "main.rs"
        and parent.parent.name == "bin"
        and parent.parent.parent.name == "src"
    )


def test_only_sources(sources: dict[Path, str], blanked: dict[Path, str]) -> set[Path]:
    """Report every source Rust reaches only through a `#[cfg(test)]` module.

    The walk starts at each crate root and carries the directory a file's own
    declarations resolve in, because that directory depends on how the file was
    reached and cannot be recovered from its name. A file reachable both inside
    and outside a test module is production, since a production build compiles
    it.
    """
    reached: dict[bool, set[Path]] = {True: set(), False: set()}
    frontier = [(path, path.parent, False) for path in sources if is_crate_root(path)]
    while frontier:
        path, directory, gated = frontier.pop()
        if path not in sources or path in reached[gated]:
            continue
        reached[gated].add(path)
        for file, child_directory, child_gated in declared_modules(
            path, sources[path], blanked[path], directory
        ):
            frontier.append((file, child_directory, gated or child_gated))
    return reached[True] - reached[False]


def in_ranges(position: int, ranges: list[tuple[int, int]]) -> bool:
    return any(start <= position <= end for start, end in ranges)


def initializer_end(code: str, start: int) -> int:
    delimiter_depth = 0
    for position in range(start, len(code)):
        char = code[position]
        if char in "([{":
            delimiter_depth += 1
        elif char in ")]}":
            delimiter_depth -= 1
        elif char == ";" and delimiter_depth == 0:
            return position
    return len(code)


def preceding_attributes(code: str, offset: int) -> str:
    """Report the attribute run standing immediately before ``offset``.

    Taken as one span of the blanked source rather than whole lines, so a
    rustfmt-wrapped `#[cfg(all(\n    test,\n    unix,\n))]` is read entire. A
    commented-out attribute was blanked to spaces and cannot match. This is what
    makes an item-level `#[cfg(test)] const ...` test-only with no module
    around it.
    """
    return TRAILING_ATTRIBUTES.search(code, 0, offset).group()


def imported_names(path: Path, code: str, blocks: list[tuple[int, int]]) -> list[Import]:
    """Report the shouting-case names the file's `use` declarations bind.

    A `use` that binds the name a derived declaration reads shadows any
    declaration further out, and this scan cannot follow it out of the file, so
    recording where each import is in scope is what lets the owner resolution
    refuse rather than reach past it. A `self::` or `super::` path is skipped:
    it re-binds an item from an enclosing module, which within one file is the
    declaration resolution would have reached anyway — unless it renames, since
    `use super::MAX_BASE as MAX_IMPORTED` binds a name no declaration here
    carries. Glob imports bind nothing nameable, and a `super::super::` path
    that climbs out of the file is treated as local; both are stated limits.
    """
    imports = []
    for statement in USE_STATEMENT.finditer(code):
        if LOCAL_USE.match(statement.group()) is not None:
            continue
        for match in IMPORTED_NAME.finditer(statement.group()):
            imports.append(
                Import(
                    path=path,
                    name=match.group("name"),
                    scope=innermost_scope(statement.start(), blocks),
                )
            )
    return imports


def is_boundary_name(name: str) -> bool:
    return bool(BOUNDARY_TOKENS.intersection(name.split("_")))


def source_files(root: Path) -> list[Path]:
    files = []
    for top_level in (root / "crates", root / "apps"):
        if top_level.exists():
            files.extend(top_level.glob("*/src/**/*.rs"))
    return sorted(path for path in files if path.is_file())


def inventory(root: Path) -> tuple[list[Bound], list[Import]]:
    sources = {path: path.read_text(encoding="utf-8") for path in source_files(root)}
    blanked = {path: blank_non_code(text) for path, text in sources.items()}
    test_sources = test_only_sources(sources, blanked)
    bounds = []
    imports = []
    for path, text in sources.items():
        code = blanked[path]
        # Scanning brace pairs costs a pass over the file, so it is paid only
        # where a boundary-named constant actually needs a scope.
        matches = [
            match
            for match in constant_pattern(numeric_aliases(code)).finditer(code)
            if is_boundary_name(match.group("name"))
        ]
        if not matches:
            continue
        ranges = test_ranges(code)
        blocks = block_ranges(code)
        external = path in test_sources
        lines = text.splitlines()
        relative = path.relative_to(root)
        imports.extend(imported_names(relative, code, blocks))
        for match in matches:
            name = match.group("name")
            line = text.count("\n", 0, match.start()) + 1
            declared = match.group("form") == "="
            end = initializer_end(code, match.end()) if declared else match.end()
            annotation = lines[line - 2] if line > 1 else ""
            bounds.append(
                Bound(
                    path=relative,
                    name=name,
                    line=line,
                    offset=match.start(),
                    reads_at=match.end(),
                    scope=innermost_scope(match.start(), blocks),
                    initializer=code[match.end() : end],
                    annotation=annotation,
                    test_only=external
                    or in_ranges(match.start(), ranges)
                    or requires_test(preceding_attributes(code, match.start())),
                )
            )
    return bounds, imports


def is_enforced(bound: Bound) -> bool:
    path = bound.path.as_posix()
    return (
        not bound.test_only
        and (path, bound.name) not in PREEXISTING_UNCLASSIFIED_BOUNDS
        and any(
            path == root or path.startswith(f"{root}/") for root in ENFORCED_ROOTS
        )
    )


def declaration(bound: Bound) -> tuple[str, str, str | None] | None:
    direct = DIRECT_DECLARATION.fullmatch(bound.annotation)
    if direct is not None:
        return direct.group("kind"), direct.group("rationale"), None
    derived = DERIVED_DECLARATION.fullmatch(bound.annotation)
    if derived is not None:
        return derived.group("kind"), "", derived.group("source")
    return None


def validate(bounds: list[Bound], imports: list[Import]) -> list[str]:
    failures = []
    enforced = [bound for bound in bounds if is_enforced(bound)]
    declarations: dict[tuple[Path, str], list[Bound]] = {}
    for bound in enforced:
        declarations.setdefault((bound.path, bound.name), []).append(bound)
    bindings: dict[tuple[Path, str], list[Import]] = {}
    for entry in imports:
        bindings.setdefault((entry.path, entry.name), []).append(entry)

    def depth(candidate: Bound | Import) -> int:
        return -1 if candidate.scope is None else candidate.scope[0]

    def in_scope(candidate: Bound | Import, reference: int) -> bool:
        """Report whether ``candidate`` is visible at the offset ``reference``.

        Visibility is judged where the name is read, not where the reading item
        is declared. That is what lets an initializer block declare its own
        source — `const TOTAL: usize = { const BASE: usize = 1; BASE * 4 };` —
        while a sibling block inside the same initializer, which does not
        contain the reference, supplies nothing.
        """
        return candidate.scope is None or (
            candidate.scope[0] <= reference <= candidate.scope[1]
        )

    def visible_owner(bound: Bound, source: str, reference: int) -> Bound | None:
        """Resolve ``source`` as the Rust scope at ``reference`` would.

        A file-level declaration is visible everywhere in its file, and one
        inside a block only within that block, so a sibling module or another
        function never supplies an owner. The nearest enclosing declaration
        shadows the rest; two at the same depth would not compile, and resolve
        to neither here.

        A `use` binding the same name from at least as near a block wins over
        the declaration in Rust, and names an item outside this file, so it
        leaves the owner unproven rather than letting resolution reach past it.
        """
        visible = [
            candidate
            for candidate in declarations.get((bound.path, source), ())
            if in_scope(candidate, reference)
        ]
        if not visible:
            return None
        nearest = [
            candidate for candidate in visible if depth(candidate) == max(map(depth, visible))
        ]
        if len(nearest) != 1:
            return None
        owner = nearest[0]
        shadowed = any(
            in_scope(binding, reference) and depth(binding) >= depth(owner)
            for binding in bindings.get((bound.path, source), ())
        )
        return None if shadowed else owner

    def resolves_to_direct(bound: Bound, kind: str, seen: set[str]) -> bool:
        parsed = declaration(bound)
        if parsed is None or parsed[0] != kind or kind == "not-a-bound":
            return False
        source = parsed[2]
        if source is None:
            return bool(parsed[1].strip())
        bare = rf"(?<![\w:]){re.escape(source)}\b"
        # Every occurrence is resolved, not just the first: an initializer may
        # both declare the source in a nested block and read it outside that
        # block, where the name means something else entirely.
        reads = [
            read
            for read in re.finditer(bare, bound.initializer)
            if DECLARATION_SITE.search(bound.initializer[: read.start()]) is None
        ]
        if source in seen or not reads:
            return False
        owners = [
            visible_owner(bound, source, bound.reads_at + read.start()) for read in reads
        ]
        if any(owner is None for owner in owners):
            return False
        if not all(resolves_to_direct(owner, kind, seen | {source}) for owner in owners):
            return False
        return all(
            contributor is not None
            and resolves_to_direct(contributor, kind, seen | {source, name})
            for name, contributor in contributors(bound, source)
        )

    def contributors(bound: Bound, source: str) -> list[tuple[str, Bound | None]]:
        """Report the other boundary-named constants the initializer reads.

        A derivation inherits one rationale, so every bound feeding the value
        has to carry the kind being inherited — and one this scan cannot
        resolve is unproven rather than harmless, so it is reported with no
        owner and fails the escape. A path-qualified name is never resolved for
        the same reason it is never accepted as the source: it denotes an item
        this scan cannot see. Only boundary-named identifiers count; the
        inventory would not track any other constant as a bound either.
        """
        found = []
        for match in REFERENCED_BOUND.finditer(bound.initializer):
            name = match.group("name")
            qualified = bool(match.group("qualifier"))
            # Only the bare spelling is the source already accounted for; a
            # qualified repetition of the same name is a different item.
            if not qualified and name in {source, bound.name}:
                continue
            if not is_boundary_name(name):
                continue
            reference = bound.reads_at + match.start("name")
            found.append(
                (name, None if qualified else visible_owner(bound, name, reference))
            )
        return found

    for bound in enforced:
        parsed = declaration(bound)
        location = f"{bound.path}:{bound.line}: {bound.name}"
        if parsed is None:
            failures.append(f"{location} has no numeric-bound declaration")
            continue
        kind, rationale, source = parsed
        if source is None and not rationale.strip():
            failures.append(f"{location} has an empty rationale")
        elif source is not None and not resolves_to_direct(bound, kind, {bound.name}):
            failures.append(
                f"{location} has an invalid derived declaration from {source}"
            )
    return failures


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, default=Path("."))
    arguments = parser.parse_args(argv)
    bounds, imports = inventory(arguments.root.resolve())
    failures = validate(bounds, imports)
    if failures:
        print("numeric-bound check FAILED:")
        for failure in failures:
            print(f"  - {failure}")
        return 1
    enforced = sum(is_enforced(bound) for bound in bounds)
    test_only = sum(bound.test_only for bound in bounds)
    outside = len(bounds) - enforced - test_only
    print(
        "numeric-bound check passed: "
        f"{enforced} enforced, {outside} outside blocking scope, "
        f"{test_only} test-only"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())

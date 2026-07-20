#!/usr/bin/env python3
"""Reject SQL strings that lock the session table with plain FOR UPDATE."""

from __future__ import annotations

import argparse
import re
import sys
import tempfile
from pathlib import Path

TOKEN = re.compile(r"[A-Za-z_][A-Za-z0-9_]*|[(),;.]")
QUERY_CALL = re.compile(
    r"sqlx::(query_file(?:_as|_scalar)?|query(?:_as|_scalar)?)(!)?"
)
RELATION_BOUNDARIES = {
    "for",
    "group",
    "having",
    "join",
    "limit",
    "on",
    "order",
    "returning",
    "union",
    "where",
}


def rust_strings(source: str) -> list[str]:
    """Extract Rust string contents while ignoring comments and character literals."""
    strings: list[str] = []
    index = 0
    while index < len(source):
        if source.startswith("//", index):
            index = source.find("\n", index + 2)
            if index < 0:
                break
            continue
        if source.startswith("/*", index):
            depth = 1
            index += 2
            while index < len(source) and depth:
                if source.startswith("/*", index):
                    depth += 1
                    index += 2
                elif source.startswith("*/", index):
                    depth -= 1
                    index += 2
                else:
                    index += 1
            continue
        character = re.match(r"'(?:\\.|[^'\\\n])'", source[index:])
        if character:
            index += len(character.group(0))
            continue
        raw = re.match(r'r(#+)?"', source[index:])
        if raw:
            hashes = raw.group(1) or ""
            start = index + len(raw.group(0))
            terminator = '"' + hashes
            end = source.find(terminator, start)
            if end < 0:
                break
            strings.append(source[start:end])
            index = end + len(terminator)
            continue
        if source[index] == '"':
            index += 1
            value: list[str] = []
            while index < len(source):
                if source[index] == "\\" and index + 1 < len(source):
                    value.extend(source[index : index + 2])
                    index += 2
                elif source[index] == '"':
                    index += 1
                    strings.append("".join(value))
                    break
                else:
                    value.append(source[index])
                    index += 1
            continue
        index += 1
    return strings


def static_strings(expression: str) -> list[str] | None:
    """Resolve a Rust string literal or a static concat! expression."""
    expression = expression.strip()
    strings = rust_strings(expression)
    if not strings:
        return None
    if expression.startswith("concat!"):
        return strings
    raw = re.fullmatch(r'r(#+)?".*"\1', expression, re.DOTALL)
    normal = re.fullmatch(r'"(?:\\.|[^"\\])*"', expression, re.DOTALL)
    return strings if raw or normal else None


def query_strings(source: str) -> list[str]:
    """Return literal SQL candidates, including statically concatenated literals."""
    candidates = rust_strings(source)
    search_from = 0
    while (macro := source.find("concat!(", search_from)) >= 0:
        start = macro + len("concat!(")
        depth = 1
        index = start
        while index < len(source) and depth:
            if source.startswith("//", index):
                newline = source.find("\n", index + 2)
                index = len(source) if newline < 0 else newline
                continue
            if source.startswith("/*", index):
                end = source.find("*/", index + 2)
                index = len(source) if end < 0 else end + 2
                continue
            raw = re.match(r'r(#+)?"', source[index:])
            if raw:
                terminator = '"' + (raw.group(1) or "")
                end = source.find(terminator, index + len(raw.group(0)))
                index = len(source) if end < 0 else end + len(terminator)
                continue
            if source[index] == '"':
                index += 1
                while index < len(source):
                    if source[index] == "\\":
                        index += 2
                    elif source[index] == '"':
                        index += 1
                        break
                    else:
                        index += 1
                continue
            if source[index] == "(":
                depth += 1
            elif source[index] == ")":
                depth -= 1
            index += 1
        body = source[start : index - 1] if depth == 0 else ""
        fragments = rust_strings(body)
        if fragments:
            candidates.append("".join(fragments))
        search_from = max(index, start)
    return candidates


def query_inputs(source: str, crate_root: Path) -> tuple[list[str], list[str]]:
    """Resolve SQLx query inputs, reporting expressions that cannot be inspected."""
    candidates: list[str] = []
    errors: list[str] = []
    for match in QUERY_CALL.finditer(source):
        cursor = match.end()
        angle_depth = 0
        while cursor < len(source):
            character = source[cursor]
            if character == "<":
                angle_depth += 1
            elif character == ">" and angle_depth:
                angle_depth -= 1
            elif character == "(" and angle_depth == 0:
                break
            cursor += 1
        if cursor == len(source):
            errors.append(f"{match.group(0)} has no inspectable argument list")
            continue

        start = cursor + 1
        index = start
        depths = {"(": 0, "[": 0, "{": 0}
        pairs = {")": "(", "]": "[", "}": "{"}
        while index < len(source):
            if source[index] in {'"', "'"} or re.match(r'r(#+)?"', source[index:]):
                before = index
                strings = rust_strings(source[index:])
                if not strings:
                    index = len(source)
                    break
                raw = re.match(r'r(#+)?"', source[index:])
                if raw:
                    marker = '"' + (raw.group(1) or "")
                    end = source.find(marker, index + len(raw.group(0)))
                    index = len(source) if end < 0 else end + len(marker)
                elif source[index] == '"':
                    index += 1
                    while index < len(source):
                        if source[index] == "\\":
                            index += 2
                        elif source[index] == '"':
                            index += 1
                            break
                        else:
                            index += 1
                else:
                    character = re.match(r"'(?:\\.|[^'\\\n])'", source[index:])
                    index += len(character.group(0)) if character else 1
                if index == before:
                    index += 1
                continue
            character = source[index]
            if character in depths:
                depths[character] += 1
            elif character in pairs:
                opener = pairs[character]
                if character == ")" and not any(depths.values()):
                    break
                if depths[opener]:
                    depths[opener] -= 1
            elif character == "," and not any(depths.values()):
                break
            index += 1

        expression = source[start:index].strip()
        fragments = static_strings(expression)
        if fragments is None:
            errors.append(f"{match.group(0)} uses dynamic SQL: {expression[:80]}")
            continue
        if match.group(1).startswith("query_file"):
            query_path = crate_root / fragments[0]
            try:
                candidates.append(query_path.read_text())
            except OSError as error:
                errors.append(f"cannot inspect {query_path}: {error}")
        else:
            candidates.append("".join(fragments))
    return candidates, errors


def sanitize_sql(sql: str) -> str:
    """Blank SQL comments and literal contents without changing token separation."""
    output = list(sql)
    index = 0
    while index < len(sql):
        if sql.startswith("--", index):
            end = sql.find("\n", index + 2)
            end = len(sql) if end < 0 else end
        elif sql.startswith("/*", index):
            depth = 1
            end = index + 2
            while end < len(sql) and depth:
                if sql.startswith("/*", end):
                    depth += 1
                    end += 2
                elif sql.startswith("*/", end):
                    depth -= 1
                    end += 2
                else:
                    end += 1
        elif sql[index] == "'":
            end = index + 1
            while end < len(sql):
                if sql.startswith("''", end):
                    end += 2
                elif sql[end] == "'":
                    end += 1
                    break
                else:
                    end += 1
        elif sql[index] == "$" and (tag := re.match(r"\$[A-Za-z_0-9]*\$", sql[index:])):
            marker = tag.group(0)
            end = sql.find(marker, index + len(marker))
            end = len(sql) if end < 0 else end + len(marker)
        elif sql[index] == '"':
            end = index + 1
            identifier: list[str] = []
            while end < len(sql):
                if sql.startswith('""', end):
                    identifier.append('"')
                    end += 2
                elif sql[end] == '"':
                    end += 1
                    break
                else:
                    identifier.append(sql[end])
                    end += 1
            replacement = "".join(identifier)
            output[index:end] = list(replacement.ljust(end - index))
            index = end
            continue
        else:
            index += 1
            continue
        output[index:end] = " " * (end - index)
        index = end
    return "".join(output)


def locks_session_with_plain_update(sql: str) -> bool:
    """Whether one SQL string applies plain FOR UPDATE to a session relation."""
    tokens = [match.group(0).lower() for match in TOKEN.finditer(sanitize_sql(sql))]
    paths: list[tuple[int, ...]] = []
    stack: list[int] = []
    next_scope = 0
    for token in tokens:
        if token == "(":
            next_scope += 1
            stack.append(next_scope)
        paths.append(tuple(stack))
        if token == ")" and stack:
            stack.pop()

    relations: list[tuple[int, tuple[int, ...], str, str]] = []
    statement_start = 0

    def relation_at(index: int) -> tuple[int, str, str] | None:
        if index < len(tokens) and tokens[index] == "only":
            index += 1
        if index >= len(tokens) or not re.fullmatch(r"[a-z_][a-z0-9_]*", tokens[index]):
            return None
        name_index = index
        while name_index + 2 < len(tokens) and tokens[name_index + 1] == ".":
            name_index += 2
        end = name_index + 1
        alias = tokens[name_index]
        if end + 1 < len(tokens) and tokens[end] == "as":
            alias = tokens[end + 1]
            end += 2
        elif (
            end < len(tokens)
            and re.fullmatch(r"[a-z_][a-z0-9_]*", tokens[end])
            and tokens[end] not in RELATION_BOUNDARIES
        ):
            alias = tokens[end]
            end += 1
        return end, tokens[name_index], alias

    for index, token in enumerate(tokens):
        if token == ";":
            statement_start = index + 1
            relations.clear()
            continue
        if token in {"from", "join"} and index + 1 < len(tokens):
            cursor = index + 1
            while parsed := relation_at(cursor):
                end, name, alias = parsed
                relations.append((index, paths[index], name, alias))
                cursor = end
                if (
                    cursor >= len(tokens)
                    or paths[cursor] != paths[index]
                    or tokens[cursor] != ","
                ):
                    break
                cursor += 1
            continue
        if token != "for" or index + 1 >= len(tokens) or tokens[index + 1] != "update":
            continue

        if index + 2 < len(tokens) and tokens[index + 2] == "of":
            targets: list[str] = []
            for candidate in tokens[index + 3 :]:
                if candidate == ";":
                    break
                targets.append(candidate)
            if any(
                relation_index >= statement_start
                and relation_path == paths[index]
                and relation == "session"
                and (relation in targets or alias in targets)
                for relation_index, relation_path, relation, alias in relations
            ):
                return True
            continue

        if any(
            relation_index >= statement_start
            and relation_path == paths[index]
            and relation == "session"
            for relation_index, relation_path, relation, _alias in relations
        ):
            return True
    return False


def self_test() -> None:
    forbidden = [
        "select * from session for update",
        "SELECT * FROM public.session FOR UPDATE",
        "SELECT * FROM other JOIN session ON true FOR UPDATE OF session",
        "SELECT * FROM session AS s FOR UPDATE OF s",
        "SELECT * FROM other, session WHERE true FOR UPDATE",
        "SELECT * FROM session WHERE id IN (SELECT id FROM other) FOR UPDATE",
        "SELECT * FROM ONLY session FOR UPDATE",
        "SELECT * FROM session /* ( */ FOR UPDATE",
        'SELECT * FROM "session" FOR UPDATE',
    ]
    allowed = [
        "SELECT * FROM session FOR NO KEY UPDATE",
        "SELECT * FROM session_scheduler FOR UPDATE",
        "SELECT * FROM session_scheduler JOIN session ON true "
        "FOR UPDATE OF session_scheduler",
        "SELECT EXISTS (SELECT 1 FROM session), "
        "(SELECT id FROM session_scheduler FOR UPDATE)",
        "SELECT * FROM session_scheduler FOR UPDATE OF session_scheduler; "
        "SELECT * FROM session",
    ]
    assert all(locks_session_with_plain_update(sql) for sql in forbidden)
    assert not any(locks_session_with_plain_update(sql) for sql in allowed)
    assert not rust_strings('// "SELECT * FROM session FOR UPDATE"\nfn main() {}')
    assert any(
        locks_session_with_plain_update(sql)
        for sql in query_strings(
            'sqlx::query(concat!("SELECT count(*) FROM session ", "FOR UPDATE"))'
        )
    )
    assert not locks_session_with_plain_update(
        "SELECT '(' FROM session_scheduler -- FROM session (\nFOR UPDATE"
    )
    candidates, errors = query_inputs(
        'sqlx::query(concat!("SELECT * FROM session ", "FOR UPDATE"))', Path(".")
    )
    assert not errors and locks_session_with_plain_update(candidates[0])
    _, errors = query_inputs(
        'sqlx::query(&format!("SELECT * FROM session {}", lock_mode))', Path(".")
    )
    assert errors
    with tempfile.TemporaryDirectory() as directory:
        crate_root = Path(directory)
        (crate_root / "lock.sql").write_text("SELECT * FROM session FOR UPDATE")
        candidates, errors = query_inputs(
            'sqlx::query_file!("lock.sql")', crate_root
        )
        assert not errors and locks_session_with_plain_update(candidates[0])


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("root", nargs="?", default="crates/persistence/src")
    arguments = parser.parse_args()
    if arguments.self_test:
        self_test()
        return 0

    root = Path(arguments.root)
    if not root.is_dir():
        print(
            f"{root}: persistence source directory is missing; "
            "session-lock protocol was not checked",
            file=sys.stderr,
        )
        return 1

    violations: list[Path] = []
    inspection_errors: list[tuple[Path, str]] = []
    for path in sorted(root.rglob("*.rs")):
        candidates, errors = query_inputs(path.read_text(), root.parent)
        inspection_errors.extend((path, error) for error in errors)
        if any(locks_session_with_plain_update(sql) for sql in candidates):
            violations.append(path)
    for path, error in inspection_errors:
        print(f"{path}: {error}", file=sys.stderr)
    if violations:
        for path in violations:
            print(f"{path}: session table locked with plain FOR UPDATE", file=sys.stderr)
    return 1 if violations or inspection_errors else 0


if __name__ == "__main__":
    raise SystemExit(main())

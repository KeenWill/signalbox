#!/usr/bin/env python3
"""Reject SQL strings that lock the session table with plain FOR UPDATE."""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

TOKEN = re.compile(r"[A-Za-z_][A-Za-z0-9_]*|[(),;]")


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


def locks_session_with_plain_update(sql: str) -> bool:
    """Whether one SQL string applies plain FOR UPDATE to a session relation."""
    tokens = [match.group(0).lower() for match in TOKEN.finditer(sql)]
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

    relations: list[tuple[int, tuple[int, ...], str]] = []
    statement_start = 0
    for index, token in enumerate(tokens):
        if token == ";":
            statement_start = index + 1
            relations.clear()
            continue
        if token in {"from", "join"} and index + 1 < len(tokens):
            relations.append((index, paths[index], tokens[index + 1]))
            continue
        if token != "for" or index + 1 >= len(tokens) or tokens[index + 1] != "update":
            continue

        if index + 2 < len(tokens) and tokens[index + 2] == "of":
            targets = tokens[index + 3 :]
            if "session" in targets:
                return True

        if any(
            relation_index >= statement_start
            and relation_path == paths[index]
            and relation == "session"
            for relation_index, relation_path, relation in relations
        ):
            return True
    return False


def self_test() -> None:
    forbidden = [
        "select * from session for update",
        "SELECT * FROM other JOIN session ON true FOR UPDATE OF session",
        "SELECT * FROM session WHERE id IN (SELECT id FROM other) FOR UPDATE",
    ]
    allowed = [
        "SELECT * FROM session FOR NO KEY UPDATE",
        "SELECT * FROM session_scheduler FOR UPDATE",
        "SELECT EXISTS (SELECT 1 FROM session), "
        "(SELECT id FROM session_scheduler FOR UPDATE)",
    ]
    assert all(locks_session_with_plain_update(sql) for sql in forbidden)
    assert not any(locks_session_with_plain_update(sql) for sql in allowed)
    assert not rust_strings('// "SELECT * FROM session FOR UPDATE"\nfn main() {}')


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
    for path in sorted(root.rglob("*.rs")):
        if any(locks_session_with_plain_update(sql) for sql in rust_strings(path.read_text())):
            violations.append(path)
    if violations:
        for path in violations:
            print(f"{path}: session table locked with plain FOR UPDATE", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

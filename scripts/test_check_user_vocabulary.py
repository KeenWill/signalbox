#!/usr/bin/env python3
"""Prove the user-vocabulary checker rejects role-sense ``owner`` prose."""

from __future__ import annotations

import subprocess
import sys
import tempfile
from pathlib import Path

CHECKER = Path(__file__).resolve().parent / "check_user_vocabulary.py"

def run_checker(root: Path) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, str(CHECKER), "--root", str(root)],
        check=False,
        capture_output=True,
        text=True,
    )

def git(root: Path, *arguments: str) -> None:
    subprocess.run(
        ["git", *arguments],
        cwd=root,
        check=True,
        capture_output=True,
        text=True,
    )

def main() -> int:
    with tempfile.TemporaryDirectory(prefix="signalbox-user-vocabulary-") as directory:
        root = Path(directory)
        allowed = root / "crates" / "example" / "src" / "lib.rs"
        imported = root / "crates" / "domain" / "src" / "imported_conversation.rs"
        violation = root / "docs" / "spec" / "example.md"
        allowed.parent.mkdir(parents=True)
        imported.parent.mkdir(parents=True)
        violation.parent.mkdir(parents=True)
        allowed.write_text(
            'const REPOSITORY: &str = "owner/repository";\n', encoding="utf-8"
        )
        violation.write_text(
            "The owner approves this tool.\n"
            "The owners approve this tool.\n"
            "session_owners = []\n"
            "owner2 = []\n"
            "Owner2 = []\n"
            "session_owner2 = []\n"
            "owner2_id = nil\n"
            "Owner2Id = nil\n"
            "session_owner2_id = nil\n"
            "The session owner approves this tool.\n",
            encoding="utf-8",
        )
        imported.write_text(
            "struct EntryFixture {\n"
            "    owner: ImportedConversationId,\n"
            "}\n"
            "fn fixture() {\n"
            "    let owner = conversation(1);\n"
            "    consume(\n"
            "        owner,\n"
            "    );\n"
            "}\n"
            "// The owner approves this tool.\n",
            encoding="utf-8",
        )
        git(root, "init", "--quiet")
        git(
            root,
            "add",
            "crates/domain/src/imported_conversation.rs",
            "crates/example/src/lib.rs",
            "docs/spec/example.md",
        )
        rejected = run_checker(root)
        assert rejected.returncode == 1, (
            f"violation unexpectedly passed:\n{rejected.stdout}{rejected.stderr}"
        )
        expected = "docs/spec/example.md:1: The owner approves this tool."
        plural = "docs/spec/example.md:2: The owners approve this tool."
        identifier = "docs/spec/example.md:3: session_owners = []"
        numeric = "docs/spec/example.md:4: owner2 = []"
        numeric_capitalized = "docs/spec/example.md:5: Owner2 = []"
        numeric_identifier = "docs/spec/example.md:6: session_owner2 = []"
        continued_numeric = "docs/spec/example.md:7: owner2_id = nil"
        continued_capitalized = "docs/spec/example.md:8: Owner2Id = nil"
        continued_numeric_identifier = "docs/spec/example.md:9: session_owner2_id = nil"
        semantic_session_owner = (
            "docs/spec/example.md:10: The session owner approves this tool."
        )
        imported_role_owner = (
            "crates/domain/src/imported_conversation.rs:10: "
            "// The owner approves this tool."
        )
        assert expected in rejected.stdout, (
            f"singular violation missing:\n{rejected.stdout}"
        )
        assert plural in rejected.stdout, (
            f"plural violation missing:\n{rejected.stdout}"
        )
        assert identifier in rejected.stdout, (
            f"identifier violation missing:\n{rejected.stdout}"
        )
        assert numeric in rejected.stdout, (
            f"numeric violation missing:\n{rejected.stdout}"
        )
        assert numeric_capitalized in rejected.stdout, (
            f"capitalized numeric violation missing:\n{rejected.stdout}"
        )
        assert numeric_identifier in rejected.stdout, (
            f"numeric identifier violation missing:\n{rejected.stdout}"
        )
        assert continued_numeric in rejected.stdout, (
            f"continued numeric violation missing:\n{rejected.stdout}"
        )
        assert continued_capitalized in rejected.stdout, (
            f"continued capitalized violation missing:\n{rejected.stdout}"
        )
        assert continued_numeric_identifier in rejected.stdout, (
            f"continued numeric identifier violation missing:\n{rejected.stdout}"
        )
        assert semantic_session_owner in rejected.stdout, (
            f"semantic session-owner violation missing:\n{rejected.stdout}"
        )
        assert imported_role_owner in rejected.stdout, (
            f"imported-file role violation missing:\n{rejected.stdout}"
        )
        violation.write_text("The user approves this tool.\n", encoding="utf-8")
        imported.write_text(
            "struct EntryFixture {\n"
            "    owner: ImportedConversationId,\n"
            "}\n"
            "fn fixture() {\n"
            "    let owner = conversation(1);\n"
            "    consume(\n"
            "        owner,\n"
            "    );\n"
            "}\n",
            encoding="utf-8",
        )
        accepted = run_checker(root)
        assert accepted.returncode == 0, (
            f"allowed vocabulary failed:\n{accepted.stdout}{accepted.stderr}"
        )
    print("user-vocabulary checker self-test passed")
    return 0

if __name__ == "__main__":
    raise SystemExit(main())

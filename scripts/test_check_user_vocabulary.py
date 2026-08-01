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
        allowed = root / "crates" / "tools-github" / "src" / "lib.rs"
        imported = root / "crates" / "domain" / "src" / "imported_conversation.rs"
        mixed_storage_path = (
            root / "apps" / "signalboxd" / "tests" / "offline_tool_loop.rs"
        )
        frozen_migration = (
            root
            / "crates"
            / "persistence"
            / "migrations"
            / "202607180001_create_session.sql"
        )
        future_migration = (
            root
            / "crates"
            / "persistence"
            / "migrations"
            / "202608020009_user_vocabulary.sql"
        )
        native_path = (
            root
            / "clients"
            / "native"
            / "Sources"
            / "SignalboxClient"
            / "SessionSynchronization.swift"
        )
        violation = root / "docs" / "spec" / "example.md"
        reviewed_domain_path = root / "docs" / "spec" / "review-workflows.md"
        reviewed_github_path = root / "docs" / "spec" / "tool-loop.md"
        reviewed_unix_path = root / "docs" / "spec" / "process-protocol.md"
        allowed.parent.mkdir(parents=True)
        imported.parent.mkdir(parents=True)
        mixed_storage_path.parent.mkdir(parents=True)
        frozen_migration.parent.mkdir(parents=True)
        native_path.parent.mkdir(parents=True)
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
            "The session owner approves this tool.\n"
            "The owner field identifies the human who approves this tool.\n"
            "sessionOwner2 = nil\n"
            "sessionOwner2Id = nil\n"
            "The current owner prevents the tool from running.\n"
            "The wrong owner approves this tool.\n"
            "The process protocol emits {\"type\":\"owner\"}.\n"
            "A request by an owner, member, or collaborator approves tools.\n"
            "isCollapsedByOwner = true\n"
            "sessionowner = []\n"
            "ownerid = nil\n"
            "Sessionowner = []\n"
            "ownerId = nil\n"
            "SESSIONOWNER = []\n"
            "OWNERID = nil\n"
            "Ownerid = nil\n",
            encoding="utf-8",
        )
        mixed_storage_path.write_text(
            'const PROCESS_ACTOR: &str = "owner";\n'
            'const DECISION_SOURCE: &str = "owner_command";\n',
            encoding="utf-8",
        )
        frozen_migration.write_text(
            "CHECK (cause = 'owner_initiated');\n", encoding="utf-8"
        )
        future_migration.write_text(
            "-- New owner actor\nCHECK (actor_kind = 'owner');\n",
            encoding="utf-8",
        )
        native_path.write_text(
            'let sessionOwner = "human who approves tools"\n', encoding="utf-8"
        )
        reviewed_domain_path.write_text(
            "The foreign owner approves this tool.\n"
            "`foreign_session_owner` names the human who approves this tool.\n",
            encoding="utf-8",
        )
        reviewed_github_path.write_text(
            "A request by an owner, member, or collaborator approves tools.\n",
            encoding="utf-8",
        )
        reviewed_unix_path.write_text(
            "The wrong owner approves this tool.\n", encoding="utf-8"
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
            "crates/tools-github/src/lib.rs",
            "apps/signalboxd/tests/offline_tool_loop.rs",
            "crates/persistence/migrations/202607180001_create_session.sql",
            "crates/persistence/migrations/202608020009_user_vocabulary.sql",
            "clients/native/Sources/SignalboxClient/SessionSynchronization.swift",
            "docs/spec/example.md",
            "docs/spec/process-protocol.md",
            "docs/spec/review-workflows.md",
            "docs/spec/tool-loop.md",
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
        generic_owner_phrase = (
            "docs/spec/example.md:11: "
            "The owner field identifies the human who approves this tool."
        )
        embedded_numeric = "docs/spec/example.md:12: sessionOwner2 = nil"
        embedded_continued_numeric = "docs/spec/example.md:13: sessionOwner2Id = nil"
        unreviewed_path_allowance = (
            "docs/spec/example.md:14: "
            "The current owner prevents the tool from running."
        )
        unix_owner_outside_reviewed_path = (
            "docs/spec/example.md:15: The wrong owner approves this tool."
        )
        stale_wire_value = (
            'docs/spec/example.md:16: The process protocol emits {"type":"owner"}.'
        )
        github_role_outside_reviewed_path = (
            "docs/spec/example.md:17: "
            "A request by an owner, member, or collaborator approves tools."
        )
        external_field_outside_reviewed_path = (
            "docs/spec/example.md:18: isCollapsedByOwner = true"
        )
        lowercase_suffix = "docs/spec/example.md:19: sessionowner = []"
        lowercase_prefix = "docs/spec/example.md:20: ownerid = nil"
        mixed_case_suffix = "docs/spec/example.md:21: Sessionowner = []"
        mixed_case_prefix = "docs/spec/example.md:22: ownerId = nil"
        uppercase_suffix = "docs/spec/example.md:23: SESSIONOWNER = []"
        uppercase_prefix = "docs/spec/example.md:24: OWNERID = nil"
        capitalized_prefix = "docs/spec/example.md:25: Ownerid = nil"
        unix_role_inside_reviewed_path = (
            "docs/spec/process-protocol.md:1: The wrong owner approves this tool."
        )
        imported_role_owner = (
            "crates/domain/src/imported_conversation.rs:10: "
            "// The owner approves this tool."
        )
        stale_actor_in_mixed_storage_path = (
            "apps/signalboxd/tests/offline_tool_loop.rs:1: "
            'const PROCESS_ACTOR: &str = "owner";'
        )
        future_migration_prose = (
            "crates/persistence/migrations/202608020009_user_vocabulary.sql:1: "
            "-- New owner actor"
        )
        future_migration_encoding = (
            "crates/persistence/migrations/202608020009_user_vocabulary.sql:2: "
            "CHECK (actor_kind = 'owner');"
        )
        native_role_identifier = (
            "clients/native/Sources/SignalboxClient/SessionSynchronization.swift:1: "
            'let sessionOwner = "human who approves tools"'
        )
        domain_role_inside_reviewed_path = (
            "docs/spec/review-workflows.md:1: The foreign owner approves this tool."
        )
        domain_role_identifier_inside_reviewed_path = (
            "docs/spec/review-workflows.md:2: "
            "`foreign_session_owner` names the human who approves this tool."
        )
        github_role_inside_reviewed_path = (
            "docs/spec/tool-loop.md:1: "
            "A request by an owner, member, or collaborator approves tools."
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
        assert generic_owner_phrase in rejected.stdout, (
            f"generic owner-phrase violation missing:\n{rejected.stdout}"
        )
        assert embedded_numeric in rejected.stdout, (
            f"embedded numeric violation missing:\n{rejected.stdout}"
        )
        assert embedded_continued_numeric in rejected.stdout, (
            f"embedded continued numeric violation missing:\n{rejected.stdout}"
        )
        assert unreviewed_path_allowance in rejected.stdout, (
            f"unreviewed-path allowance violation missing:\n{rejected.stdout}"
        )
        assert unix_owner_outside_reviewed_path in rejected.stdout, (
            f"Unix-owner allowance violation missing:\n{rejected.stdout}"
        )
        assert stale_wire_value in rejected.stdout, (
            f"stale wire-value violation missing:\n{rejected.stdout}"
        )
        assert github_role_outside_reviewed_path in rejected.stdout, (
            f"GitHub-role allowance violation missing:\n{rejected.stdout}"
        )
        assert external_field_outside_reviewed_path in rejected.stdout, (
            f"external-field allowance violation missing:\n{rejected.stdout}"
        )
        assert lowercase_suffix in rejected.stdout, (
            f"lowercase suffix violation missing:\n{rejected.stdout}"
        )
        assert lowercase_prefix in rejected.stdout, (
            f"lowercase prefix violation missing:\n{rejected.stdout}"
        )
        assert mixed_case_suffix in rejected.stdout, (
            f"mixed-case suffix violation missing:\n{rejected.stdout}"
        )
        assert mixed_case_prefix in rejected.stdout, (
            f"mixed-case prefix violation missing:\n{rejected.stdout}"
        )
        assert uppercase_suffix in rejected.stdout, (
            f"uppercase suffix violation missing:\n{rejected.stdout}"
        )
        assert uppercase_prefix in rejected.stdout, (
            f"uppercase prefix violation missing:\n{rejected.stdout}"
        )
        assert capitalized_prefix in rejected.stdout, (
            f"capitalized prefix violation missing:\n{rejected.stdout}"
        )
        assert unix_role_inside_reviewed_path in rejected.stdout, (
            f"reviewed-path Unix-role violation missing:\n{rejected.stdout}"
        )
        assert imported_role_owner in rejected.stdout, (
            f"imported-file role violation missing:\n{rejected.stdout}"
        )
        assert stale_actor_in_mixed_storage_path in rejected.stdout, (
            f"mixed storage-path actor violation missing:\n{rejected.stdout}"
        )
        assert future_migration_prose in rejected.stdout, (
            f"future migration prose violation missing:\n{rejected.stdout}"
        )
        assert future_migration_encoding in rejected.stdout, (
            f"future migration encoding violation missing:\n{rejected.stdout}"
        )
        assert native_role_identifier in rejected.stdout, (
            f"native role identifier violation missing:\n{rejected.stdout}"
        )
        assert domain_role_inside_reviewed_path in rejected.stdout, (
            f"domain reviewed-path role violation missing:\n{rejected.stdout}"
        )
        assert domain_role_identifier_inside_reviewed_path in rejected.stdout, (
            f"domain reviewed-path identifier violation missing:\n{rejected.stdout}"
        )
        assert github_role_inside_reviewed_path in rejected.stdout, (
            f"GitHub reviewed-path role violation missing:\n{rejected.stdout}"
        )
        violation.write_text("The user approves this tool.\n", encoding="utf-8")
        reviewed_unix_path.write_text(
            "signalboxd binds a socket with owner-only `0600` permissions.\n",
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
            "}\n",
            encoding="utf-8",
        )
        mixed_storage_path.write_text(
            'const PROCESS_ACTOR: &str = "user";\n'
            'const DECISION_SOURCE: &str = "owner_command";\n',
            encoding="utf-8",
        )
        future_migration.write_text(
            "-- New user actor\nCHECK (actor_kind = 'user');\n",
            encoding="utf-8",
        )
        native_path.write_text(
            "private enum SignalboxSnapshotModelCallOwnership {}\n",
            encoding="utf-8",
        )
        reviewed_domain_path.write_text(
            "closed on a foreign owner, run-workflow or policy mismatch.\n",
            encoding="utf-8",
        )
        reviewed_github_path.write_text(
            "`@codex review` request by an owner, member, or collaborator.\n",
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

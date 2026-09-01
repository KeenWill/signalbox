#!/usr/bin/env python3
"""Prove the user-vocabulary checker rejects retired and ambiguous prose."""

from __future__ import annotations

import subprocess
import sys
import tempfile
from collections import Counter
from pathlib import Path

CHECKER = Path(__file__).resolve().parent / "check_user_vocabulary.py"


def run_checker(root: Path, *arguments: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, str(CHECKER), "--root", str(root), *arguments],
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


def fixture_text(lines: tuple[str, ...]) -> str:
    return "".join(f"{line}\n" for line in lines)


def expected_diagnostics(
    path: str,
    fixture_lines: tuple[str, ...],
    rejected_lines: tuple[str, ...] | None = None,
) -> list[str]:
    selected = fixture_lines if rejected_lines is None else rejected_lines
    return [
        f"{path}:{fixture_lines.index(line) + 1}: {line.strip()}"
        for line in selected
    ]


def main() -> int:
    with tempfile.TemporaryDirectory(prefix="signalbox-user-vocabulary-") as directory:
        root = Path(directory)
        allowed = root / "crates" / "tools-github" / "src" / "lib.rs"
        reviewed_author = (
            root
            / "crates"
            / "tools-code-host"
            / "src"
            / "code_host"
            / "review_slog"
            / "convergence.rs"
        )
        author_violation = reviewed_author.with_name("example.rs")
        imported = root / "crates" / "domain" / "src" / "imported_conversation.rs"
        application_import = (
            root / "crates" / "application" / "src" / "conversation_import.rs"
        )
        domain_record = root / "crates" / "domain" / "src" / "session.rs"
        legacy_actor = (
            root / "crates" / "persistence" / "src" / "session_metadata.rs"
        )
        storage_mapping = root / "crates" / "persistence" / "src" / "mapping.rs"
        mixed_storage_path = (
            root / "apps" / "signalboxd" / "tests" / "offline_tool_loop.rs"
        )
        frozen_migration = (
            root
            / "crates"
            / "persistence"
            / "migrations"
            / "202609010011_blobs_and_web.sql"
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
        cross_fragment_path = (
            root
            / "clients"
            / "native"
            / "Tests"
            / "SignalboxAppTests"
            / "ViewModelTests.swift"
        )
        violation = root / "docs" / "spec" / "example.md"
        ambiguous_message = root / "docs" / "spec" / "user-message.md"
        reviewed_domain_path = root / "docs" / "spec" / "review-workflows.md"
        reviewed_github_path = root / "docs" / "spec" / "tool-loop.md"
        reviewed_identity_path = root / "docs" / "spec" / "identity-and-commands.md"
        reviewed_unix_path = root / "docs" / "spec" / "process-protocol.md"
        local_socket_path = (
            root / "apps" / "signalboxd" / "src" / "local_socket.rs"
        )
        convergence_runtime_path = (
            root
            / "apps"
            / "signalboxd"
            / "src"
            / "convergence_sweep_runtime.rs"
        )
        violation_lines = (
            "The owner approves this tool.",
            "The owners approve this tool.",
            "session_owners = []",
            "owner2 = []",
            "Owner2 = []",
            "session_owner2 = []",
            "owner2_id = nil",
            "Owner2Id = nil",
            "session_owner2_id = nil",
            "The session owner approves this tool.",
            "The owner field identifies the human who approves this tool.",
            "sessionOwner2 = nil",
            "sessionOwner2Id = nil",
            "The current owner prevents the tool from running.",
            "The wrong owner approves this tool.",
            'The process protocol emits {"type":"owner"}.',
            "A request by an owner, member, or collaborator approves tools.",
            "isCollapsedByOwner = true",
            "sessionowner = []",
            "ownerid = nil",
            "Sessionowner = []",
            "ownerId = nil",
            "SESSIONOWNER = []",
            "OWNERID = nil",
            "Ownerid = nil",
            "ownershipowner names the human who approves tools.",
            "ownerownership names the human who approves tools.",
            "OWNERSHIPOWNER names the human who approves tools.",
            "ownershipoWnEr names the human who approves tools.",
            "session_ownerid = nil",
            "SESSION_OWNERID = nil",
            "The oWnEr approves this tool.",
            "The ownEr approves this tool.",
            "The ownER approves this tool.",
        )
        mixed_storage_lines = (
            'const PROCESS_ACTOR: &str = "owner";',
            'const DECISION_SOURCE: &str = "owner_command";',
        )
        future_migration_lines = (
            "-- New owner actor",
            "CHECK (actor_kind = 'owner');",
        )
        frozen_migration_lines = (
            "ADD CONSTRAINT imported_transcript_entry_owner_fk FOREIGN KEY (id);",
            "human_owner TEXT;",
        )
        native_lines = (
            'let sessionOwner = "human who approves tools"',
            "enum HumanPrincipal { case owner }",
        )
        reviewed_domain_lines = (
            "The foreign owner approves this tool.",
            "human_foreign_owner names the person who approves tools.",
            "`foreign_session_owner` names the human who approves this tool.",
        )
        reviewed_github_lines = (
            "A request by an owner, member, or collaborator approves tools.",
            'author_association = "HUMAN_OWNER"',
            'The protocol emits {"owner": "the human who approves tools"}.',
            'The protocol emits json!({"owner": owner, "meaning": "the human principal"}).',
            "Merge pull request after the owner approves owner/repository.",
        )
        reviewed_identity_lines = (
            "The process protocol still accepts the human actor 'owner'.",
        )
        reviewed_unix_lines = (
            "The wrong owner approves this tool.",
            "HumanRoleOwnerMismatch = true",
            "Owner-only access lets the owner approve this socket.",
        )
        local_socket_lines = (
            "fn tool_is_approved(owner: u32) -> bool { owner == HUMAN_USER_ID }",
            "fn owner_access() -> bool { true } // human who approves tools",
        )
        convergence_runtime_lines = (
            "query PullRequestConvergence($namespace: String!) {",
            "  repository(owner: $namespace, name: $name) {",
            "    headRepository { name_with_owner: nameWithOwner }",
            "let slug = pull.pointer(\"/headRepository/name_with_owner\");",
            "let owner = the_human_who_approves_tools;",
        )
        imported_lines = (
            "struct EntryFixture {",
            "    owner: ImportedConversationId,",
            "}",
            "fn converted() {",
            "    let owner = conversation(1);",
            "    consume(",
            "        owner,",
            "    );",
            "}",
            "// The owner approves this tool.",
        )
        domain_record_lines = ('let OwnerID = "human who approves tools";',)
        legacy_actor_lines = (
            'let human_principal = String::from("owner"); // approves tools',
            'let human = Principal { kind: "owner" }; // approves tools',
            'let human = ("owner", None, None); // approves tools',
            'let human = match role { "owner" | "model" => Human };',
            'let human = ("owner", None); // approves tools',
        )
        application_import_lines = (
            "fn converted() {}",
            "define_human_role!(",
            "    owner,",
            ")",
            'let returned_owner = "human who approves tools";',
        )
        allowed.parent.mkdir(parents=True)
        imported.parent.mkdir(parents=True)
        mixed_storage_path.parent.mkdir(parents=True)
        frozen_migration.parent.mkdir(parents=True)
        native_path.parent.mkdir(parents=True)
        cross_fragment_path.parent.mkdir(parents=True)
        violation.parent.mkdir(parents=True)
        github_accessor_lines = (
            'const REPOSITORY: &str = "owner/repository";',
            "let approval = session.owner();",
            'fn owner(&self) -> &str { "human who approves tools" }',
            "impl Session {",
            "    fn owner(&self) -> &str {",
            '    "human who approves tools"',
            "    }",
            "}",
            "impl GitHubRepository {",
            "// unmatched non-code brace: {",
            "}",
            "impl Session {",
            "        fn owner(&self) -> &str {",
            "let owner = human_owner + repository.owner_end;",
            "mutation Approve($owner: ID!) { approve(human: $owner) }",
            "fn approve_repository(owner: HumanPrincipal) {}",
        )
        reviewed_author_lines = ('author: Some(String::from("owner")),',)
        author_violation_lines = (
            'let principal = Fixture { author: Some(String::from("owner")) };',
        )
        allowed.write_text(fixture_text(github_accessor_lines), encoding="utf-8")
        reviewed_author.parent.mkdir(parents=True)
        reviewed_author.write_text(
            fixture_text(reviewed_author_lines), encoding="utf-8"
        )
        author_violation.write_text(
            fixture_text(author_violation_lines), encoding="utf-8"
        )
        violation.write_text(fixture_text(violation_lines), encoding="utf-8")
        ambiguous_message.write_text(
            "The runtime sends a user message.\n"
            "The runtime also sends a user\n"
            "message after tool output.\n",
            encoding="utf-8",
        )
        mixed_storage_path.write_text(
            fixture_text(mixed_storage_lines), encoding="utf-8"
        )
        frozen_migration.write_text(
            fixture_text(frozen_migration_lines), encoding="utf-8"
        )
        future_migration.write_text(
            fixture_text(future_migration_lines), encoding="utf-8"
        )
        native_path.write_text(fixture_text(native_lines), encoding="utf-8")
        cross_fragment_path.write_text(
            "ImportedContinuationRetryFixture.sendOutcomeUnknownError,\n",
            encoding="utf-8",
        )
        reviewed_domain_path.write_text(
            fixture_text(reviewed_domain_lines), encoding="utf-8"
        )
        reviewed_github_path.write_text(
            fixture_text(reviewed_github_lines), encoding="utf-8"
        )
        reviewed_identity_path.write_text(
            fixture_text(reviewed_identity_lines), encoding="utf-8"
        )
        reviewed_unix_path.write_text(
            fixture_text(reviewed_unix_lines), encoding="utf-8"
        )
        local_socket_path.parent.mkdir(parents=True, exist_ok=True)
        local_socket_path.write_text(
            fixture_text(local_socket_lines), encoding="utf-8"
        )
        convergence_runtime_path.write_text(
            fixture_text(convergence_runtime_lines), encoding="utf-8"
        )
        imported.write_text(fixture_text(imported_lines), encoding="utf-8")
        application_import.parent.mkdir(parents=True, exist_ok=True)
        application_import.write_text(
            fixture_text(application_import_lines), encoding="utf-8"
        )
        domain_record.write_text(
            fixture_text(domain_record_lines), encoding="utf-8"
        )
        legacy_actor.parent.mkdir(parents=True, exist_ok=True)
        legacy_actor.write_text(
            fixture_text(legacy_actor_lines), encoding="utf-8"
        )
        storage_mapping.write_text(
            'const DECISION_SOURCE: &str = "owner_command";\n', encoding="utf-8"
        )
        git(root, "init", "--quiet")
        git(
            root,
            "add",
            "crates/domain/src/imported_conversation.rs",
            "crates/application/src/conversation_import.rs",
            "crates/domain/src/session.rs",
            "crates/persistence/src/session_metadata.rs",
            "crates/persistence/src/mapping.rs",
            "crates/tools-code-host/src/code_host/review_slog/convergence.rs",
            "crates/tools-code-host/src/code_host/review_slog/example.rs",
            "crates/tools-github/src/lib.rs",
            "apps/signalboxd/tests/offline_tool_loop.rs",
            "crates/persistence/migrations/202609010011_blobs_and_web.sql",
            "crates/persistence/migrations/202608020009_user_vocabulary.sql",
            "clients/native/Sources/SignalboxClient/SessionSynchronization.swift",
            "clients/native/Tests/SignalboxAppTests/ViewModelTests.swift",
            "docs/spec/example.md",
            "docs/spec/user-message.md",
            "docs/spec/identity-and-commands.md",
            "docs/spec/process-protocol.md",
            "docs/spec/review-workflows.md",
            "docs/spec/tool-loop.md",
            "apps/signalboxd/src/local_socket.rs",
            "apps/signalboxd/src/convergence_sweep_runtime.rs",
        )
        rejected = run_checker(root)
        assert rejected.returncode == 1, (
            f"violation unexpectedly passed:\n{rejected.stdout}{rejected.stderr}"
        )
        reported = [
            line.removeprefix("  - ")
            for line in rejected.stdout.splitlines()
            if line.startswith("  - ")
        ]
        expected = [
            *expected_diagnostics("docs/spec/example.md", violation_lines),
            "docs/spec/user-message.md:1: The runtime sends a user message.",
            "docs/spec/user-message.md:2: The runtime also sends a user",
            *expected_diagnostics(
                "docs/spec/process-protocol.md", reviewed_unix_lines
            ),
            *expected_diagnostics(
                "apps/signalboxd/src/local_socket.rs", local_socket_lines
            ),
            *expected_diagnostics(
                "apps/signalboxd/src/convergence_sweep_runtime.rs",
                convergence_runtime_lines,
                convergence_runtime_lines[-1:],
            ),
            *expected_diagnostics(
                "docs/spec/review-workflows.md", reviewed_domain_lines
            ),
            *expected_diagnostics("docs/spec/tool-loop.md", reviewed_github_lines),
            *expected_diagnostics(
                "docs/spec/identity-and-commands.md", reviewed_identity_lines
            ),
            *expected_diagnostics(
                "crates/tools-github/src/lib.rs",
                github_accessor_lines,
                (
                    github_accessor_lines[1],
                    github_accessor_lines[2],
                    github_accessor_lines[4],
                    github_accessor_lines[12],
                    github_accessor_lines[13],
                    github_accessor_lines[14],
                    github_accessor_lines[15],
                ),
            ),
            *expected_diagnostics(
                "crates/tools-code-host/src/code_host/review_slog/example.rs",
                author_violation_lines,
            ),
            *expected_diagnostics(
                "crates/domain/src/imported_conversation.rs",
                imported_lines,
                imported_lines[-1:],
            ),
            *expected_diagnostics(
                "crates/application/src/conversation_import.rs",
                application_import_lines,
                (application_import_lines[2], application_import_lines[4]),
            ),
            *expected_diagnostics(
                "crates/domain/src/session.rs", domain_record_lines
            ),
            *expected_diagnostics(
                "crates/persistence/src/session_metadata.rs", legacy_actor_lines
            ),
            # Both lines are violations now that storage spells the human
            # principal `user`: the retired `owner_command` encoding no longer
            # has a reviewed allowance to hide behind.
            *expected_diagnostics(
                "apps/signalboxd/tests/offline_tool_loop.rs",
                mixed_storage_lines,
            ),
            'crates/persistence/src/mapping.rs:1: const DECISION_SOURCE: &str '
            '= "owner_command";',
            *expected_diagnostics(
                "crates/persistence/migrations/202609010011_blobs_and_web.sql",
                frozen_migration_lines,
                frozen_migration_lines[1:],
            ),
            *expected_diagnostics(
                "crates/persistence/migrations/202608020009_user_vocabulary.sql",
                future_migration_lines,
            ),
            *expected_diagnostics(
                "clients/native/Sources/SignalboxClient/SessionSynchronization.swift",
                native_lines,
            ),
        ]
        assert Counter(reported) == Counter(expected), (
            f"reported diagnostics differ: {rejected.stdout}"
        )
        violation.write_text("The user approves this tool.\n", encoding="utf-8")
        allowed.write_text(
            'const REPOSITORY: &str = "owner/repository";\n'
            "segments.push(repository.owner());\n"
            "impl GitHubRepository {\n"
            "fn owner(&self) -> &str {\n"
            "    &self.value[..self.owner_end]\n"
            "}\n"
            "let payload = json!({\n"
            '    "owner": repository.owner(),\n'
            "});\n"
            "query PullRequestReviewThreads($owner: String!, $name: String!, $number: Int!) {\n"
            "  repository(owner: $owner, name: $name) {\n"
            "  }\n"
            "}\n",
            encoding="utf-8",
        )
        author_violation.write_text(
            'let principal = Fixture { author: Some(String::from("user")) };\n',
            encoding="utf-8",
        )
        ambiguous_message.write_text(
            "The runtime sends a user-role message.\n"
            "The runtime also sends a message from the user.\n"
            "The user messages the daemon through the terminal.\n",
            encoding="utf-8",
        )
        reviewed_unix_path.write_text(
            "signalboxd binds a socket with owner-only `0600` permissions.\n",
            encoding="utf-8",
        )
        local_socket_path.write_text(
            "fn ancestor_owner_is_trusted(owner: u32, effective_user: u32) -> bool {\n"
            "    owner == 0 || owner == effective_user\n"
            "}\n"
            "fn guarded_bind_listens_only_with_owner_access() {}\n",
            encoding="utf-8",
        )
        convergence_runtime_path.write_text(
            fixture_text(convergence_runtime_lines[:-1]), encoding="utf-8"
        )
        imported.write_text(fixture_text(imported_lines[:-1]), encoding="utf-8")
        application_import.write_text(
            "fn converted() {\n"
            "    consume(\n"
            "        owner,\n"
            "    );\n"
            "}\n"
            "let converter = FakeConverter {\n"
            "    returned_owner: None,\n"
            "};\n",
            encoding="utf-8",
        )
        frozen_migration.write_text(
            "ADD CONSTRAINT imported_transcript_entry_owner_fk"
            " FOREIGN KEY (id);\n",
            encoding="utf-8",
        )
        domain_record.write_text(
            'let user_id = "human who approves tools";\n', encoding="utf-8"
        )
        # Storage now spells the human principal the way the domain does, so
        # the encoder and both decoders read `user` end to end and need no
        # allowance at all.
        legacy_actor.write_text(
            "fn encode_actor(actor: Actor) -> EncodedActor {\n"
            "    match actor {\n"
            "        Actor::User => EncodedActor {\n"
            '            kind: "user",\n'
            "        }\n"
            "    }\n"
            "}\n"
            "fn decode_actor() {\n"
            "    match stored {\n"
            '        ("user", None, None) => Ok(Actor::User),\n'
            '        ("user" | "model" | "recovery" | "tool", _, _) => {\n'
            "        }\n"
            "    }\n"
            "}\n"
            "fn decode_command() {\n"
            "    match stored {\n"
            '        ("user", None) => ReplaceSessionMetadata::new(command_id, session, content),\n'
            '        ("user", Some(_)) | ("tool", None) => {\n'
            "        }\n"
            "    }\n"
            "}\n",
            encoding="utf-8",
        )
        mixed_storage_path.write_text(
            'const PROCESS_ACTOR: &str = "user";\n'
            'const DECISION_SOURCE: &str = "user_command";\n',
            encoding="utf-8",
        )
        storage_mapping.write_text(
            'const DECISION_SOURCE: &str = "user_command";\n', encoding="utf-8"
        )
        future_migration.write_text(
            "-- New user actor\nCHECK (actor_kind = 'user');\n",
            encoding="utf-8",
        )
        native_path.write_text(
            "private enum SignalboxSnapshotRequiredModelCallOwnership {\n"
            "  case identity(SignalboxCanonicalUUID)\n"
            "  case owner\n"
            "}\n"
            "case .required(.owner):\n"
            "  return .required(.owner)\n"
            "case .impossible, .permitted, .required(.owner):\n",
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
        reviewed_identity_path.write_text(
            "The process protocol accepts the human actor 'user'.\n",
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

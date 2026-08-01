#!/usr/bin/env python3
"""Reject role-sense ``owner`` vocabulary outside reviewed homonyms."""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

SCAN_ROOTS = ("crates", "apps", "clients", "docs/spec")
OWNER = re.compile(
    r"(?i:\bowners?\d*(?:\b|[_-])|owners?[_-]|(?<=[_-])owners?\d*(?:\b|[_-]))|"
    r"\b[Oo]wners?\d+[A-Z][A-Za-z0-9_]*|"
    r"\b[Oo]wners?[A-Z][A-Za-z0-9_]*|"
    r"\b[A-Za-z0-9_]+Owners?(?:[A-Z][A-Za-z0-9_]*)?"
)

@dataclass(frozen=True)
class Allowance:
    """One reviewed non-role use of owner vocabulary."""

    name: str
    paths: re.Pattern[str]
    lines: re.Pattern[str]

    def covers(self, path: str, line: str, match: re.Match[str]) -> bool:
        if self.paths.search(path) is None:
            return False
        return any(
            allowed.start() <= match.start() and allowed.end() >= match.end()
            for allowed in self.lines.finditer(line)
        )

ALLOWLIST = (
    Allowance(
        "GitHub owner/repository coordinates and API fields",
        re.compile(
            r"^(?:apps/client/src/(?:arguments|presentation)[.]rs|"
            r"apps/client/tests/end_to_end[.]rs|"
            r"apps/signalboxd/tests/offline_tool_loop[.]rs|"
            r"crates/application/src/review_orchestration[.]rs|"
            r"crates/process-protocol/src/lib[.]rs|crates/runner-wire/src/tests[.]rs|"
            r"crates/tools-code-host/src/code_host/.+[.]rs|"
            r"crates/tools-github/src/lib[.]rs|"
            r"docs/spec/(?:configuration-and-credentials|runner-protocol|tool-loop)[.]md)$"
        ),
        re.compile(
            r"owner/repository|owner/name|repos/owner/|repository\(owner:|\$owner\b|"
            r"[\"']owner[\"']:\s*owner|[\"']owner[\"']\s*:|"
            r"\.owner\(\)|let owner = .*owner_end|\bowner_end\b|"
            r"let \(owner, name\) = repository|"
            r"Exact owner/repository|canonical `owner/repository`|"
            r"owner, member, or collaborator|OWNER.*MEMBER.*COLLABORATOR|"
            r"author_association.*OWNER|author:.*[\"']owner[\"']|"
            r"valid_repository_segment\(owner\)|fn owner\(&self\)|"
            r"Merge pull request.*owner/",
            re.IGNORECASE,
        ),
    ),
    Allowance(
        "Unix file-owner and permission semantics",
        re.compile(
            r"^(?:apps/signalbox-runner/src/(?:configuration|protocol|state)[.]rs|"
            r"apps/signalboxd/src/(?:local_socket|runner_protocol_runtime)[.]rs|"
            r"apps/signalboxd/tests/process_substrate[.]rs|"
            r"crates/model-runtime-codex-cli/tests/live_smoke[.]rs|"
            r"docs/spec/(?:configuration-and-credentials|process-protocol|"
            r"runner-protocol)[.]md)$"
        ),
        re.compile(
            r"owner-(?:only|private)|wrong-owner|"
            r"wrong owner|unprivileged different owner|untrusted owner|effective-user ownership|"
            r"owner(?:-private)? (?:directory )?permissions|"
            r"owner-private mode|owner\s*==|"
            r"owner:\s*u32|child_owner|ParentOwnerMismatch|AncestorOwnerMismatch|"
            r"[A-Za-z0-9_]*OwnerMismatch|ancestor_owner_is_trusted|ancestor_owner_must_be|file owner|"
            r"owner_access|dropping the owner|its owner, so it cannot shadow|"
            r"owner-vs-other",
            re.IGNORECASE,
        ),
    ),
    Allowance(
        "Unix local-socket owner identifiers",
        re.compile(r"^apps/signalboxd/src/local_socket[.]rs$"),
        re.compile(
            r"OWNER_(?:ONLY_MODE|PRIVATE_DIRECTORY_MODE)|"
            r"(?:Parent|Ancestor|ExistingSocket)OwnerMismatch|"
            r"child_owner|ancestor_owner_is_trusted"
        ),
    ),
    Allowance(
        "immutable applied migration vocabulary",
        re.compile(r"^crates/persistence/migrations/"),
        re.compile(r"[A-Za-z0-9_]*owner[A-Za-z0-9_]*", re.IGNORECASE),
    ),
    Allowance(
        "imported-conversation record owner identifiers",
        re.compile(r"^crates/(?:application/src/conversation_import|domain/src/imported_conversation)[.]rs$"),
        re.compile(
            r"returned_owner|"
            r"^\s*owner:\s*ImportedConversationId,\s*$|"
            r"^\s*owner,\s*$|"
            r"^\s*self[.]owner,\s*$|"
            r"^\s*let owner = conversation\(\d+\);\s*$|"
            r"self[.]observed[.]push\(\(owner, source[.]to_vec\(\)\)\);|"
            r"self[.]returned_owner[.]unwrap_or\(owner\)|"
            r"self[.]convert\(owner, source, next_entry_id\)\?",
        ),
    ),
    Allowance(
        "context-frontier fixture owner identifiers",
        re.compile(r"^crates/domain/src/context_frontier[.]rs$"),
        re.compile(
            r"same owner|"
            r"^\s*let owner = session_id\(\d+\);\s*$|"
            r"snapshot\(owner,|"
            r"owning_session\(\), owner|"
            r"^\s*owner,\s*$"
        ),
    ),
    Allowance(
        "native model-call usage ownership",
        re.compile(r"^clients/native/(?:Sources/SignalboxClient/SessionSynchronization|Tests/SignalboxClientTests/SessionSynchronizationTests)[.]swift$"),
        re.compile(
            r"[A-Za-z0-9_]*Owner[A-Za-z0-9_]*|terminal owner|case owner|[.]owner"
        ),
    ),
    Allowance(
        "external imported wire fields",
        re.compile(
            r"^(?:clients/native/SignalboxNativeTests/SignalboxNativeTests[.]swift|"
            r"clients/native/Sources/SignalboxApp/MockSignalboxFixtures[.]swift|"
            r"clients/native/Sources/SignalboxModels/SignalboxEvents[.]swift|"
            r"clients/native/Tests/SignalboxModelsTests/SignalboxModelsTests[.]swift|"
            r"crates/model-runtime-codex-cli/tests/live_smoke[.]rs)$"
        ),
        re.compile(r"isCollapsedByOwner|is_collapsed_by_owner|workspace_owner_usage_nudge"),
    ),
    Allowance(
        "legacy PostgreSQL user encodings",
        re.compile(
            r"^(?:apps/signalboxd/tests/offline_tool_loop[.]rs|"
            r"crates/persistence/src/(?:create_session|"
            r"create_session_from_imported_frontier|model_execution|session|"
            r"session_metadata|submit_input|tool_loop)[.]rs|"
            r"crates/persistence/tests/(?:conversation_import_postgres|postgres_integration|"
            r"runner_protocol_postgres|session_metadata_postgres)[.]rs|"
            r"docs/spec/identity-and-commands[.]md)$"
        ),
        re.compile(
            r"[\"']owner_initiated[\"']|[\"']owner_command(?:_id)?[\"']|"
            r"[\"']owner[\"']|`owner`|legacy owner|owner/tool|"
            r"\bowner_command_id\b"
        ),
    ),
    Allowance(
        "Rust and domain-record ownership phrasing",
        re.compile(
            r"^(?:apps/signalbox-runner/src/state[.]rs|"
            r"apps/signalboxd/src/runner_protocol_runtime[.]rs|"
            r"apps/signalboxd/tests/process_protocol_runtime[.]rs|"
            r"clients/native/Sources/SignalboxClient/SessionSynchronization[.]swift|"
            r"crates/application/src/(?:conversation_import|scheduler)[.]rs|"
            r"crates/domain/src/(?:context_frontier|imported_session|model_execution|"
            r"replace_session_defaults|review_workflow|runner|session|submit_input|"
            r"tool_execution|turn_eligibility)[.]rs|"
            r"crates/persistence/tests/(?:postgres_integration|review_workflow_postgres)[.]rs|"
            r"docs/spec/(?:conversation-import|model-call-execution|persistence-protocol|"
            r"process-protocol|review-workflows|sessions-and-transcript|"
            r"turn-lifecycle-and-scheduling)[.]md)$"
        ),
        re.compile(
            r"[A-Za-z0-9_]*Ownership[A-Za-z0-9_]*|ownership|\bowned\b|\bowning\b|"
            r"active slot owner|active-slot owner|slot owner|lease owner|"
            r"current-defaults pointer owner|selected defaults-row owner|stored defaults-row owner|"
            r"cross-wired defaults owner|event owner must match|observation owner must match|"
            r"attachment owner must match|terminal-record owner|owner projection|"
            r"owner replacement|owner facts|operation-owner facts|"
            r"owner cross-wired|OwnerMismatch|OwnerIDs?|"
            r"ModelCallOwners|attempt_owners|wrong_owner|wrong_terminal_owner|"
            r"cross_wired_[A-Za-z0-9_]+_owner|"
            r"foreign(?:_[A-Za-z0-9_]+)?_owner|different_owner|returned_owner|"
            r"one owner for bounds|current owner prevents|one owner:|owner must equal|owner, defaults owner|"
            r"cross-wired owner|foreign-owner|foreign owner|member.s owner|"
            r"let owner = stored[.]owning_session|owner != session|owner, non-successor|"
            r"already-claimed owner|process-lifetime root owner|extension owners|"
            r"named as owner|validated owner|checked owner placement|file-owner",
            re.IGNORECASE,
        ),
    ),
    Allowance(
        "GitHub process-governance test fixtures",
        re.compile(r"^crates/(?:tools-github/tests/live_smoke|tools-code-host/src/code_host/review_slog/inventory)[.]rs$"),
        re.compile(r"owner gate|owner[- ]ratif|owner judgment|owner answered", re.IGNORECASE),
    ),
)

class InventoryError(RuntimeError):
    """Git could not provide a trustworthy tracked-file inventory."""

def tracked_files(root: Path) -> list[Path]:
    result = subprocess.run(
        ["git", "ls-files", "-z", "--", *SCAN_ROOTS],
        cwd=root,
        check=False,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        detail = result.stderr.strip() or "git ls-files failed"
        raise InventoryError(detail)
    labels = [label for label in result.stdout.split("\0") if label]
    if not labels:
        raise InventoryError("git ls-files returned no vocabulary inputs")
    return [root / label for label in labels]

def violations(root: Path) -> list[str]:
    failures: list[str] = []
    for path in tracked_files(root):
        relative = path.relative_to(root).as_posix()
        try:
            text = path.read_text(encoding="utf-8")
        except UnicodeDecodeError:
            continue
        for number, line in enumerate(text.splitlines(), 1):
            matches = tuple(OWNER.finditer(line))
            if not matches:
                continue
            if all(
                any(allowance.covers(relative, line, match) for allowance in ALLOWLIST)
                for match in matches
            ):
                continue
            failures.append(f"{relative}:{number}: {line.strip()}")
    return failures

def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument("--show-allowlist", action="store_true")
    args = parser.parse_args()
    if args.show_allowlist:
        for allowance in ALLOWLIST:
            print(allowance.name)
        return 0
    try:
        failures = violations(args.root.resolve())
    except (InventoryError, OSError) as error:
        print(f"user-vocabulary check failed: {error}", file=sys.stderr)
        return 1
    if failures:
        print("role-sense owner vocabulary is forbidden:")
        for failure in failures:
            print(f"  - {failure}")
        print("Rename the human principal to user or extend the reviewed homonym allowlist.")
        return 1
    print("user-vocabulary check passed")
    return 0

if __name__ == "__main__":
    raise SystemExit(main())

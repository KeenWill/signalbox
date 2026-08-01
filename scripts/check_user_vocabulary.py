#!/usr/bin/env python3
"""Reject retired role vocabulary and ambiguous human/wire-role prose."""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

SCAN_ROOTS = ("crates", "apps", "clients", "docs/spec")
OWNER = re.compile(
    r"\b(?![A-Za-z0-9]*(?:ownership|OWNERSHIP|Ownership)[A-Za-z0-9]*\b)"
    r"[A-Za-z0-9]*(?:owner|OWNER|Owner)[A-Za-z0-9]*\b|"
    r"(?i:\bowners?\d*(?:\b|[_-])|owners?[_-]|(?<=[_-])owners?\d*(?:\b|[_-]))|"
    r"\b[Oo]wners?\d+[A-Z][A-Za-z0-9_]*|"
    r"\b[Oo]wners?[A-Z][A-Za-z0-9_]*|"
    r"\b[A-Za-z0-9_]+Owners?(?:[A-Z][A-Za-z0-9_]*)?"
)
BARE_USER_MESSAGE = re.compile(r"(?i:\buser[ \t\r\n]+messages?\b)")

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
            r"[\"']owner[\"']:\s*(?:arguments[.]repository\(\)[.]owner\(\)|"
            r"repository[.]owner\(\)|owner)(?:,|\s*$)|"
            r"\.owner\(\)|let owner = .*owner_end|\bowner_end\b|"
            r"let \(owner, name\) = repository|"
            r"Exact owner/repository|canonical `owner/repository`|"
            r"`@codex review` request by an owner, member, or collaborator|"
            r"association is `OWNER`, `MEMBER`, or `COLLABORATOR`|"
            r"matches!\(association, \"OWNER\" \| \"MEMBER\" \| \"COLLABORATOR\"\)|"
            r"author_association:\s*String::from\(\"OWNER\"\)|"
            r"author:.*[\"']owner[\"']|"
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
            r"(?:socket|listener|directory|file|root|state|parent|sidecar|spool|mode|"
            r"permissions|fixture|enrollment|durable).*owner-(?:only|private)|"
            r"owner-(?:only|private).*(?:socket|listener|directory|file|root|state|"
            r"parent|sidecar|spool|mode|permissions|fixture|enrollment|durable)|"
            r"unreadable, oversized, wrong-owner, wrong-mode|"
            r"unprivileged different owner cannot make a currently protected directory|"
            r"An untrusted owner, a non-sticky writable ancestor|"
            r"local process socket parent ancestry has an untrusted owner|"
            r"local process socket parent has the wrong owner|"
            r"stale local process socket has the wrong owner|"
            r"effective-user ownership|owner\s*==|"
            r"owner:\s*u32|child_owner|ParentOwnerMismatch|AncestorOwnerMismatch|"
            r"ExistingSocketOwnerMismatch|PeerOwnerMismatch|\bOwnerMismatch\b|"
            r"ancestor_owner_is_trusted|ancestor_owner_must_be|file owner|"
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
        re.compile(
            r"^crates/persistence/migrations/(?:"
            r"202607180001_create_session|"
            r"202607180002_replace_session_defaults|"
            r"202607180003_submit_input|"
            r"202607180004_turn_lifecycle_storage|"
            r"202607200001_bounded_user_content|"
            r"202607220001_model_call_execution|"
            r"202607220005_stop_requests|"
            r"202607240001_conversation_import|"
            r"202607240002_imported_session_seed|"
            r"202607250001_tool_loop|"
            r"202607260101_session_metadata|"
            r"202607280002_review_workflow|"
            r"202607280202_metadata_command_issuer|"
            r"202607280302_review_workflow_commands|"
            r"202607280303_session_system_prompt|"
            r"202607280401_runner_protocol|"
            r"202608020001_review_orchestration|"
            r"202608020002_review_orchestration_command_recovery|"
            r"202608020003_runner_wire_contract"
            r")[.]sql$"
        ),
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
            r"candidate with the same owner and an identity different from|"
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
            r"test(?:AwaitingToolApproval|AwaitingToolRecovery|ToolReconciliation)"
            r"RequiresTerminalUsageOwner|"
            r"SignalboxSnapshot(?:Required)?ModelCallOwnership|"
            r"(?:unmatchedTerminal|forbidden)ModelCallOwners|"
            r"unmatchedTerminalModelCallOwnerIDs|"
            r"terminal owner, merely permit historical calls|"
            r"case owner\b|[.]required\([.]owner\)"
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
            r"\bowner_command_id\b"
        ),
    ),
    Allowance(
        "legacy PostgreSQL actor encodings",
        re.compile(
            r"^(?:crates/persistence/src/(?:session_metadata|submit_input)[.]rs|"
            r"crates/persistence/tests/(?:postgres_integration|"
            r"session_metadata_postgres)[.]rs|"
            r"docs/spec/identity-and-commands[.]md)$"
        ),
        re.compile(
            r"\(\"owner\", (?:None|Some\(_\))(?:, None)?\)|"
            r"\"owner\" \| \"model\"|kind:\s*\"owner\"|"
            r"String::from\(\"owner\"\)|expected_issuer\s*=\s*\(\"owner\"|"
            r"\(`owner`/(?:`model`|`tool`)"
        ),
    ),
    Allowance(
        "legacy PostgreSQL SQL actor literals",
        re.compile(
            r"^crates/persistence/tests/(?:postgres_integration|"
            r"session_metadata_postgres)[.]rs$"
        ),
        re.compile(r"(?:^|,|=\s*)\s*'owner'(?=\s*[,)]|$)"),
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
            r"acceptance positions, typed priority relations, and active-slot owner are|"
            r"active slot owner[.]$|sole active slot owner, when present|"
            r"exact active slot owner[.]$|records the active slot owner, stale|"
            r"has no active slot owner[.]$|start binding, slot owner, or attempt after rollback|"
            r"runner recorded as the lease owner|"
            r"current-defaults pointer owner|selected defaults-row owner|stored defaults-row owner|"
            r"a cross-wired defaults owner must fail|"
            r"event owner must match the aggregate finding|"
            r"observation owner must match the aggregate link|"
            r"attachment owner must match the aggregate link|terminal-record owner,|"
            r"loss before and after pin, owner replacement|complete owner facts|"
            r"operation-owner facts|"
            r"(?:defaults|pending steering|snapshot) owner cross-wired|"
            r"OwnerMismatch|OwnerIDs?|"
            r"ModelCallOwners|attempt_owners|wrong_owner|wrong_terminal_owner|"
            r"cross_wired_attempt_owner|cross_wired_defaults_owner|"
            r"foreign_attachment_owner|foreign_event_owner|foreign_observation_owner|"
            r"foreign_owner|different_owner|returned_owner|"
            r"against the current owner prevents evidence-free phase reconstruction|"
            r"exactly one owner: activation=|durable boundary must have one owner:|"
            r"event owner must equal the loaded finding|"
            r"attachment owner must equal the loaded external link|"
            r"observation owner must equal the loaded external link|"
            r"owner, defaults owner, or selected-version mismatch|"
            r"a cross-wired owner, non-successor|foreign-owner rejection|"
            r"foreign owner, run-workflow or policy mismatch|"
            r"agreement between every member.s owner and header|"
            r"let owner = stored[.]owning_session|owner != session|"
            r"second process-lifetime root owner must fail closed|extension owners in|"
            r"turn named as owner by the active-phase record|"
            r"complete owner projection derives the (?:prepared|running) attempt|"
            r"validated owner projection|inside the validated owner$|"
            r"the checked owner placement|application-level file-owner proof",
            re.IGNORECASE,
        ),
    ),
    Allowance(
        "GitHub process-governance test fixtures",
        re.compile(r"^crates/(?:tools-github/tests/live_smoke|tools-code-host/src/code_host/review_slog/inventory)[.]rs$"),
        re.compile(
            r"The owner answered the pending question|"
            r"owner-ratified interrupt deferral|"
            r"matching-Interrupt milestone choice was an owner gate|"
            r"Owner-ratified matching-interrupt milestone deferral|"
            r"owner judgment because the current slices|"
            r"an owner gate and should have blocked and been reported|"
            r"owner ratifies the current nonclaiming|"
            r"equivalent owner gate instead of deciding it|"
            r"Restrict the owner gate to the affected track|"
            r"owner gate should have blocked the entire autonomous run|"
            r"owner gate blocks and is reported on the affected matching-interrupt track",
            re.IGNORECASE,
        ),
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
        lines = text.splitlines()
        for match in BARE_USER_MESSAGE.finditer(text):
            number = text.count("\n", 0, match.start()) + 1
            failures.append(f"{relative}:{number}: {lines[number - 1].strip()}")
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
        print("retired or ambiguous role vocabulary is forbidden:")
        for failure in failures:
            print(f"  - {failure}")
        print(
            "Rename the human principal to user, distinguish a user-role message "
            "from a message from the user, or extend the reviewed homonym allowlist."
        )
        return 1
    print("user-vocabulary check passed")
    return 0

if __name__ == "__main__":
    raise SystemExit(main())

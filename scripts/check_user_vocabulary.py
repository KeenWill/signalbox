#!/usr/bin/env python3
"""Reject retired role vocabulary and ambiguous human/wire-role prose."""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from collections.abc import Iterator
from dataclasses import dataclass
from pathlib import Path
from typing import Mapping

SCAN_ROOTS = ("crates", "apps", "clients", "docs/spec")
IDENTIFIER = re.compile(r"[A-Za-z0-9_]+")
OWNER_FRAGMENT = re.compile("owner", re.IGNORECASE)
CROSS_FRAGMENT_OWNER = re.compile(r"(?:Unknown|Known)(?:Error|Rejection)")
BARE_USER_MESSAGE = re.compile(r"(?i:\buser[ \t\r\n]+message\b)")
def owner_matches(line: str) -> Iterator[re.Match[str]]:
    for identifier in IDENTIFIER.finditer(line):
        token = identifier.group()
        fragments = tuple(OWNER_FRAGMENT.finditer(token))
        cross_fragments = tuple(CROSS_FRAGMENT_OWNER.finditer(token))
        if any(
            not any(
                cross.start() <= fragment.start()
                and cross.end() >= fragment.end()
                for cross in cross_fragments
            )
            for fragment in fragments
        ):
            yield identifier


@dataclass(frozen=True)
class Allowance:
    """One reviewed non-role use of owner vocabulary."""

    name: str
    paths: re.Pattern[str]
    lines: re.Pattern[str]
    enclosing_blocks: Mapping[str, str] | None = None
    enclosing_functions: Mapping[str, frozenset[str]] | None = None
    previous_line: re.Pattern[str] | None = None

    @staticmethod
    def _inside_block(
        source_lines: list[str], index: int, expected_block: str
    ) -> bool:
        start = next(
            (
                candidate
                for candidate in range(index - 1, -1, -1)
                if source_lines[candidate].strip() == expected_block
            ),
            None,
        )
        if start is None:
            return False
        return not any(
            re.fullmatch(r"}\s*(?://.*)?", candidate)
            for candidate in source_lines[start + 1 : index]
        )

    @staticmethod
    def _inside_function(
        source_lines: list[str], index: int, expected_names: frozenset[str]
    ) -> bool:
        for candidate in range(index - 1, -1, -1):
            declaration = re.match(
                r"^(?P<indent>\s*)(?:async\s+)?fn\s+(?P<name>[A-Za-z0-9_]+)\b",
                source_lines[candidate],
            )
            if declaration is None:
                continue
            if declaration.group("name") not in expected_names:
                return False
            if "}" in source_lines[candidate][declaration.end() :]:
                return False
            indentation = len(declaration.group("indent"))
            return not any(
                line.strip() == "}"
                and len(line) - len(line.lstrip()) <= indentation
                for line in source_lines[candidate + 1 : index]
            )
        return False

    def covers(
        self,
        path: str,
        source_lines: list[str],
        index: int,
        match: re.Match[str],
    ) -> bool:
        if self.paths.search(path) is None:
            return False
        line = source_lines[index]
        if self.enclosing_blocks is not None:
            expected_block = self.enclosing_blocks.get(path)
            if expected_block is None or not self._inside_block(
                source_lines, index, expected_block
            ):
                return False
        if self.enclosing_functions is not None:
            expected_names = self.enclosing_functions.get(path)
            if expected_names is None or not self._inside_function(
                source_lines, index, expected_names
            ):
                return False
        if self.previous_line is not None and (
            index == 0
            or self.previous_line.fullmatch(source_lines[index - 1]) is None
        ):
            return False
        token_start = match.start()
        while token_start > 0 and (
            line[token_start - 1].isalnum() or line[token_start - 1] == "_"
        ):
            token_start -= 1
        token_end = match.end()
        while token_end < len(line) and (
            line[token_end].isalnum() or line[token_end] == "_"
        ):
            token_end += 1
        return any(
            allowed.start() <= token_start and allowed.end() >= token_end
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
            r"owner/repository|owner/name|repos/owner/|"
            r"(?:arguments[.]repository[(][)]|repository)[.]owner[(][)]|"
            r"let owner = &value\[\.\.owner_end\]|\bowner_end\b|"
            r"let \(owner, name\) = repository|"
            r"Exact owner/repository|canonical `owner/repository`|"
            r"`@codex review` request by an owner, member, or collaborator|"
            r"association is `OWNER`, `MEMBER`, or `COLLABORATOR`|"
            r"matches!\(association, \"OWNER\" \| \"MEMBER\" \| \"COLLABORATOR\"\)|"
            r"author_association:\s*String::from\(\"OWNER\"\)|"
            r"valid_repository_segment\(owner\)",
            re.IGNORECASE,
        ),
    ),
    Allowance(
        "GitHub repository GraphQL owner variables",
        re.compile(
            r"^(?:apps/signalboxd/src/repo_watch_runtime[.]rs|"
            r"crates/tools-github/src/lib[.]rs|"
            r"crates/tools-code-host/src/code_host/github[.]rs)$"
        ),
        re.compile(
            r"^query (?:PullRequestReviewThreads|ReviewThreads|Convergence|"
            r"ThreadInventory)\(\$owner: String!,|"
            r"^\s*repository\(owner: \$(?:namespace|owner), name: \$name\) \{\s*$"
        ),
    ),
    Allowance(
        "GitHub stack GraphQL owner declarations",
        re.compile(r"^crates/tools-code-host/src/code_host/github[.]rs$"),
        re.compile(r"^\s*\$owner: String!\s*$"),
        previous_line=re.compile(
            r"^query (?:StackComparison|StackChildren)\(\s*$"
        ),
    ),
    Allowance(
        "GitHub repository-coordinate request fields",
        re.compile(
            r"^(?:crates/tools-github/src/lib[.]rs|"
            r"crates/tools-code-host/src/code_host/github[.]rs)$"
        ),
        re.compile(
            r"[\"']owner[\"']:\s*(?:arguments[.]repository\(\)[.]owner\(\)|"
            r"repository[.]owner\(\)|owner)(?:,|\s*$)"
        ),
    ),
    Allowance(
        "GitHub repository-coordinate method declarations",
        re.compile(
            r"^(?:crates/tools-github/src/lib[.]rs|"
            r"crates/tools-code-host/src/code_host/arguments[.]rs)$"
        ),
        re.compile(
            r"^\s*(?:fn|pub[(]super[)] fn) owner[(]&self[)] -> &str [{]\s*$"
        ),
        {
            "crates/tools-github/src/lib.rs": "impl GitHubRepository {",
            "crates/tools-code-host/src/code_host/arguments.rs":
                "impl CodeHostRepository {",
        },
    ),
    Allowance(
        "GitHub fixture author usernames",
        re.compile(
            r"^crates/tools-code-host/src/code_host/review_slog/convergence[.]rs$"
        ),
        re.compile(
            r'^\s*author:\s*Some\(String::from\("owner"\)\),\s*$'
        ),
    ),
    Allowance(
        "Unix file-owner and permission semantics",
        re.compile(
            r"^(?:apps/signalbox-runner/src/(?:configuration|dispatch_https|main|protocol|state|workspace)[.]rs|"
            r"apps/signalboxd/src/(?:local_socket|runner_protocol_runtime)[.]rs|"
            r"apps/signalboxd/tests/process_substrate[.]rs|"
            r"crates/model-runtime-(?:claude|codex)-cli/tests/live_smoke[.]rs|"
            r"docs/spec/(?:configuration-and-credentials|process-protocol|"
            r"runner-protocol)[.]md)$"
        ),
        re.compile(
            r"(?:socket|listener|directory|file|root|state|storage|store|descriptor|"
            r"connection|parent|sidecar|spool|mode|permissions|fixture|enrollment|"
            r"durable)(?:(?!owner).)*"
            r"owner-(?:only|private)|"
            r"owner-(?:only|private)(?:(?!owner).)*(?:socket|listener|directory|file|root|state|"
            r"storage|store|descriptor|connection|parent|sidecar|spool|mode|permissions|"
            r"fixture|enrollment|durable)|"
            r"unreadable, oversized, wrong-owner, wrong-mode|"
            r"unprivileged different owner cannot make a currently protected directory|"
            r"An untrusted owner, a non-sticky writable ancestor|"
            r"local process socket parent ancestry has an untrusted owner|"
            r"local process socket parent has the wrong owner|"
            r"stale local process socket has the wrong owner|"
            r"effective-user ownership|"
            r"^\s*fn ancestor_owner_is_trusted\(owner: u32, effective_user: u32\) -> bool \{\s*$|"
            r"^\s*owner == 0 \|\| owner == effective_user\s*$|"
            r"child_owner|ParentOwnerMismatch|AncestorOwnerMismatch|"
            r"ExistingSocketOwnerMismatch|PeerOwnerMismatch|\bOwnerMismatch\b|"
            r"ancestor_owner_is_trusted|"
            r"ancestor_owner_must_be_root_or_the_effective_user|file owner|"
            r"dropping the owner|its owner, so it cannot shadow|"
            r"owner-vs-other|"
            r"endpoint fixture root is owner-private|"
            r"concurrent winner directory is owner-private|"
            r"real runner-root fixture is owner-private|"
            r"trash fixture is owner-private|"
            r"live owner connection|"
            r"guarded_bind_listens_only_with_owner_access",
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
            r"202608020003_runner_wire_contract|"
            r"202608020015_llm_delegated_tool_approval|"
            r"202608020018_session_delegation"
            r")[.]sql$"
        ),
        re.compile(
            r"(?<![A-Za-z0-9_])(?:"
            r"imported_conversation_raw_record_owner_fk|"
            r"imported_transcript_entry_owner_fk|"
            r"imported_transcript_entry_owner_identity_key|"
            r"owner_command_id|owner_command|owner_initiated|"
            r"owner_tool_approval_requires_command|"
            r"review_pass_produced_finding_owner|"
            r"tool_approval_decision_owner_command_fk|owner"
            r")(?![A-Za-z0-9_])",
            re.IGNORECASE,
        ),
    ),
    Allowance(
        # The migration that retires the stored spelling has to name it in
        # order to rename it: it renames the column and the objects named
        # after it, rewrites the stored values, and records which four
        # record-ownership constraint names it deliberately leaves alone.
        # Every other reference to these spellings is now a failure.
        "storage vocabulary rename migration",
        re.compile(
            r"^crates/persistence/migrations/"
            r"202608110001_user_role_storage_vocabulary[.]sql$"
        ),
        re.compile(
            r"(?<![A-Za-z0-9_])(?:"
            r"imported_conversation_raw_record_owner_fk|"
            r"imported_transcript_entry_owner_fk|"
            r"imported_transcript_entry_owner_identity_key|"
            r"tool_approval_decision_owner_command_id_key|"
            r"tool_approval_decision_owner_command_fk|"
            r"owner_tool_approval_requires_command|"
            r"review_pass_produced_finding_owner|"
            r"owner_command_id|owner_command|owner_initiated|owner"
            r")(?![A-Za-z0-9_])",
            re.IGNORECASE,
        ),
    ),
    Allowance(
        # Fixtures that seed a database standing at an earlier migration, so
        # that a later migration can be exercised against real rows. The
        # CHECK constraints in force at that point admit only the retired
        # spelling, so writing the current one fails with `23514` before the
        # migration under test ever runs. This is not compatibility with the
        # retired vocabulary: it is the evidence that the rename upgrades data
        # that was written before it, and every one of these lines is inside a
        # test whose pool is deliberately held short of the rename.
        "pre-rename migration fixtures",
        re.compile(
            r"^crates/persistence/tests/(?:conversation_import_postgres|"
            r"runner_protocol_postgres|"
            r"postgres_integration/(?:approval_decisions|"
            r"model_call_execution_and_recovery))[.]rs$"
        ),
        re.compile(
            r'^const RETIRED_CREATION_CAUSE: &str = "owner_initiated";$|'
            r'^\s*"owner_initiated",$|'
            r"^\s*\(request_id, decision_kind, decision_source, "
            r"owner_command_id\)$|"
            r"^\s*VALUES \(\$1, 'approve', 'owner_command', \$2\)\",$|"
            r"^\s*'owner_initiated', 'none'\);$|"
            r"^\s*'create_session', 1, 'owner_initiated', 'none', 1,$|"
            r"^\s*'owner', NULL, NULL, 'text', 'queued before migration',$"
        ),
    ),
    Allowance(
        "imported-conversation record owner identifiers",
        re.compile(
            r"^crates/(?:application/src/conversation_import|"
            r"domain/src/imported_conversation)[.]rs$"
        ),
        re.compile(
            r"^\s*owner:\s*ImportedConversationId,\s*$|"
            r"^\s*self[.]owner,\s*$|"
            r"^\s*let owner = conversation\(\d+\);\s*$|"
            r"self[.]observed[.]push\(\(owner, source[.]to_vec\(\)\)\);|"
            r"self[.]returned_owner[.]unwrap_or\(owner\)|"
            r"self[.]convert\(owner, source, next_entry_id\)\?",
        ),
    ),
    Allowance(
        "imported-conversation converter fixture owner members",
        re.compile(r"^crates/application/src/conversation_import[.]rs$"),
        re.compile(
            r"^\s*returned_owner: Option<ImportedConversationId>,\s*$|"
            r"^\s*self[.]returned_owner[.]unwrap_or\(owner\),\s*$|"
            r"^\s*returned_owner: None,\s*$|"
            r"^\s*service[.]converter[.]returned_owner = Some\(converted\);\s*$"
        ),
    ),
    Allowance(
        "imported-conversation fixture owner arguments",
        re.compile(
            r"^crates/(?:application/src/conversation_import|"
            r"domain/src/imported_conversation)[.]rs$"
        ),
        re.compile(r"^\s*owner,\s*$"),
        enclosing_functions={
            "crates/application/src/conversation_import.rs": frozenset(
                {"converted"}
            ),
            "crates/domain/src/imported_conversation.rs": frozenset(
                {
                    "new",
                    "claude_code_summary_fixture",
                    "claude_code_user_text_fixture",
                    "codex_session_meta_fixture",
                    "converted",
                    "inv002_inv038_raw_hash_corruption_retains_complete_input",
                    "inv038_complete_normalized_record_rejects_129_containers",
                    "inv038_coordinated_normalized_and_entry_corruption_fails_closed",
                    "inv038_empty_raw_source_record_fails_closed",
                    "inv038_entry_carried_structured_value_rejects_129_containers",
                    "inv038_entry_content_must_match_the_complete_normalized_record",
                    "inv038_entry_metadata_must_match_the_complete_normalized_record",
                    "inv038_first_entry_cannot_skip_first_raw_record",
                    "inv038_message_content_without_source_speaker_fails_closed",
                    "inv038_message_speaker_must_match_the_raw_record_type",
                    "inv038_raw_record_entry_count_must_match_its_normalized_projection",
                    "inv038_source_event_rejects_a_message_record_type",
                    "s28_inv002_inv038_checks_raw_depth_before_recursive_conversion_digest",
                    "s28_inv002_inv038_converted_raw_depth_fails_closed_and_drops_safely",
                    "s28_inv038_converter_versions_do_not_reinterpret_result_blocks",
                    "s28_inv038_converter_versions_do_not_reinterpret_source_blocks",
                }
            ),
        },
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
            r"terminal owner, merely permit historical calls"
        ),
    ),
    Allowance(
        "native model-call usage ownership enum case",
        re.compile(
            r"^clients/native/Sources/SignalboxClient/"
            r"SessionSynchronization[.]swift$"
        ),
        re.compile(r"^\s*case owner\s*$"),
        enclosing_blocks={
            "clients/native/Sources/SignalboxClient/SessionSynchronization.swift":
                "private enum SignalboxSnapshotRequiredModelCallOwnership {"
        },
    ),
    Allowance(
        "native model-call usage ownership expressions",
        re.compile(
            r"^clients/native/Sources/SignalboxClient/"
            r"SessionSynchronization[.]swift$"
        ),
        re.compile(
            r"^\s*case [.]required\([.]owner\):\s*$|"
            r"^\s*return [.]required\([.]owner\)\s*$|"
            r"^\s*case [.]impossible, [.]permitted, [.]required\([.]owner\):\s*$"
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
        "Rust borrow/ownership phrasing",
        re.compile(r"^(?:crates|apps|clients|docs/spec)/"),
        re.compile(
            r"(?<![A-Za-z0-9_])"
            r"(?![A-Za-z0-9_]*owner[A-Za-z0-9_]*owner)"
            r"[A-Za-z0-9_]*ownership[A-Za-z0-9_]*"
            r"(?![A-Za-z0-9_])",
            re.IGNORECASE,
        ),
    ),
    Allowance(
        "domain-record ownership phrasing",
        re.compile(
            r"^(?:apps/signalbox-runner/src/state[.]rs|"
            r"apps/signalboxd/src/runner_protocol_runtime[.]rs|"
            r"apps/signalboxd/tests/process_protocol_runtime[.]rs|"
            r"clients/native/Sources/SignalboxClient/SessionSynchronization[.]swift|"
            r"crates/application/src/(?:conversation_import|scheduler)[.]rs|"
            r"crates/domain/src/(?:context_frontier|imported_session|model_execution|"
            r"replace_session_defaults|review_workflow|runner|session|submit_input|"
            r"tool_execution|turn_eligibility)[.]rs|"
            r"crates/persistence/tests/(?:postgres_integration/[a-z_]+|"
            r"review_workflow_postgres|runner_protocol_postgres)[.]rs|"
            r"docs/spec/(?:conversation-import|model-call-execution|persistence-protocol|"
            r"process-protocol|review-workflows|sessions-and-transcript|"
            r"turn-lifecycle-and-scheduling)[.]md)$"
        ),
        re.compile(
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
            r"validate_operation_journal_owner|"
            r"[A-Za-z0-9_]*lost_cleanup_owner[A-Za-z0-9_]*|"
            r"lost cleanup owner|"
            r"live owner connection|"
            r"only command owner|"
            r"(?:defaults|pending steering|snapshot) owner cross-wired|"
            r"OwnerMismatch|"
            r"ModelCallOwners|attempt_owners|wrong_owner|wrong_terminal_owner|"
            r"cross_wired_attempt_owner|cross_wired_defaults_owner|"
            r"foreign_attachment_owner|foreign_event_owner|foreign_observation_owner|"
            r"foreign_owner|different_owner|"
            r"s29_inv040_finding_history_rejects_foreign_event_owner|"
            r"inv040_external_link_rejects_foreign_observation_owner|"
            r"inv041_external_link_rejects_foreign_attachment_owner|"
            r"s03_active_reconstitution_rejects_cross_wired_attempt_owner|"
            r"inv040_finding_event_rejects_foreign_owner|"
            r"inv041_external_attachment_rejects_foreign_owner|"
            r"inv040_external_observation_rejects_foreign_owner|"
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

def audit(root: Path) -> list[str]:
    failures: list[str] = []
    for path in tracked_files(root):
        relative = path.relative_to(root).as_posix()
        try:
            text = path.read_text(encoding="utf-8")
        except UnicodeDecodeError:
            continue
        lines = text.splitlines()
        for index, line in enumerate(lines):
            number = index + 1
            matches = tuple(owner_matches(line))
            if not matches:
                continue
            if all(
                any(
                    allowance.covers(relative, lines, index, match)
                    for allowance in ALLOWLIST
                )
                for match in matches
            ):
                continue
            failures.append(f"{relative}:{number}: {line.strip()}")
        for match in BARE_USER_MESSAGE.finditer(text):
            number = text.count("\n", 0, match.start()) + 1
            failures.append(f"{relative}:{number}: {lines[number - 1].strip()}")
    return failures


def main() -> int:
    repository_root = Path(__file__).resolve().parents[1]
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, default=repository_root)
    parser.add_argument("--show-allowlist", action="store_true")
    args = parser.parse_args()
    if args.show_allowlist:
        for allowance in ALLOWLIST:
            print(allowance.name)
        return 0
    root = args.root.resolve()
    try:
        failures = audit(root)
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

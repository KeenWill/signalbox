#!/usr/bin/env python3
"""Client-fed Signalbox review-orchestration driver.

The daemon remains the authority for review state.  This process derives stable
identities from the immutable pull-request snapshot and replays commands after
an interruption instead of keeping a second state file.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import socket
import subprocess
import sys
import tempfile
import time
import uuid
from dataclasses import dataclass
from pathlib import Path
from typing import Protocol, Sequence


PROVIDER = "github"
CONCERN_SET_VERSION = "initial-five-v1"
STAGE_TEMPLATES = {
    "import": "review-import",
    "judgment": "review-judgment",
    "repair": "review-repair",
    "publication": "review-publication",
}
CONCERNS = (
    ("correctness", "review-concern-correctness"),
    ("interface-and-type-design", "review-concern-interface-and-type-design"),
    ("test-quality", "review-concern-test-quality"),
    ("security", "review-concern-security"),
    ("documentation-code-drift", "review-concern-documentation-code-drift"),
)
IDENTITY_NAMESPACE = uuid.UUID("75b65a9c-bccf-5b68-a921-f73bb1ccbb90")
REPOSITORY_PATTERN = re.compile(
    r"[A-Za-z0-9](?:[A-Za-z0-9_.-]*[A-Za-z0-9])?/[A-Za-z0-9_.-]+\Z"
)
ACTIVE_TURN_STATES = {
    "active_running",
    "active_awaiting_model_call_recovery",
    "active_awaiting_tool_approval",
    "active_awaiting_child",
    "active_awaiting_tool_recovery",
    "active_awaiting_runner_recovery",
}
TERMINAL_TURN_STATES = {
    "completed",
    "failed",
    "refused",
    "cancelled",
    "reconciliation_required",
    "tool_reconciliation_required",
}


class DriverFailure(Exception):
    """One typed failure safe for shell automation to classify."""

    def __init__(self, code: str, stage: str, detail: str):
        super().__init__(detail)
        self.code = code
        self.stage = stage
        self.detail = detail

    def line(self) -> str:
        detail = json.dumps(self.detail, ensure_ascii=True, separators=(",", ":"))
        return f"REVIEW_DRIVER_FAILURE code={self.code} stage={self.stage} detail={detail}"


@dataclass(frozen=True)
class PullRequestFacts:
    repository: str
    number: int
    head_sha: str
    base_sha: str
    head_repository: str
    head_branch: str
    base_branch: str


@dataclass(frozen=True)
class AttemptIdentities:
    target: str
    attempt: str


@dataclass(frozen=True)
class PassIdentities:
    commission_command: str
    run: str
    review_pass: str
    start_command: str
    activate_command: str
    complete_command: str
    stage_outcome_command: str


@dataclass(frozen=True)
class TranscriptSnapshot:
    session_id: str
    accepted_input_id: str | None
    turn_id: str | None
    turn_state: str | None
    terminal_frontier_id: str | None
    assistant_text: str


@dataclass(frozen=True)
class CompletedPass:
    identities: PassIdentities
    session_id: str
    accepted_input_id: str
    turn_id: str
    turn_state: str
    terminal_frontier_id: str | None
    assistant_text: str


class GitHubBoundary(Protocol):
    def read_pull_request(self, repository: str, number: int) -> PullRequestFacts: ...


class ReviewCliBoundary(Protocol):
    def create_target(self, facts: PullRequestFacts, target_id: str, command_id: str) -> None: ...

    def start_orchestration(
        self, target_id: str, attempt_id: str, command_id: str
    ) -> None: ...

    def read_orchestration_state(self, attempt_id: str) -> str: ...

    def start_run(
        self,
        target_id: str,
        identities: PassIdentities,
        workflow: str,
        session_id: str,
        accepted_input_id: str,
    ) -> None: ...

    def activate_pass(self, identities: PassIdentities, turn_id: str) -> None: ...

    def complete_pass(self, completed: CompletedPass, outcome: str) -> None: ...

    def record_import_outcome(
        self,
        attempt_id: str,
        completed: CompletedPass,
        outcome: str,
        context_digest: str | None,
    ) -> None: ...

    def record_concern_outcome(
        self, attempt_id: str, concern: str, completed: CompletedPass, outcome: str
    ) -> None: ...


class SessionBoundary(Protocol):
    def commission(
        self,
        facts: PullRequestFacts,
        template: str,
        command_id: str,
        statement: str,
        content: str,
    ) -> str: ...

    def transcript(
        self, session_id: str, turn_id: str | None = None
    ) -> TranscriptSnapshot: ...


def stable_id(facts: PullRequestFacts, role: str) -> str:
    material = (
        f"signalbox-review-driver:v1:{facts.repository}:{facts.number}:"
        f"{facts.head_sha}:{facts.base_sha}:{role}"
    )
    return str(uuid.uuid5(IDENTITY_NAMESPACE, material))


def attempt_identities(facts: PullRequestFacts) -> AttemptIdentities:
    return AttemptIdentities(
        target=stable_id(facts, "target"),
        attempt=stable_id(facts, "orchestration-attempt"),
    )


def pass_identities(facts: PullRequestFacts, stage: str) -> PassIdentities:
    return PassIdentities(
        commission_command=stable_id(facts, f"{stage}:commission-command"),
        run=stable_id(facts, f"{stage}:run"),
        review_pass=stable_id(facts, f"{stage}:pass"),
        start_command=stable_id(facts, f"{stage}:start-command"),
        activate_command=stable_id(facts, f"{stage}:activate-command"),
        complete_command=stable_id(facts, f"{stage}:complete-command"),
        stage_outcome_command=stable_id(facts, f"{stage}:outcome-command"),
    )


class GitHubCli:
    def __init__(self, executable: str = "gh") -> None:
        self.executable = executable

    def read_pull_request(self, repository: str, number: int) -> PullRequestFacts:
        command = [
            self.executable,
            "api",
            f"repos/{repository}/pulls/{number}",
        ]
        result = run_process(command, "github-facts")
        try:
            payload = json.loads(result.stdout)
            head = payload["head"]
            base = payload["base"]
            return PullRequestFacts(
                repository=repository,
                number=number,
                head_sha=head["sha"],
                base_sha=base["sha"],
                head_repository=head["repo"]["full_name"],
                head_branch=head["ref"],
                base_branch=base["ref"],
            )
        except (KeyError, TypeError, json.JSONDecodeError) as error:
            raise DriverFailure(
                "github-response-invalid",
                "github-facts",
                f"gh returned an invalid pull-request object: {error}",
            ) from error


class SignalboxCli:
    def __init__(self, socket_path: Path, executable: str = "signalbox") -> None:
        self.socket_path = socket_path
        self.executable = executable

    def _review(self, arguments: Sequence[str], stage: str) -> str:
        command = [
            self.executable,
            "--socket",
            str(self.socket_path),
            "review",
            *arguments,
        ]
        return run_process(command, stage).stdout

    def create_target(self, facts: PullRequestFacts, target_id: str, command_id: str) -> None:
        self._review(
            [
                "create-target",
                target_id,
                "--provider",
                PROVIDER,
                "--repository",
                facts.repository,
                "--change-request",
                str(facts.number),
                "--head-revision",
                facts.head_sha,
                "--base-revision",
                facts.base_sha,
                "--command-id",
                command_id,
            ],
            "create-target",
        )

    def start_orchestration(
        self, target_id: str, attempt_id: str, command_id: str
    ) -> None:
        concerns = {
            "concerns": [
                {"key": concern, "template_name": template}
                for concern, template in CONCERNS
            ]
        }
        with tempfile.NamedTemporaryFile(
            mode="w", encoding="utf-8", suffix=".json"
        ) as concerns_file:
            json.dump(concerns, concerns_file, separators=(",", ":"))
            concerns_file.flush()
            self._review(
                [
                    "start-orchestration",
                    attempt_id,
                    target_id,
                    "--concern-set-version",
                    CONCERN_SET_VERSION,
                    "--import-template-name",
                    STAGE_TEMPLATES["import"],
                    "--judgment-template-name",
                    STAGE_TEMPLATES["judgment"],
                    "--repair-template-name",
                    STAGE_TEMPLATES["repair"],
                    "--publication-template-name",
                    STAGE_TEMPLATES["publication"],
                    "--concerns-file",
                    concerns_file.name,
                    "--command-id",
                    command_id,
                ],
                "start-orchestration",
            )

    def read_orchestration_state(self, attempt_id: str) -> str:
        output = self._review(["read-orchestration", attempt_id], "read-orchestration")
        match = re.search(r"\bstate=([a-z_]+)\b", output)
        if match is None:
            raise DriverFailure(
                "cli-response-invalid",
                "read-orchestration",
                "signalbox output did not contain the orchestration state",
            )
        return match.group(1)

    def start_run(
        self,
        target_id: str,
        identities: PassIdentities,
        workflow: str,
        session_id: str,
        accepted_input_id: str,
    ) -> None:
        self._review(
            [
                "start-run",
                target_id,
                identities.run,
                identities.review_pass,
                "--workflow",
                workflow,
                "--session-id",
                session_id,
                "--accepted-input-id",
                accepted_input_id,
                "--command-id",
                identities.start_command,
            ],
            "start-run",
        )

    def activate_pass(self, identities: PassIdentities, turn_id: str) -> None:
        self._review(
            [
                "activate-pass",
                identities.run,
                identities.review_pass,
                "--turn-id",
                turn_id,
                "--command-id",
                identities.activate_command,
            ],
            "activate-pass",
        )

    def complete_pass(self, completed: CompletedPass, outcome: str) -> None:
        arguments = [
            "complete-pass",
            completed.identities.run,
            completed.identities.review_pass,
            "--outcome",
            outcome,
            "--turn-id",
            completed.turn_id,
        ]
        if outcome == "succeeded":
            if completed.terminal_frontier_id is None:
                raise DriverFailure(
                    "terminal-evidence-invalid",
                    "complete-pass",
                    "a completed turn did not expose its terminal frontier",
                )
            arguments.extend(
                ["--output-frontier-id", completed.terminal_frontier_id]
            )
        arguments.extend(["--command-id", completed.identities.complete_command])
        self._review(arguments, "complete-pass")

    def record_import_outcome(
        self,
        attempt_id: str,
        completed: CompletedPass,
        outcome: str,
        context_digest: str | None,
    ) -> None:
        arguments = [
            "record-import-outcome",
            attempt_id,
            "--outcome",
            outcome,
            "--pass-id",
            completed.identities.review_pass,
        ]
        if context_digest is not None:
            arguments.extend(["--context-digest", context_digest])
        arguments.extend(
            ["--command-id", completed.identities.stage_outcome_command]
        )
        self._review(arguments, "record-import-outcome")

    def record_concern_outcome(
        self, attempt_id: str, concern: str, completed: CompletedPass, outcome: str
    ) -> None:
        self._review(
            [
                "record-concern-outcome",
                attempt_id,
                concern,
                "--outcome",
                outcome,
                "--pass-id",
                completed.identities.review_pass,
                "--command-id",
                completed.identities.stage_outcome_command,
            ],
            f"concern-{concern}",
        )


class UnixSessionClient:
    def __init__(self, socket_path: Path) -> None:
        self.socket_path = socket_path

    def _request(self, request: dict[str, object], *, sequence: bool) -> list[dict[str, object]]:
        frame = {"version": 1, "request_id": "1", "request": request}
        encoded = json.dumps(frame, separators=(",", ":")).encode("utf-8") + b"\n"
        messages: list[dict[str, object]] = []
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
                connection.connect(str(self.socket_path))
                connection.sendall(encoded)
                reader = connection.makefile("rb")
                while True:
                    line = reader.readline()
                    if not line:
                        raise DriverFailure(
                            "socket-response-incomplete",
                            "session-socket",
                            "daemon closed the socket before the response completed",
                        )
                    response = json.loads(line)
                    if response.get("version") != 1 or response.get("request_id") != "1":
                        raise DriverFailure(
                            "socket-response-invalid",
                            "session-socket",
                            "daemon response version or request identity did not match",
                        )
                    message = response.get("message")
                    if not isinstance(message, dict) or not isinstance(message.get("type"), str):
                        raise DriverFailure(
                            "socket-response-invalid",
                            "session-socket",
                            "daemon response did not contain a typed message",
                        )
                    if message["type"] == "error":
                        raise DriverFailure(
                            "socket-command-rejected",
                            "session-socket",
                            f"daemon rejected request with {message.get('code', 'unknown')}",
                        )
                    messages.append(message)
                    if not sequence or message["type"] == "transcript_snapshot_end":
                        return messages
        except OSError as error:
            raise DriverFailure("socket-io", "session-socket", str(error)) from error
        except json.JSONDecodeError as error:
            raise DriverFailure(
                "socket-response-invalid", "session-socket", str(error)
            ) from error

    def commission(
        self,
        facts: PullRequestFacts,
        template: str,
        command_id: str,
        statement: str,
        content: str,
    ) -> str:
        messages = self._request(
            {
                "type": "commission_session",
                "command_id": command_id,
                "template_name": template,
                "fence": {
                    "target": "pull_request",
                    "repository": facts.repository,
                    "pull_request": str(facts.number),
                    "head_sha": facts.head_sha,
                    "head_repository": facts.head_repository,
                    "head_branch": facts.head_branch,
                    "base_branch": facts.base_branch,
                },
                "statement": statement,
                "content": content,
            },
            sequence=False,
        )
        message = messages[0]
        session_id = message.get("session_id")
        if message["type"] != "session_commissioned" or not isinstance(session_id, str):
            raise DriverFailure(
                "socket-response-invalid",
                "commission-session",
                "daemon did not return a commissioned-session receipt",
            )
        return session_id

    def transcript(
        self, session_id: str, turn_id: str | None = None
    ) -> TranscriptSnapshot:
        messages = self._request(
            {"type": "read_transcript", "session_id": session_id}, sequence=True
        )
        return transcript_snapshot(messages, session_id, turn_id)


def transcript_snapshot(
    messages: Sequence[dict[str, object]],
    session_id: str,
    selected_turn_id: str | None = None,
) -> TranscriptSnapshot:
    """Project one exact turn from a possibly multi-turn session transcript."""
    positions: dict[str, int] = {}
    states: dict[str, str] = {}
    frontiers: dict[str, str] = {}
    accepted_inputs: dict[str, str] = {}
    assistant_entry_turns: dict[str, str] = {}
    assistant_fragments: dict[str, list[str]] = {}
    for message in messages:
        message_type = message["type"]
        if message_type == "transcript_turn":
            candidate_turn = message.get("turn_id")
            position = message.get("acceptance_position")
            state = message.get("state")
            if (
                not isinstance(candidate_turn, str)
                or not isinstance(position, str)
                or not isinstance(state, dict)
                or not isinstance(state.get("type"), str)
            ):
                raise DriverFailure(
                    "socket-response-invalid",
                    "read-transcript",
                    "daemon transcript contained an invalid turn projection",
                )
            try:
                positions[candidate_turn] = int(position)
            except ValueError as error:
                raise DriverFailure(
                    "socket-response-invalid",
                    "read-transcript",
                    "daemon transcript contained a non-numeric acceptance position",
                ) from error
            states[candidate_turn] = state["type"]
            if isinstance(state.get("terminal_frontier_id"), str):
                frontiers[candidate_turn] = state["terminal_frontier_id"]
            if isinstance(state.get("accepted_input_id"), str):
                accepted_inputs[candidate_turn] = state["accepted_input_id"]
        elif message_type == "transcript_text_entry":
            entry = message.get("entry")
            index = message.get("entry_index")
            if not isinstance(entry, dict) or not isinstance(index, str):
                continue
            entry_turn = entry.get("turn_id")
            if entry.get("type") == "user" and isinstance(entry_turn, str):
                accepted_input = entry.get("accepted_input_id")
                if isinstance(accepted_input, str):
                    accepted_inputs[entry_turn] = accepted_input
            elif entry.get("type") == "assistant" and isinstance(entry_turn, str):
                assistant_entry_turns[index] = entry_turn
        elif message_type == "transcript_content":
            index = message.get("entry_index")
            fragment = message.get("content_fragment")
            if isinstance(index, str) and isinstance(fragment, str):
                assistant_fragments.setdefault(index, []).append(fragment)
    if selected_turn_id is None:
        candidates = positions.keys() & accepted_inputs.keys()
        selected_turn_id = min(
            candidates,
            key=lambda candidate: (positions[candidate], candidate),
            default=None,
        )
    if selected_turn_id not in positions:
        return TranscriptSnapshot(session_id, None, None, None, None, "")
    assistant_indexes = {
        index
        for index, entry_turn in assistant_entry_turns.items()
        if entry_turn == selected_turn_id
    }
    try:
        ordered_indexes = sorted(assistant_indexes, key=int)
    except ValueError as error:
        raise DriverFailure(
            "socket-response-invalid",
            "read-transcript",
            "daemon transcript contained a non-numeric entry index",
        ) from error
    assistant_text = "".join(
        fragment
        for index in ordered_indexes
        for fragment in assistant_fragments.get(index, [])
    )
    return TranscriptSnapshot(
        session_id=session_id,
        accepted_input_id=accepted_inputs.get(selected_turn_id),
        turn_id=selected_turn_id,
        turn_state=states[selected_turn_id],
        terminal_frontier_id=frontiers.get(selected_turn_id),
        assistant_text=assistant_text,
    )


class ReviewDriver:
    def __init__(
        self,
        github: GitHubBoundary,
        cli: ReviewCliBoundary,
        sessions: SessionBoundary,
        timeout_seconds: float,
        poll_seconds: float,
    ) -> None:
        self.github = github
        self.cli = cli
        self.sessions = sessions
        self.timeout_seconds = timeout_seconds
        self.poll_seconds = poll_seconds

    def run(self, repository: str, pull_request: int) -> AttemptIdentities:
        facts = self.github.read_pull_request(repository, pull_request)
        identities = attempt_identities(facts)
        self.cli.create_target(
            facts, identities.target, stable_id(facts, "create-target-command")
        )
        self.cli.start_orchestration(
            identities.target,
            identities.attempt,
            stable_id(facts, "start-orchestration-command"),
        )
        state = self.cli.read_orchestration_state(identities.attempt)
        if state == "complete":
            return identities
        if state == "awaiting_import":
            self._drive_import(facts, identities)
            state = self.cli.read_orchestration_state(identities.attempt)
        if state == "complete":
            return identities
        if state in {"import_incomplete", "fanout_incomplete"}:
            raise DriverFailure(
                "stage-terminal-unsuccessful",
                state,
                "the durable orchestration attempt cannot advance",
            )
        if state == "awaiting_concerns":
            self._drive_concerns_to_typed_boundary(facts, identities)
        raise DriverFailure(
            "resume-stage-not-implemented",
            state,
            "this state is downstream of the unavailable typed session-output adapter",
        )

    def _drive_import(
        self, facts: PullRequestFacts, attempt: AttemptIdentities
    ) -> None:
        content = (
            f"Import repository evidence for {facts.repository} pull request "
            f"{facts.number} at exact head {facts.head_sha} against base {facts.base_sha}."
        )
        completed = self._drive_session_pass(
            facts=facts,
            stage="import",
            template=STAGE_TEMPLATES["import"],
            workflow="import-external-context",
            statement=f"Import exact review context for pull request {facts.number}.",
            content=content,
        )
        outcome = pass_outcome(completed.turn_state)
        self.cli.complete_pass(completed, outcome)
        context_digest = None
        if outcome == "succeeded":
            context_digest = hashlib.sha256(
                completed.assistant_text.encode("utf-8")
            ).hexdigest()
        self.cli.record_import_outcome(
            attempt.attempt, completed, outcome, context_digest
        )
        if outcome != "succeeded":
            raise DriverFailure(
                "stage-terminal-unsuccessful",
                "import",
                f"import session ended as {completed.turn_state}",
            )

    def _drive_concerns_to_typed_boundary(
        self, facts: PullRequestFacts, attempt: AttemptIdentities
    ) -> None:
        successful: list[str] = []
        for concern, template in CONCERNS:
            completed = self._drive_session_pass(
                facts=facts,
                stage=f"concern:{concern}",
                template=template,
                workflow="read-only-review",
                statement=(
                    f"Review pull request {facts.number} for the {concern} concern."
                ),
                content=(
                    f"Review exact head {facts.head_sha} against base {facts.base_sha}. "
                    "Return findings through the daemon's typed review-result contract."
                ),
            )
            outcome = pass_outcome(completed.turn_state)
            if outcome != "succeeded":
                self.cli.complete_pass(completed, outcome)
                self.cli.record_concern_outcome(
                    attempt.attempt, concern, completed, outcome
                )
                raise DriverFailure(
                    "stage-terminal-unsuccessful",
                    concern,
                    f"concern session ended as {completed.turn_state}",
                )
            successful.append(concern)
        raise DriverFailure(
            "typed-stage-output-unavailable",
            "concerns",
            "the five sessions succeeded, but the implemented daemon exposes no typed "
            "submit_review_findings result; transcript prose cannot be admitted as findings "
            f"(completed: {','.join(successful)})",
        )

    def _drive_session_pass(
        self,
        *,
        facts: PullRequestFacts,
        stage: str,
        template: str,
        workflow: str,
        statement: str,
        content: str,
    ) -> CompletedPass:
        identities = pass_identities(facts, stage)
        session_id = self.sessions.commission(
            facts,
            template,
            identities.commission_command,
            statement,
            content,
        )
        snapshot = self._wait_for(
            session_id,
            stage,
            lambda value: value.accepted_input_id is not None and value.turn_id is not None,
        )
        if snapshot.accepted_input_id is None or snapshot.turn_id is None:
            raise DriverFailure(
                "pass-evidence-invalid",
                stage,
                "commissioned transcript omitted the accepted input or origin turn",
            )
        accepted_input_id = snapshot.accepted_input_id
        origin_turn_id = snapshot.turn_id
        self.cli.start_run(
            facts_target_id(facts),
            identities,
            workflow,
            session_id,
            accepted_input_id,
        )
        snapshot = self._wait_for(
            session_id,
            stage,
            lambda value: value.turn_state in ACTIVE_TURN_STATES
            or value.turn_state in TERMINAL_TURN_STATES,
            turn_id=origin_turn_id,
        )
        try:
            self.cli.activate_pass(identities, origin_turn_id)
        except DriverFailure as error:
            if snapshot.turn_state in TERMINAL_TURN_STATES:
                raise DriverFailure(
                    "pass-activation-window-missed",
                    stage,
                    "the commissioned turn terminalized before its pass could be activated",
                ) from error
            raise
        snapshot = self._wait_for(
            session_id,
            stage,
            lambda value: value.turn_state in TERMINAL_TURN_STATES,
            turn_id=origin_turn_id,
        )
        if snapshot.accepted_input_id is None or snapshot.turn_id is None:
            raise DriverFailure(
                "terminal-evidence-invalid",
                stage,
                "terminal transcript omitted pass ownership identities",
            )
        if snapshot.turn_state is None:
            raise DriverFailure(
                "terminal-evidence-invalid",
                stage,
                "terminal transcript omitted the turn state",
            )
        snapshot = self.sessions.transcript(session_id, origin_turn_id)
        if (
            snapshot.accepted_input_id != accepted_input_id
            or snapshot.turn_id != origin_turn_id
            or snapshot.turn_state not in TERMINAL_TURN_STATES
        ):
            raise DriverFailure(
                "terminal-evidence-invalid",
                stage,
                "the exact pass turn did not re-verify as terminal",
            )
        return CompletedPass(
            identities=identities,
            session_id=session_id,
            accepted_input_id=snapshot.accepted_input_id,
            turn_id=snapshot.turn_id,
            turn_state=snapshot.turn_state,
            terminal_frontier_id=snapshot.terminal_frontier_id,
            assistant_text=snapshot.assistant_text,
        )

    def _wait_for(
        self,
        session_id: str,
        stage: str,
        predicate,
        *,
        turn_id: str | None = None,
    ) -> TranscriptSnapshot:
        deadline = time.monotonic() + self.timeout_seconds
        while True:
            snapshot = self.sessions.transcript(session_id, turn_id)
            if predicate(snapshot):
                return snapshot
            if time.monotonic() >= deadline:
                raise DriverFailure(
                    "stage-timeout",
                    stage,
                    f"session {session_id} did not reach the required state",
                )
            time.sleep(self.poll_seconds)


def facts_target_id(facts: PullRequestFacts) -> str:
    return attempt_identities(facts).target


def pass_outcome(turn_state: str) -> str:
    if turn_state == "completed":
        return "succeeded"
    if turn_state in {"reconciliation_required", "tool_reconciliation_required"}:
        return "blocked"
    if turn_state == "cancelled":
        return "cancelled"
    return "failed"


def run_process(command: Sequence[str], stage: str) -> subprocess.CompletedProcess[str]:
    try:
        result = subprocess.run(
            command,
            check=False,
            capture_output=True,
            text=True,
        )
    except OSError as error:
        raise DriverFailure("command-exec", stage, str(error)) from error
    if result.returncode != 0:
        detail = result.stderr.strip() or result.stdout.strip() or "command failed"
        raise DriverFailure("command-failed", stage, detail)
    return result


def repository_argument(value: str) -> str:
    if REPOSITORY_PATTERN.fullmatch(value) is None:
        raise argparse.ArgumentTypeError("repository must be an OWNER/NAME slug")
    return value


def positive_integer(value: str) -> int:
    try:
        parsed = int(value)
    except ValueError as error:
        raise argparse.ArgumentTypeError("value must be a positive integer") from error
    if parsed <= 0:
        raise argparse.ArgumentTypeError("value must be a positive integer")
    return parsed


def positive_float(value: str) -> float:
    try:
        parsed = float(value)
    except ValueError as error:
        raise argparse.ArgumentTypeError("value must be positive") from error
    if parsed <= 0:
        raise argparse.ArgumentTypeError("value must be positive")
    return parsed


def parser() -> argparse.ArgumentParser:
    argument_parser = argparse.ArgumentParser(
        description="Drive one immutable Signalbox pull-request review attempt."
    )
    argument_parser.add_argument("repository", type=repository_argument)
    argument_parser.add_argument("pull_request", type=positive_integer)
    argument_parser.add_argument("socket", type=Path)
    argument_parser.add_argument(
        "--signalbox-bin",
        default=os.environ.get("SIGNALBOX_BIN", "signalbox"),
        help="signalbox executable (default: $SIGNALBOX_BIN or signalbox on PATH)",
    )
    argument_parser.add_argument("--gh-bin", default="gh")
    argument_parser.add_argument(
        "--timeout-seconds", type=positive_float, default=1800.0
    )
    argument_parser.add_argument("--poll-seconds", type=positive_float, default=1.0)
    return argument_parser


def main(arguments: Sequence[str] | None = None) -> int:
    parsed = parser().parse_args(arguments)
    try:
        driver = ReviewDriver(
            GitHubCli(parsed.gh_bin),
            SignalboxCli(parsed.socket, parsed.signalbox_bin),
            UnixSessionClient(parsed.socket),
            parsed.timeout_seconds,
            parsed.poll_seconds,
        )
        identities = driver.run(parsed.repository, parsed.pull_request)
    except DriverFailure as error:
        print(error.line(), file=sys.stderr)
        return 1
    print(
        f"REVIEW_DRIVER_COMPLETE target={identities.target} attempt={identities.attempt}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

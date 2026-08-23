"""Contract tests for the client-fed review driver."""

from __future__ import annotations

import sys
import unittest
import uuid
from pathlib import Path


sys.path.insert(0, str(Path(__file__).parent))

from review_driver import (  # noqa: E402
    CompletedPass,
    DriverFailure,
    PassIdentities,
    PullRequestFacts,
    ReviewDriver,
    TranscriptSnapshot,
    attempt_identities,
    parser,
)


HEAD_ONE = "1" * 40
HEAD_TWO = "2" * 40
BASE = "a" * 40
REPOSITORY = "sample-owner/sample-repository"
PULL_REQUEST = 41


def pull_request_facts(head_sha: str) -> PullRequestFacts:
    return PullRequestFacts(
        repository=REPOSITORY,
        number=PULL_REQUEST,
        head_sha=head_sha,
        base_sha=BASE,
        head_repository=REPOSITORY,
        head_branch="agent/sample-change",
        base_branch="main",
    )


class FakeGitHub:
    def __init__(self, facts: PullRequestFacts) -> None:
        self.facts = facts

    def read_pull_request(self, repository: str, number: int) -> PullRequestFacts:
        if repository != self.facts.repository or number != self.facts.number:
            raise AssertionError("driver requested a different pull request")
        return self.facts


class FakeReviewCli:
    def __init__(self, *, state: str) -> None:
        self.state = state
        self.targets: dict[str, PullRequestFacts] = {}
        self.attempts: dict[str, str] = {}
        self.started_runs: dict[str, tuple[str, str, str]] = {}
        self.activated_passes: set[str] = set()
        self.raise_after_first_activation = False
        self.activation_interrupted = False
        self.completed_passes: set[str] = set()

    def create_target(
        self, facts: PullRequestFacts, target_id: str, command_id: str
    ) -> None:
        recorded = self.targets.setdefault(target_id, facts)
        if recorded != facts:
            raise AssertionError("target identity was reused for moved facts")

    def start_orchestration(
        self, target_id: str, attempt_id: str, command_id: str
    ) -> None:
        recorded = self.attempts.setdefault(attempt_id, target_id)
        if recorded != target_id:
            raise AssertionError("attempt identity was reused for another target")

    def read_orchestration_state(self, attempt_id: str) -> str:
        return self.state

    def start_run(
        self,
        target_id: str,
        identities: PassIdentities,
        workflow: str,
        session_id: str,
        accepted_input_id: str,
    ) -> None:
        request = (target_id, session_id, accepted_input_id)
        recorded = self.started_runs.setdefault(identities.run, request)
        if recorded != request:
            raise AssertionError("run identity was reused for different evidence")

    def activate_pass(self, identities: PassIdentities, turn_id: str) -> None:
        replay = identities.review_pass in self.activated_passes
        self.activated_passes.add(identities.review_pass)
        if self.raise_after_first_activation and not replay:
            self.activation_interrupted = True
            raise DriverFailure(
                "command-failed", "activate-pass", "response was interrupted"
            )

    def complete_pass(self, completed: CompletedPass, outcome: str) -> None:
        self.completed_passes.add(completed.identities.review_pass)

    def record_import_outcome(
        self,
        attempt_id: str,
        completed: CompletedPass,
        outcome: str,
        context_digest: str | None,
    ) -> None:
        self.state = "complete"

    def record_concern_outcome(
        self, attempt_id: str, concern: str, completed: CompletedPass, outcome: str
    ) -> None:
        raise AssertionError("the resume fixture should complete after import")


class FakeSessions:
    def __init__(self) -> None:
        self.sessions_by_command: dict[str, str] = {}
        self.transcript_reads = 0
        self.terminal_after_interruption = False

    def commission(
        self,
        facts: PullRequestFacts,
        template: str,
        command_id: str,
        statement: str,
        content: str,
    ) -> str:
        session = self.sessions_by_command.setdefault(
            command_id, str(uuid.uuid5(uuid.NAMESPACE_URL, command_id))
        )
        return session

    def transcript(self, session_id: str) -> TranscriptSnapshot:
        self.transcript_reads += 1
        state = "completed" if self.terminal_after_interruption else "active_running"
        frontier = str(uuid.UUID(int=4)) if state == "completed" else None
        return TranscriptSnapshot(
            session_id=session_id,
            accepted_input_id=str(uuid.UUID(int=2)),
            turn_id=str(uuid.UUID(int=3)),
            turn_state=state,
            terminal_frontier_id=frontier,
            assistant_text="imported context",
        )


class UnusedSessions:
    def commission(self, *args, **kwargs) -> str:
        raise AssertionError("a complete attempt must not commission a session")

    def transcript(self, session_id: str) -> TranscriptSnapshot:
        raise AssertionError("a complete attempt must not read a transcript")


class ReviewDriverTests(unittest.TestCase):
    def test_arguments_reject_invalid_repository_and_pull_request(self) -> None:
        with self.assertRaises(SystemExit):
            parser().parse_args(["missing-owner", "41", "/tmp/signalbox.sock"])
        with self.assertRaises(SystemExit):
            parser().parse_args([REPOSITORY, "0", "/tmp/signalbox.sock"])

    def test_resume_after_interrupted_activation_reuses_session_and_attempt(self) -> None:
        facts = pull_request_facts(HEAD_ONE)
        github = FakeGitHub(facts)
        cli = FakeReviewCli(state="awaiting_import")
        cli.raise_after_first_activation = True
        sessions = FakeSessions()
        driver = ReviewDriver(github, cli, sessions, 1.0, 0.001)

        with self.assertRaises(DriverFailure):
            driver.run(REPOSITORY, PULL_REQUEST)
        sessions.terminal_after_interruption = True
        resumed = driver.run(REPOSITORY, PULL_REQUEST)

        expected = attempt_identities(facts)
        self.assertTrue(cli.activation_interrupted)
        self.assertEqual(resumed, expected)
        self.assertEqual(len(cli.targets), 1)
        self.assertEqual(len(cli.attempts), 1)
        self.assertEqual(len(cli.started_runs), 1)
        self.assertEqual(len(sessions.sessions_by_command), 1)
        self.assertEqual(len(cli.completed_passes), 1)

    def test_moved_head_creates_a_new_target_and_attempt(self) -> None:
        first_facts = pull_request_facts(HEAD_ONE)
        second_facts = pull_request_facts(HEAD_TWO)
        github = FakeGitHub(first_facts)
        cli = FakeReviewCli(state="complete")
        driver = ReviewDriver(github, cli, UnusedSessions(), 1.0, 0.001)

        first = driver.run(REPOSITORY, PULL_REQUEST)
        github.facts = second_facts
        second = driver.run(REPOSITORY, PULL_REQUEST)

        self.assertNotEqual(first.target, second.target)
        self.assertNotEqual(first.attempt, second.attempt)
        self.assertEqual(len(cli.targets), 2)
        self.assertEqual(len(cli.attempts), 2)


if __name__ == "__main__":
    unittest.main()
